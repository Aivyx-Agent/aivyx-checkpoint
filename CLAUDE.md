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
(migrated 2026-08-18). `aivyx` does not yet — that integration is
separate follow-on work, tracked outside this repo.

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

`refs/aivyx/checkpoints/{millis:013}-{seq:04}` — zero-padded millis sorts
lexically == chronologically, which both `prune` (retention cutoff) and
`latest_ref` (`.rfind`) rely on. `RETAIN` (default 50) is the number kept;
`prune` runs after every successful checkpoint and deletes the oldest refs
beyond that count — deleting the ref is enough, the underlying commit/tree
objects become unreferenced and age out via normal `git gc`.

## Where to look next

- `README.md` — quick orientation and the design-doc pointer.
- `aivyx-ecosystem/docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md`
  — the full design: why this was extracted, why `aivyx-coder`'s migration
  is part of the same project (unlike `aivyx-recall`/`aivyx-kvcache`,
  which shipped standalone with no consumer yet), and `aivyx`'s adoption
  as explicit deferred follow-on.
