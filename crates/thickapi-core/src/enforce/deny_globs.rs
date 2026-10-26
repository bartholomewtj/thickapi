use std::path::Path;

use crate::adapters::path_deny_rule;
use crate::types::EnforcePolicy;

use super::patterns::{matches, normalize_path, permitted};

pub fn path_deny_rule_export(pattern: &str) -> String {
    path_deny_rule(pattern)
}

/// A concrete path the glob would match, used to ask `permitted()` whether the write set
/// already covers this deny. `*`/`?` become `x`; `**/` becomes `x/`.
fn probe_path(pattern: &str) -> String {
    let pattern = normalize_path(pattern);
    if pattern.ends_with('/') {
        return format!("{pattern}x");
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let Some(&ch) = chars.get(i) else {
            break;
        };
        if ch == '*' && chars.get(i + 1) == Some(&'*') {
            if chars.get(i + 2) == Some(&'/') {
                out.push_str("x/");
                i += 3;
            } else {
                out.push('x');
                i += 2;
            }
        } else if ch == '*' || ch == '?' {
            out.push('x');
            i += 1;
        } else {
            out.push(ch);
            i += 1;
        }
    }
    out
}

/// CLI `--deny` only for a protected pattern `permitted()` actually rejects. A write glob
/// that already covers the path (`writes: ["**/*.rs"]` + `protected_files: ["src/*.rs"]`)
/// is not exported. A named unlock under a protected prefix still drops that prefix.
pub fn deny_globs(policy: &EnforcePolicy, data_dir: &Path) -> Vec<String> {
    let grants = policy.writes.clone().unwrap_or_default();
    policy
        .protected_files
        .iter()
        .filter(|pattern| {
            let unlocked = grants
                .iter()
                .any(|grant| grant == *pattern || matches(grant, pattern));
            if unlocked {
                return false;
            }
            !permitted(&probe_path(pattern), policy, data_dir)
        })
        .cloned()
        .collect()
}
