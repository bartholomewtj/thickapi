use std::env;
use std::path::{Path, PathBuf};

/// Resolve a CLI binary on PATH, honouring PATHEXT and `.cmd` shims on Windows.
pub fn resolve_cli(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    let paths = env::split_paths(&path_var);

    #[cfg(windows)]
    let pathext: Vec<String> = env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .map(|s| s.to_ascii_lowercase())
        .collect();

    for dir in paths {
        let candidate = dir.join(name);
        #[cfg(not(windows))]
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if candidate.is_file() && has_windows_executable_ext(&candidate, &pathext) {
                return Some(candidate);
            }
            for ext in &pathext {
                let with_ext = dir.join(format!("{}{}", name, ext));
                if with_ext.is_file() {
                    return resolve_shim(&with_ext).or(Some(with_ext));
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn has_windows_executable_ext(path: &Path, pathext: &[String]) -> bool {
    match path.extension() {
        Some(ext) => pathext.contains(&format!(".{}", ext.to_string_lossy().to_ascii_lowercase())),
        None => false,
    }
}

#[cfg(windows)]
fn resolve_shim(shim: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(shim).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("\"%dp0") || trimmed.starts_with("%dp0") {
            // npm shims write `%dp0%\...` (delayed expansion form) or `%dp0\...`.
            let rest = trimmed
                .trim_start_matches('"')
                .trim_start_matches("%dp0")
                .trim_start_matches('%')
                .trim_start_matches('\\')
                .trim_start_matches('/');
            let end = rest.find('"').unwrap_or(rest.len());
            let rel = rest[..end].trim();
            if rel.ends_with(".exe") || rel.ends_with(".cmd") {
                let base = shim.parent()?;
                let target = base.join(rel);
                if target.is_file() {
                    return Some(target);
                }
            }
        }
        if trimmed.ends_with(".exe") && !trimmed.contains(' ') {
            let base = shim.parent()?;
            let target = base.join(trimmed.trim_matches('"'));
            if target.is_file() {
                return Some(target);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn resolve_shim(_shim: &Path) -> Option<PathBuf> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn resolve_cli_skips_extensionless_npm_shims() {
        if std::env::var("THICKAPI_LIVE").as_deref() != Ok("1") {
            return;
        }
        let claude = resolve_cli("claude");
        assert!(claude.is_some(), "claude should resolve on PATH");
        let path = claude.unwrap();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        assert!(
            ext == "cmd" || ext == "exe" || ext == "bat",
            "expected Windows executable shim, got {}",
            path.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_shim_handles_delayed_expansion_dp0() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target_dir = dir.path().join("node_modules").join("pkg").join("bin");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("tool.exe");
        std::fs::write(&target, b"MZ").unwrap();
        let shim = dir.path().join("tool.cmd");
        std::fs::write(
            &shim,
            "@ECHO off\r\nSETLOCAL\r\nCALL :find_dp0\r\n\"%dp0%\\node_modules\\pkg\\bin\\tool.exe\"   %*\r\n",
        )
        .unwrap();
        assert_eq!(resolve_shim(&shim), Some(target));
    }
}
