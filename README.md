# aivyx-checkpoint

[![CI](https://github.com/Aivyx-Agent/aivyx-checkpoint/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/Aivyx-Agent/aivyx-checkpoint/actions/workflows/ci.yml)
[![License: BUSL-1.1](https://img.shields.io/badge/license-BUSL--1.1-blue.svg)](LICENSE)

Git-ref checkpoint/rollback for agent tool calls.

`GitCheckpointer::detect(cwd, deny_paths)` builds a checkpointer for a
working directory, or returns `None` if it isn't inside a git worktree
(checkpointing then stays disabled for that root — no error, one log
line). `checkpoint(tool_name, cancellation)` snapshots the current
worktree to a shadow ref (`refs/aivyx/checkpoints/<millis>-<pid>-<seq>`)
via plumbing — a private index, `write-tree`/`commit-tree`/`update-ref` —
so the caller's real HEAD, index, and worktree are never touched. The pid
disambiguates refs minted by different processes sharing one repo within
the same millisecond; refs written before this (`<millis>-<seq>`, no pid)
are still listed and pruned correctly, since both forms sort lexically by
their leading millis field. Identical trees are deduplicated (skipped)
automatically. `latest_ref(cancellation)` returns the most recent
checkpoint ref, or `None` if none exist yet. `restore_to(ref_name,
cancellation)` restores the worktree to exactly match that ref's tree —
including deleting files created since the checkpoint, which a plain `git
checkout <ref> -- .` cannot do. A configurable retention count (`RETAIN`,
default 50) prunes the oldest checkpoint refs after each new one.

`checkpoint`/`restore_to` clear a leftover `<git-dir>/aivyx/index.lock`
before staging, but only once it's older than the crate's own git timeout
— a checkpoint cancelled or timed out mid-`git add` leaves that lock
behind, which would otherwise fail every later checkpoint (this session
and future ones) silently forever, since nothing else ever touches this
private index.

`deny_paths` are excluded from every snapshot and every restore via
`exclude_pathspecs`, also `pub` for direct reuse by any other git
operation that needs the same carve-out. A path entry (absolute, inside
`cwd`) becomes a `:(exclude)` pathspec; a *bare* entry — a single path
component, possibly with glob characters, e.g. `.env`, `.env.*`, `*.pem`,
`id_rsa` — isn't a path at all, so it becomes a recursive glob exclusion
(`:(exclude,glob)**/<pattern>`) matching that basename anywhere in the
tree, the same "bare pattern" notion `aivyx-coder` uses for its own
deny-paths matching. Without this, content a sandboxing layer keeps an
agent from *reading* could still be copied into readable `.git` objects
simply by existing on disk. Checkpoint failures are always best-effort by
default: `checkpoint` logs and returns rather than failing the tool call
it's protecting; `try_checkpoint` does the identical work but returns the
`Result` instead, for a caller that wants to know a checkpoint failed.

`run_git` (the one plumbing-invocation primitive everything above is built
from) and a `test_support` module (`git`/`init_repo` real-git test
fixtures, `#[doc(hidden)]` but plain `pub` so downstream crates' own tests
can use them) are both exported for direct reuse — `aivyx-coder`'s own
`wiki.rs` and several of its git tools' test suites use them directly.

Extracted 2026-08-18 from `aivyx-coder`'s own `aivyx-tools` crate, which
now depends on this crate instead of maintaining its own copy (see that
repo's `CLAUDE.md` for the migration). **`aivyx` (the flagship Personal
Assistant) adopted this crate 2026-08-18/19**, extended to every real
`ConcreteAgent` construction site (9 of 9, except one singular-session
function with zero production callers) by 2026-08-20 — `fs.write`/
`fs.delete`/`shell.exec`, `git.commit`, and `workspace.*` all checkpoint
through it today. Both consumers now depend on this crate.

See `docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md` in the
`aivyx-ecosystem` repo for the full design rationale.
