#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::fs;
use std::path::{Path, PathBuf};

use thickapi_core::trust::{
    claude_config_json_paths, claude_trust_key, grok_trust_key, seed_claude_trust, seed_grok_trust,
};

fn trust_fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/trust")
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst");
    for entry in fs::read_dir(src).expect("read src") {
        let entry = entry.expect("read entry");
        let entry_path = entry.path();
        let target = dst.join(entry.file_name());
        if entry_path.is_dir() {
            copy_dir_all(&entry_path, &target);
        } else {
            fs::copy(&entry_path, &target).expect("copy file");
        }
    }
}

fn fixture_home(backend: &str, case: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().to_path_buf();
    let src = trust_fixtures().join(backend).join(case).join("home");
    copy_dir_all(&src, &home);
    (temp, home)
}

#[test]
fn claude_trust_key_forward_slashes_windows_path() {
    let key = claude_trust_key(r"<HOME>\.agents\worktrees\app\run-x");
    assert_eq!(key, "<HOME>/.agents/worktrees/app/run-x");
}

#[test]
fn claude_config_paths_nested_then_legacy() {
    let paths = claude_config_json_paths(PathBuf::from("/home/user").as_path(), None);
    assert_eq!(paths.len(), 2);
    assert!(paths[0].to_string_lossy().contains(".claude"));
}

#[test]
fn seed_claude_trust_existing_projects_fixture() {
    let (_temp, home) = fixture_home("claude", "existing_projects");
    let cwd = PathBuf::from(r"<HOME>\.agents\worktrees\app\run-x");
    let result = seed_claude_trust(cwd.as_path(), &home, None);
    assert!(result.ok);
    assert_eq!(result.key, "<HOME>/.agents/worktrees/app/run-x");
    assert_eq!(result.updated.len(), 1);
    let nested = home.join(".claude").join(".claude.json");
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&nested).expect("read")).expect("json");
    assert_eq!(written["numStartups"], 3);
    assert_eq!(written["projects"]["C:/work/projects/app"]["lastHint"], 1);
    assert_eq!(
        written["projects"]["<HOME>/.agents/worktrees/app/run-x"]["hasTrustDialogAccepted"],
        true
    );
}

#[test]
fn seed_claude_trust_already_trusted_fixture() {
    let (_temp, home) = fixture_home("claude", "already_trusted");
    let cwd = PathBuf::from(r"<HOME>\.agents\worktrees\app\run-x");
    let nested = home.join(".claude").join(".claude.json");
    let before = fs::read_to_string(&nested).expect("read before");
    let result = seed_claude_trust(cwd.as_path(), &home, None);
    assert!(result.ok);
    assert!(result.updated.is_empty());
    assert_eq!(fs::read_to_string(&nested).expect("read after"), before);
}

#[test]
fn seed_claude_trust_both_configs_fixture() {
    let (_temp, home) = fixture_home("claude", "both_configs");
    let result = seed_claude_trust(Path::new("/wt"), &home, None);
    assert!(result.ok);
    assert_eq!(result.updated.len(), 2);
    let nested = home.join(".claude").join(".claude.json");
    let legacy = home.join(".claude.json");
    let nested_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&nested).expect("read nested")).expect("json");
    let legacy_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&legacy).expect("read legacy")).expect("json");
    assert_eq!(
        nested_json["projects"]["/wt"]["hasTrustDialogAccepted"],
        true
    );
    assert_eq!(
        legacy_json["projects"]["/wt"]["hasTrustDialogAccepted"],
        true
    );
}

#[test]
fn seed_claude_trust_corrupt_nested_fixture() {
    let (_temp, home) = fixture_home("claude", "corrupt_nested");
    let nested = home.join(".claude").join(".claude.json");
    let before_nested = fs::read_to_string(&nested).expect("read nested");
    let result = seed_claude_trust(Path::new("/wt"), &home, None);
    assert!(result.ok);
    assert_eq!(result.updated.len(), 1);
    assert_eq!(
        fs::read_to_string(&nested).expect("unchanged nested"),
        before_nested
    );
    let legacy = home.join(".claude.json");
    let legacy_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&legacy).expect("read legacy")).expect("json");
    assert_eq!(
        legacy_json["projects"]["/wt"]["hasTrustDialogAccepted"],
        true
    );
}

#[test]
fn seed_claude_trust_empty_home_fixture() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().to_path_buf();
    let result = seed_claude_trust(Path::new("/wt"), &home, None);
    assert!(result.ok);
    let nested = home.join(".claude").join(".claude.json");
    assert_eq!(result.updated, vec![nested.clone()]);
    let written: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&nested).expect("read")).expect("json");
    assert_eq!(
        written,
        serde_json::json!({
            "projects": {
                "/wt": { "hasTrustDialogAccepted": true }
            }
        })
    );
}

#[test]
fn grok_trust_key_empty_file_win32_uses_backslash() {
    let key = grok_trust_key(r"C:\wt\run", &[], "win32");
    assert_eq!(key, r"C:\wt\run");
}

#[test]
fn seed_grok_trust_creates_section_fixture() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().to_path_buf();
    let cwd = PathBuf::from(r"C:\wt\run");
    let result = seed_grok_trust(cwd.as_path(), &home);
    assert!(result.ok);
    assert_eq!(result.key, r"C:\wt\run");
    let file = home.join(".grok").join("trusted_folders.toml");
    assert_eq!(result.updated, vec![file.clone()]);
    let written = fs::read_to_string(&file).expect("read");
    assert!(written.contains("[folders.'C:\\wt\\run']"));
    assert!(written.contains("trusted = true"));
    assert!(written.contains("decided_at = "));
}

#[test]
fn seed_grok_trust_appends_fixture() {
    let (_temp, home) = fixture_home("grok", "existing_folder");
    let cwd = PathBuf::from(r"C:\wt\run");
    let result = seed_grok_trust(cwd.as_path(), &home);
    assert!(result.ok);
    let file = home.join(".grok").join("trusted_folders.toml");
    let written = fs::read_to_string(&file).expect("read");
    assert!(written.contains("[folders.'C:\\work']"));
    assert!(written.contains("[folders.'C:\\wt\\run']"));
}

#[test]
fn seed_grok_trust_already_trusted_fixture() {
    let (_temp, home) = fixture_home("grok", "already_trusted");
    let cwd = PathBuf::from(r"C:\wt\run");
    let file = home.join(".grok").join("trusted_folders.toml");
    let before = fs::read_to_string(&file).expect("read before");
    let result = seed_grok_trust(cwd.as_path(), &home);
    assert!(result.ok);
    assert!(result.updated.is_empty());
    assert_eq!(fs::read_to_string(&file).expect("read after"), before);
}

#[test]
fn seed_grok_trust_forward_slash_keys_fixture() {
    let (_temp, home) = fixture_home("grok", "forward_slash_keys");
    let cwd = PathBuf::from(r"C:\wt\run");
    let result = seed_grok_trust(cwd.as_path(), &home);
    assert!(result.ok);
    assert_eq!(result.key, "C:/wt/run");
    let written =
        fs::read_to_string(home.join(".grok").join("trusted_folders.toml")).expect("read");
    assert!(written.contains("[folders.'C:/wt/run']"));
}

#[test]
fn seed_grok_trust_rejects_non_toml_fixture() {
    let (_temp, home) = fixture_home("grok", "not_toml");
    let file = home.join(".grok").join("trusted_folders.toml");
    let before = fs::read_to_string(&file).expect("read before");
    let result = seed_grok_trust(Path::new("/wt"), &home);
    assert!(!result.ok);
    assert_eq!(fs::read_to_string(&file).expect("unchanged"), before);
}
