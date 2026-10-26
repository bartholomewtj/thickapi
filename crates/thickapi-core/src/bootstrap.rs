use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Embedded at compile time (D3 deliverable 6): a `cargo install`ed exe bootstraps
/// `~/.thickapi` without `CARGO_MANIFEST_DIR` existing on the machine it runs on — that env
/// var is a build-time constant baked into the binary, but it points at the *build* checkout,
/// which need not exist (or be this one) at install time.
const EXAMPLE_FILES: &[(&str, &str)] = &[
    (
        "router.yaml",
        include_str!("../../../configs/router.yaml.example"),
    ),
    (
        "accounts.yaml",
        include_str!("../../../configs/accounts.yaml.example"),
    ),
    (
        "combos.yaml",
        include_str!("../../../configs/combos.yaml.example"),
    ),
    (
        "models.yaml",
        include_str!("../../../configs/models.yaml.example"),
    ),
];

/// Still used by tools that want the on-disk examples directly (this source checkout only).
pub fn examples_source_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs")
}

pub fn bootstrap_config(config_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(config_dir)
        .with_context(|| format!("create config dir {}", config_dir.display()))?;

    let mut copied = Vec::new();

    for (target_name, contents) in EXAMPLE_FILES {
        let target = config_dir.join(target_name);
        if target.exists() {
            continue;
        }
        fs::write(&target, contents).with_context(|| format!("write {}", target.display()))?;
        copied.push(target);
    }

    Ok(copied)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_copies_missing_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let copied = bootstrap_config(temp.path()).expect("bootstrap");
        assert_eq!(copied.len(), 4);
        for name in ["router.yaml", "accounts.yaml", "combos.yaml", "models.yaml"] {
            assert!(temp.path().join(name).exists(), "missing {}", name);
        }
        let again = bootstrap_config(temp.path()).expect("bootstrap again");
        assert!(again.is_empty());
    }

    /// D3 deliverable 6: the four examples are `include_str!`-embedded, not copied from
    /// `CARGO_MANIFEST_DIR` at runtime — a `cargo install`ed exe must bootstrap `~/.thickapi`
    /// even when that build-time path does not exist on this machine.
    #[test]
    fn bootstrap_works_with_cargo_manifest_dir_unset_or_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("CARGO_MANIFEST_DIR").ok();
        std::env::remove_var("CARGO_MANIFEST_DIR");
        let result = bootstrap_config(temp.path());
        std::env::set_var(
            "CARGO_MANIFEST_DIR",
            prev.unwrap_or_else(|| r"C:\nonexistent\build\checkout".into()),
        );
        let copied = result.expect("bootstrap without CARGO_MANIFEST_DIR at runtime");
        assert_eq!(copied.len(), 4);
        for name in ["router.yaml", "accounts.yaml", "combos.yaml", "models.yaml"] {
            let contents = fs::read_to_string(temp.path().join(name)).expect("read");
            assert!(!contents.is_empty(), "{name} must be embedded, not empty");
        }
    }
}
