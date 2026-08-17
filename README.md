# aivyx-checkpoint

Git-ref checkpoint/rollback for agent tool calls.

`GitCheckpointer::detect(cwd, deny_paths)` builds a checkpointer for a
working directory, or returns `None` if it isn't inside a git worktree
(checkpointing then stays disabled for that root — no error, one log
line). `checkpoint(tool_name, cancellation)` snapshots the current
worktree to a shadow ref (`refs/aivyx/checkpoints/<millis>-<seq>`) via
plumbing — a private index, `write-tree`/`commit-tree`/`update-ref` — so
the caller's real HEAD, index, and worktree are never touched. Identical
trees are deduplicated (skipped) automatically. `latest_ref(cancellation)`
returns the most recent checkpoint ref, or `None` if none exist yet.
`restore_to(ref_name, cancellation)` restores the worktree to exactly
match that ref's tree — including deleting files created since the
checkpoint, which a plain `git checkout <ref> -- .` cannot do. A
configurable retention count (`RETAIN`, default 50) prunes the oldest
checkpoint refs after each new one.

`deny_paths` are excluded from every snapshot and every restore via
`:(exclude)` pathspecs (`exclude_pathspecs`, also `pub` for direct reuse
by any other git operation that needs the same carve-out) — without this,
content a sandboxing layer keeps an agent from *reading* could still be
copied into readable `.git` objects simply by existing on disk. Checkpoint
failures are always best-effort: `checkpoint` logs and returns rather than
failing the tool call it's protecting.

`run_git` (the one plumbing-invocation primitive everything above is built
from) and a `test_support` module (`git`/`init_repo` real-git test
fixtures, `#[doc(hidden)]` but plain `pub` so downstream crates' own tests
can use them) are both exported for direct reuse — `aivyx-coder`'s own
`wiki.rs` and several of its git tools' test suites use them directly.

Extracted 2026-08-18 from `aivyx-coder`'s own `aivyx-tools` crate, which
now depends on this crate instead of maintaining its own copy (see that
repo's `CLAUDE.md` for the migration). `aivyx` (the flagship Personal
Assistant) does not yet adopt this crate — that integration is separate,
explicit follow-on work, not assumed here.

See `docs/superpowers/specs/2026-08-18-aivyx-checkpoint-design.md` in the
`aivyx-ecosystem` repo for the full design rationale.
