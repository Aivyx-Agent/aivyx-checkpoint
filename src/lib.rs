//! Git-ref checkpoint/rollback for agent tool calls.
//!
//! Before each approved mutating tool call, the worktree is snapshotted to
//! a commit object via plumbing — a private index at `.git/aivyx/index`
//! plus `write-tree`/`commit-tree`/`update-ref` — so the user's HEAD,
//! index, and worktree are never touched, and any agent change (including
//! arbitrary shell-command effects) can be rewound with plain git commands:
//! `git log refs/aivyx/checkpoints/...`, `git checkout <ref> -- <path>`.
//!
//! Best-effort by design: [`GitCheckpointer::checkpoint`] logs a warning on
//! failure and never blocks the tool call it's protecting — it's a safety
//! net, not a gate. [`GitCheckpointer::try_checkpoint`] is the same
//! operation for a caller that wants the error instead.
//!
//! The git this crate runs is unconfined, but `.git` lies inside the
//! directory a confined process may write. So the repository is not trusted
//! to steer it: its own config may hold only settings on an allow-list
//! (checked before every git invocation), program-running and network
//! settings are switched off on every invocation regardless, the git
//! directory and work tree found at detection are passed explicitly, the
//! paths written under `.git` are checked for symbolic links before each
//! operation, and only checkpoints this checkpointer took (recorded in
//! memory) are ever returned or restored by name.

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

/// A checkpoint this checkpointer took: its ref, and the commit and tree
/// it wrote. Recorded in memory when it is taken, so nothing written to the
/// repository afterwards (a planted or moved ref) can change what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub ref_name: String,
    pub commit: String,
    pub tree: String,
}

pub struct GitCheckpointer {
    cwd: PathBuf,
    /// `--absolute-git-dir`, resolved and canonicalised once at detection
    /// and passed to every git as `GIT_DIR`: a `.git` file or directory
    /// swapped in later is never followed.
    git_dir: PathBuf,
    /// `--git-common-dir` (the main repository's `.git` for a linked
    /// worktree; else `git_dir`), where objects, refs and logs live.
    common_dir: PathBuf,
    /// The top of the work tree containing `cwd`, resolved at detection
    /// and passed to every git as `GIT_WORK_TREE`, so no config can move
    /// it.
    work_tree: PathBuf,
    deny_paths: Vec<PathBuf>,
    /// Generated files (build output, caches) in `.gitignore` syntax,
    /// kept out of snapshots while git doesn't track them. See
    /// [`Self::with_generated_ignore`].
    generated_ignore: Vec<String>,
    /// Tree oid of the most recent checkpoint, for skipping no-op
    /// snapshots (several mutations often land between identical trees,
    /// e.g. a failed edit).
    last_tree: Mutex<Option<String>>,
    /// Disambiguates checkpoints created within the same millisecond.
    seq: AtomicU64,
    retain: usize,
    /// The checkpoints this checkpointer took, oldest first, at most
    /// `retain` of them. The only refs [`Self::latest_ref`] returns and
    /// [`Self::restore_to`] accepts.
    taken: Mutex<Vec<Checkpoint>>,
}

impl GitCheckpointer {
    /// `None` (with one log line) when `cwd` isn't inside a git worktree —
    /// checkpointing is then disabled for the whole session rather than
    /// warning on every mutation.
    ///
    /// Also `None` when the repository can't be checkpointed safely: its
    /// config holds a setting outside the allow-list (see [`run_git`]), or
    /// its git directory is neither `<work tree>/.git` nor a linked
    /// worktree of that work tree, or a path this crate writes under it is
    /// a symbolic link.
    pub async fn detect(cwd: &Path, deny_paths: Vec<PathBuf>) -> Option<Self> {
        match locate(cwd).await {
            Ok((git_dir, common_dir, work_tree)) => Some(Self {
                cwd: cwd.to_path_buf(),
                git_dir,
                common_dir,
                work_tree,
                deny_paths,
                generated_ignore: Vec::new(),
                last_tree: Mutex::new(None),
                seq: AtomicU64::new(0),
                retain: RETAIN,
                taken: Mutex::new(Vec::new()),
            }),
            Err(err) => {
                tracing::info!(
                    cwd = %cwd.display(),
                    reason = %err,
                    "no usable git repository — worktree checkpointing disabled"
                );
                None
            }
        }
    }

    /// Keeps generated files out of every snapshot: build output and
    /// caches matching `patterns` (`.gitignore` syntax, e.g. `target/`,
    /// `*.pyc`) in a project whose own `.gitignore` doesn't cover them.
    /// Only while untracked — a file in the user's index is snapshotted
    /// as usual, exactly as `.gitignore` itself behaves. Without this, a
    /// fresh project's build output is copied into every checkpoint, and
    /// the retained refs keep it from ever being collected (2026-10-22:
    /// one 33-minute agent run left 305 MB of `target/` objects). Restore
    /// leaves these files alone, as it does gitignored ones.
    pub fn with_generated_ignore(mut self, patterns: Vec<String>) -> Self {
        self.generated_ignore = patterns;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_retain(&mut self, retain: usize) {
        self.retain = retain;
    }

    /// Snapshots the current worktree. Never fails the caller: every error
    /// path logs and returns. Calls [`Self::try_checkpoint`] and discards
    /// the error after logging it — use that instead when the caller needs
    /// to know a checkpoint failed.
    pub async fn checkpoint(&self, tool_name: &str, cancellation: &CancellationToken) {
        if let Err(err) = self.try_checkpoint(tool_name, cancellation).await {
            tracing::warn!(tool = %tool_name, error = %err, "checkpoint failed (tool call proceeds)");
        }
    }

    /// Snapshots the current worktree, like [`Self::checkpoint`], but
    /// returns the error instead of only logging it — for callers that
    /// want to surface or act on a checkpoint failure rather than silently
    /// proceeding.
    pub async fn try_checkpoint(
        &self,
        tool_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        self.checkpoint_inner(tool_name, cancellation).await
    }

    async fn checkpoint_inner(
        &self,
        tool_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let index = self.private_index()?;
        let index_env: Vec<(&str, &str)> =
            vec![("GIT_INDEX_FILE", index.to_str().ok_or("non-utf8 git dir")?)];

        // Stage the whole worktree into the private index. deny_paths are
        // excluded via pathspecs — without this, denied content (which
        // Landlock carves out of the kernel sandbox) would be copied into
        // readable .git objects, and `git show <checkpoint>:<denied-file>`
        // would read it straight through the sandbox.
        self.stage_worktree(&index_env, cancellation).await?;

        let write_tree_args = vec!["write-tree".to_string()];
        let tree = self
            .git(&write_tree_args, &index_env, cancellation)
            .await?
            .trim()
            .to_string();

        if self.last_tree.lock().unwrap().as_deref() == Some(tree.as_str()) {
            return Ok(()); // identical to the previous checkpoint
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
        let mut commit_args: Vec<String> =
            vec!["commit-tree".into(), tree.clone(), "-m".into(), message];
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
        // year 2286), which is what the retention pass below relies on —
        // and still does with the pid segment added below, since millis
        // remains the leading field either way. The pid disambiguates
        // across processes: `seq` alone starts at 0 in every process (each
        // has its own `GitCheckpointer`), so two processes checkpointing
        // the same repo within the same millisecond used to mint the exact
        // same ref name and clobber each other's ref. `pid` is zero-padded
        // to a fixed width too, for the same reason millis is: two refs
        // sharing a millisecond must still compare consistently regardless
        // of how many digits either pid happens to have (unpadded, pid 9
        // would sort after pid 10 lexically, even though 9 < 10).
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let pid = std::process::id();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let ref_name = format!("refs/aivyx/checkpoints/{millis:013}-{pid:07}-{seq:04}");
        // `--no-deref` and an empty old value: create this exact ref, and
        // fail rather than follow or overwrite anything already there.
        let update_args: Vec<String> = vec![
            "update-ref".into(),
            "--no-deref".into(),
            ref_name.clone(),
            commit.clone(),
            String::new(),
        ];
        self.check_layout()?;
        self.git(&update_args, &[], cancellation).await?;
        // Only now: a snapshot whose ref was never written must not make
        // the next identical one look like a duplicate of it, or that one
        // reports success with no ref behind it.
        *self.last_tree.lock().unwrap() = Some(tree.clone());
        {
            let mut taken = self.taken.lock().unwrap();
            taken.push(Checkpoint {
                ref_name: ref_name.clone(),
                commit,
                tree,
            });
            let excess = taken.len().saturating_sub(self.retain.max(1));
            taken.drain(..excess);
        }
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
            // `--no-deref`: a symbolic ref planted here is deleted itself,
            // never the branch it points at.
            let delete_args: Vec<String> = vec![
                "update-ref".into(),
                "--no-deref".into(),
                "-d".into(),
                (*stale).to_string(),
            ];
            self.git(&delete_args, &[], cancellation).await?;
        }
        Ok(())
    }

    /// The ref of the most recent checkpoint *this checkpointer* took, or
    /// `None` if it hasn't taken one yet. Never read back from the
    /// repository: refs under `refs/aivyx/checkpoints/` can be written by a
    /// confined process (or another process sharing the repository), so a
    /// name that merely sorts last is never taken for a checkpoint.
    /// `async` and `cancellation` are kept for API stability.
    pub async fn latest_ref(&self, _cancellation: &CancellationToken) -> Option<String> {
        self.taken
            .lock()
            .unwrap()
            .last()
            .map(|taken| taken.ref_name.clone())
    }

    /// The checkpoints this checkpointer has taken, oldest first (at most
    /// the retention count): each one's ref with the commit and tree it
    /// wrote, as recorded when it was taken.
    pub fn checkpoints(&self) -> Vec<Checkpoint> {
        self.taken.lock().unwrap().clone()
    }

    /// What [`Self::restore_to`] resets to for `target`: the commit
    /// recorded for a checkpoint ref this checkpointer took, or a full
    /// object id the caller holds itself (a content address, which nothing
    /// in the repository can redirect: replace refs are ignored). Any other
    /// name — a ref planted by a confined process, a branch, an abbreviated
    /// id — is refused.
    fn restore_target(&self, target: &str) -> Result<String, String> {
        if let Some(taken) = self
            .taken
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|taken| taken.ref_name == target)
        {
            return Ok(taken.commit.clone());
        }
        if is_full_object_id(target) {
            return Ok(target.to_ascii_lowercase());
        }
        Err(format!(
            "{target} is not a checkpoint this checkpointer took (nor a full object id)"
        ))
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
    ///
    /// `ref_name` must be a checkpoint ref this checkpointer took (see
    /// [`Self::latest_ref`] and [`Self::checkpoints`]), restored to the
    /// commit recorded when it was taken even if the ref has moved since,
    /// or a full object id; anything else is an error.
    pub async fn restore_to(
        &self,
        ref_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let target = self.restore_target(ref_name)?;
        let index = self.private_index()?;
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
        // Generated files are excluded the same way, so ones that are in
        // no snapshot survive the reset rather than being deleted.
        self.stage_worktree(&index_env, cancellation).await?;

        let reset_args: Vec<String> = vec![
            "read-tree".into(),
            "--reset".into(),
            "-u".into(),
            "--end-of-options".into(),
            target,
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

    /// The tree a checkpoint taken now would hold, staged into `index`
    /// instead of the private index — for a caller that wants to check a
    /// snapshot against the worktree. Start `index` as a copy of
    /// `<git-dir>/aivyx/index`: git keeps an already-indexed path even once
    /// it's ignored, so only the same starting index gives the same tree
    /// (and its stat data spares re-hashing the worktree).
    pub async fn tree_in_index(
        &self,
        index: &Path,
        cancellation: &CancellationToken,
    ) -> Result<String, String> {
        self.check_layout()?;
        let index_env: Vec<(&str, &str)> = vec![(
            "GIT_INDEX_FILE",
            index.to_str().ok_or("non-utf8 index path")?,
        )];
        self.stage_worktree(&index_env, cancellation).await?;
        let write_tree_args = vec!["write-tree".to_string()];
        Ok(self
            .git(&write_tree_args, &index_env, cancellation)
            .await?
            .trim()
            .to_string())
    }

    /// Stages the worktree into the private index (`index_env`): `add -A`
    /// minus `deny_paths` and untracked generated files. Generated paths
    /// an earlier snapshot staged (before they matched, or by an older
    /// version) are dropped from the index first; `add -A` would otherwise
    /// keep updating them, since exclusions only stop new paths.
    async fn stage_worktree(
        &self,
        index_env: &[(&str, &str)],
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        let generated = self.untracked_generated(cancellation).await?;
        if !generated.is_empty() {
            let mut rm_args: Vec<String> =
                ["rm", "--cached", "-f", "-r", "-q", "--ignore-unmatch", "--"]
                    .into_iter()
                    .map(String::from)
                    .collect();
            rm_args.extend(generated.iter().map(|path| format!(":(literal){path}")));
            self.git(&rm_args, index_env, cancellation).await?;
        }
        let mut add_args: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        add_args.extend(exclude_pathspecs(&self.cwd, &self.deny_paths));
        add_args.extend(
            generated
                .iter()
                .map(|path| format!(":(exclude,literal){path}")),
        );
        self.git(&add_args, index_env, cancellation)
            .await
            .map(|_| ())
    }

    /// Untracked paths under `cwd` matching `generated_ignore`, relative
    /// to `cwd`; a wholly generated directory is one entry (`target/`).
    /// "Untracked" is judged against the user's own index, not the private
    /// one, so this needs no `GIT_INDEX_FILE`.
    ///
    /// Two queries: `--directory` collapses generated directories, but it
    /// also reports a new, not-yet-tracked directory as a single
    /// non-ignored entry, hiding the generated files inside it
    /// (`pkg/mod.pyc` in an untracked `pkg/`). The second query lists
    /// those file by file, skipping the directories the first one found
    /// so `target/` isn't listed a file at a time.
    async fn untracked_generated(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, String> {
        if self.generated_ignore.is_empty() {
            return Ok(Vec::new());
        }
        // `extra` goes last: it may end in `-- <pathspec>...`.
        let query = |extra: &[&str]| {
            let mut args: Vec<String> = ["ls-files", "-z", "--others", "--ignored"]
                .into_iter()
                .map(String::from)
                .collect();
            args.extend(
                self.generated_ignore
                    .iter()
                    .map(|pattern| format!("--exclude={pattern}")),
            );
            args.extend(extra.iter().map(|arg| arg.to_string()));
            args
        };
        let split = |listed: String| -> Vec<String> {
            listed
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_string)
                .collect()
        };
        let directories = query(&["--directory", "--no-empty-directory"]);
        let mut generated = split(self.git(&directories, &[], cancellation).await?);

        let mut files = query(&["--", "."]);
        files.extend(
            generated
                .iter()
                .filter(|path| path.ends_with('/'))
                .map(|dir| format!(":(exclude,literal){dir}")),
        );
        for path in split(self.git(&files, &[], cancellation).await?) {
            if !generated.contains(&path) {
                generated.push(path);
            }
        }
        if generated.is_empty() {
            return Ok(generated);
        }

        // Leave out what the project's own ignore rules already cover: `add
        // -A` skips those anyway, and git refuses an `add` whose pathspec
        // names an ignored path, even an exclude (2026-10-23: every
        // checkpoint failed in a project whose .gitignore had node_modules/).
        let mut standard: Vec<String> = [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        standard.extend(generated.iter().map(|path| format!(":(literal){path}")));
        let standard = split(self.git(&standard, &[], cancellation).await?);
        generated.retain(|path| {
            !standard.iter().any(|ignored| {
                ignored == path || (ignored.ends_with('/') && path.starts_with(ignored))
            })
        });
        Ok(generated)
    }

    /// Ensures `<git-dir>/aivyx/` exists and returns the private index
    /// path, first clearing a leftover `index.lock` old enough to be a
    /// stale lock rather than a genuinely in-flight write. A checkpoint
    /// that's cancelled or times out mid-`git add` leaves that lock file
    /// behind (this is git's own locking convention, not ours); since
    /// nothing else ever touches this private index, a lock surviving
    /// longer than [`GIT_TIMEOUT`] — the longest a legitimate `git`
    /// invocation here is ever allowed to run — cannot belong to a call
    /// still in progress, so it's safe to remove before staging. Without
    /// this, every later checkpoint (this session and future ones, since
    /// the lock is a file on disk) fails against the same stale lock,
    /// silently (`checkpoint()`'s own best-effort contract).
    ///
    /// The directory is never followed through a symbolic link (a confined
    /// process can replace `.git/aivyx` with one): it must be a real
    /// directory owned by the git directory's owner, created `0700` when
    /// missing, and the layout checks of [`Self::check_layout`] run first.
    /// Only a regular-file lock is ever removed.
    fn private_index(&self) -> Result<PathBuf, String> {
        self.check_layout()?;
        let index_dir = self.git_dir.join("aivyx");
        if !real_dir(&index_dir)? {
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&index_dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", index_dir.display())),
            }
            if !real_dir(&index_dir)? {
                return Err(format!("{} vanished", index_dir.display()));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = |path: &Path| {
                std::fs::symlink_metadata(path)
                    .map(|m| m.uid())
                    .map_err(|e| format!("{}: {e}", path.display()))
            };
            if owner(&index_dir)? != owner(&self.git_dir)? {
                return Err(format!("{} is owned by another user", index_dir.display()));
            }
        }
        let index = index_dir.join("index");
        if std::fs::symlink_metadata(&index).is_ok_and(|m| !m.is_file()) {
            return Err(format!("{} is not a regular file", index.display()));
        }
        let lock = index_dir.join("index.lock");
        if let Ok(metadata) = std::fs::symlink_metadata(&lock)
            && metadata.is_file()
            && let Ok(modified) = metadata.modified()
            && modified.elapsed().is_ok_and(|age| age > GIT_TIMEOUT)
        {
            // Best-effort: if another process wins a race and removes it
            // first, there's nothing left to clean up either way.
            let _ = std::fs::remove_file(&lock);
        }
        Ok(index)
    }

    async fn git(
        &self,
        args: &[String],
        envs: &[(&str, &str)],
        cancellation: &CancellationToken,
    ) -> Result<String, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let git_dir = self.git_dir.to_str().ok_or("non-utf8 git dir")?;
        let work_tree = self.work_tree.to_str().ok_or("non-utf8 work tree")?;
        // Always this repository and this work tree, whatever `.git`,
        // `core.worktree` or the inherited environment say.
        let mut envs: Vec<(&str, &str)> = envs.to_vec();
        envs.push(("GIT_DIR", git_dir));
        envs.push(("GIT_WORK_TREE", work_tree));
        tokio::select! {
            result = run_git(&self.cwd, &args, &envs) => result,
            _ = cancellation.cancelled() => Err("cancelled".to_string()),
        }
    }

    /// Re-checks, before each operation, that the paths this crate's
    /// unconfined git writes under are still what they were at detection:
    /// the git directory and common directory are real, unmoved
    /// directories, and none of `aivyx/`, `objects/` (with its fan-out
    /// directories), `refs/aivyx/checkpoints/` or the matching reflog
    /// directories is a symbolic link, which would make git write outside
    /// the repository. Also refuses `objects/info/alternates`, through which
    /// a restore could read objects from another repository. A confined
    /// process can change all of these between calls, so they are checked
    /// every time, not once.
    fn check_layout(&self) -> Result<(), String> {
        check_layout(&self.git_dir, &self.common_dir)
    }
}

/// See [`GitCheckpointer::check_layout`].
fn check_layout(git_dir: &Path, common_dir: &Path) -> Result<(), String> {
    for dir in [git_dir, common_dir] {
        if !real_dir(dir)? {
            return Err(format!("{} is missing", dir.display()));
        }
        if dir.canonicalize().map_err(|e| e.to_string())? != dir {
            return Err(format!("{} has moved", dir.display()));
        }
    }
    real_dir(&git_dir.join("aivyx"))?;
    for sub in [
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/aivyx",
        "refs/aivyx/checkpoints",
        "logs",
        "logs/refs",
        "logs/refs/aivyx",
        "logs/refs/aivyx/checkpoints",
    ] {
        real_dir(&common_dir.join(sub))?;
    }
    if std::fs::symlink_metadata(common_dir.join("objects/info/alternates")).is_ok() {
        return Err("the repository borrows objects from another (objects/info/alternates)".into());
    }
    for dir in [
        "objects",
        "refs/aivyx/checkpoints",
        "logs/refs/aivyx/checkpoints",
    ] {
        let dir = common_dir.join(dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                return Err(format!("{} is a symbolic link", entry.path().display()));
            }
        }
    }
    Ok(())
}

/// `Ok(true)` for a real directory, `Ok(false)` when nothing is there, and
/// an error for a symbolic link or anything else — never followed.
fn real_dir(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            Err(format!("{} is a symbolic link", path.display()))
        }
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(format!("{} is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A full hexadecimal object id (SHA-1 or SHA-256), not an abbreviation.
fn is_full_object_id(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Finds the repository for `cwd` at detection: its git directory, common
/// directory and work tree, canonicalised. Refuses (an error) a git
/// directory that is neither `<work tree>/.git` nor a linked worktree whose
/// `gitdir` file points back at `<work tree>/.git` — a `.git` file a
/// confined process planted would otherwise point the checkpoint's writes
/// at another repository — and a layout [`check_layout`] refuses. The
/// query itself goes through [`run_git`], so a repository whose config is
/// outside the allow-list (e.g. `core.worktree`) is refused here too.
async fn locate(cwd: &Path) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let out = run_git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--absolute-git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
        &[],
    )
    .await?;
    let mut lines = out.lines();
    let mut next = |what: &str| -> Result<PathBuf, String> {
        let line = lines
            .next()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| format!("git did not report the {what}"))?;
        PathBuf::from(line)
            .canonicalize()
            .map_err(|e| format!("{line}: {e}"))
    };
    let git_dir = next("git directory")?;
    let common_dir = next("common git directory")?;
    let work_tree = next("work tree")?;
    let canonical_cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
    if !canonical_cwd.starts_with(&work_tree) {
        return Err(format!(
            "{} is outside its work tree {}",
            cwd.display(),
            work_tree.display()
        ));
    }
    let dot_git = work_tree.join(".git");
    if git_dir != dot_git {
        let points_back = std::fs::read_to_string(git_dir.join("gitdir"))
            .ok()
            .and_then(|back| PathBuf::from(back.trim()).canonicalize().ok())
            .is_some_and(|back| Some(back) == dot_git.canonicalize().ok());
        if !points_back {
            return Err(format!(
                "the git directory {} belongs to neither {} nor a linked worktree of it",
                git_dir.display(),
                work_tree.display()
            ));
        }
    }
    check_layout(&git_dir, &common_dir)?;
    Ok((git_dir, common_dir, work_tree))
}

/// Builds `:(exclude)` pathspecs for every deny path inside `cwd`, relative
/// to `cwd` (git resolves pathspecs against the command's working
/// directory). A *bare* entry — a single path component, possibly with
/// glob characters, e.g. `.env`, `.env.*`, `*.pem`, `id_rsa` — isn't a path
/// under `cwd` at all; it's a basename pattern meant to match anywhere in
/// the tree (the same "bare pattern" notion `aivyx-coder`'s own
/// `aivyx-confine::is_bare_pattern` uses for its unrelated deny-paths
/// matching), so it becomes a recursive glob exclusion
/// (`:(exclude,glob)**/<pattern>`) instead of being resolved against `cwd`
/// — which would simply never match and silently drop the entry, letting
/// files it was meant to deny (e.g. a nested `svc/.env`) into the snapshot.
/// `pub` because it has consumers beyond this file: any git operation that
/// sweeps "everything under the worktree" needs the same carve-outs —
/// `aivyx-coder`'s own `git_read`/`git_commit` tools call this directly,
/// cross-crate, from their own production code.
pub fn exclude_pathspecs(cwd: &Path, deny_paths: &[PathBuf]) -> Vec<String> {
    deny_paths
        .iter()
        .filter_map(|denied| {
            if is_bare_deny_pattern(denied) {
                return Some(format!(":(exclude,glob)**/{}", denied.display()));
            }
            let rel = denied.strip_prefix(cwd).ok()?;
            if rel.as_os_str().is_empty() {
                return None;
            }
            Some(format!(":(exclude){}", rel.display()))
        })
        .collect()
}

/// True for a deny-paths entry with a single path component — no directory
/// separator anywhere in it, so it can't be `strip_prefix`'d against `cwd`
/// as a real path. Identical classification to `aivyx-confine::is_bare_pattern`
/// (duplicated rather than depended on: this crate stays config-agnostic
/// with no knowledge of the sandbox crate).
fn is_bare_deny_pattern(path: &Path) -> bool {
    path.parent() == Some(Path::new(""))
}

/// Always passed to unconfined git, whatever the repository's config says
/// (a second layer under [`check_repo_config`]'s allow-list): no fsmonitor,
/// no hooks (plumbing like `update-ref` runs `reference-transaction`), no
/// signing, no network transport of any kind (which also stops a lazy
/// fetch), and no recursion into submodules, whose own config is not
/// checked.
const SAFE_GIT_CONFIG: [&str; 12] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "protocol.allow=never",
    "-c",
    "submodule.recurse=false",
];

/// Environment set on every git this crate spawns: replace refs are
/// ignored (a confined command could otherwise swap the content behind an
/// object id), a missing object is never fetched from a promisor remote,
/// and git never prompts.
const HARDENED_ENV: [(&str, &str); 3] = [
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// Lists every config entry git will use, with its scope and origin, so
/// [`check_repo_config`] can tell the repository's own entries (which a
/// confined process can write) from the user's.
const CONFIG_LISTING: [&str; 6] = [
    "config",
    "--list",
    "--includes",
    "--show-scope",
    "--show-origin",
    "-z",
];

#[cfg(test)]
thread_local! {
    /// Test-only: accept any repository config, so a test can show that
    /// the second layer (the fixed `-c` overrides, the hardened
    /// environment, the explicit work tree) holds on its own.
    static ACCEPT_ANY_REPO_CONFIG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Settings the repository's own config may hold. Everything else makes
/// [`check_repo_config`] refuse to run git: a list of what to forbid could
/// never be complete (git keeps adding settings that run a program or reach
/// a remote), so this lists what is known to be harmless for the commands
/// this crate and its consumers run unconfined. `key` is as `git config
/// --list` prints it: section and name lower-case, a subsection as written.
fn allowed_repo_setting(key: &str, value: Option<&str>) -> bool {
    let Some((section, rest)) = key.split_once('.') else {
        return false;
    };
    let (subsection, name) = match rest.rsplit_once('.') {
        Some((subsection, name)) => (Some(subsection), name),
        None => (None, rest),
    };
    let is_false = |v: Option<&str>| {
        v.is_some_and(|v| {
            ["false", "no", "off", "0"]
                .iter()
                .any(|f| v.eq_ignore_ascii_case(f))
        })
    };
    match (section, subsection) {
        ("core", None) => match name {
            // Version 1 only matters with extensions, which are listed below.
            "repositoryformatversion" => matches!(value, Some("0" | "1")),
            "bare" => is_false(value),
            // Both always switched off by `SAFE_GIT_CONFIG`.
            "fsmonitor" | "hookspath" => true,
            _ => [
                "filemode",
                "logallrefupdates",
                "ignorecase",
                "precomposeunicode",
                "symlinks",
                "autocrlf",
                "safecrlf",
                "eol",
                "quotepath",
                "checkstat",
                "trustctime",
                "ignorestat",
                "untrackedcache",
                "splitindex",
                "preloadindex",
                "fscache",
                "compression",
                "loosecompression",
                "bigfilethreshold",
                "abbrev",
                "commentchar",
                "commentstring",
                "whitespace",
                "sparsecheckout",
                "sparsecheckoutcone",
                "warnambiguousrefs",
                "multipackindex",
                "commitgraph",
                "deltabasecachelimit",
                "packedgitwindowsize",
                "packedgitlimit",
                "fsync",
                "fsyncmethod",
                "fsyncobjectfiles",
                "filesreflocktimeout",
                "packedrefstimeout",
                "longpaths",
                "hidedotfiles",
                "fsmonitorhookversion",
                "checkroundtripencoding",
            ]
            .contains(&name),
        },
        // What `git init`, `git worktree` and `git sparse-checkout` write;
        // a partial clone (`partialclone`) and any other extension are not
        // allowed.
        ("extensions", None) => ["objectformat", "refstorage", "worktreeconfig"].contains(&name),
        ("remote", Some(_)) => [
            "url",
            "pushurl",
            "fetch",
            "push",
            "tagopt",
            "mirror",
            "prune",
            "prunetags",
            "skipdefaultupdate",
            "skipfetchall",
            "followremotehead",
            "gh-resolved",
        ]
        .contains(&name),
        // Upstream tracking and editor bookkeeping only.
        ("branch", Some(_)) => true,
        ("user", None) => ["name", "email", "signingkey", "useconfigonly"].contains(&name),
        ("author" | "committer", None) => ["name", "email"].contains(&name),
        // Signing is always switched off by `SAFE_GIT_CONFIG`.
        ("commit", None) => ["gpgsign", "verbose", "status", "cleanup"].contains(&name),
        ("tag", None) => ["gpgsign", "sort"].contains(&name),
        // Programs switched off by [`repo_overrides`]; signing is off anyway.
        ("gpg", None) => ["format", "program"].contains(&name),
        ("gpg", Some(_)) => name == "program",
        // Programs switched off by [`repo_overrides`].
        ("filter", Some(_)) => ["clean", "smudge", "process", "required"].contains(&name),
        ("lfs", None) => ["repositoryformatversion", "locksverify"].contains(&name),
        ("lfs", Some(_)) => ["access", "locksverify"].contains(&name),
        ("submodule", None) => ["recurse", "active"].contains(&name),
        ("submodule", Some(_)) => [
            "url",
            "active",
            "branch",
            "ignore",
            "shallow",
            "fetchrecursesubmodules",
        ]
        .contains(&name),
        ("pull", None) => ["rebase", "ff"].contains(&name),
        ("push", None) => ["default", "autosetupremote", "followtags"].contains(&name),
        ("fetch", None) => ["prune", "prunetags"].contains(&name),
        ("rebase", None) => ["autosquash", "autostash", "updaterefs"].contains(&name),
        ("merge", None) => ["ff", "conflictstyle"].contains(&name),
        ("diff", None) => [
            "renames",
            "algorithm",
            "colormoved",
            "mnemonicprefix",
            "noprefix",
            // Switched off by [`repo_overrides`] (git-crypt, for one, sets a
            // repository textconv driver).
            "external",
        ]
        .contains(&name),
        ("diff", Some(_)) => [
            // Programs switched off by [`repo_overrides`].
            "textconv",
            "command",
            "cachetextconv",
            "binary",
            "xfuncname",
            "funcname",
            "wordregex",
        ]
        .contains(&name),
        ("status", None) => [
            "showuntrackedfiles",
            "submodulesummary",
            "relativepaths",
            "short",
            "branch",
            "showstash",
            "aheadbehind",
        ]
        .contains(&name),
        ("log", None) => ["decorate", "date", "follow", "abbrevcommit"].contains(&name),
        ("color" | "advice", _) => true,
        ("gui", None) => ["wmstate", "geometry"].contains(&name),
        ("i18n", None) => ["commitencoding", "logoutputencoding"].contains(&name),
        ("init", None) => name == "defaultbranch",
        ("rerere", None) => ["enabled", "autoupdate"].contains(&name),
        ("gc", None) => [
            "auto",
            "autodetach",
            "autopacklimit",
            "pruneexpire",
            "reflogexpire",
            "reflogexpireunreachable",
            "writecommitgraph",
        ]
        .contains(&name),
        ("maintenance", None) => ["auto", "strategy"].contains(&name),
        ("index", None) => [
            "version",
            "threads",
            "skiphash",
            "sparse",
            "recordendofindexentries",
            "recordoffsettable",
        ]
        .contains(&name),
        ("feature", None) => ["manyfiles", "experimental"].contains(&name),
        ("blame", None) => [
            "ignorerevsfile",
            "markignoredlines",
            "markunblamablelines",
            "date",
            "coloring",
        ]
        .contains(&name),
        ("checkout", None) => [
            "defaultremote",
            "workers",
            "thresholdforparallelism",
            "guess",
        ]
        .contains(&name),
        ("worktree", None) => name == "guessremote",
        ("splitindex", None) => ["maxpercentchange", "sharedindexexpire"].contains(&name),
        ("apply", None) => name == "whitespace",
        _ => false,
    }
}

/// Checks a [`CONFIG_LISTING`] of the config git will use in `cwd`, and
/// returns the `-c` overrides switching off every program the
/// repository's own config names (see [`repo_overrides`]).
///
/// The repository's own entries — `.git/config`, a worktree's
/// `config.worktree`, anything they include, and any config file that
/// lies inside `cwd` — can be written by a confined process, since `.git`
/// sits inside the directory it may write under. Each such entry must be
/// on [`allowed_repo_setting`]'s allow-list, or this returns an error
/// naming the settings (never their values, which an include could have
/// read from anywhere). The user's own system and global config, and `-c`
/// settings, are trusted and left alone (so git-lfs or a global excludes
/// file keep working).
fn check_repo_config(listing: &[u8], cwd: &Path) -> Result<Vec<String>, String> {
    let listing = String::from_utf8_lossy(listing);
    let mut fields = listing.split('\0');
    let mut refused: Vec<String> = Vec::new();
    let mut programs: Vec<String> = Vec::new();
    while let (Some(scope), Some(origin), Some(entry)) =
        (fields.next(), fields.next(), fields.next())
    {
        let (key, value) = match entry.split_once('\n') {
            Some((key, value)) => (key, Some(value)),
            None => (entry, None),
        };
        let inside_cwd = origin
            .strip_prefix("file:")
            .is_some_and(|file| cwd.join(file).starts_with(cwd));
        let repository_controlled = match scope {
            "system" | "global" => inside_cwd,
            "command" => false,
            _ => true,
        };
        if !repository_controlled {
            continue;
        }
        if !programs.iter().any(|k| k == key) {
            programs.push(key.to_string());
        }
        #[cfg(test)]
        if ACCEPT_ANY_REPO_CONFIG.with(std::cell::Cell::get) {
            continue;
        }
        if !allowed_repo_setting(key, value) && !refused.iter().any(|k| k == key) {
            refused.push(key.to_string());
        }
    }
    if refused.is_empty() {
        Ok(repo_overrides(&programs))
    } else {
        Err(format!(
            "the repository's git config sets {}, which git run outside the \
             sandbox must not honour; remove {} to re-enable checkpoints",
            refused.join(", "),
            if refused.len() == 1 { "it" } else { "them" }
        ))
    }
}

/// `-c` overrides switching off every program named by the repository's
/// own config `keys` (each an allowed key, as listed): its filter drivers
/// (`filter.<d>.clean`/`smudge`/`process` emptied, `required` off), its
/// diff drivers and external diff (`diff.<d>.textconv`/`command`,
/// `diff.external` emptied — git then refuses such a diff instead of
/// running anything; callers pass `--no-ext-diff --no-textconv` and never
/// notice) and its signing programs (`gpg.program`, `gpg.<format>.program`
/// emptied; signing itself is off in `SAFE_GIT_CONFIG`). The user's own
/// global and system drivers are not in `keys` and keep working.
fn repo_overrides(keys: &[String]) -> Vec<String> {
    let mut settings: Vec<String> = Vec::new();
    let mut add = |setting: String| {
        if !settings.contains(&setting) {
            settings.push(setting);
        }
    };
    for key in keys {
        let Some((section, rest)) = key.split_once('.') else {
            continue;
        };
        let subsection = rest.rsplit_once('.').map(|(subsection, _)| subsection);
        match (section, subsection) {
            ("filter", Some(driver)) => {
                for kind in ["clean", "smudge", "process"] {
                    add(format!("filter.{driver}.{kind}="));
                }
                add(format!("filter.{driver}.required=false"));
            }
            ("diff", Some(driver)) => {
                add(format!("diff.{driver}.textconv="));
                add(format!("diff.{driver}.command="));
            }
            ("diff", None) if rest == "external" => add("diff.external=".into()),
            ("gpg", None) if rest == "program" => add("gpg.program=".into()),
            ("gpg", Some(format)) if rest.ends_with(".program") => {
                add(format!("gpg.{format}.program="))
            }
            _ => {}
        }
    }
    settings
        .into_iter()
        .flat_map(|setting| ["-c".to_string(), setting])
        .collect()
}

/// Checks the config git will use in `cwd` (with `envs`, so `GIT_DIR` and
/// `GIT_WORK_TREE` are honoured exactly as the real invocation honours
/// them) and returns the `-c` overrides that switch off the programs the
/// repository's own config names (see [`repo_overrides`]). An error when the repository's config holds a
/// setting outside the allow-list (see [`check_repo_config`]), and when the
/// config can't be read at all (a malformed or unreadable file, a timeout):
/// running unconfined git on a config this couldn't check would defeat
/// the check. Outside any repository git lists only the user's own config,
/// which is the bootstrapping case (`detect`'s first probe, a `git init`).
async fn repo_program_overrides(cwd: &Path, envs: &[(&str, &str)]) -> Result<Vec<String>, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(CONFIG_LISTING)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .envs(HARDENED_ENV);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.env("LC_ALL", "C").env("LANGUAGE", "");
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "git config --list timed out".to_string())?
        .map_err(|e| format!("failed to run git config: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git config --list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    check_repo_config(&output.stdout, cwd)
}

/// The arguments to put before the subcommand of any *blocking*, unconfined
/// git a caller spawns itself (a preview, a branch-name lookup): the same
/// protections [`run_git`] applies — the repository's config checked
/// against the allow-list, no fsmonitor, hooks, signing or transport,
/// replace refs ignored, and the programs the repository's own config
/// names (filter and diff drivers, signing programs) switched off. An error when the config holds a setting outside the allow-list or
/// can't be read; the caller must then not run git at all. Diff callers
/// still add `--no-ext-diff --no-textconv` to the diff itself, and should
/// set [`HARDENED_ENV`]'s `GIT_NO_LAZY_FETCH=1` where they can.
pub fn unconfined_git_args_blocking(cwd: &Path) -> Result<Vec<String>, String> {
    let mut command = std::process::Command::new("git");
    command
        .args(CONFIG_LISTING)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .envs(HARDENED_ENV)
        .env("LC_ALL", "C")
        .env("LANGUAGE", "");
    let output = blocking_output_within(command, GIT_TIMEOUT)
        .ok_or("git config --list timed out or could not start")?;
    if !output.status.success() {
        return Err(format!(
            "git config --list failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let overrides = check_repo_config(&output.stdout, cwd)?;
    let mut args = vec!["--no-replace-objects".to_string()];
    args.extend(SAFE_GIT_CONFIG.iter().map(|s| s.to_string()));
    args.extend(overrides);
    Ok(args)
}

/// Runs `command` to completion, killing it and returning `None` if it
/// doesn't finish within `timeout` (or can't be started). The one blocking
/// (non-tokio) process invocation this crate makes — [`run_git`]'s own
/// async path already gets a bound from `tokio::time::timeout`; this gives
/// [`unconfined_git_args_blocking`]'s config query the same [`GIT_TIMEOUT`]
/// bound rather than running unbounded. stdout and stderr are drained on
/// their own threads, so a large listing can't fill a pipe and stall the
/// child until the timeout.
fn blocking_output_within(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    fn drain(
        pipe: Option<impl std::io::Read + Send + 'static>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().ok()?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    Some(std::process::Output {
        status,
        stdout: stdout.join().ok()?,
        stderr: stderr.join().ok()?,
    })
}

/// One plumbing invocation: trusted fixed argv (never model-controlled),
/// writing only under `.git`, so it runs unconfined; `kill_on_drop` +
/// timeout bound it instead. `pub` because it has a consumer beyond this
/// file: `aivyx-coder`'s own `wiki.rs` module calls this directly,
/// cross-crate, for its own (unrelated) git plumbing needs.
///
/// Because it is unconfined, it must not let the repository steer it — a
/// confined process can write `.git/config`, `.git/hooks` and
/// `.gitattributes`, since `.git` lies inside the directory it may write
/// under. Two layers: the repository's own config must hold only settings
/// on an allow-list, checked before every invocation (see
/// [`check_repo_config`]; an error otherwise), and every invocation runs
/// with fsmonitor, hooks, signing, network transport and submodule
/// recursion switched off, replace refs ignored, lazy fetching disabled and
/// the programs the repository's own config names switched off (filter and
/// diff drivers, signing programs; see [`repo_overrides`]). Callers that
/// diff still pass `--no-ext-diff --no-textconv` themselves.
pub async fn run_git(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let neutralise = repo_program_overrides(cwd, envs).await?;
    let mut command = tokio::process::Command::new("git");
    command
        .args(SAFE_GIT_CONFIG)
        .args(&neutralise)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .envs(HARDENED_ENV);
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
/// test fixture, not part of this crate's real API surface. `init_repo`
/// builds a repo with one committed `tracked.txt` on `main`, with a
/// repo-local identity so tests don't depend on the machine's global git
/// config.
#[doc(hidden)]
pub mod test_support {
    use super::run_git;
    use std::path::Path;

    /// [`run_git`], plus the local `file` transport (which `run_git`
    /// switches off with every other one), so fixtures can push to a bare
    /// repository in a temp directory.
    pub async fn git(dir: &Path, args: &[&str]) -> String {
        let mut argv = vec!["-c", "protocol.file.allow=always"];
        argv.extend_from_slice(args);
        run_git(dir, &argv, &[])
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

    /// A marker-writing script, and a shell snippet that runs it.
    fn marker_script(dir: &Path, name: &str) -> (PathBuf, String) {
        let marker = dir.join(format!("{name}-ran"));
        (
            marker.clone(),
            format!("sh -c 'touch {}; cat'", marker.display()),
        )
    }

    #[tokio::test]
    async fn repo_configured_filters_and_hooks_never_run_unconfined() {
        // What a confined process could plant by writing .git/config,
        // .git/hooks or .gitattributes: none of it may run in the
        // checkpointer's unconfined git, on checkpoint or on restore.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let marks = tempfile::tempdir().unwrap();
        let (clean, clean_cmd) = marker_script(marks.path(), "clean");
        let (smudge, smudge_cmd) = marker_script(marks.path(), "smudge");
        run_git(&cwd, &["config", "filter.evil.clean", &clean_cmd], &[])
            .await
            .unwrap();
        run_git(&cwd, &["config", "filter.evil.smudge", &smudge_cmd], &[])
            .await
            .unwrap();
        std::fs::write(cwd.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        let hook = cwd.join(".git/hooks/reference-transaction");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        let reftx = marks.path().join("reftx-ran");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", reftx.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(cwd.join("tracked.txt"), "changed\n").unwrap();

        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let refs = checkpoint_refs(&cwd).await;
        assert_eq!(refs.len(), 1, "the checkpoint itself still works");
        std::fs::write(cwd.join("tracked.txt"), "later\n").unwrap();
        cp.restore_to(&refs[0], &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(cwd.join("tracked.txt")).unwrap(),
            "changed\n"
        );

        assert!(!clean.exists(), "a repo clean filter ran unconfined");
        assert!(!smudge.exists(), "a repo smudge filter ran unconfined");
        assert!(!reftx.exists(), "a repo hook ran unconfined");
    }

    #[test]
    fn filter_overrides_switch_off_every_listed_driver_once() {
        let listed = listing(&[
            ("local", "file:.git/config", "filter.evil.clean", Some("x")),
            ("local", "file:.git/config", "filter.evil.smudge", Some("x")),
            (
                "local",
                "file:.git/config",
                "filter.with.dots.process",
                Some("x"),
            ),
            (
                "worktree",
                "file:.git/config.worktree",
                "filter.evil.clean",
                Some("x"),
            ),
            // The user's own (git-lfs, say) is trusted and left alone.
            (
                "global",
                "file:/home/u/.gitconfig",
                "filter.lfs.clean",
                Some("x"),
            ),
        ]);
        let args = check_repo_config(&listed, Path::new("/repo")).unwrap();
        let settings: Vec<&str> = args.iter().skip(1).step_by(2).map(String::as_str).collect();
        assert_eq!(
            settings,
            vec![
                "filter.evil.clean=",
                "filter.evil.smudge=",
                "filter.evil.process=",
                "filter.evil.required=false",
                "filter.with.dots.clean=",
                "filter.with.dots.smudge=",
                "filter.with.dots.process=",
                "filter.with.dots.required=false",
            ]
        );
        assert!(args.iter().step_by(2).all(|a| a == "-c"));
        assert!(repo_overrides(&[]).is_empty());
    }

    /// A `git config --list --show-scope --show-origin -z` listing.
    fn listing(entries: &[(&str, &str, &str, Option<&str>)]) -> Vec<u8> {
        let mut out = String::new();
        for (scope, origin, key, value) in entries {
            out.push_str(&format!("{scope}\0{origin}\0{key}"));
            if let Some(value) = value {
                out.push_str(&format!("\n{value}"));
            }
            out.push('\0');
        }
        out.into_bytes()
    }

    #[tokio::test]
    async fn blocking_args_cover_a_repo_filter() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        run_git(&cwd, &["config", "filter.evil.clean", "touch x"], &[])
            .await
            .unwrap();
        let args = unconfined_git_args_blocking(&cwd).unwrap();
        for needed in [
            "core.fsmonitor=false",
            "core.hooksPath=/dev/null",
            "filter.evil.clean=",
        ] {
            assert!(
                args.iter().any(|a| a == needed),
                "{needed} missing from {args:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_global_filter_still_applies() {
        // Filters from the user's own (global) config — git-lfs, say — sit
        // outside anything a confined process can write and keep working.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let global = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(global.path(), "[filter \"upper\"]\n\tclean = tr a-z A-Z\n").unwrap();
        std::fs::write(cwd.join(".gitattributes"), "*.txt filter=upper\n").unwrap();
        std::fs::write(cwd.join("tracked.txt"), "shout\n").unwrap();
        let env = [("GIT_CONFIG_GLOBAL", global.path().to_str().unwrap())];
        run_git(&cwd, &["add", "tracked.txt"], &env).await.unwrap();
        let staged = run_git(&cwd, &["show", ":tracked.txt"], &env)
            .await
            .unwrap();
        assert_eq!(staged, "SHOUT\n");
    }

    #[tokio::test]
    async fn a_repo_configured_fsmonitor_never_runs() {
        // `.git/config` can name a program for git to run (core.fsmonitor);
        // checkpoints run unconfined, so they must not run it.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let marker = dir.path().join("fsmonitor-ran");
        let hook = format!("sh -c 'touch {}; exit 1' --", marker.display());
        run_git(dir.path(), &["config", "core.fsmonitor", &hook], &[])
            .await
            .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();

        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;

        assert_eq!(checkpoint_refs(dir.path()).await.len(), 1);
        assert!(!marker.exists(), "a checkpoint ran the repo's fsmonitor");
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
    async fn a_snapshot_whose_ref_failed_is_not_a_duplicate_of_the_next() {
        // The ref directory is read-only, so the first snapshot fails at
        // `update-ref`. The second, of the same tree, used to be skipped as
        // a duplicate and report success with no ref behind it — and
        // aivyx-coder then took an older ref as "just before this call".
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let refs = dir.path().join(".git/refs/aivyx/checkpoints");
        std::fs::create_dir_all(&refs).unwrap();
        std::fs::set_permissions(&refs, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        let cancel = CancellationToken::new();
        assert!(cp.try_checkpoint("write_file", &cancel).await.is_err());
        assert!(
            cp.try_checkpoint("edit_file", &cancel).await.is_err(),
            "still no ref"
        );

        std::fs::set_permissions(&refs, std::fs::Permissions::from_mode(0o755)).unwrap();
        cp.try_checkpoint("edit_file", &cancel).await.unwrap();
        assert_eq!(checkpoint_refs(dir.path()).await.len(), 1);
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
    async fn try_checkpoint_returns_the_underlying_error() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        std::fs::remove_dir_all(cwd.join(".git")).unwrap();

        let result = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await;

        assert!(result.is_err(), "try_checkpoint must surface the failure");
    }

    #[tokio::test]
    async fn checkpoint_still_logs_and_continues_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        std::fs::remove_dir_all(cwd.join(".git")).unwrap();

        // Must not panic — checkpoint() keeps its best-effort behaviour.
        cp.checkpoint("write_file", &CancellationToken::new()).await;
    }

    #[tokio::test]
    async fn bare_deny_patterns_exclude_every_matching_basename_from_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        std::fs::write(cwd.join(".env"), "SECRET=1\n").unwrap();
        std::fs::create_dir(cwd.join("svc")).unwrap();
        std::fs::write(cwd.join("svc").join(".env"), "SECRET=2\n").unwrap();
        std::fs::create_dir(cwd.join("certs")).unwrap();
        std::fs::write(cwd.join("certs").join("a.pem"), "CERT\n").unwrap();
        std::fs::write(cwd.join("public.txt"), "fine\n").unwrap();

        let deny = vec![PathBuf::from(".env"), PathBuf::from("*.pem")];
        let cp = GitCheckpointer::detect(&cwd, deny).await.unwrap();
        cp.checkpoint("run_shell", &CancellationToken::new()).await;

        let refs = checkpoint_refs(&cwd).await;
        let tree = run_git(&cwd, &["ls-tree", "-r", "--name-only", &refs[0]], &[])
            .await
            .unwrap();
        assert!(
            !tree.contains(".env"),
            "bare `.env` leaked into the snapshot: {tree}"
        );
        assert!(
            !tree.contains("a.pem"),
            "bare `*.pem` leaked into the snapshot: {tree}"
        );
        assert!(tree.contains("public.txt"));
    }

    #[tokio::test]
    async fn restore_to_leaves_a_bare_pattern_denied_file_created_after_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();

        let deny = vec![PathBuf::from(".env")];
        let cp = GitCheckpointer::detect(&cwd, deny).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        std::fs::create_dir(cwd.join("svc")).unwrap();
        std::fs::write(cwd.join("svc").join(".env"), "SECRET=1\n").unwrap();

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        assert!(
            cwd.join("svc").join(".env").exists(),
            "restore_to must never delete a bare-pattern denied file"
        );
    }

    #[tokio::test]
    async fn a_malformed_local_config_makes_try_checkpoint_fail_instead_of_silently_skipping_overrides()
     {
        // Regression guard: repo_program_overrides used to only push a
        // listing on command success, silently treating a genuine config
        // read failure (not just "no matching key") as "no filters to
        // neutralise" — which could let a repo-configured filter run
        // unconfined.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();

        let config = cwd.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents.push_str("[bad\n");
        std::fs::write(&config, contents).unwrap();

        let result = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await;

        assert!(
            result.is_err(),
            "a malformed local config must surface as an error, not be silently ignored"
        );
    }

    #[tokio::test]
    async fn blocking_args_still_time_out_a_wedged_config_query() {
        let start = std::time::Instant::now();
        let mut command = std::process::Command::new("sleep");
        command.arg("5");
        let result = blocking_output_within(command, Duration::from_millis(100));
        assert!(
            result.is_none(),
            "a timed-out command must not return output"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the timeout must actually bound the wait, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn git_dir_env_still_gets_its_overrides_with_no_dotgit_ancestor_in_cwd() {
        // Regression guard: GIT_DIR (inherited by the spawned `git` process
        // exactly like any other env var) lets a caller point git at a
        // repository that isn't anywhere in `cwd`'s own ancestry. The old
        // ancestor-walk pre-check (`finds_a_repository`) didn't know about
        // this and would skip querying — and therefore neutralising —
        // that repository's own filters entirely.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let repo_dir = repo.path().canonicalize().unwrap();
        run_git(
            &repo_dir,
            &["config", "filter.evil.clean", "touch should-not-run"],
            &[],
        )
        .await
        .unwrap();
        let git_dir = repo_dir.join(".git");

        let elsewhere = tempfile::tempdir().unwrap();
        let cwd = elsewhere.path().canonicalize().unwrap();
        assert!(
            !cwd.join(".git").exists(),
            "sanity check: cwd must have no .git of its own"
        );

        let envs = [("GIT_DIR", git_dir.to_str().unwrap())];
        let overrides = repo_program_overrides(&cwd, &envs).await.unwrap();
        assert!(
            overrides.iter().any(|a| a == "filter.evil.clean="),
            "GIT_DIR's own repo-configured filter must still be neutralised: {overrides:?}"
        );
    }

    #[tokio::test]
    async fn git_dir_env_with_unreadable_config_still_surfaces_as_an_error() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let repo_dir = repo.path().canonicalize().unwrap();
        let config = repo_dir.join(".git/config");
        let mut contents = std::fs::read_to_string(&config).unwrap();
        contents.push_str("[bad\n");
        std::fs::write(&config, contents).unwrap();
        let git_dir = repo_dir.join(".git");

        let elsewhere = tempfile::tempdir().unwrap();
        let cwd = elsewhere.path().canonicalize().unwrap();

        let envs = [("GIT_DIR", git_dir.to_str().unwrap())];
        let result = repo_program_overrides(&cwd, &envs).await;
        assert!(
            result.is_err(),
            "a malformed config reached only via GIT_DIR must still surface as an error"
        );
    }

    #[tokio::test]
    async fn checkpoint_ref_names_pad_the_pid_to_a_fixed_width() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;

        let refs = checkpoint_refs(dir.path()).await;
        assert_eq!(refs.len(), 1);
        let pid_field = refs[0]
            .rsplit('/')
            .next()
            .unwrap()
            .split('-')
            .nth(1)
            .expect("ref name must have a pid field");
        assert_eq!(
            pid_field,
            format!("{:07}", std::process::id()),
            "pid field must be zero-padded to a fixed width: {}",
            refs[0]
        );
    }

    #[tokio::test]
    async fn checkpoint_ref_names_include_the_process_id() {
        // Regression guard: two checkpointer instances (standing in for two
        // separate processes sharing one repo) both start their own `seq`
        // counter at 0, so a millis-collision between them would otherwise
        // produce the exact same ref name and clobber each other's ref.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;

        let refs = checkpoint_refs(dir.path()).await;
        assert_eq!(refs.len(), 1);
        let pid = std::process::id().to_string();
        assert!(
            refs[0].contains(&pid),
            "ref name must include this process's pid: {}",
            refs[0]
        );
    }

    #[tokio::test]
    async fn ref_names_from_every_historical_format_still_list_and_prune_together() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let head = run_git(&cwd, &["rev-parse", "HEAD"], &[]).await.unwrap();
        let head = head.trim();

        // Three ref shapes this crate has produced, oldest to newest:
        // no pid at all, an unpadded pid (this fix's own immediately
        // preceding format), and — what `checkpoint` mints today — a
        // zero-padded pid. All three must sort, list, and prune together.
        // Comfortably older than anything `checkpoint` mints below.
        let base_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - 20_000;
        let no_pid_ref = format!("refs/aivyx/checkpoints/{base_millis:013}-0000");
        let unpadded_pid_ref = format!("refs/aivyx/checkpoints/{:013}-42-0000", base_millis + 1);
        for r in [&no_pid_ref, &unpadded_pid_ref] {
            run_git(&cwd, &["update-ref", r, head], &[]).await.unwrap();
        }

        let mut cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.set_retain(2);
        for i in 0..3 {
            std::fs::write(cwd.join("tracked.txt"), format!("v{i}\n")).unwrap();
            cp.checkpoint("write_file", &CancellationToken::new()).await;
        }

        let refs = checkpoint_refs(&cwd).await;
        assert_eq!(
            refs.len(),
            2,
            "retention must still cap total refs with older-format refs mixed in: {refs:?}"
        );
        assert!(
            !refs.contains(&no_pid_ref) && !refs.contains(&unpadded_pid_ref),
            "the two oldest refs (older formats) should have been pruned first: {refs:?}"
        );
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
    async fn checkpoint_removes_a_stale_private_index_lock() {
        // Regression guard: a checkpoint cancelled or timed out mid-`git
        // add` leaves `<git-dir>/aivyx/index.lock` behind; every later
        // checkpoint must still succeed rather than failing silently
        // forever against that stale lock.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();

        let index_dir = cwd.join(".git/aivyx");
        std::fs::create_dir_all(&index_dir).unwrap();
        let lock = index_dir.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let stale = std::time::SystemTime::now() - GIT_TIMEOUT - Duration::from_secs(5);
        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(stale)
            .unwrap();

        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.checkpoint("write_file", &CancellationToken::new()).await;

        assert_eq!(
            checkpoint_refs(&cwd).await.len(),
            1,
            "checkpoint must succeed despite the stale lock"
        );
        assert!(!lock.exists(), "the stale lock must be removed");
    }

    #[tokio::test]
    async fn a_fresh_private_index_lock_is_left_alone() {
        // A lock younger than the crate's git timeout belongs to a
        // checkpoint that may still be genuinely in flight (e.g. a
        // concurrent call) — it must not be swept away.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();

        let index_dir = cwd.join(".git/aivyx");
        std::fs::create_dir_all(&index_dir).unwrap();
        let lock = index_dir.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        // Freshly written: mtime is "now", well inside the timeout window.

        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        let result = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await;

        assert!(
            result.is_err(),
            "a fresh lock should still block git add, proving it wasn't removed"
        );
        assert!(lock.exists(), "a fresh lock must not be removed");
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

    async fn tree_of(dir: &Path, rev: &str) -> String {
        run_git(dir, &["ls-tree", "-r", "--name-only", rev], &[])
            .await
            .unwrap()
    }

    fn generated() -> Vec<String> {
        vec!["target/".to_string(), "*.pyc".to_string()]
    }

    #[tokio::test]
    async fn untracked_generated_files_stay_out_of_the_snapshot() {
        // A fresh project with no .gitignore: build output must not be
        // copied into every checkpoint (2026-10-22: 305 MB of `target/`
        // objects after one 33-minute run).
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/app"), "binary\n").unwrap();
        std::fs::create_dir(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/mod.pyc"), "bytecode\n").unwrap();
        std::fs::write(dir.path().join("pkg/mod.py"), "source\n").unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(generated());
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();

        let tree = tree_of(dir.path(), &checkpoint_refs(dir.path()).await[0]).await;
        assert!(tree.contains("pkg/mod.py"), "{tree}");
        assert!(!tree.contains("target/"), "{tree}");
        assert!(!tree.contains(".pyc"), "{tree}");
    }

    #[tokio::test]
    async fn generated_paths_the_gitignore_already_covers_dont_fail_the_checkpoint() {
        // 2026-10-23, ckl build test: a project whose .gitignore lists
        // node_modules/ failed every checkpoint (87 in one run) because the
        // generated-files exclusion named that already-ignored path in
        // `git add`, which git refuses.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::write(
            dir.path().join(".gitignore"),
            "node_modules/\ndist/\n*.log\n",
        )
        .unwrap();
        test_support::git(dir.path(), &["add", ".gitignore"]).await;
        std::fs::create_dir_all(dir.path().join("node_modules/react")).unwrap();
        std::fs::write(dir.path().join("node_modules/react/index.js"), "x\n").unwrap();
        std::fs::create_dir(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/app.pyc"), "bytecode\n").unwrap();
        std::fs::create_dir(dir.path().join("pkg")).unwrap();
        std::fs::write(dir.path().join("pkg/mod.pyc"), "bytecode\n").unwrap();
        std::fs::write(dir.path().join("pkg/mod.py"), "source\n").unwrap();
        std::fs::write(dir.path().join("run.log"), "log\n").unwrap();
        std::fs::write(dir.path().join("src.ts"), "code\n").unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(vec!["node_modules/".into(), "*.pyc".into(), "*.log".into()]);
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();

        let tree = tree_of(dir.path(), &checkpoint_refs(dir.path()).await[0]).await;
        assert!(
            tree.contains("src.ts") && tree.contains("pkg/mod.py"),
            "{tree}"
        );
        for gone in ["node_modules", "dist", ".pyc", "run.log"] {
            assert!(!tree.contains(gone), "{gone} in {tree}");
        }
    }

    #[tokio::test]
    async fn tracked_files_matching_a_generated_pattern_are_still_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::write(dir.path().join("keep.pyc"), "v1\n").unwrap();
        test_support::git(dir.path(), &["add", "keep.pyc"]).await;
        std::fs::write(dir.path().join("keep.pyc"), "v2\n").unwrap();

        let cp = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(generated());
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();

        let refs = checkpoint_refs(dir.path()).await;
        let content = run_git(dir.path(), &["show", &format!("{}:keep.pyc", refs[0])], &[])
            .await
            .unwrap();
        assert_eq!(content, "v2\n");
    }

    #[tokio::test]
    async fn generated_files_snapshotted_before_are_dropped_from_later_checkpoints() {
        // The private index persists across sessions: entries an older
        // version (or an empty ignore list) staged must not linger.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/out"), "1\n").unwrap();
        let before = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        before
            .try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        assert!(
            tree_of(dir.path(), &checkpoint_refs(dir.path()).await[0])
                .await
                .contains("target/out")
        );

        std::fs::write(dir.path().join("target/out"), "2\n").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "v2\n").unwrap();
        let after = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(generated());
        after
            .try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let latest = after.latest_ref(&CancellationToken::new()).await.unwrap();
        let tree = tree_of(dir.path(), &latest).await;
        assert!(tree.contains("tracked.txt"), "{tree}");
        assert!(!tree.contains("target/"), "{tree}");
    }

    #[tokio::test]
    async fn restore_to_leaves_generated_files_alone() {
        // Not in any snapshot, so a rewind must neither delete nor revert
        // build output — while still deleting real files created since.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(generated());
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let good_ref = cp.latest_ref(&CancellationToken::new()).await.unwrap();

        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/out"), "built\n").unwrap();
        std::fs::write(dir.path().join("new.rs"), "oops\n").unwrap();
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();

        cp.restore_to(&good_ref, &CancellationToken::new())
            .await
            .unwrap();

        assert!(!dir.path().join("new.rs").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target/out")).unwrap(),
            "built\n"
        );
    }

    #[tokio::test]
    async fn tree_in_index_matches_the_checkpoint_it_would_take() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/out"), "built\n").unwrap();
        std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();
        let cp = GitCheckpointer::detect(dir.path(), vec![])
            .await
            .unwrap()
            .with_generated_ignore(generated());
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let latest = cp.latest_ref(&CancellationToken::new()).await.unwrap();
        let want = run_git(
            dir.path(),
            &["rev-parse", &format!("{latest}^{{tree}}")],
            &[],
        )
        .await
        .unwrap();

        let git_dir = dir.path().join(".git");
        let copy = git_dir.join("verify-index");
        std::fs::copy(git_dir.join("aivyx/index"), &copy).unwrap();
        let got = cp
            .tree_in_index(&copy, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(got, want.trim());
    }

    #[tokio::test]
    async fn a_symlinked_private_index_dir_is_not_written_through() {
        // `.git` lies inside the directory a confined command may write, so
        // it can replace `.git/aivyx` with a symlink to any directory.
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let cp = GitCheckpointer::detect(repo.path(), vec![]).await.unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join(".git/aivyx")).unwrap();

        let result = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await;

        let written: Vec<_> = std::fs::read_dir(outside.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            written.is_empty(),
            "unconfined git wrote outside the work tree: {written:?}"
        );
        assert!(result.is_err(), "the checkpoint must fail, not succeed");
        assert!(
            GitCheckpointer::detect(repo.path(), vec![]).await.is_none(),
            "detection refuses the planted layout too"
        );
    }

    #[tokio::test]
    async fn a_stale_lock_behind_a_symlinked_private_index_dir_is_not_deleted() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let lock = outside.path().join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let stale = std::time::SystemTime::now() - GIT_TIMEOUT - Duration::from_secs(5);
        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        let cp = GitCheckpointer::detect(repo.path(), vec![]).await.unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join(".git/aivyx")).unwrap();

        let _ = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await;
        assert!(lock.exists(), "a lock file was deleted through a symlink");
    }

    /// Plain git, as a confined command could run it.
    fn plain_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Runs `body` with the allow-list switched off, so only the second
    /// layer (fixed overrides, hardened environment, explicit work tree)
    /// protects the checkpoint.
    async fn second_layer_only<F: std::future::Future<Output = ()>>(body: F) {
        ACCEPT_ANY_REPO_CONFIG.with(|accept| accept.set(true));
        body.await;
        ACCEPT_ANY_REPO_CONFIG.with(|accept| accept.set(false));
    }

    #[tokio::test]
    async fn without_the_allow_list_no_transport_or_lazy_fetch_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir(&cwd).unwrap();
        init_repo(&cwd).await;
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        let marker = tmp.path().join("MARKER");
        for (key, value) in [
            ("core.repositoryformatversion", "1".to_string()),
            ("extensions.partialClone", "origin".into()),
            ("remote.origin.promisor", "true".into()),
            ("remote.origin.url", "ssh://example.invalid/x".into()),
            (
                "core.sshCommand",
                format!("touch {}; false", marker.display()),
            ),
        ] {
            plain_git(&cwd, &["config", key, &value]);
        }
        let head = run_git(&cwd, &["symbolic-ref", "HEAD"], &[])
            .await
            .unwrap_err();
        assert!(head.contains("sshcommand"), "the allow-list is on: {head}");
        std::fs::write(
            cwd.join(".git/refs/heads/main"),
            "1111111111111111111111111111111111111111\n",
        )
        .unwrap();
        std::fs::write(cwd.join("tracked.txt"), "changed\n").unwrap();

        second_layer_only(async {
            // Fails on the missing parent instead of fetching it.
            let _ = cp
                .try_checkpoint("edit_file", &CancellationToken::new())
                .await;
        })
        .await;
        assert!(!marker.exists(), "a transport command ran");
    }

    #[tokio::test]
    async fn without_the_allow_list_the_work_tree_is_still_the_detected_one() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        let secret = tmp.path().join("secret");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("id_ed25519"), "SECRETKEY").unwrap();
        std::fs::write(secret.join("tracked.txt"), "keep me\n").unwrap();
        init_repo(&cwd).await;
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        plain_git(&cwd, &["config", "core.worktree", secret.to_str().unwrap()]);
        std::fs::write(cwd.join("tracked.txt"), "changed\n").unwrap();

        second_layer_only(async {
            let cancel = CancellationToken::new();
            cp.try_checkpoint("edit_file", &cancel).await.unwrap();
            let latest = cp.latest_ref(&cancel).await.unwrap();
            let tree = cp
                .git(
                    &[
                        "ls-tree".into(),
                        "-r".into(),
                        "--name-only".into(),
                        latest.clone(),
                    ],
                    &[],
                    &cancel,
                )
                .await
                .unwrap();
            assert!(!tree.contains("id_ed25519"), "{tree}");
            assert!(tree.contains("tracked.txt"), "{tree}");
            std::fs::write(cwd.join("tracked.txt"), "later\n").unwrap();
            cp.restore_to(&latest, &cancel).await.unwrap();
        })
        .await;
        assert_eq!(
            std::fs::read_to_string(cwd.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(secret.join("tracked.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn an_ordinary_repository_config_is_allowed() {
        let local = |key: &'static str, value: &'static str| {
            ("local", "file:.git/config", key, Some(value))
        };
        let listed = listing(&[
            local("core.repositoryformatversion", "1"),
            local("core.filemode", "true"),
            local("core.bare", "false"),
            local("core.logallrefupdates", "true"),
            local("extensions.objectformat", "sha256"),
            local("extensions.worktreeconfig", "true"),
            local("remote.origin.url", "git@github.com:a/b.git"),
            local("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"),
            local("remote.upstream.gh-resolved", "base"),
            local("branch.main.remote", "origin"),
            local("branch.main.merge", "refs/heads/main"),
            local("branch.feature.vscode-merge-base", "origin/main"),
            local("user.email", "me@example.invalid"),
            local("lfs.repositoryformatversion", "0"),
            local("lfs.https://example.invalid/b.git/info/lfs.access", "basic"),
            local("filter.lfs.clean", "git-lfs clean -- %f"),
            local("filter.lfs.required", "true"),
            local("diff.external", "/x"),
            local("diff.git-crypt.textconv", "\"git-crypt\" diff"),
            local("gpg.format", "ssh"),
            local("gpg.program", "/x"),
            local("gpg.ssh.program", "/x"),
            local("submodule.vendor/lib.url", "https://example.invalid/lib"),
            local("submodule.active", "."),
            local("gui.wmstate", "normal"),
            local("pull.rebase", "true"),
            (
                "worktree",
                "file:.git/config.worktree",
                "core.sparsecheckout",
                Some("true"),
            ),
            (
                "worktree",
                "file:.git/config.worktree",
                "core.sparsecheckoutcone",
                Some("true"),
            ),
            // The user's own config is not checked.
            (
                "global",
                "file:/home/u/.gitconfig",
                "credential.helper",
                Some("store"),
            ),
            ("command", "command line:", "core.sshcommand", Some("ssh")),
        ]);
        let args = check_repo_config(&listed, Path::new("/repo")).unwrap();
        let settings: Vec<&str> = args.iter().skip(1).step_by(2).map(String::as_str).collect();
        assert_eq!(
            settings,
            vec![
                "filter.lfs.clean=",
                "filter.lfs.smudge=",
                "filter.lfs.process=",
                "filter.lfs.required=false",
                "diff.external=",
                "diff.git-crypt.textconv=",
                "diff.git-crypt.command=",
                "gpg.program=",
                "gpg.ssh.program=",
            ]
        );
    }

    #[test]
    fn settings_outside_the_allow_list_are_refused_by_name_only() {
        for (key, value) in [
            ("core.sshcommand", Some("SECRET-VALUE")),
            ("core.worktree", Some("SECRET-VALUE")),
            ("core.bare", Some("SECRET-VALUE")),
            ("core.bare", None),
            ("core.repositoryformatversion", Some("SECRET-VALUE")),
            ("core.pager", Some("SECRET-VALUE")),
            ("core.editor", Some("SECRET-VALUE")),
            ("core.excludesfile", Some("SECRET-VALUE")),
            ("core.alternaterefscommand", Some("SECRET-VALUE")),
            ("extensions.partialclone", Some("SECRET-VALUE")),
            ("remote.origin.promisor", Some("SECRET-VALUE")),
            ("remote.origin.uploadpack", Some("SECRET-VALUE")),
            ("remote.origin.vcs", Some("SECRET-VALUE")),
            ("include.path", Some("SECRET-VALUE")),
            ("includeif.gitdir:/.path", Some("SECRET-VALUE")),
            ("credential.helper", Some("SECRET-VALUE")),
            ("gpg.ssh.defaultkeycommand", Some("SECRET-VALUE")),
            ("diff.evil.unknown", Some("SECRET-VALUE")),
            ("merge.evil.driver", Some("SECRET-VALUE")),
            ("gc.recentobjectshook", Some("SECRET-VALUE")),
            ("lfs.customtransfer.evil.path", Some("SECRET-VALUE")),
            ("alias.st", Some("SECRET-VALUE")),
            ("sequence.editor", Some("SECRET-VALUE")),
            ("uploadpack.packobjectshook", Some("SECRET-VALUE")),
            ("core.sharedrepository", Some("SECRET-VALUE")),
        ] {
            let listed = listing(&[("local", "file:.git/config", key, value)]);
            let err = check_repo_config(&listed, Path::new("/repo")).unwrap_err();
            assert!(err.contains(key), "{key}: {err}");
            if value.is_some() {
                assert!(
                    !err.contains("SECRET-VALUE"),
                    "{key} leaked its value: {err}"
                );
            }
        }
        // A global config file that lies inside the work tree is the
        // repository's to write, so it is checked too.
        let listed = listing(&[(
            "global",
            "file:/home/u/.gitconfig",
            "core.sshcommand",
            Some("x"),
        )]);
        assert!(check_repo_config(&listed, Path::new("/home/u")).is_err());
        assert!(check_repo_config(&listed, Path::new("/home/u/proj")).is_ok());
    }

    #[tokio::test]
    async fn run_git_and_the_blocking_args_refuse_a_disallowed_setting() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        plain_git(&cwd, &["config", "core.pager", "cat"]);
        let err = run_git(&cwd, &["status"], &[]).await.unwrap_err();
        assert!(err.contains("core.pager"), "{err}");
        let err = unconfined_git_args_blocking(&cwd).unwrap_err();
        assert!(err.contains("core.pager"), "{err}");
        assert!(
            GitCheckpointer::detect(&cwd, vec![]).await.is_none(),
            "detection refuses it too"
        );
        plain_git(&cwd, &["config", "--unset", "core.pager"]);
        assert!(GitCheckpointer::detect(&cwd, vec![]).await.is_some());
    }

    #[tokio::test]
    async fn repo_diff_drivers_are_allowed_but_never_run() {
        // git-crypt and textconv setups put diff drivers in .git/config:
        // checkpoints keep working, and no diff run through `run_git` can
        // start one, even without `--no-ext-diff --no-textconv`.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let marks = tempfile::tempdir().unwrap();
        let (ext, ext_cmd) = marker_script(marks.path(), "ext");
        let (conv, conv_cmd) = marker_script(marks.path(), "conv");
        plain_git(&cwd, &["config", "diff.external", &ext_cmd]);
        plain_git(&cwd, &["config", "diff.conv.textconv", &conv_cmd]);
        plain_git(&cwd, &["config", "gpg.program", &ext_cmd]);
        std::fs::write(cwd.join(".gitattributes"), "*.txt diff=conv\n").unwrap();
        std::fs::write(cwd.join("tracked.txt"), "v2\n").unwrap();

        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let _ = run_git(&cwd, &["diff"], &[]).await;
        let _ = run_git(&cwd, &["log", "-p", "-1"], &[]).await;
        let diff = run_git(&cwd, &["diff", "--no-ext-diff", "--no-textconv"], &[])
            .await
            .unwrap();
        assert!(diff.contains("+v2"), "{diff}");
        assert!(!ext.exists(), "a repo external diff ran unconfined");
        assert!(!conv.exists(), "a repo textconv ran unconfined");
    }

    #[tokio::test]
    async fn prune_deletes_a_planted_symbolic_ref_itself_not_its_target() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let planted = cwd.join(".git/refs/aivyx/checkpoints");
        std::fs::create_dir_all(&planted).unwrap();
        // Sorts first, so it is the first to be pruned.
        plain_git(
            &cwd,
            &[
                "symbolic-ref",
                "refs/aivyx/checkpoints/0000000000000-0000000-0000",
                "refs/heads/main",
            ],
        );
        let mut cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        cp.set_retain(1);
        std::fs::write(cwd.join("tracked.txt"), "v2\n").unwrap();
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        run_git(&cwd, &["rev-parse", "--verify", "refs/heads/main"], &[])
            .await
            .expect("the branch survives");
        assert_eq!(checkpoint_refs(&cwd).await.len(), 1);
    }

    #[tokio::test]
    async fn a_symlinked_ref_or_object_directory_is_refused() {
        for planted in ["refs/aivyx", "objects/ab", "logs/refs/aivyx"] {
            let dir = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            init_repo(dir.path()).await;
            let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
            let path = dir.path().join(".git").join(planted);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let _ = std::fs::remove_dir_all(&path);
            std::os::unix::fs::symlink(outside.path(), &path).unwrap();
            std::fs::write(dir.path().join("tracked.txt"), "v2\n").unwrap();
            let err = cp
                .try_checkpoint("write_file", &CancellationToken::new())
                .await
                .unwrap_err();
            assert!(err.contains("symbolic link"), "{planted}: {err}");
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn borrowed_objects_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        std::fs::write(
            dir.path().join(".git/objects/info/alternates"),
            format!("{}\n", other.path().display()),
        )
        .unwrap();
        let err = cp
            .try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.contains("alternates"), "{err}");
    }

    #[tokio::test]
    async fn a_planted_dot_git_file_is_not_followed() {
        // A checkpointer detected earlier keeps using the git directory it
        // found; a new one refuses a `.git` file pointing at a repository
        // that isn't a linked worktree of this one.
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        init_repo(other.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
        let real = cwd.join("real-git");
        std::fs::rename(cwd.join(".git"), &real).unwrap();
        std::fs::write(
            cwd.join(".git"),
            format!("gitdir: {}\n", other.path().join(".git").display()),
        )
        .unwrap();
        assert!(GitCheckpointer::detect(&cwd, vec![]).await.is_none());
        // The old git directory has moved, so the earlier one refuses.
        assert!(
            cp.try_checkpoint("write_file", &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(
            run_git(other.path(), &["for-each-ref", "refs/aivyx/"], &[])
                .await
                .unwrap()
                .is_empty(),
            "nothing was written into the other repository"
        );
    }

    #[tokio::test]
    async fn a_linked_worktree_still_checkpoints_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let main = dir.path().canonicalize().unwrap();
        let linked = main.parent().unwrap().join(format!(
            "{}-linked",
            main.file_name().unwrap().to_string_lossy()
        ));
        run_git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
            &[],
        )
        .await
        .unwrap();
        let cp = GitCheckpointer::detect(&linked, vec![]).await.unwrap();
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let good = cp.latest_ref(&CancellationToken::new()).await.unwrap();
        std::fs::write(linked.join("tracked.txt"), "broken\n").unwrap();
        cp.restore_to(&good, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(linked.join("tracked.txt")).unwrap(),
            "v1\n"
        );
        std::fs::remove_dir_all(&linked).unwrap();
    }

    #[tokio::test]
    async fn a_subdirectory_checkpointer_snapshots_only_its_subtree() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "a\n").unwrap();
        let cp = GitCheckpointer::detect(&sub, vec![]).await.unwrap();
        cp.try_checkpoint("write_file", &CancellationToken::new())
            .await
            .unwrap();
        let latest = cp.latest_ref(&CancellationToken::new()).await.unwrap();
        let tree = tree_of(dir.path(), &latest).await;
        assert_eq!(tree.trim(), "sub/a.txt");
    }

    #[tokio::test]
    async fn restore_to_accepts_a_full_object_id_but_no_other_name() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cp = GitCheckpointer::detect(dir.path(), vec![]).await.unwrap();
        let cancel = CancellationToken::new();
        cp.try_checkpoint("write_file", &cancel).await.unwrap();
        let taken = cp.checkpoints();
        assert_eq!(taken.len(), 1);
        assert_eq!(
            cp.latest_ref(&cancel).await.as_deref(),
            Some(taken[0].ref_name.as_str())
        );
        std::fs::write(dir.path().join("tracked.txt"), "broken\n").unwrap();
        for refused in ["main", "HEAD", &taken[0].commit[..12]] {
            assert!(cp.restore_to(refused, &cancel).await.is_err(), "{refused}");
        }
        cp.restore_to(&taken[0].commit, &cancel).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "v1\n"
        );
    }
}
