//! Repository state a confined command can write — `.git/config`, refs and
//! paths under `.git`, all inside the directory it may write — must not
//! steer the checkpoint's unconfined git: no program runs, nothing outside
//! the work tree is read or written, and no planted ref is taken for a
//! checkpoint. (2026-10-28 shared-crates audit, N1 and N2.)
//!
//! Every fixture lives in a temp directory; a payload only ever creates a
//! marker file there.

use aivyx_checkpoint::GitCheckpointer;
use std::path::Path;
use std::process::Command;
use tokio_util::sync::CancellationToken;

/// Plain git, not the crate's hardened `run_git`: this is what a confined
/// command could run itself.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Every object in the repository, decompressed.
fn all_objects(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["cat-file", "--batch-all-objects", "--batch"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn repository_config_cannot_make_the_checkpoint_run_a_program() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    std::fs::create_dir(&cwd).unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();

    // A promisor remote whose transport command creates the marker, and a
    // branch pointing at an object that isn't there, so that resolving
    // HEAD would fetch it lazily.
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
        git(&cwd, &["config", key, &value]);
    }
    let head = git(&cwd, &["symbolic-ref", "HEAD"]);
    std::fs::write(
        cwd.join(".git").join(head.trim()),
        "1111111111111111111111111111111111111111\n",
    )
    .unwrap();
    std::fs::write(cwd.join("tracked.txt"), "changed\n").unwrap();

    let _ = cp
        .try_checkpoint("edit_file", &CancellationToken::new())
        .await;
    assert!(
        !marker.exists(),
        "the checkpoint's unconfined git ran a program named in .git/config"
    );
}

#[tokio::test]
async fn repository_config_cannot_point_the_snapshot_outside_the_work_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    let secret = tmp.path().join("secret");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("id_ed25519"), "SECRETKEY").unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();

    git(&cwd, &["config", "core.worktree", secret.to_str().unwrap()]);
    let _ = cp
        .try_checkpoint("edit_file", &CancellationToken::new())
        .await;

    assert!(
        !all_objects(&cwd).contains("SECRETKEY"),
        "a directory outside the work tree was copied into .git objects"
    );
    if let Some(latest) = cp.latest_ref(&CancellationToken::new()).await {
        let tree = git(&cwd, &["ls-tree", "-r", "--name-only", &latest]);
        assert!(!tree.contains("id_ed25519"), "{tree}");
    }
}

#[tokio::test]
async fn repository_config_cannot_point_a_restore_outside_the_work_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("tracked.txt"), "keep me\n").unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
    cp.try_checkpoint("edit_file", &CancellationToken::new())
        .await
        .unwrap();
    let good = cp.latest_ref(&CancellationToken::new()).await.unwrap();

    git(
        &cwd,
        &["config", "core.worktree", elsewhere.to_str().unwrap()],
    );
    let _ = cp.restore_to(&good, &CancellationToken::new()).await;

    assert_eq!(
        std::fs::read_to_string(elsewhere.join("tracked.txt")).unwrap(),
        "keep me\n",
        "a restore wrote outside the work tree"
    );
}

#[tokio::test]
async fn a_planted_checkpoint_ref_is_not_taken_as_the_latest() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    std::fs::create_dir(&cwd).unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
    std::fs::write(cwd.join("tracked.txt"), "v2\n").unwrap();
    cp.try_checkpoint("edit_file", &CancellationToken::new())
        .await
        .unwrap();

    // A ref that sorts after every real one.
    git(
        &cwd,
        &[
            "update-ref",
            "refs/aivyx/checkpoints/9999999999999-0000000-0000",
            "HEAD",
        ],
    );
    let latest = cp.latest_ref(&CancellationToken::new()).await.unwrap();
    assert!(
        !latest.starts_with("refs/aivyx/checkpoints/9999999999999"),
        "planted ref chosen: {latest}"
    );
}

#[tokio::test]
async fn a_restore_refuses_a_ref_this_checkpointer_did_not_create() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    std::fs::create_dir(&cwd).unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
    std::fs::write(cwd.join("tracked.txt"), "planted\n").unwrap();
    git(&cwd, &["add", "tracked.txt"]);
    git(
        &cwd,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x.invalid",
            "commit",
            "-qm",
            "planted",
        ],
    );
    let planted = "refs/aivyx/checkpoints/9999999999999-0000000-0000";
    git(&cwd, &["update-ref", planted, "HEAD"]);
    std::fs::write(cwd.join("tracked.txt"), "mine\n").unwrap();

    let result = cp.restore_to(planted, &CancellationToken::new()).await;
    assert!(result.is_err(), "a planted ref was restored");
    assert_eq!(
        std::fs::read_to_string(cwd.join("tracked.txt")).unwrap(),
        "mine\n"
    );
}

#[tokio::test]
async fn a_restore_uses_the_commit_recorded_not_where_the_ref_points_now() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("proj");
    std::fs::create_dir(&cwd).unwrap();
    aivyx_checkpoint::test_support::init_repo(&cwd).await;
    let cp = GitCheckpointer::detect(&cwd, vec![]).await.unwrap();
    cp.try_checkpoint("edit_file", &CancellationToken::new())
        .await
        .unwrap();
    let good = cp.latest_ref(&CancellationToken::new()).await.unwrap();

    // Move the checkpoint ref to other content.
    std::fs::write(cwd.join("tracked.txt"), "planted\n").unwrap();
    git(&cwd, &["add", "tracked.txt"]);
    git(
        &cwd,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x.invalid",
            "commit",
            "-qm",
            "planted",
        ],
    );
    git(&cwd, &["update-ref", &good, "HEAD"]);
    std::fs::write(cwd.join("tracked.txt"), "later\n").unwrap();

    cp.restore_to(&good, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("tracked.txt")).unwrap(),
        "v1\n",
        "the restore followed a moved ref"
    );
}
