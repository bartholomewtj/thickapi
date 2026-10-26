//! Fixture sets on disk, `fixtures/<vendor>/<version>/` (SPEC §19.11), and [`TOKEN_PATTERNS`]:
//! the one leak table the capture redactor, `identity_violations` and `wire_fixtures` share.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use crate::thin::static_regex;
use Scope::{HeaderName, HeaderValue, Text};

pub const REQUEST_FILE: &str = "request.json";
pub const RESPONSE_FILE: &str = "response.json";
pub const FRAMES_FILE: &str = "frames.jsonl";
pub const EVENTS_FILE: &str = "events.jsonl";
pub const META_FILE: &str = "meta.json";

/// Where a pattern applies: any text in a set, or a response header's name or value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Text,
    HeaderName,
    HeaderValue,
}

/// One leak: the name a violation reports, where it applies, and its regex.
pub type TokenPattern = (&'static str, Scope, &'static str);

#[rustfmt::skip]
pub const TOKEN_PATTERNS: &[TokenPattern] = &[
    ("ta_ token", Text, r"\bta_[A-Za-z0-9_]{8,}\b"),
    ("sk-ant token", Text, r"\bsk-ant-[A-Za-z0-9_-]{8,}"),
    ("sk- token", Text, r"\bsk-[A-Za-z0-9]{20,}"),
    ("ghp_ token", Text, r"\bghp_[A-Za-z0-9]{20,}"),
    ("Bearer token", Text, r"(?i)\bBearer\s+[A-Za-z0-9._\-+/=]{20,}"),
    ("JWT", Text, r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*"),
    ("ya29. token", Text, r"\bya29\.[A-Za-z0-9_-]{10,}"),
    ("1// refresh token", Text, r"1//[0-9A-Za-z_-]{10,}"),
    (r"C:\Users", Text, r"(?i)C:(?:\\+|/)Users"),
    ("C:%5CUsers", Text, r"(?i)C:%5CUsers"),
    ("email", Text, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}"),
    ("anthropic-organization-id", HeaderName, r"(?i)^anthropic-organization-id$"),
    ("response header UUID", HeaderValue, r"(?i)[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"),
];

/// [`TOKEN_PATTERNS`] compiled once, in table order.
pub fn compiled() -> &'static [(&'static str, Scope, Regex)] {
    static COMPILED: OnceLock<Vec<(&'static str, Scope, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let compile = |&(name, scope, re): &TokenPattern| (name, scope, static_regex(re));
        TOKEN_PATTERNS.iter().map(compile).collect()
    })
}

/// Names of the `scope` patterns in `text` (a header: `HeaderName` / `HeaderValue`).
pub fn leaks(scope: Scope, text: &str) -> Vec<&'static str> {
    let hit = |(name, s, re): &(&'static str, Scope, Regex)| {
        (*s == scope && re.is_match(text)).then_some(*name)
    };
    compiled().iter().filter_map(hit).collect()
}

/// The newest set under `fixtures/<vendor>/`, by `meta.json` `captured_at`, else by name. Only
/// a directory with a `meta.json` whose name does not end `.tmp` (capture's temp tree) counts.
pub fn newest_set(fixtures_root: &Path, vendor: &str) -> Option<PathBuf> {
    let stamped = |set: PathBuf| {
        let meta = std::fs::read_to_string(set.join(META_FILE)).ok()?;
        let name = set.file_name()?.to_string_lossy().into_owned();
        let stamp = serde_json::from_str::<Value>(&meta)
            .ok()
            .and_then(|m| m.get("captured_at")?.as_str().map(str::to_string));
        Some((stamp.unwrap_or(name), set))
    };
    let mut sets: Vec<_> = set_dirs(&fixtures_root.join(vendor))
        .into_iter()
        .filter_map(stamped)
        .collect();
    sets.sort();
    sets.pop().map(|(_, set)| set)
}

/// Every set of these vendors that has at least one scenario (`.tmp` trees excluded).
pub fn committed_sets(fixtures_root: &Path, vendors: &[&str]) -> Vec<PathBuf> {
    let sets = vendors
        .iter()
        .flat_map(|v| set_dirs(&fixtures_root.join(v)));
    sets.filter(|set| !scenarios(set).is_empty()).collect()
}

/// Scenario directories inside a set (`text`, `tool`, …), sorted.
pub fn scenarios(set_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = set_dirs(set_dir);
    dirs.retain(|d| d.join(FRAMES_FILE).exists());
    dirs
}

/// Sorted subdirectories, skipping names that end `.tmp`.
fn set_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && !p.to_string_lossy().ends_with(".tmp"))
        .collect();
    dirs.sort();
    dirs
}

/// SPEC §19.11 warning when `fixtures/<vendor>/<cli_version>/` is not a set (no `meta.json`).
pub fn missing_fixture_warn(root: &Path, vendor: &str, cli_version: &str) -> Option<String> {
    let meta = root.join(vendor).join(cli_version).join(META_FILE);
    (cli_version.is_empty() || !meta.is_file())
        .then(|| format!("{vendor} {cli_version}: no fixture set (run thickapi capture {vendor})"))
}

/// Repo `fixtures/`, baked from `thickapi-core`'s manifest (checkout / `cargo test`).
pub fn compiled_fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}
