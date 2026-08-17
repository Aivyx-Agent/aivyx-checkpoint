//! Git-ref checkpoint/rollback for agent tool calls.
//!
//! Before each approved mutating tool call, the worktree is snapshotted to
//! a commit object via plumbing — a private index at `.git/aivyx/index`
//! plus `write-tree`/`commit-tree`/`update-ref` — so the user's HEAD,
//! index, and worktree are never touched, and any agent change (including
//! arbitrary shell-command effects) can be rewound with plain git commands:
//! `git log refs/aivyx/checkpoints/...`, `git checkout <ref> -- <path>`.
//!
//! Best-effort by design: a checkpoint failure logs a warning and never
//! blocks the tool call — it's a safety net, not a gate.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Checkpoints kept before the oldest are pruned. Deleting the ref is
/// enough — the snapshot's objects become unreferenced and age out via
/// normal `git gc`.
const RETAIN: usize = 50;

/// Plumbing commands are local-only and fast; anything slower than this is
/// a wedged repo, not a slow one.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct GitCheckpointer {
    cwd: PathBuf,
    /// `--absolute-git-dir`, resolved once at detection.
    git_dir: PathBuf,
    deny_paths: Vec<PathBuf>,
    /// Tree oid of the most recent checkpoint, for skipping no-op
    /// snapshots (several mutations often land between identical trees,
    /// e.g. a failed edit).
    last_tree: Mutex<Option<String>>,
    /// Disambiguates checkpoints created within the same millisecond.
    seq: AtomicU64,
    retain: usize,
}

impl GitCheckpointer {
    /// `None` (with one log line) when `cwd` isn't inside a git worktree —
    /// checkpointing is then disabled for the whole session rather than
    /// warning on every mutation.
    pub async fn detect(cwd: &Path, deny_paths: Vec<PathBuf>) -> Option<Self> {
        let git_dir = match run_git(cwd, &["rev-parse", "--absolute-git-dir"], &[]).await {
            Ok(out) => PathBuf::from(out.trim()),
            Err(err) => {
                tracing::info!(
                    cwd = %cwd.display(),
                    reason = %err,
                    "not a git repository — worktree checkpointing disabled"
                );
                return None;
            }
        };
        Some(Self {
            cwd: cwd.to_path_buf(),
            git_dir,
            deny_paths,
            last_tree: Mutex::new(None),
            seq: AtomicU64::new(0),
            retain: RETAIN,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_retain(&mut self, retain: usize) {
        self.retain = retain;
    }

    /// Snapshots the current worktree. Never fails the caller: every error
    /// path logs and returns.
    pub async fn checkpoint(&self, tool_name: &str, cancellation: &CancellationToken) {
        if let Err(err) = self.checkpoint_inner(tool_name, cancellation).await {
            tracing::warn!(tool = %tool_name, error = %err, "checkpoint failed (tool call proceeds)");
        }
    }

    async fn checkpoint_inner(
        &self,
        tool_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let index_dir = self.git_dir.join("aivyx");
        std::fs::create_dir_all(&index_dir).map_err(|e| e.to_string())?;
        let index = index_dir.join("index");
        let index_env: Vec<(&str, &str)> =
            vec![("GIT_INDEX_FILE", index.to_str().ok_or("non-utf8 git dir")?)];

        // Stage the whole worktree into the private index. deny_paths are
        // excluded via pathspecs — without this, denied content (which
        // Landlock carves out of the kernel sandbox) would be copied into
        // readable .git objects, and `git show <checkpoint>:<denied-file>`
        // would read it straight through the sandbox.
        let mut add_args: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        add_args.extend(exclude_pathspecs(&self.cwd, &self.deny_paths));
        self.git(&add_args, &index_env, cancellation).await?;

        let write_tree_args = vec!["write-tree".to_string()];
        let tree = self
            .git(&write_tree_args, &index_env, cancellation)
            .await?
            .trim()
            .to_string();

        {
            let mut last = self.last_tree.lock().unwrap();
            if last.as_deref() == Some(tree.as_str()) {
                return Ok(()); // identical to the previous checkpoint
            }
            *last = Some(tree.clone());
        }

        // Synthetic identity: checkpoints must work on machines with no
        // git identity configured, and shouldn't impersonate the user.
        let identity: Vec<(&str, &str)> = vec![
            ("GIT_AUTHOR_NAME", "aivyx"),
            ("GIT_AUTHOR_EMAIL", "checkpoint@aivyx.invalid"),
            ("GIT_COMMITTER_NAME", "aivyx"),
            ("GIT_COMMITTER_EMAIL", "checkpoint@aivyx.invalid"),
        ];
        let message = format!("aivyx checkpoint before {tool_name}");
        let mut commit_args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), message];
        // Parent on HEAD when it exists (normal case) so `git log <ref>`
        // shows the checkpoint in context; an unborn branch just gets a
        // parentless snapshot.
        let head_args: Vec<String> = vec!["rev-parse".into(), "--verify".into(), "HEAD".into()];
        if let Ok(head) = self.git(&head_args, &[], cancellation).await {
            commit_args.push("-p".into());
            commit_args.push(head.trim().to_string());
        }
        let commit = self
            .git(&commit_args, &identity, cancellation)
            .await?
            .trim()
            .to_string();

        // Zero-padded millis sort lexically == chronologically (until the
        // year 2286), which is what the retention pass below relies on.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let ref_name = format!("refs/aivyx/checkpoints/{millis:013}-{seq:04}");
        let update_args: Vec<String> = vec!["update-ref".into(), ref_name.clone(), commit];
        self.git(&update_args, &[], cancellation).await?;
        tracing::info!(tool = %tool_name, r#ref = %ref_name, "worktree checkpoint saved");

        self.prune(cancellation).await
    }

    async fn prune(&self, cancellation: &CancellationToken) -> Result<(), String> {
        let list_args: Vec<String> = vec![
            "for-each-ref".into(),
            "--format=%(refname)".into(),
            "refs/aivyx/checkpoints/".into(),
        ];
        let refs = self.git(&list_args, &[], cancellation).await?;
        let refs: Vec<&str> = refs.lines().filter(|l| !l.is_empty()).collect();
        if refs.len() <= self.retain {
            return Ok(());
        }
        for stale in &refs[..refs.len() - self.retain] {
            let delete_args: Vec<String> =
                vec!["update-ref".into(), "-d".into(), (*stale).to_string()];
            self.git(&delete_args, &[], cancellation).await?;
        }
        Ok(())
    }

    /// The most recent checkpoint ref, or `None` if none have been taken
    /// yet. Reuses `for-each-ref`'s default lexical sort — checkpoint ref
    /// names are zero-padded-millis-prefixed, so lexical order is
    /// chronological order, the same property `prune` above already relies
    /// on for its retention cutoff.
    pub async fn latest_ref(&self, cancellation: &CancellationToken) -> Option<String> {
        let list_args: Vec<String> = vec![
            "for-each-ref".into(),
            "--format=%(refname)".into(),
            "refs/aivyx/checkpoints/".into(),
        ];
        let refs = self.git(&list_args, &[], cancellation).await.ok()?;
        refs.lines().rfind(|l| !l.is_empty()).map(str::to_string)
    }

    /// Restores the worktree to exactly match `ref_name`'s tree — including
    /// deleting files created since that checkpoint, which a plain
    /// `git checkout <ref> -- .` would not do. Uses the same private index
    /// checkpointing itself uses (`GIT_INDEX_FILE`-scoped), never touching
    /// the user's real index, HEAD, or branch. `deny_paths` are never
    /// touched, matching what checkpointing itself excludes. Note this
    /// "exactly match" promise inherits the same limitation checkpointing
    /// has for gitignored paths: `git add -A` (no `--force`) never stages
    /// ignored content, so a gitignored file created since the checkpoint
    /// (e.g. a stray `target/` artifact or `.env`) is neither captured by
    /// checkpoints nor removed by restore — it silently survives.
    pub async fn restore_to(
        &self,
        ref_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let index_dir = self.git_dir.join("aivyx");
        std::fs::create_dir_all(&index_dir).map_err(|e| e.to_string())?;
        let index = index_dir.join("index");
        let index_env: Vec<(&str, &str)> =
            vec![("GIT_INDEX_FILE", index.to_str().ok_or("non-utf8 git dir")?)];

        // Stage the CURRENT (post-experiment, possibly broken) worktree
        // into the private index first, so read-tree below knows what to
        // remove as well as what to restore — its deletion logic diffs the
        // index it's resetting FROM against the tree it's resetting TO.
        // deny_paths are excluded exactly as checkpoint_inner excludes them:
        // a checkpoint's tree never contains a deny-listed path, so without
        // this exclusion a deny-listed file that currently exists on disk
        // would show up as "present in FROM-index, absent from target tree"
        // and get deleted by read-tree below.
        let mut add_args: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        add_args.extend(exclude_pathspecs(&self.cwd, &self.deny_paths));
        self.git(&add_args, &index_env, cancellation).await?;

        let reset_args: Vec<String> = vec![
            "read-tree".into(),
            "--reset".into(),
            "-u".into(),
            ref_name.to_string(),
        ];
        self.git(&reset_args, &index_env, cancellation).await?;

        // The dedup cache no longer reflects the worktree (which just
        // changed out from under it) — invalidate rather than compute the
        // restored tree's oid; a harmless extra checkpoint next time beats
        // a false "identical, skip it" that would silently miss a real
        // change.
        *self.last_tree.lock().unwrap() = None;
        Ok(())
    }

    async fn git(
        &self,
        args: &[String],
        envs: &[(&str, &str)],
        cancellation: &CancellationToken,
    ) -> Result<String, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        tokio::select! {
            result = run_git(&self.cwd, &args, envs) => result,
            _ = cancellation.cancelled() => Err("cancelled".to_string()),
        }
    }
}

/// Builds `:(exclude)` pathspecs for every deny path inside `cwd`, relative
/// to `cwd` (git resolves pathspecs against the command's working
/// directory). `pub` because it has consumers beyond this file: any git
/// operation that sweeps "everything under the worktree" needs the same
/// carve-outs — `aivyx-coder`'s own `git_read`/`git_commit` tools call this
/// directly, cross-crate, from their own production code.
pub fn exclude_pathspecs(cwd: &Path, deny_paths: &[PathBuf]) -> Vec<String> {
    deny_paths
        .iter()
        .filter_map(|denied| denied.strip_prefix(cwd).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(|rel| format!(":(exclude){}", rel.display()))
        .collect()
}

/// One plumbing invocation: trusted fixed argv (never model-controlled),
/// writing only under `.git`, so it runs unconfined; `kill_on_drop` +
/// timeout bound it instead. `pub` because it has a consumer beyond this
/// file: `aivyx-coder`'s own `wiki.rs` module calls this directly,
/// cross-crate, for its own (unrelated) git plumbing needs.
pub async fn run_git(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }

    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("git {} timed out", args.first().unwrap_or(&"")))?
        .map_err(|e| format!("failed to run git: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Real-git test fixtures for exercising `GitCheckpointer` and its
/// consumers' own git-based tests. Deliberately **not** `#[cfg(test)]`:
/// that attribute only applies while this crate's own tests compile, so a
/// `#[cfg(test)]`-gated item is invisible to a *different* crate's tests
/// even under `cargo test` there — and this module has consumers in
/// exactly that position (`aivyx-coder`'s `aivyx-tools` crate). `pub` for
/// the same cross-crate reason; `#[doc(hidden)]` because it's a shared
/// test fixture, not part of this crate's real API surface.
#[doc(hidden)]
pub mod test_support {
    use super::run_git;
    use std::path::Path;

    pub async fn git(dir: &Path, args: &[&str]) -> String {
        run_git(dir, args, &[])
            .await
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"))
    }

    pub async fn init_repo(dir: &Path) {
        for argv in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@test.invalid"],
        ] {
            git(dir, &argv).await;
        }
        std::fs::write(dir.join("tracked.txt"), "v1\n").unwrap();
        git(dir, &["add", "-A"]).await;
        git(dir, &["commit", "-q", "-m", "initial"]).await;
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::init_repo;
    use super::*;

    async fn checkpoint_refs(dir: &Path) -> Vec<String> {
        run_git(
            dir,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/aivyx/checkpoints/",
            ],
            &[],
        )
        .await
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
    }

    #[tokio::test]
    async fn detect_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GitCheckpointer::detect(dir.path(), vec![]).await.is_none());
    }

    #[tokio::test]
    async fn checkpoint_creates_a_ref_without_touching_user_state() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let head_before = run_git(dir.path(), &["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        let status_before = run_git(dir.path(), &["status", "--porcelain"], &[])
            .await
            .unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;

        let refs = checkpoint_refs(dir.path()).await;
        assert_eq!(refs.len(), 1, "one checkpoint ref expected");

        // Snapshot captured both the modification and the untracked file...
        let tree = run_git(dir.path(), &["ls-tree", "-r", "--name-only", &refs[0]], &[])
            .await
            .unwrap();
        assert!(tree.contains("tracked.txt"));
        assert!(tree.contains("untracked.txt"));
        let content = run_git(
            dir.path(),
            &["show", &format!("{}:tracked.txt", refs[0])],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(content, "modified\n");

        // ...while HEAD, the index, and the worktree state are untouched.
        let head_after = run_git(dir.path(), &["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        let status_after = run_git(dir.path(), &["status", "--porcelain"], &[])
            .await
            .unwrap();
        assert_eq!(head_before, head_after);
        assert_eq!(status_before, status_after);
    }

    #[tokio::test]
    async fn denied_subpaths_are_excluded_from_the_snapshot() {
        // Regression guard for the sandbox bypass: without the exclude
        // pathspecs, `git show <checkpoint>:secret/key` would read denied
        // content through .git even though Landlock blocks the file itself.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let secret_dir = dir.path().join("secret");
        std::fs::create_dir(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("key"), "TOP-SECRET\n").unwrap();
        std::fs::write(dir.path().join("public.txt"), "fine\n").unwrap();

        let deny = vec![dir.path().canonicalize().unwrap().join("secret")];
        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, deny).await.unwrap();
        cp.checkpoint("run_shell", &CancellationToken::new()).await;

        let refs = checkpoint_refs(dir.path()).await;
        let tree = run_git(dir.path(), &["ls-tree", "-r", "--name-only", &refs[0]], &[])
            .await
            .unwrap();
        assert!(tree.contains("public.txt"));
        assert!(
            !tree.contains("secret"),
            "denied subtree leaked into snapshot: {tree}"
        );
    }

    #[tokio::test]
    async fn identical_trees_are_not_checkpointed_twice() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        cp.checkpoint("edit_file", &CancellationToken::new()).await;

        assert_eq!(checkpoint_refs(dir.path()).await.len(), 1);

        // A real change checkpoints again.
        std::fs::write(dir.path().join("tracked.txt"), "modified again\n").unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        assert_eq!(checkpoint_refs(dir.path()).await.len(), 2);
    }

    #[tokio::test]
    async fn retention_prunes_the_oldest_refs() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;

        let mut cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.set_retain(3);
        for i in 0..5 {
            std::fs::write(dir.path().join("tracked.txt"), format!("v{i}\n")).unwrap();
            cp.checkpoint("write_file", &CancellationToken::new()).await;
        }

        let refs = checkpoint_refs(dir.path()).await;
        assert_eq!(
            refs.len(),
            3,
            "retention should keep the newest 3: {refs:?}"
        );
        // The survivors hold the newest content.
        let content = run_git(
            dir.path(),
            &["show", &format!("{}:tracked.txt", refs.last().unwrap())],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(content, "v4\n");
    }

    #[tokio::test]
    async fn latest_ref_returns_the_most_recent_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        assert!(
            cp.latest_ref(&CancellationToken::new()).await.is_none(),
            "no checkpoints taken yet"
        );

        std::fs::write(dir.path().join("tracked.txt"), "v2\n").unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let first = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        std::fs::write(dir.path().join("tracked.txt"), "v3\n").unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let second = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        assert_ne!(first, second, "the ref must advance after a new checkpoint");
    }

    #[tokio::test]
    async fn restore_to_reverts_modified_content() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        let before = cp.latest_ref(&CancellationToken::new()).await;
        assert!(before.is_none());

        // Checkpoint the known-good state, then make a bad edit.
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "broken\n").unwrap();

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "v1\n",
            "content must revert to what the checkpoint captured"
        );
    }

    #[tokio::test]
    async fn restore_to_deletes_files_added_since_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();

        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        // Simulate a discarded experiment that created a brand-new file —
        // this is exactly what a plain `git checkout <ref> -- .` would fail
        // to clean up, since checkout only updates paths present in <ref>.
        std::fs::write(dir.path().join("newly_created.txt"), "oops\n").unwrap();

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        assert!(
            !dir.path().join("newly_created.txt").exists(),
            "restore_to must delete files created since the checkpoint"
        );
    }

    #[tokio::test]
    async fn restore_to_leaves_head_and_index_untouched() {
        // Same promise checkpointing itself makes — a rewind must not
        // surprise the user's own git workflow.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        let head_before = run_git(dir.path(), &["rev-parse", "HEAD"], &[])
            .await
            .unwrap();

        // Stage a real-index change that does NOT match what the checkpoint
        // captured (the checkpoint saw "v1\n"; here the REAL index — no
        // GIT_INDEX_FILE override — gets "staged-by-user\n"). If restore_to
        // ever leaked into using the real index instead of its private one,
        // this staged state would be clobbered by the read-tree reset; the
        // final assertions below would then fail.
        std::fs::write(dir.path().join("tracked.txt"), "staged-by-user\n").unwrap();
        run_git(dir.path(), &["add", "tracked.txt"], &[])
            .await
            .unwrap();
        // `ls-files -s` reports the blob oid actually recorded in the real
        // index (stage 0) — unlike `status --porcelain`, it isn't also
        // sensitive to worktree content, so it isolates "did the real index
        // change" from "did the worktree change" (which restore_to is
        // *supposed* to do).
        let indexed_blob_before = run_git(dir.path(), &["ls-files", "-s", "tracked.txt"], &[])
            .await
            .unwrap();
        assert!(
            indexed_blob_before.starts_with("100644 "),
            "sanity check: tracked.txt should be staged in the real index: {indexed_blob_before}"
        );

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        let head_after = run_git(dir.path(), &["rev-parse", "HEAD"], &[])
            .await
            .unwrap();
        assert_eq!(head_before, head_after);
        // The real index's staged blob must survive restore_to byte-for-byte
        // — proving restore_to operated on its own private index, not this
        // one. (The worktree file itself is expected to change — that's
        // restore_to doing its job — so we deliberately don't assert on
        // worktree content or on `status --porcelain` here.)
        let indexed_blob_after = run_git(dir.path(), &["ls-files", "-s", "tracked.txt"], &[])
            .await
            .unwrap();
        assert_eq!(
            indexed_blob_before, indexed_blob_after,
            "restore_to must not touch the real index"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "v1\n",
            "restore_to must still restore the worktree content from the checkpoint"
        );
    }

    #[tokio::test]
    async fn restore_to_does_not_delete_denied_subpaths() {
        // Regression guard: restore_to's internal `add -A -- .` must exclude
        // deny_paths the same way checkpoint_inner does. A checkpoint's tree
        // never contains a deny-listed path, so without the exclusion, a
        // deny-listed file that exists on disk would look like "present in
        // the FROM-index, absent from the target tree" and get deleted by
        // `read-tree --reset -u`.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let secret_dir = dir.path().join("secret");
        std::fs::create_dir(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("key"), "TOP-SECRET\n").unwrap();

        let cwd = dir.path().canonicalize().unwrap();
        let deny = vec![cwd.join("secret")];
        let cp = GitCheckpointer::detect(&cwd, deny).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        std::fs::write(dir.path().join("tracked.txt"), "broken\n").unwrap();

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        assert!(
            secret_dir.join("key").exists(),
            "restore_to must never delete a deny-listed path"
        );
        assert_eq!(
            std::fs::read_to_string(secret_dir.join("key")).unwrap(),
            "TOP-SECRET\n",
            "deny-listed content must survive restore_to unchanged"
        );
    }
}
