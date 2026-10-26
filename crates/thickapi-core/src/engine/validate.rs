use std::path::{Path, PathBuf};

use crate::config::RouterConfig;
use crate::error::TurnError;
use crate::types::TurnSpec;

pub fn validate_spec(spec: &TurnSpec, router: &RouterConfig) -> Result<PathBuf, TurnError> {
    if spec.prompt.trim().is_empty() {
        return Err(TurnError::InvalidSpec("prompt is required".into()));
    }
    if spec.session_id.trim().is_empty() {
        return Err(TurnError::InvalidSpec("session_id is required".into()));
    }

    let cwd = dunce::canonicalize(&spec.cwd).map_err(|e| {
        TurnError::InvalidSpec(format!("invalid cwd {}: {}", spec.cwd.display(), e))
    })?;

    for refused in &router.refuse_paths {
        let refused_canon = dunce::canonicalize(refused).unwrap_or_else(|_| refused.clone());
        if path_prefix_match(&refused_canon, &cwd) {
            return Err(TurnError::RefusedWorkspace(format!(
                "cwd {} is under refused path {}",
                cwd.display(),
                refused_canon.display()
            )));
        }
    }

    Ok(cwd)
}

fn path_prefix_match(prefix: &Path, path: &Path) -> bool {
    let prefix_s = prefix
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let path_s = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    path_s == prefix_s || path_s.starts_with(&format!("{}\\", prefix_s))
}
