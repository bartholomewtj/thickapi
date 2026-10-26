//! Checking a fixture set (SPEC §19.11): `identity_violations` (the leak table plus the repo
//! roots, over every file and every decoded body), and `capture --check`'s replay and diff.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use regex::Regex;
use serde_json::Value;
use thickapi_core::config::router::ThinConfig;
use thickapi_core::thin::capture::{
    load_events, load_frames, load_request, summarize, FrameRecord,
};
use thickapi_core::thin::fixture_set::{
    self, newest_set, scenarios, Scope, EVENTS_FILE, FRAMES_FILE, META_FILE, REQUEST_FILE,
    RESPONSE_FILE,
};
use thickapi_core::thin::oauth::vendor::VendorRequest;
use thickapi_core::thin::oauth::{antigravity, claude_oauth, codex, cursor, grok};
use thickapi_core::thin::{claude_bridge, ThinEvent};

use super::redact::{gunzip_bytes, parse_connect_frames, path_to_pattern};

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Every leak in a set: each file's text, decoded Connect / base64 bodies and frames, and
/// response headers, against [`fixture_set::TOKEN_PATTERNS`] and the repo `roots`.
pub fn identity_violations(set_dir: &Path, roots: &[PathBuf]) -> Vec<String> {
    let prefix = format!(
        "{}/{}",
        set_dir.parent().map(name_of).unwrap_or_default(),
        name_of(set_dir)
    );
    let root_res: Vec<Regex> = roots
        .iter()
        .map(|r| format!("(?i){}", path_to_pattern(&r.to_string_lossy())))
        .filter_map(|pattern| Regex::new(&pattern).ok())
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut report = |rel: &str, names: Vec<&str>| {
        for name in names {
            let v = format!("{rel}: {name}");
            if !out.contains(&v) {
                out.push(v);
            }
        }
    };
    let mut check = |text: &str, rel: &str| {
        let mut names = fixture_set::leaks(Scope::Text, text);
        if root_res.iter().any(|re| re.is_match(text)) {
            names.push("repo root");
        }
        report(rel, names);
    };

    if let Ok(meta) = std::fs::read_to_string(set_dir.join(META_FILE)) {
        check(&meta, &format!("{prefix}/{META_FILE}"));
    }
    let mut header_hits: Vec<(String, Vec<&str>)> = Vec::new();
    for sc_dir in scenarios(set_dir) {
        let sc = format!("{prefix}/{}", name_of(&sc_dir));
        let read = |file: &str| std::fs::read_to_string(sc_dir.join(file)).ok();

        if let Some(text) = read(REQUEST_FILE) {
            let rel = format!("{sc}/{REQUEST_FILE}");
            check(&text, &rel);
            let body = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
                let b64 = v.get("body_b64")?.as_str()?.to_string();
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            });
            for decoded in body.iter().flat_map(|b| decoded_bodies(b, true)) {
                check(&decoded, &rel);
            }
        }

        if let Some(text) = read(RESPONSE_FILE) {
            let rel = format!("{sc}/{RESPONSE_FILE}");
            check(&text, &rel);
            let value = serde_json::from_str::<Value>(&text).unwrap_or_default();
            let headers = value.get("headers").and_then(Value::as_array).cloned();
            for pair in headers.iter().flatten().filter_map(Value::as_array) {
                let (Some(name), Some(value)) = (pair.first(), pair.get(1)) else {
                    continue;
                };
                let (name, value) = (name.as_str().unwrap_or(""), value.as_str().unwrap_or(""));
                let mut names = fixture_set::leaks(Scope::HeaderName, name);
                names.extend(fixture_set::leaks(Scope::HeaderValue, value));
                header_hits.push((rel.clone(), names));
            }
        }

        if let Some(text) = read(FRAMES_FILE) {
            let rel = format!("{sc}/{FRAMES_FILE}");
            check(&text, &rel);
            let records: Vec<FrameRecord> = text
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let all: Vec<u8> = records.iter().flat_map(FrameRecord::bytes).collect();
            // Connect framing is read frame by frame; anything else, per binary record.
            if parse_connect_frames(&all).is_some() {
                for decoded in decoded_bodies(&all, false) {
                    check(&decoded, &rel);
                }
            } else {
                for rec in records.iter().filter(|r| r.b64.is_some()) {
                    check(&String::from_utf8_lossy(&rec.bytes()), &rel);
                }
            }
        }

        if let Some(text) = read(EVENTS_FILE) {
            check(&text, &format!("{sc}/{EVENTS_FILE}"));
        }
    }
    for (rel, names) in header_hits {
        report(&rel, names);
    }
    out
}

/// Readable text inside a body: each Connect frame's payload (gunzipped when flagged), or —
/// when it is not Connect framing and `raw_fallback` — the bytes themselves.
fn decoded_bodies(bytes: &[u8], raw_fallback: bool) -> Vec<String> {
    let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match parse_connect_frames(bytes) {
        Some(frames) => frames
            .into_iter()
            .map(|(flags, payload)| match flags & 0x01 {
                0 => lossy(&payload),
                _ => gunzip_bytes(&payload).map_or_else(|_| lossy(&payload), |u| lossy(&u)),
            })
            .collect(),
        None if raw_fallback => vec![lossy(bytes)],
        None => Vec::new(),
    }
}

/// The offline replay a vendor's set runs through (as `wire_fixtures` does): the OAuth-direct
/// vendors' `VendorRequest::replay`, the bridge's `claude_bridge::replay`.
pub fn replay(
    vendor: &str,
    scenario_dir: &Path,
    frames: &[Vec<u8>],
    model: &str,
) -> Vec<ThinEvent> {
    match vendor {
        "claude" => claude_oauth::ClaudeDirectRequest::offline().replay(model, frames),
        "claude-bridge" => {
            let name = scenario_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let dialect = name == "dialect" || name == "todo";
            let declared = bridge_declared(scenario_dir);
            if dialect && !declared.is_empty() {
                claude_bridge::replay_with_tools(frames, &declared, true)
            } else {
                let tools = bridge_tools(scenario_dir);
                let names: Vec<&str> = tools.iter().map(String::as_str).collect();
                claude_bridge::replay(frames, &names)
            }
        }
        "grok" => {
            grok::GrokAdapter::new(&ThinConfig::default(), Arc::default()).replay(model, frames)
        }
        "agy" => antigravity::AgyAdapter::new(&ThinConfig::default(), Arc::default())
            .replay(model, frames),
        "codex" => {
            codex::CodexAdapter::new(&ThinConfig::default(), Arc::default()).replay(model, frames)
        }
        // A built-in read of a missing file is answered in protocol (G1), judged as the
        // capture host saw it: `thickapi capture` runs from the checkout it redacts to
        // `<REPO>` (F1 `create*` reads sit in a fictional cwd, so every one is missing).
        _ => {
            let checkout = std::env::current_dir().unwrap_or_default();
            cursor_request(scenario_dir)
                .answering_missing_reads(cursor::ReadMiss::for_fixtures(&checkout))
                .replay(model, frames)
        }
    }
}

/// Offline Cursor replay declaring what the captured frame 0 declared (G2, #182): a
/// built-in becomes one of those tools or is denied, as on the live Run.
fn cursor_request(scenario_dir: &Path) -> cursor::CursorRequest<'static> {
    let body = load_request(scenario_dir)
        .ok()
        .and_then(|r| r.body_bytes())
        .unwrap_or_default();
    cursor::CursorRequest::offline().declaring(&cursor::declared_tools_in_body(&body))
}

/// Declared tool schemas (`body.declared`), so a dialect scenario replays with the rows on.
fn bridge_declared(scenario_dir: &Path) -> Vec<thickapi_core::thin::openai::ToolDef> {
    let request = std::fs::read_to_string(scenario_dir.join(REQUEST_FILE)).unwrap_or_default();
    let request: Value = serde_json::from_str(&request).unwrap_or_default();
    request
        .get("body")
        .and_then(|b| b.get("declared"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect()
}

/// Client tool names the bridge declared for a scenario (`request.json` → `body.tools`).
fn bridge_tools(scenario_dir: &Path) -> Vec<String> {
    let request = std::fs::read_to_string(scenario_dir.join(REQUEST_FILE)).unwrap_or_default();
    let request: Value = serde_json::from_str(&request).unwrap_or_default();
    let tools = request.get("body").and_then(|b| b.get("tools")?.as_array());
    tools
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// A scenario's recorded events and its replay, as `wire_fixtures` compares them: rate-limit
/// advisories come from headers, and Cursor's usage is estimated from the prompt, so both
/// are left out.
fn recorded_and_replayed(
    vendor: &str,
    scenario_dir: &Path,
    wire_model: &str,
) -> (Vec<Value>, Vec<Value>) {
    let keep =
        |v: &Value| v["type"] != "rate_limit" && !(vendor == "cursor" && v["type"] == "usage");
    let recorded: Vec<Value> = load_events(scenario_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(keep)
        .collect();
    let model = recorded
        .iter()
        .find(|v| v["type"] == "started")
        .and_then(|v| v["model"].as_str())
        .unwrap_or(wire_model)
        .to_string();
    let frames = load_frames(scenario_dir).unwrap_or_default();
    let replayed = summarize(&replay(vendor, scenario_dir, &frames, &model))
        .into_iter()
        .filter(keep)
        .collect();
    (recorded, replayed)
}

/// `thickapi capture --check` over a freshly captured set outside `fixtures/`.
#[derive(Debug, Default)]
pub struct CheckReport {
    /// What makes the check exit 1: identity leaks, or a scenario whose frames no longer
    /// replay to the events the live run produced.
    pub failures: Vec<String>,
    /// The diff summary against the newest committed set (information, never a failure).
    pub diff: Vec<String>,
}

/// Leak-check `fresh_set`, replay each of its scenarios, and summarise how it differs from the
/// newest committed set under `fixtures_root`. Reads only; never promotes.
pub fn check_set(
    vendor: &str,
    fresh_set: &Path,
    fixtures_root: &Path,
    roots: &[PathBuf],
) -> CheckReport {
    let mut report = CheckReport {
        failures: identity_violations(fresh_set, roots),
        ..Default::default()
    };
    let meta = read_meta(fresh_set);
    let wire_model = meta
        .get("wire_model")
        .and_then(Value::as_str)
        .unwrap_or("model");
    for dir in scenarios(fresh_set) {
        let (recorded, replayed) = recorded_and_replayed(vendor, &dir, wire_model);
        if recorded != replayed {
            report.failures.push(format!(
                "{}: replay yields {} but the live run recorded {}",
                name_of(&dir),
                kinds(&replayed),
                kinds(&recorded)
            ));
        }
    }
    let Some(committed) = newest_set(fixtures_root, vendor) else {
        report
            .diff
            .push(format!("no committed {vendor} set to compare"));
        return report;
    };
    report.diff.push(format!(
        "fresh {} vs committed {}",
        name_of(fresh_set),
        name_of(&committed)
    ));
    let names: BTreeSet<String> = scenarios(fresh_set)
        .iter()
        .chain(scenarios(&committed).iter())
        .map(|d| name_of(d))
        .collect();
    for name in names {
        let (fresh, old) = (fresh_set.join(&name), committed.join(&name));
        if !fresh.is_dir() || !old.is_dir() {
            let side = if fresh.is_dir() { "committed" } else { "fresh" };
            report.diff.push(format!("{name}: no {side} scenario"));
            continue;
        }
        report.diff.extend(scenario_diff(&name, &fresh, &old));
    }
    report
}

fn read_meta(set: &Path) -> Value {
    let text = std::fs::read_to_string(set.join(META_FILE)).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

fn kinds(summary: &[Value]) -> String {
    let kinds: Vec<&str> = summary.iter().filter_map(|v| v["type"].as_str()).collect();
    format!("[{}]", kinds.join(", "))
}

/// Event shape, request header names, request body keys and frame count, fresh vs committed.
fn scenario_diff(name: &str, fresh: &Path, old: &Path) -> Vec<String> {
    let events = |dir: &Path| {
        let events = load_events(dir).unwrap_or_default();
        let keep = |v: &&Value| v["type"] != "rate_limit" && v["type"] != "usage";
        kinds(&events.iter().filter(keep).cloned().collect::<Vec<_>>())
    };
    let request = |dir: &Path| -> Value {
        let text = std::fs::read_to_string(dir.join(REQUEST_FILE)).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or_default()
    };
    let header_names = |r: &Value| -> BTreeSet<String> {
        let headers = r["headers"].as_array().cloned().unwrap_or_default();
        headers
            .iter()
            .filter_map(|h| h.get(0)?.as_str().map(str::to_ascii_lowercase))
            .collect()
    };
    let body_keys = |r: &Value| -> BTreeSet<String> {
        r["body"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let frames = |dir: &Path| load_frames(dir).map_or(0, |f| f.len());
    let (fresh_req, old_req) = (request(fresh), request(old));
    let mut out = Vec::new();
    let (fresh_events, old_events) = (events(fresh), events(old));
    out.push(if fresh_events == old_events {
        format!("{name}: same event shape {fresh_events}")
    } else {
        format!("{name}: event shape {fresh_events}, committed {old_events}")
    });
    for (what, fresh_set, old_set) in [
        (
            "request headers",
            header_names(&fresh_req),
            header_names(&old_req),
        ),
        (
            "request body keys",
            body_keys(&fresh_req),
            body_keys(&old_req),
        ),
    ] {
        let added: Vec<&String> = fresh_set.difference(&old_set).collect();
        let removed: Vec<&String> = old_set.difference(&fresh_set).collect();
        if !added.is_empty() || !removed.is_empty() {
            out.push(format!(
                "{name}: {what} added {added:?}, removed {removed:?}"
            ));
        }
    }
    out.push(format!(
        "{name}: {} frame(s), committed {}",
        frames(fresh),
        frames(old)
    ));
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn identity_violations_reads_response_headers_and_repo_roots() {
        let temp = tempfile::tempdir().unwrap();
        let sc = temp.path().join("vendor").join("1.0").join("text");
        std::fs::create_dir_all(&sc).unwrap();
        std::fs::write(
            sc.join(FRAMES_FILE),
            "{\"seq\":0,\"ms\":0,\"text\":\"at D:/work/repo/x\"}\n",
        )
        .unwrap();
        std::fs::write(
            sc.join(RESPONSE_FILE),
            r#"{"status":200,"headers":[["anthropic-organization-id","org"],["x-trace","11111111-2222-3333-4444-555555555555"]]}"#,
        )
        .unwrap();
        let v = identity_violations(sc.parent().unwrap(), &[PathBuf::from(r"D:\work\repo")]);
        let has = |needle: &str| v.iter().any(|x| x.ends_with(needle));
        assert!(has("frames.jsonl: repo root"), "{v:?}");
        assert!(has("response.json: anthropic-organization-id"), "{v:?}");
        assert!(has("response.json: response header UUID"), "{v:?}");
    }
}
