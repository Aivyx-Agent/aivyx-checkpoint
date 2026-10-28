# aivyx-checkpoint

[![CI](https://github.com/Aivyx-Agent/aivyx-checkpoint/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/Aivyx-Agent/aivyx-checkpoint/actions/workflows/ci.yml)
[![License: BUSL-1.1](https://img.shields.io/badge/license-BUSL--1.1-blue.svg)](LICENSE)

Git-ref checkpoint/rollback for agent tool calls.

`GitCheckpointer::detect(cwd, deny_paths)` builds a checkpointer for a
working directory, or returns `None` if it isn't inside a git worktree
(checkpointing then stays disabled for that root — no error, one log
line). `checkpoint(tool_name, cancellation)` snapshots the current
worktree to a shadow ref (`refs/aivyx/checkpoints/<millis>-<pid>-<seq>`,
`pid` zero-padded to a fixed width so same-millisecond refs always compare
consistently) via plumbing — a private index, `write-tree`/`commit-tree`/
`update-ref` — so the caller's real HEAD, index, and worktree are never
touched. The pid disambiguates refs minted by different processes sharing
one repo within the same millisecond; refs in either older format this
crate has produced (`<millis>-<seq>`, no pid; or `<millis>-<pid>-<seq>`
with an unpadded pid) are still listed and pruned correctly alongside
current ones, since every form sorts lexically by its leading millis
field. Identical trees are deduplicated (skipped)
automatically. `latest_ref(cancellation)` returns the most recent
checkpoint ref *this checkpointer* took, or `None` if it hasn't taken one
yet, and `checkpoints()` lists them with the commit and tree each one
wrote — recorded in memory, never read back from `refs/aivyx/`.
`restore_to(ref_name, cancellation)` restores the worktree to exactly match
that checkpoint's recorded commit — including deleting files created since
the checkpoint, which a plain `git checkout <ref> -- .` cannot do. It
accepts only a checkpoint ref this checkpointer took, or a full object id
the caller kept itself; any other name is an error. A configurable retention count (`RETAIN`,
default 50) prunes the oldest checkpoint refs after each new one.

`checkpoint`/`restore_to` clear a leftover `<git-dir>/aivyx/index.lock`
before staging, but only once it's older than the crate's own git timeout
— a checkpoint cancelled or timed out mid-`git add` leaves that lock
behind, which would otherwise fail every later checkpoint (this session
and future ones) silently forever, since nothing else ever touches this
private index.

### Repository state is not trusted

Checkpoint git runs outside the sandbox, but `.git` lies inside the
directory a sandboxed command may write. So the repository's own config
(`.git/config`, `config.worktree`, anything they include) may hold only
settings on an allow-list of known-harmless ones — remotes, branches,
user identity, line-ending and index options, LFS and sparse-checkout
bookkeeping and the like. Anything else (a setting that can run a program,
move the work tree or reach a remote, an include, an unknown extension)
makes `run_git` and `unconfined_git_args_blocking` return an error naming
the setting, and `detect` return `None` (`detect_outcome` returns
`DetectOutcome::Refused(reason)` instead, so a caller can tell the user that
undo is off for this repository, rather than treating it as "not a git
repository"); checkpoints resume once the
setting is removed. The check runs before every git invocation. As a
second layer, every invocation switches off fsmonitor, hooks, signing,
network transport and submodule recursion, ignores replace refs, disables
lazy fetching and switches off every program the repository's own config
names (filter and diff drivers, signing programs) — git-crypt and textconv
setups keep their checkpoints, but their programs never run here. The
user's own global and system config stay trusted (unless the file lies
inside the work tree), so git-lfs and a global excludes file keep working.

A `GitCheckpointer` passes the git directory and work tree it found at
detection to every git explicitly, and before each operation checks that
`.git/aivyx`, the object directories and the checkpoint ref and reflog
directories are real directories, not symbolic links, and that the
repository borrows no objects from elsewhere. Repositories this refuses:
partial clones, submodule checkouts (whose config sets `core.worktree`),
repositories sharing objects through `objects/info/alternates`, and git
directories that are neither `<work tree>/.git` nor a linked worktree of
it.

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
