# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working
with code in this repository.

## What this is

`aivyx-checkpoint` is a small, config-agnostic git-ref checkpoint/rollback
crate: `GitCheckpointer` snapshots a working directory's worktree to a
shadow `refs/aivyx/checkpoints/*` ref before a mutating action, and can
restore it on demand — real undo without ever touching the caller's real
HEAD, index, or branch. It exists so `aivyx-coder` and `aivyx` (the
flagship Personal Assistant) can share one implementation of the same
recoverability primitive, rather than each maintaining — and potentially
drifting on — its own copy. See `README.md` and
`aivyx-ecosystem/docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md`
for the full rationale — this file only covers what's specific to working
in this repo's code.

`aivyx-coder`'s own `aivyx-tools` crate depends on this crate today
(migrated 2026-08-18). **`aivyx` also depends on this crate**, adopted
2026-08-18/19 and extended to every real `ConcreteAgent` construction
site by 2026-08-20 (`fs.write`/`fs.delete`/`shell.exec`, `git.commit`,
`workspace.*` all checkpoint through it). Both real consumers now
depend on this crate.

## Build, test, lint

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt
```

Single crate, no workspace — no `-p` flag needed. Single test:
`cargo test <test_name>`. All tests use real git fixtures (via
`test_support::init_repo`) rather than mocking git — there is no in-memory
git stand-in anywhere in this crate.

## Architecture

Single file, `lib.rs`:

- `GitCheckpointer` — the checkpoint/restore state machine. Its private
  index at `<git-dir>/aivyx/index` (via the `GIT_INDEX_FILE` env var) is
  the load-bearing trick that keeps every operation here from ever
  touching the caller's real index — both `checkpoint_inner` and
  `restore_to` stage into it, never the default index at `<git-dir>/index`.
- `exclude_pathspecs`/`run_git` — `pub`, not just `pub(crate)`, because
  each has a real consumer beyond this crate: `aivyx-coder`'s `wiki.rs`
  calls `run_git` directly for its own unrelated git plumbing, and its
  `git_read`/`git_commit` tools call `exclude_pathspecs` directly to
  build the same deny-path carve-outs for their own git invocations.
- `test_support` — real-git fixture helpers (`git`, `init_repo`),
  deliberately **not** `#[cfg(test)]` (that attribute doesn't survive
  across a crate boundary, so a `#[cfg(test)]`-gated item would be
  invisible to a downstream crate's own tests) — `#[doc(hidden)] pub`
  instead. `aivyx-coder`'s `aivyx-tools` crate uses this module directly
  in six of its own test modules.

### Checkpoint ref naming and retention

`refs/aivyx/checkpoints/{millis:013}-{pid:07}-{seq:04}` — zero-padded
millis sorts lexically == chronologically, which `prune` (retention
cutoff) relies on; the zero-padded pid comes next,
so refs sharing a millisecond still compare consistently. The pid keeps
checkpoints from two processes sharing a repo distinct; `seq` alone can't,
since each process's `GitCheckpointer` starts its own `seq` at 0. Shorter
ref names written by older versions (no pid, or an unpadded one) still list
and prune correctly alongside current ones: neither function parses the
name beyond treating it as one lexically-sortable string. `RETAIN` (default
50) is the number kept; `prune` runs after every successful checkpoint and
deletes the oldest refs beyond that count — deleting the ref is enough, the
underlying commit/tree objects become unreferenced and age out via normal
`git gc`.

`latest_ref`, `checkpoints()` and `restore_to` never read refs back from
the repository: each `GitCheckpointer` records the checkpoints it took
(ref, commit, tree) in memory, at most `RETAIN` of them, and those are the
only names `restore_to` accepts — restored to the recorded commit even if
the ref has since moved. A full object id is accepted too (aivyx-coder's
`/undo` keeps commit ids across sessions). Refs under
`refs/aivyx/checkpoints/` can be written by a confined process, so a name
that merely sorts last must never be taken for a checkpoint. Refs are
created with `update-ref --no-deref <ref> <commit> ""` (fails if anything
is already there) and pruned with `--no-deref -d`, so a planted symbolic
ref is never followed to a branch.

### Generated files

`with_generated_ignore(patterns)` (opt-in; `aivyx-coder` passes its
`[git] ignore` list) keeps untracked build output and caches out of
snapshots in a project whose `.gitignore` doesn't cover them. Before every
`add -A` (`stage_worktree`, shared by checkpoint and restore),
`untracked_generated` asks `git ls-files --others --ignored --exclude=...`
against the user's own index — twice: once with `--directory` so `target/`
is one entry, once without (skipping those directories) for generated
files inside a not-yet-tracked directory, which `--directory` hides. The
paths are removed from the private index (`rm --cached -f`, since entries an
earlier snapshot staged would otherwise keep being updated) and excluded
from `add -A`. Restore therefore leaves them alone, like gitignored files.

### Private-index lock

`checkpoint`/`restore_to` both stage into the private index
(`<git-dir>/aivyx/index`) via `GitCheckpointer::private_index`, which
clears a leftover `index.lock` once it's older than `GIT_TIMEOUT` — the
lock git itself leaves behind when a checkpoint is cancelled or times out
mid-`git add`. Nothing else ever touches this index, so an old-enough lock
can't belong to a call still in progress; without clearing it, every later
checkpoint fails against the same lock, silently, forever (`checkpoint`'s
own best-effort contract swallows the error).

### Repository state is not trusted (2026-10-28)

All git here runs unconfined, and `.git` lies inside the directory a
confined process may write — so its config, refs and paths must not steer
that git. Two layers, both on every invocation:

1. `repo_program_overrides` lists the config git will use (`git config
   --list --includes --show-scope --show-origin -z`, same `cwd` and `envs`
   as the real invocation, `LC_ALL=C`) and `check_repo_config` checks every
   entry whose scope is the repository's (`local`, `worktree`, anything not
   `system`/`global`/`command`) or whose file lies inside `cwd` against
   `allowed_repo_setting`, an **allow-list**. Anything else is an `Err`
   naming the keys (never values — an include can read any file). A failed
   listing (malformed config, timeout) is an `Err` too. Outside any
   repository the listing has only the user's config: the bootstrapping
   case (`detect`, `init_repo`'s `git init`). Add to the allow-list only
   settings that cannot run a program, move the work tree, read outside it
   or reach a remote, for any command run through `run_git` or
   `unconfined_git_args_blocking` (`gc.recentObjectsHook` is why `gc.*`
   is not a wildcard).
2. `SAFE_GIT_CONFIG` (`-c` fsmonitor/hooksPath/gpgSign off,
   `protocol.allow=never`, `submodule.recurse=false`), `HARDENED_ENV`
   (`GIT_NO_REPLACE_OBJECTS`, `GIT_NO_LAZY_FETCH`, `GIT_TERMINAL_PROMPT=0`)
   and `repo_overrides`, which empties every program the repository's
   own config names (filter and diff drivers, `diff.external`,
   `gpg.program`) — those keys are allowed *only* because of this. This
   is the older deny-list, kept under the allow-list; the
   `without_the_allow_list_*` tests switch the allow-list off
   (`ACCEPT_ANY_REPO_CONFIG`, test-only) to prove it holds alone.

`GitCheckpointer` adds: `locate` (at `detect`) canonicalises the git dir,
common dir and work tree, requires `cwd` inside the work tree and the git
dir to be `<work tree>/.git` or a linked worktree whose `gitdir` points
back; `git()` passes `GIT_DIR`/`GIT_WORK_TREE` explicitly; `check_layout`
(before each operation, and again before `update-ref`) refuses symlinked
or moved git/common dirs, symlinks at `aivyx/`, `objects/` and its fan-out
dirs, `refs/aivyx/checkpoints/` and `logs/refs/aivyx/checkpoints/`, and
`objects/info/alternates`. `private_index` creates `aivyx/` with mode 0700,
requires the git dir's owner, and removes only a regular-file stale lock.
The checks are re-run per call because the confined process can change
`.git` between calls; a confined process still running *during* a
checkpoint could race them (check-then-use), which is why the layer-2
overrides stay.

`unconfined_git_args_blocking` returns `Result`: an `Err` means don't run
git at all.

## Where to look next

- `README.md` — quick orientation and the design-doc pointer.
- `aivyx-ecosystem/docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md`
  — the full design: why this was extracted, and why `aivyx-coder`'s
  migration was part of the same project (unlike `aivyx-recall`/
  `aivyx-kvcache`, which shipped standalone with no consumer yet).
  `aivyx`'s own adoption (2026-08-18/19, extended 2026-08-20) has its
  own design docs in that repo — see `aivyx-ecosystem/ROADMAP.md`'s
  `aivyx-checkpoint` entry.
