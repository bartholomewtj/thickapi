use std::path::Path;

use crate::types::EnforcePolicy;

/// Git HEAD sentinel — cannot collide with a real path.
pub const HEAD_KEY: &str = "\x00HEAD";

pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

/// Normalised for comparison: Windows filesystems are case-insensitive, so `Config.yaml`
/// must not slip past `protected_files: ["config.yaml"]`.
fn comparable(path: &str) -> String {
    let path = normalize_path(path);
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    }
}

/// Whole-path glob: `*` and `?` stay inside one segment, `**` crosses segments and `**/`
/// also matches no segment at all. Anchored at both ends — `src/*.rs` must not permit
/// `vendor/src/x.rs`. `None` only when the pattern is too large to compile.
pub fn glob_pattern(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::from("^");
    let chars: Vec<char> = normalize_path(pattern).chars().collect();
    let mut i = 0;
    while let Some(&ch) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if ch == '*' && next == Some('*') {
            if chars.get(i + 2) == Some(&'/') {
                out.push_str("(?:.*/)?");
                i += 3;
            } else {
                out.push_str(".*");
                i += 2;
            }
        } else if ch == '*' {
            out.push_str("[^/]*");
            i += 1;
        } else if ch == '?' {
            out.push_str("[^/]");
            i += 1;
        } else {
            out.push_str(&regex::escape(&ch.to_string()));
            i += 1;
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

pub fn matches(path: &str, pattern: &str) -> bool {
    let path = comparable(path);
    let pattern = comparable(pattern);
    if pattern.is_empty() {
        return false;
    }
    if pattern.ends_with('/') {
        return path.starts_with(&pattern) || path == pattern.trim_end_matches('/');
    }
    if pattern.contains('*') || pattern.contains('?') {
        return glob_pattern(&pattern).is_some_and(|re| re.is_match(&path));
    }
    path == pattern
}

/// Deny-side matching for `protected_files`. An allow-list must be exact; a deny-list must
/// not be dodged by depth: `src/*` still protects `src/a/b`, a bare `*.env` protects
/// `config/prod.env`, and a glob too large to compile protects everything.
pub fn protected_match(path: &str, pattern: &str) -> bool {
    if matches(path, pattern) {
        return true;
    }
    let pattern = comparable(pattern);
    if !(pattern.contains('*') || pattern.contains('?')) || pattern.ends_with('/') {
        return false;
    }
    let Some(re) = glob_pattern(&pattern) else {
        return true;
    };
    let path = comparable(path);
    let ancestors = path.match_indices('/').filter_map(|(i, _)| path.get(..i));
    let components = path.split('/').filter(|_| !pattern.contains('/'));
    ancestors.chain(components).any(|part| re.is_match(part))
}

/// Out-of-scope denylist matching (suffix + prefix + directory).
pub fn denied_match(name: &str, path: &str) -> bool {
    let name = comparable(name.trim());
    let path = comparable(path);
    if name.is_empty() || path.is_empty() {
        return false;
    }
    if name.ends_with('/') {
        return path == name.trim_end_matches('/') || path.starts_with(&name);
    }
    if path == name {
        return true;
    }
    let prefix = format!("{}/", name.trim_end_matches('/'));
    if path.starts_with(&prefix) {
        return true;
    }
    if path.ends_with(&format!("/{}", name)) {
        return true;
    }
    false
}

pub fn permitted(path: &str, policy: &EnforcePolicy, data_dir: &Path) -> bool {
    let path = normalize_path(path);
    let always = always_writable(policy, data_dir);
    if always.iter().any(|p| matches(&path, p)) {
        return true;
    }
    let Some(denied) = policy.out_of_scope.as_ref() else {
        return false;
    };
    if denied.iter().any(|name| denied_match(name, &path)) {
        return false;
    }
    if policy
        .writes
        .as_ref()
        .is_some_and(|w| w.iter().any(|p| matches(&path, p)))
    {
        return true;
    }
    if policy
        .protected_files
        .iter()
        .any(|p| protected_match(&path, p))
    {
        return false;
    }
    policy.writes.is_none()
}

pub fn always_writable(policy: &EnforcePolicy, data_dir: &Path) -> Vec<String> {
    let mut paths = policy.always_writable.clone();
    let data = normalize_path(data_dir.to_string_lossy().trim_end_matches('/'));
    if !data.is_empty() {
        paths.push(format!("{}/", data));
    }
    paths
}

pub fn changed_paths(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let keys: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    keys.into_iter()
        .filter(|p| *p != HEAD_KEY && before.get(p) != after.get(p))
        .collect()
}
