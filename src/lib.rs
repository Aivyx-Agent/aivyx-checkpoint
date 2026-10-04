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
        // year 2286), which is what the retention pass below relies on —
        // and still does with the pid segment added below, since millis
        // remains the leading field either way. The pid disambiguates
        // across processes: `seq` alone starts at 0 in every process (each
        // has its own `GitCheckpointer`), so two processes checkpointing
        // the same repo within the same millisecond used to mint the exact
        // same ref name and clobber each other's ref.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let pid = std::process::id();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let ref_name = format!("refs/aivyx/checkpoints/{millis:013}-{pid}-{seq:04}");
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
    fn private_index(&self) -> Result<PathBuf, String> {
        let index_dir = self.git_dir.join("aivyx");
        std::fs::create_dir_all(&index_dir).map_err(|e| e.to_string())?;
        let index = index_dir.join("index");
        let lock = index_dir.join("index.lock");
        if let Ok(metadata) = std::fs::metadata(&lock)
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
        tokio::select! {
            result = run_git(&self.cwd, &args, envs) => result,
            _ = cancellation.cancelled() => Err("cancelled".to_string()),
        }
    }
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

/// Always passed to unconfined git: no fsmonitor, no hooks (plumbing like
/// `update-ref` runs `reference-transaction`), no commit signing.
const SAFE_GIT_CONFIG: [&str; 6] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "commit.gpgSign=false",
];

/// The `git config` query listing filter drivers defined in the
/// repository's own config for one scope (`--local` / `--worktree`).
const REPO_FILTER_QUERY: [&str; 5] = [
    "config",
    "--includes",
    "--name-only",
    "--get-regexp",
    r"^filter\..+\.(clean|smudge|process)$",
];

/// True when `cwd` or any ancestor contains a `.git` entry — mirroring
/// git's own repository discovery, without invoking git (which would
/// recurse back into [`repo_program_overrides`], the only caller). A `.git`
/// entry can be a directory (the normal case) or a file (a linked worktree
/// or submodule's gitdir pointer) — `exists()` covers either, and reading
/// it to resolve where it actually points isn't needed here: all this
/// answers is "does a repository exist to query", not "where exactly is
/// it".
fn finds_a_repository(cwd: &Path) -> bool {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

fn repo_filter_query(scope: &'static str) -> Vec<&'static str> {
    let mut args: Vec<&str> = SAFE_GIT_CONFIG.to_vec();
    args.push(REPO_FILTER_QUERY[0]);
    args.push(scope);
    args.extend(&REPO_FILTER_QUERY[1..]);
    args
}

/// `-c` overrides switching off every filter driver whose keys appear in
/// `listings` (the output of [`repo_filter_query`], one listing per scope).
fn filter_overrides<'a>(listings: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut drivers: Vec<String> = Vec::new();
    for key in listings.into_iter().flat_map(str::lines) {
        if let Some(driver) = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
            .map(|(driver, _)| driver.to_string())
            && !drivers.contains(&driver)
        {
            drivers.push(driver);
        }
    }
    drivers
        .iter()
        .flat_map(|driver| {
            ["clean", "smudge", "process"]
                .into_iter()
                .map(move |kind| format!("filter.{driver}.{kind}="))
                .chain(std::iter::once(format!("filter.{driver}.required=false")))
        })
        .flat_map(|setting| ["-c".to_string(), setting])
        .collect()
}

/// `-c` overrides that switch off every filter driver defined in the
/// *repository's own* config (`.git/config`, its includes, and a
/// worktree's `config.worktree`) — the config a confined process can write,
/// since `.git` sits inside the directory it may write under. A clean,
/// smudge or process filter there would otherwise run with this crate's
/// unconfined privileges on `add`/`read-tree`. Filters from the user's own
/// global or system config (git-lfs, say) are trusted and left alone.
/// Reading config runs nothing; a query that finds no matching keys (exit
/// status 1 — `--get-regexp`'s documented "not found" status, the normal
/// case for a repository with no filters configured) means nothing to
/// switch off for that scope. Any other failure — a malformed or unreadable
/// config file, the query timing out, failing to spawn `git` at all — is a
/// real error and is returned as one rather than silently treated the same
/// as "no filters": this is the query that decides whether a repo-defined
/// filter driver gets neutralised before running with this crate's
/// unconfined privileges, so continuing with no overrides when the query
/// itself couldn't be trusted would risk running one unneutralised.
async fn repo_program_overrides(cwd: &Path, envs: &[(&str, &str)]) -> Result<Vec<String>, String> {
    // No repository anywhere in `cwd`'s ancestry (the bootstrapping case —
    // `GitCheckpointer::detect`'s own first probe, or a `git init` call
    // that's about to create one, as `test_support::init_repo` makes
    // first) means there's no repo-local config to query at all, which is
    // an entirely different, benign situation from a repository that
    // exists but whose config this crate's query below fails to read —
    // only the latter is the real error this function now surfaces.
    if !finds_a_repository(cwd) {
        return Ok(Vec::new());
    }
    let mut listings = Vec::new();
    for scope in ["--local", "--worktree"] {
        let mut command = tokio::process::Command::new("git");
        command
            .args(repo_filter_query(scope))
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
            .map_err(|_| format!("git config --get-regexp ({scope}) timed out"))?
            .map_err(|e| format!("failed to run git config ({scope}): {e}"))?;
        if output.status.success() {
            listings.push(String::from_utf8_lossy(&output.stdout).into_owned());
        } else if output.status.code() != Some(1) {
            return Err(format!(
                "git config --get-regexp ({scope}) failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(filter_overrides(listings.iter().map(String::as_str)))
}

/// The arguments to put before the subcommand of any *blocking*, unconfined
/// git a caller spawns itself (a preview, a branch-name lookup): the same
/// protections [`run_git`] applies — no fsmonitor, hooks or signing, and the
/// repository's own filter drivers switched off. Diff callers still add
/// `--no-ext-diff --no-textconv` to the diff itself.
pub fn unconfined_git_args_blocking(cwd: &Path) -> Vec<String> {
    let mut args: Vec<String> = SAFE_GIT_CONFIG.iter().map(|s| s.to_string()).collect();
    let listings: Vec<String> = ["--local", "--worktree"]
        .into_iter()
        .filter_map(|scope| {
            let mut command = std::process::Command::new("git");
            command
                .args(repo_filter_query(scope))
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            blocking_output_within(command, GIT_TIMEOUT)
                .map(|out| String::from_utf8_lossy(&out).into_owned())
        })
        .collect();
    args.extend(filter_overrides(listings.iter().map(String::as_str)));
    args
}

/// Runs `command` to completion, killing it and returning `None` if it
/// doesn't finish within `timeout`. The one blocking (non-tokio) process
/// invocation this crate makes — [`run_git`]'s own async path already gets
/// a bound from `tokio::time::timeout`; this gives
/// [`unconfined_git_args_blocking`]'s config queries the same [`GIT_TIMEOUT`]
/// bound rather than running unbounded. Polls with `try_wait` instead of a
/// watcher thread so there's nothing else to clean up on the success path;
/// stdout is only read after the child has exited, which is fine for the
/// short config-key listings this is used for, but would risk a pipe-buffer
/// deadlock for a command with substantial output.
fn blocking_output_within(
    mut command: std::process::Command,
    timeout: Duration,
) -> Option<Vec<u8>> {
    command.stdout(std::process::Stdio::piped());
    let mut child = command.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut buf = Vec::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_end(&mut buf);
                }
                return Some(buf);
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// One plumbing invocation:/// One plumbing invocation: trusted fixed argv (never model-controlled),
/// writing only under `.git`, so it runs unconfined; `kill_on_drop` +
/// timeout bound it instead. `pub` because it has a consumer beyond this
/// file: `aivyx-coder`'s own `wiki.rs` module calls this directly,
/// cross-crate, for its own (unrelated) git plumbing needs.
///
/// Because it is unconfined, it never runs a program the repository's own
/// config names — a confined process can write `.git/config`, `.git/hooks`
/// and `.gitattributes`, since `.git` lies inside the directory it may write
/// under. So fsmonitor and hooks are off, commit signing is off, and every
/// filter driver defined in the repository's own config is switched off
/// (see [`repo_program_overrides`]). Diff drivers only run for diffs:
/// callers that diff pass `--no-ext-diff --no-textconv` themselves.
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
/// test fixture, not part of this crate's real API surface. `init_repo`
/// builds a repo with one committed `tracked.txt` on `main`, with a
/// repo-local identity so tests don't depend on the machine's global git
/// config.
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

    /// A marker-writing script, and a shell snippet that runs it.
    fn marker_script(dir: &Path, name: &str) -> (PathBuf, String) {
        let marker = dir.join(format!("{name}-ran"));
        (marker.clone(), format!("sh -c 'touch {}; cat'", marker.display()))
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
        run_git(&cwd, &["config", "filter.evil.clean", &clean_cmd], &[]).await.unwrap();
        run_git(&cwd, &["config", "filter.evil.smudge", &smudge_cmd], &[]).await.unwrap();
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
        cp.restore_to(&refs[0], &CancellationToken::new()).await.unwrap();
        assert_eq!(std::fs::read_to_string(cwd.join("tracked.txt")).unwrap(), "changed\n");

        assert!(!clean.exists(), "a repo clean filter ran unconfined");
        assert!(!smudge.exists(), "a repo smudge filter ran unconfined");
        assert!(!reftx.exists(), "a repo hook ran unconfined");
    }

    #[test]
    fn filter_overrides_switch_off_every_listed_driver_once() {
        let local = "filter.evil.clean\nfilter.evil.smudge\nfilter.with.dots.process\n";
        let worktree = "filter.evil.clean\n";
        let args = filter_overrides([local, worktree]);
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
        assert!(filter_overrides([""]).is_empty());
    }

    #[tokio::test]
    async fn blocking_args_cover_a_repo_filter() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();
        run_git(&cwd, &["config", "filter.evil.clean", "touch x"], &[]).await.unwrap();
        let args = unconfined_git_args_blocking(&cwd);
        for needed in ["core.fsmonitor=false", "core.hooksPath=/dev/null", "filter.evil.clean="] {
            assert!(args.iter().any(|a| a == needed), "{needed} missing from {args:?}");
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
        let staged = run_git(&cwd, &["show", ":tracked.txt"], &env).await.unwrap();
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
    async fn old_format_refs_are_still_listed_and_pruned_alongside_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path()).await;
        let cwd = dir.path().canonicalize().unwrap();

        // Simulate a ref created by a pre-pid build: `{millis}-{seq}`, no
        // pid component, comfortably older than anything made below.
        let old_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - 10_000;
        let old_ref = format!("refs/aivyx/checkpoints/{old_millis:013}-0000");
        let head = run_git(&cwd, &["rev-parse", "HEAD"], &[]).await.unwrap();
        run_git(&cwd, &["update-ref", &old_ref, head.trim()], &[])
            .await
            .unwrap();

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
            "retention must still cap total refs with an old-format ref mixed in: {refs:?}"
        );
        assert!(
            !refs.contains(&old_ref),
            "the oldest ref (old format) should have been pruned first: {refs:?}"
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
}
