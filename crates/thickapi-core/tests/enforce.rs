#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::fs;
use std::path::Path;

use thickapi_core::enforce::{
    changed_paths, denied_match, deny_globs, git_snapshot, init_test_repo, matches, permitted,
    Enforcer, Snapshot, HEAD_KEY,
};
use thickapi_core::{EnforceMode, EnforcePolicy};

fn policy(writes: Option<Vec<String>>, out_of_scope: Option<Vec<String>>) -> EnforcePolicy {
    EnforcePolicy {
        writes,
        protected_files: vec!["adws/".into(), "secrets.env".into()],
        out_of_scope,
        always_writable: vec![],
        mode: EnforceMode::GitFingerprint,
    }
}

#[test]
fn out_of_scope_suffix_match() {
    assert!(denied_match("detail.tsx", "web/src/routes/detail.tsx"));
    assert!(denied_match("bridge/", "bridge/server.ts"));
    assert!(!denied_match("other.tsx", "web/src/routes/detail.tsx"));
}

#[test]
fn writes_not_suffix_matching() {
    assert!(!matches("src/config.ts", "config.ts"));
    assert!(matches("config.ts", "config.ts"));
    assert!(matches("src/foo.ts", "src/"));
}

#[test]
fn none_fail_closed() {
    let p = policy(None, None);
    let data = std::env::temp_dir();
    assert!(!permitted("README.md", &p, &data));
    assert!(!permitted("src/main.rs", &p, &data));
}

#[test]
fn empty_writes_noop() {
    let p = policy(Some(vec![]), Some(vec![]));
    let data = std::env::temp_dir();
    assert!(!permitted("README.md", &p, &data));
    let p2 = policy(Some(vec![]), Some(vec!["README.md".into()]));
    assert!(!permitted("README.md", &p2, &data));
}

#[test]
fn unlock_by_naming() {
    let p = EnforcePolicy {
        writes: Some(vec!["adws/adw_foo.py".into()]),
        protected_files: vec!["adws/".into()],
        out_of_scope: Some(vec![]),
        always_writable: vec![],
        mode: EnforceMode::GitFingerprint,
    };
    let globs = deny_globs(&p, Path::new("/tmp"));
    assert!(!globs.contains(&"adws/".to_string()));
}

#[test]
fn deny_globs_never_forbids_what_permitted_allows() {
    let p = policy(Some(vec!["src/".into()]), Some(vec![]));
    let globs = deny_globs(&p, Path::new("/tmp"));
    for pattern in &globs {
        assert!(!matches("src/main.rs", pattern));
    }
}

#[test]
fn deny_globs_does_not_deny_the_write_set() {
    let mut p = policy(Some(vec!["**/*.rs".into()]), Some(vec![]));
    p.protected_files = vec!["src/*.rs".into(), "secrets.env".into()];
    let data = Path::new("/tmp");
    let globs = deny_globs(&p, data);
    assert!(
        !globs.iter().any(|g| g == "src/*.rs"),
        "write glob already covers src/*.rs: {globs:?}"
    );
    assert!(permitted("src/foo.rs", &p, data));
    assert!(globs.iter().any(|g| g == "secrets.env"));
}

#[test]
fn uncommit_committed_breach() {
    let temp = tempfile::tempdir().expect("tempdir");
    init_test_repo(temp.path()).expect("init");
    fs::write(temp.path().join("allowed.txt"), "ok").expect("write");
    thickapi_core::enforce::git_add_commit(temp.path(), "init").expect("commit");

    let before = git_snapshot(temp.path()).expect("snapshot");
    fs::write(temp.path().join("allowed.txt"), "changed").expect("write");
    fs::write(temp.path().join("blocked.txt"), "nope").expect("write");
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(temp.path())
        .status()
        .expect("git add");
    std::process::Command::new("git")
        .args(["commit", "-m", "agent commit"])
        .current_dir(temp.path())
        .status()
        .expect("git commit");

    let pol = policy(Some(vec!["allowed.txt".into()]), Some(vec![]));
    let enforcer = Enforcer;
    let outcome = enforcer
        .enforce(temp.path(), &pol, temp.path(), &Snapshot::Git(before))
        .expect("enforce");
    assert!(outcome.has_breach());
    assert!(outcome.breaches.iter().any(|b| b.path == "blocked.txt"));
    assert!(!temp.path().join("blocked.txt").exists());
}

#[test]
fn rewritten_history_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    init_test_repo(temp.path()).expect("init");
    fs::write(temp.path().join("a.txt"), "a").expect("write");
    thickapi_core::enforce::git_add_commit(temp.path(), "init").expect("commit");
    let before = git_snapshot(temp.path()).expect("snapshot");

    fs::write(temp.path().join("orphan.txt"), "x").expect("write");
    std::process::Command::new("git")
        .args(["checkout", "--orphan", "newbranch"])
        .current_dir(temp.path())
        .status()
        .expect("orphan");
    std::process::Command::new("git")
        .args(["add", "orphan.txt"])
        .current_dir(temp.path())
        .status()
        .expect("add");
    std::process::Command::new("git")
        .args(["commit", "-m", "orphan"])
        .current_dir(temp.path())
        .status()
        .expect("commit");

    let pol = policy(None, Some(vec![]));
    let enforcer = Enforcer;
    let outcome = enforcer
        .enforce(temp.path(), &pol, temp.path(), &Snapshot::Git(before))
        .expect("enforce");
    assert!(outcome.breaches.iter().any(|b| b.path == HEAD_KEY));
}

#[test]
fn head_not_changed_path() {
    let before = std::collections::BTreeMap::from([
        (HEAD_KEY.to_string(), "abc123".to_string()),
        ("file.txt".to_string(), "1,0".to_string()),
    ]);
    let after = std::collections::BTreeMap::from([
        (HEAD_KEY.to_string(), "def456".to_string()),
        ("file.txt".to_string(), "1,0".to_string()),
    ]);
    let changed = changed_paths(&before, &after);
    assert!(!changed.contains(&HEAD_KEY.to_string()));
    assert!(changed.is_empty());
}

#[test]
fn globs_match_the_whole_path() {
    assert!(matches("src/main.rs", "src/*.rs"));
    assert!(!matches("vendor/src/evil.rs", "src/*.rs"));
    assert!(!matches("src/main.rs.bak", "src/*.rs"));
    assert!(!matches("src/deep/main.rs", "src/*.rs"));
    assert!(matches("app.log", "*.log"));
    assert!(!matches("secrets.log.txt", "*.log"));
    assert!(matches("src/deep/er/main.rs", "src/**/*.rs"));
    assert!(matches("src/main.rs", "src/**/*.rs"));
    assert!(matches("a[1].txt", "a[1].*"));
}

#[cfg(windows)]
#[test]
fn matching_ignores_case_on_windows() {
    assert!(matches("Config.YAML", "config.yaml"));
    assert!(matches("SRC/Main.rs", "src/*.rs"));
    assert!(denied_match("Detail.tsx", "web/src/routes/detail.tsx"));
    let p = policy(None, Some(vec![]));
    assert!(!permitted("Secrets.env", &p, &std::env::temp_dir()));
}

/// A file that predates the turn is restored from its backup, never deleted. New out-of-scope
/// files are still removed.
#[test]
fn tree_rollback_never_deletes_a_snapshotted_file() {
    use thickapi_core::enforce::{tree_enforce, tree_snapshot};

    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    let backups = temp.path().join("backups");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(cwd.join("b-user.txt"), "the user's work").expect("user file");

    let before = tree_snapshot(&cwd, &backups).expect("snapshot");
    assert!(!before.cap_exceeded);
    assert!(before.files["b-user.txt"].backup.is_some());

    fs::write(cwd.join("b-user.txt"), "agent edit").expect("agent edit");
    fs::write(cwd.join("c-new.txt"), "agent file").expect("agent file");

    let p = policy(Some(vec![]), Some(vec![]));
    let outcome = tree_enforce(&cwd, &p, Path::new(""), &before).expect("enforce");

    assert_eq!(outcome.breaches.len(), 2);
    assert_eq!(
        fs::read_to_string(cwd.join("b-user.txt")).expect("read user"),
        "the user's work",
        "pre-existing file must be restored, not deleted"
    );
    assert!(
        !cwd.join("c-new.txt").exists(),
        "new out-of-scope file stays deleted"
    );
    assert!(outcome
        .rolled_back
        .iter()
        .any(|line| line.starts_with("b-user.txt — rolled back")));
}

#[test]
fn tree_snapshot_refuses_past_the_cap() {
    use thickapi_core::enforce::{tree_snapshot, TREE_SNAPSHOT_CAP};

    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    let backups = temp.path().join("backups");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(cwd.join("a-big.bin"), vec![0u8; TREE_SNAPSHOT_CAP + 1]).expect("big");

    let err = tree_snapshot(&cwd, &backups).expect_err("cap");
    let msg = err.to_string();
    assert!(
        msg.contains("byte cap") || msg.contains("invalid_spec"),
        "{msg}"
    );
}

#[test]
fn tree_walk_skips_symlinks_git_and_target() {
    use thickapi_core::enforce::{tree_snapshot, TREE_WALK_MAX_DEPTH};

    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    let backups = temp.path().join("backups");
    fs::create_dir_all(cwd.join(".git")).expect("git");
    fs::create_dir_all(cwd.join("target").join("debug")).expect("target");
    fs::write(cwd.join(".git").join("HEAD"), "secret-git").expect("HEAD");
    fs::write(
        cwd.join("target").join("debug").join("app"),
        "secret-target",
    )
    .expect("app");
    fs::write(cwd.join("ok.txt"), "ok").expect("ok");

    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("secret.txt"), "leaked").expect("secret");
    let link = cwd.join("link");
    let linked = make_dir_link(&link, &outside);

    let before = tree_snapshot(&cwd, &backups).expect("snapshot");
    assert!(before.files.contains_key("ok.txt"));
    assert!(
        !before
            .files
            .keys()
            .any(|p| p.contains(".git") || p.contains("target")),
        "ignored dirs leaked: {:?}",
        before.files.keys().collect::<Vec<_>>()
    );
    if linked {
        assert!(
            !before
                .files
                .keys()
                .any(|p| p.contains("secret") || p.starts_with("link/")),
            "symlink/junction was followed: {:?}",
            before.files.keys().collect::<Vec<_>>()
        );
    }

    let mut deep = cwd.join("deep");
    for i in 0..=TREE_WALK_MAX_DEPTH {
        deep.push(format!("d{i}"));
    }
    fs::create_dir_all(&deep).expect("deep");
    fs::write(deep.join("f.txt"), "x").expect("deep file");
    let err = tree_snapshot(&cwd, &backups).expect_err("depth cap");
    let msg = err.to_string();
    assert!(
        msg.contains("depth cap") || msg.contains("invalid_spec"),
        "{msg}"
    );
}

fn make_dir_link(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

#[test]
fn git_snapshot_propagates_errors() {
    let temp = tempfile::tempdir().expect("tempdir");
    let err = git_snapshot(temp.path()).expect_err("not a repo");
    let msg = err.to_string();
    assert!(msg.contains("git") || msg.contains("invalid_spec"), "{msg}");
}

#[test]
fn git_fingerprint_is_content_not_numstat() {
    let temp = tempfile::tempdir().expect("tempdir");
    init_test_repo(temp.path()).expect("init");
    fs::write(temp.path().join("same-lines.txt"), "aaa\n").expect("write");
    thickapi_core::enforce::git_add_commit(temp.path(), "init").expect("commit");

    fs::write(temp.path().join("same-lines.txt"), "bbb\n").expect("first dirty");
    let before = git_snapshot(temp.path()).expect("before");
    fs::write(temp.path().join("same-lines.txt"), "ccc\n").expect("second dirty");
    let after = git_snapshot(temp.path()).expect("after");
    assert_ne!(
        before.get("same-lines.txt"),
        after.get("same-lines.txt"),
        "same add/del counts must not hide a content change: {before:?} vs {after:?}"
    );

    fs::write(temp.path().join("untracked.txt"), "a").expect("untracked a");
    let before = git_snapshot(temp.path()).expect("untracked before");
    fs::write(temp.path().join("untracked.txt"), "b").expect("untracked b");
    let after = git_snapshot(temp.path()).expect("untracked after");
    assert_ne!(
        before.get("untracked.txt"),
        after.get("untracked.txt"),
        "untracked content must be fingerprinted: {before:?} vs {after:?}"
    );
}

#[test]
fn git_snapshot_fingerprints_quoted_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    init_test_repo(temp.path()).expect("init");
    fs::write(temp.path().join("ok.txt"), "ok\n").expect("ok");
    thickapi_core::enforce::git_add_commit(temp.path(), "init").expect("commit");

    let spaced = temp.path().join("a b.txt");
    fs::write(&spaced, "one").expect("space name");
    let cafe = temp.path().join("café.txt");
    fs::write(&cafe, "accent").expect("non-ascii name");

    let before = git_snapshot(temp.path()).expect("before");
    assert!(
        before.keys().any(|k| k == "a b.txt"),
        "space in name must not be git-quoted: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        before
            .keys()
            .any(|k| k == "café.txt" || k.ends_with("caf\u{e9}.txt")),
        "non-ascii name must not fingerprint as deleted: {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        !before
            .values()
            .any(|v| v == "u:deleted" || v == "h:deleted"),
        "quoted-path miss must not look like a deletion: {before:?}"
    );

    fs::write(&spaced, "two").expect("edit space");
    let after = git_snapshot(temp.path()).expect("after");
    assert_ne!(
        before.get("a b.txt"),
        after.get("a b.txt"),
        "edit of a quoted name must change the fingerprint"
    );
}

#[test]
fn tree_enforce_rolls_back_files_past_the_snapshot_depth_cap() {
    use thickapi_core::enforce::{tree_enforce, tree_snapshot, TREE_WALK_MAX_DEPTH};

    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().join("work");
    let backups = temp.path().join("backups");
    fs::create_dir_all(&cwd).expect("cwd");
    fs::write(cwd.join("ok.txt"), "ok").expect("ok");
    let before = tree_snapshot(&cwd, &backups).expect("snapshot");

    let mut deep = cwd.join("deep");
    for i in 0..=TREE_WALK_MAX_DEPTH {
        deep.push(format!("d{i}"));
    }
    fs::create_dir_all(&deep).expect("deep");
    fs::write(deep.join("sneak.txt"), "agent").expect("deep file");

    let p = policy(Some(vec![]), Some(vec![]));
    let outcome = tree_enforce(&cwd, &p, Path::new(""), &before).expect("enforce");
    assert!(
        outcome
            .breaches
            .iter()
            .any(|b| b.path.ends_with("sneak.txt")),
        "deep new file must be a breach: {:?}",
        outcome.breaches
    );
    assert!(
        !deep.join("sneak.txt").exists(),
        "deep new file must be deleted"
    );
}

/// Anchoring is for the allow-list; a deny pattern must not be dodged by nesting deeper.
#[test]
fn protected_globs_cover_nested_paths() {
    use thickapi_core::enforce::protected_match;
    assert!(protected_match("config/prod.env", "*.env"));
    assert!(protected_match("prod.env", "*.env"));
    assert!(protected_match("src/a/b.rs", "src/*"));
    assert!(!protected_match("vendor/src/a.rs", "src/*"));
    assert!(!protected_match("config/prod.envx", "*.env"));
    assert!(!protected_match("src/config.ts", "config.ts"));
    assert!(
        protected_match("x", &"?".repeat(2_000_000)),
        "uncompilable fails closed"
    );

    let mut p = policy(None, Some(vec![]));
    p.protected_files = vec!["*.env".into()];
    assert!(!permitted("config/prod.env", &p, &std::env::temp_dir()));
    assert!(permitted("config/prod.txt", &p, &std::env::temp_dir()));
}
