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

`refs/aivyx/checkpoints/{millis:013}-{pid}-{seq:04}` — zero-padded millis
sorts lexically == chronologically, which both `prune` (retention cutoff)
and `latest_ref` (`.rfind`) rely on; the pid comes right after it
specifically so it never disturbs that ordering. `pid` disambiguates
checkpoints minted by two different processes sharing a repo within the
same millisecond — `seq` alone can't, since each process's `GitCheckpointer`
starts its own `seq` at 0. Refs written before the pid was added
(`{millis}-{seq}`, no pid segment) still list and prune correctly: neither
function parses the ref name beyond treating the whole thing as one
lexically-sortable string. `RETAIN` (default 50) is the number kept;
`prune` runs after every successful checkpoint and deletes the oldest refs
beyond that count — deleting the ref is enough, the underlying commit/tree
objects become unreferenced and age out via normal `git gc`.

### Private-index lock and config-query reliability

`checkpoint`/`restore_to` both stage into the private index
(`<git-dir>/aivyx/index`) via `GitCheckpointer::private_index`, which
clears a leftover `index.lock` once it's older than `GIT_TIMEOUT` — the
lock git itself leaves behind when a checkpoint is cancelled or times out
mid-`git add`. Nothing else ever touches this index, so an old-enough lock
can't belong to a call still in progress; without clearing it, every later
checkpoint fails against the same lock, silently, forever (`checkpoint`'s
own best-effort contract swallows the error).

`run_git` neutralises repo-configured filter drivers before every
invocation (`repo_program_overrides`) by querying `git config
--local`/`--worktree` for them first. That query's own failure is split
three ways: no repository anywhere in `cwd`'s ancestry (`finds_a_repository`
— the bootstrapping case, e.g. `GitCheckpointer::detect`'s first probe or
`test_support::init_repo`'s `git init` call) skips the query entirely and
proceeds with no overrides, since there's nothing to query yet; exit status
1 (`--get-regexp`'s "no matching key") means a real repo with no filters
configured, also not an error; anything else (a malformed or unreadable
local config, the query timing out) is a real failure and now propagates
as an `Err` from `run_git` itself, rather than being silently treated the
same as "no filters to neutralise" — the previous behavior, which risked
running a repo-configured filter unconfined if the query meant to detect it
had itself failed to read.

## Where to look next

- `README.md` — quick orientation and the design-doc pointer.
- `aivyx-ecosystem/docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md`
  — the full design: why this was extracted, and why `aivyx-coder`'s
  migration was part of the same project (unlike `aivyx-recall`/
  `aivyx-kvcache`, which shipped standalone with no consumer yet).
  `aivyx`'s own adoption (2026-08-18/19, extended 2026-08-20) has its
  own design docs in that repo — see `aivyx-ecosystem/ROADMAP.md`'s
  `aivyx-checkpoint` entry.
