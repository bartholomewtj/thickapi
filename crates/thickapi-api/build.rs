//! Stamps `THICKAPI_GIT_SHA` for `thickapi --version` and `GET /health` (SPEC header, §1.7
//! Health, §11): `git rev-parse --short=7 HEAD`, or `unknown` when there is no `.git` above
//! the crate or no `git` on PATH, so a build from a tarball or a copied tree still succeeds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let sha = match git_dir(&manifest) {
        Some(dir) => {
            watch_head(&dir);
            short_sha(&manifest).unwrap_or_else(|| "unknown".into())
        }
        None => "unknown".into(),
    };
    println!("cargo:rustc-env=THICKAPI_GIT_SHA={sha}");
}

/// The git dir for the checkout holding `start`: `.git` itself, or, in a worktree where `.git`
/// is a file, the `gitdir:` that file names.
fn git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let named = text.trim().strip_prefix("gitdir:")?.trim();
            return Some(relative_to(dir, named));
        }
    }
    None
}

/// Rerun when HEAD moves: `HEAD` (a checkout or a detached commit), the branch ref it names (a
/// commit on the branch) and `packed-refs`. Branch refs live in the common dir, which a
/// worktree's git dir names in `commondir`.
fn watch_head(git_dir: &Path) {
    let head = git_dir.join("HEAD");
    watch(&head);
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .map(|c| relative_to(git_dir, c.trim()))
        .unwrap_or_else(|_| git_dir.to_path_buf());
    if let Ok(text) = std::fs::read_to_string(&head) {
        if let Some(reference) = text.trim().strip_prefix("ref:") {
            watch(&common.join(reference.trim()));
        }
    }
    watch(&common.join("packed-refs"));
}

/// Only existing paths: cargo treats a missing `rerun-if-changed` path as always changed.
fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn relative_to(base: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn short_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}
