#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! The capture tooling against the one leak table (`fixture_set::TOKEN_PATTERNS`, SPEC §19.11):
//! every entry has a positive and a negative sample, the redactor removes each positive, and
//! `identity_violations` reports it; the newest-set rule skips `.tmp` trees; committed sets
//! carry no identity; and `capture --check`'s set check passes on a clean set and writes
//! nothing under `fixtures/`.

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use thickapi_cli::capture::{check_set, identity_violations, redact_text, rewrite_set};
use thickapi_cli::capture::{repo_roots, RedactContext};
use thickapi_core::thin::fixture_set::{
    committed_sets, compiled_fixtures_root, leaks, newest_set, Scope, META_FILE, RESPONSE_FILE,
    TOKEN_PATTERNS,
};

const VENDORS: &[&str] = &["claude", "claude-bridge", "grok", "agy", "cursor"];

/// `(positive, negative)` per `TOKEN_PATTERNS` name. Token-shaped positives are assembled at
/// runtime so no secret-looking literal sits in the source.
fn samples() -> BTreeMap<&'static str, (String, String)> {
    let pair = |pos: String, neg: &str| (pos, neg.to_string());
    BTreeMap::from([
        (
            "ta_ token",
            pair(format!("key ta_{}", "a1b2c3d4e5f6"), "beta_features_on"),
        ),
        (
            "sk-ant token",
            pair(format!("sk-ant-{}", "oat01-abcdefgh"), "sk-ant-short"),
        ),
        (
            "sk- token",
            pair(format!("sk-{}", "A1".repeat(12)), "sk-1234 is short"),
        ),
        (
            "ghp_ token",
            pair(format!("ghp_{}", "x9".repeat(18)), "ghp_abc"),
        ),
        (
            "Bearer token",
            pair(
                format!("Authorization: Bearer {}", "abcDEF123._-".repeat(3)),
                "Authorization: Bearer <redacted>",
            ),
        ),
        (
            "JWT",
            pair(
                format!(
                    "{}.{}.{}",
                    "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiIxIn0x", "sig"
                ),
                "eyJhbGciOi alone",
            ),
        ),
        (
            "ya29. token",
            pair(format!("ya29.{}", "a0AfB_byC1234567"), "ya29.x"),
        ),
        (
            "1// refresh token",
            pair(format!("1//{}", "0gABCDEFGHIJ12"), "see 1//abc"),
        ),
        (
            r"C:\Users",
            pair(
                r"read C:\Users\someone\notes.txt".into(),
                r"C:\Program Files\x",
            ),
        ),
        (
            "C:%5CUsers",
            pair("C:%5CUsers%5Csomeone".into(), "C:%5CWindows"),
        ),
        (
            "email",
            pair("mail someone@example.com".into(), "npm @types/node"),
        ),
        (
            "anthropic-organization-id",
            pair(
                "Anthropic-Organization-Id".into(),
                "anthropic-ratelimit-requests-limit",
            ),
        ),
        (
            "response header UUID",
            pair("4fc37a03-1234-4234-9234-123456789abc".into(), "<UUID-1>"),
        ),
    ])
}

fn empty_ctx() -> RedactContext {
    RedactContext {
        secrets: vec![],
        roots: vec![],
        uuids: Default::default(),
    }
}

#[test]
fn every_token_pattern_has_a_positive_and_a_negative_sample() {
    let samples = samples();
    let names: Vec<&str> = TOKEN_PATTERNS.iter().map(|(n, ..)| *n).collect();
    assert_eq!(
        samples.len(),
        names.len(),
        "one sample pair per entry: {names:?}"
    );
    for &(name, scope, _) in TOKEN_PATTERNS {
        let (pos, neg) = samples
            .get(name)
            .unwrap_or_else(|| panic!("no samples for {name}"));
        assert!(
            leaks(scope, pos).contains(&name),
            "{name}: positive {pos:?} not caught"
        );
        assert!(
            !leaks(scope, neg).contains(&name),
            "{name}: negative {neg:?} caught"
        );
        if scope == Scope::Text {
            // The redactor removes it...
            let redacted = redact_text(pos, &mut empty_ctx());
            assert!(
                !leaks(Scope::Text, &redacted).contains(&name),
                "{name}: redaction left {redacted:?}"
            );
        }
        // ...and identity_violations reports it.
        let temp = tempfile::tempdir().unwrap();
        let set = temp.path().join("vendor").join("1.0");
        let scenario = set.join("text");
        std::fs::create_dir_all(&scenario).unwrap();
        std::fs::write(scenario.join("frames.jsonl"), "").unwrap();
        match scope {
            Scope::Text => std::fs::write(
                set.join(META_FILE),
                serde_json::json!({ "note": pos }).to_string(),
            )
            .unwrap(),
            Scope::HeaderName | Scope::HeaderValue => {
                let header = if scope == Scope::HeaderName {
                    [pos.as_str(), "v"]
                } else {
                    ["x-trace", pos.as_str()]
                };
                let response = serde_json::json!({ "status": 200, "headers": [header] });
                std::fs::write(scenario.join(RESPONSE_FILE), response.to_string()).unwrap();
            }
        }
        let violations = identity_violations(&set, &[]);
        assert!(
            violations.iter().any(|v| v.ends_with(&format!(": {name}"))),
            "{name}: identity_violations missed it: {violations:?}"
        );
    }
}

#[test]
fn newest_set_ignores_a_tmp_sibling_and_dirs_without_meta() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let write_meta = |dir: &str, at: &str| {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        let meta = serde_json::json!({ "captured_at": at });
        std::fs::write(root.join(dir).join(META_FILE), meta.to_string()).unwrap();
    };
    write_meta("grok/1.0.9", "2026-09-01T00:00:00Z");
    write_meta("grok/1.0.30", "2026-09-17T00:00:00Z");
    // Name order alone would pick "1.0.9"; the stamp says 1.0.30 is newer.
    let newest = || newest_set(root, "grok").unwrap();
    assert_eq!(newest().file_name().unwrap(), "1.0.30");
    // A half-written capture tree and a set without meta.json never win.
    write_meta("grok/1.0.40.tmp", "2026-09-29T00:00:00Z");
    std::fs::create_dir_all(root.join("grok/9.9.9")).unwrap();
    assert_eq!(newest().file_name().unwrap(), "1.0.30");
    assert!(newest_set(root, "nope").is_none());
}

/// Every committed set (not the newest only) is clean under the full check: the leak table
/// over every file and every decoded body, plus the repo roots. `THICKAPI_FIXTURE_REWRITE=1`
/// re-redacts the committed sets in place first.
#[test]
fn committed_sets_carry_no_identity() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = repo_roots(manifest_dir);
    let sets = committed_sets(&compiled_fixtures_root(), VENDORS);
    assert!(!sets.is_empty(), "no committed fixture sets found");
    if std::env::var("THICKAPI_FIXTURE_REWRITE").as_deref() == Ok("1") {
        for set in &sets {
            let mut ctx = RedactContext::for_checkout(manifest_dir);
            rewrite_set(set, &mut ctx).expect("rewrite_set");
        }
    }
    let violations: Vec<String> = sets
        .iter()
        .flat_map(|set| identity_violations(set, &roots))
        .collect();
    assert!(
        violations.is_empty(),
        "committed fixture sets carry identity leaks:\n{}",
        violations.join("\n")
    );
}

/// Every path under `root`: a file maps to its length and a hash of its bytes, a directory to
/// `None`. "Nothing written" means the same paths holding the same bytes. Timestamps are left
/// out: on Windows `read_dir` reports a directory's mtime from its parent's index entry, which
/// NTFS refreshes lazily, so on a fresh checkout the first walk moves it with nothing written.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<(usize, u64)>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path.clone());
                out.insert(path, None);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                let mut hasher = DefaultHasher::new();
                bytes.hash(&mut hasher);
                out.insert(path, Some((bytes.len(), hasher.finish())));
            }
        }
    }
    out
}

fn copy_tree(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dest.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

/// `capture --check` on a clean capture (each vendor's newest committed set, copied to a temp
/// dir as a fresh capture would be): no failure, a diff summary naming the committed set, and
/// nothing under `fixtures/` touched.
#[test]
fn check_on_a_clean_set_passes_and_writes_nothing_under_fixtures() {
    let fixtures = compiled_fixtures_root();
    let roots = repo_roots(Path::new(env!("CARGO_MANIFEST_DIR")));
    let before = tree(&fixtures);
    for vendor in VENDORS {
        let Some(committed) = newest_set(&fixtures, vendor) else {
            continue;
        };
        let temp = tempfile::tempdir().unwrap();
        let fresh = temp
            .path()
            .join(vendor)
            .join(committed.file_name().unwrap());
        copy_tree(&committed, &fresh);
        let report = check_set(vendor, &fresh, &fixtures, &roots);
        assert!(
            report.failures.is_empty(),
            "{vendor}: {:?}",
            report.failures
        );
        let name = committed
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            report
                .diff
                .iter()
                .any(|l| l.contains(&format!("committed {name}"))),
            "{vendor}: {:?}",
            report.diff
        );
        assert!(
            report.diff.iter().any(|l| l.contains("same event shape")),
            "{vendor}: {:?}",
            report.diff
        );
    }
    assert_eq!(before, tree(&fixtures), "check wrote under fixtures/");
}

/// A fresh set whose frames no longer replay to its recorded events fails the check.
#[test]
fn check_fails_when_the_fresh_set_does_not_replay() {
    let fixtures = compiled_fixtures_root();
    let committed = newest_set(&fixtures, "grok").expect("committed grok set");
    let temp = tempfile::tempdir().unwrap();
    let fresh = temp.path().join("grok").join("9.9.9");
    copy_tree(&committed, &fresh);
    let events = fresh.join("text").join("events.jsonl");
    let tampered = std::fs::read_to_string(&events)
        .unwrap()
        .replace("\"stop\"", "\"length\"");
    std::fs::write(&events, tampered).unwrap();
    let report = check_set("grok", &fresh, &fixtures, &[]);
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.starts_with("text: replay")),
        "{:?}",
        report.failures
    );
}
