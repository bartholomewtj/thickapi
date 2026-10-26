//! Writing a fixture set (SPEC §19.11): one [`Recording`] per scenario, `events.jsonl`, the
//! set's `meta.json`, re-redacting a committed set in place, and promoting a clean temp tree.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thickapi_core::thin::capture::{FrameRecord, RequestRecord, ResponseRecord};
use thickapi_core::thin::fixture_set::{
    scenarios, EVENTS_FILE, FRAMES_FILE, META_FILE, REQUEST_FILE, RESPONSE_FILE,
};

use super::redact::{
    connect_frame_header, keep_response_header, redact_bytes_preserving, redact_connect,
    redact_request, redact_text, RedactContext,
};
use super::verify::identity_violations;

/// One scenario's exchange as the [`super::Recorder`] kept it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recording {
    pub request: Option<RequestRecord>,
    pub response: Option<ResponseRecord>,
    pub frames: Vec<FrameRecord>,
}

impl Recording {
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        if let Some(r) = &self.request {
            std::fs::write(dir.join(REQUEST_FILE), serde_json::to_string_pretty(r)?)?;
        }
        if let Some(r) = &self.response {
            std::fs::write(dir.join(RESPONSE_FILE), serde_json::to_string_pretty(r)?)?;
        }
        let mut frames = String::new();
        for f in &self.frames {
            frames.push_str(&serde_json::to_string(f)?);
            frames.push('\n');
        }
        std::fs::write(dir.join(FRAMES_FILE), frames)
    }
}

pub fn write_events(scenario_dir: &Path, summary: &[Value]) -> std::io::Result<()> {
    let mut out = String::new();
    for v in summary {
        out.push_str(&v.to_string());
        out.push('\n');
    }
    std::fs::write(scenario_dir.join(EVENTS_FILE), out)
}

/// The path a set was captured on, as `meta.json` records it: the bridge set is `bridge`,
/// every other vendor set is the OAuth-direct path.
pub fn capture_path(vendor: &str) -> &'static str {
    if vendor == "claude-bridge" {
        "bridge"
    } else {
        "oauth"
    }
}

/// A set's `meta.json`.
pub fn meta(
    vendor: &str,
    version: &str,
    model: &str,
    wire_model: &str,
    scenarios: &[String],
) -> Value {
    json!({
        "vendor": vendor,
        "version": version,
        "model": model,
        "wire_model": wire_model,
        "path": capture_path(vendor),
        "captured_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "thickapi_version": env!("CARGO_PKG_VERSION"),
        "scenarios": scenarios,
    })
}

fn frame_record(
    seq: usize,
    ms: u64,
    bytes: &[u8],
    text: impl FnOnce(&str) -> String,
) -> FrameRecord {
    match std::str::from_utf8(bytes) {
        Ok(t) => FrameRecord {
            seq,
            ms,
            text: Some(text(t)),
            b64: None,
        },
        Err(_) => FrameRecord {
            seq,
            ms,
            text: None,
            b64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        },
    }
}

/// Redact the raw chunks into `recording.frames`. A Connect stream that parses whole is
/// re-framed one record per Connect frame (payloads gunzipped, redacted, re-gzipped); anything
/// else is one record per chunk.
pub(crate) fn process_frames_for_recording(
    recording: &mut Recording,
    raw_chunks: Vec<(u64, Vec<u8>)>,
    ctx: &mut RedactContext,
) {
    if raw_chunks.is_empty() {
        return;
    }
    let is_connect = recording.response.as_ref().is_some_and(|resp| {
        resp.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("content-type")
                && v.to_ascii_lowercase().starts_with("application/connect")
        })
    });
    if is_connect {
        if let Some(frames) = connect_records(&raw_chunks, ctx) {
            recording.frames = frames;
            return;
        }
    }
    recording.frames = raw_chunks
        .into_iter()
        .enumerate()
        .map(|(seq, (ms, chunk))| match std::str::from_utf8(&chunk) {
            Ok(_) => frame_record(seq, ms, &chunk, |t| redact_text(t, ctx)),
            Err(_) => FrameRecord {
                seq,
                ms,
                text: None,
                b64: Some(
                    base64::engine::general_purpose::STANDARD
                        .encode(redact_bytes_preserving(&chunk, ctx)),
                ),
            },
        })
        .collect();
}

/// One record per Connect frame, stamped with the arrival time of the chunk it starts in;
/// `None` when the chunks do not parse as whole Connect frames.
fn connect_records(raw: &[(u64, Vec<u8>)], ctx: &mut RedactContext) -> Option<Vec<FrameRecord>> {
    let mut all = Vec::new();
    let mut bounds = Vec::new();
    for (ms, c) in raw {
        let start = all.len();
        all.extend_from_slice(c);
        bounds.push((start, all.len(), *ms));
    }
    let mut frame_ms = Vec::new();
    let mut offset = 0;
    while offset < all.len() {
        let (_, len) = connect_frame_header(all.get(offset..)?)?;
        if all.len() < offset + 5 + len {
            return None;
        }
        let ms = bounds
            .iter()
            .find(|(start, end, _)| offset >= *start && offset < *end)
            .map_or(0, |(_, _, ms)| *ms);
        frame_ms.push(ms);
        offset += 5 + len;
    }
    let frames = redact_connect(&all, ctx)?;
    let records = frames
        .into_iter()
        .enumerate()
        .map(|(seq, (flags, payload))| {
            let mut bytes = Vec::with_capacity(5 + payload.len());
            bytes.push(flags);
            bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&payload);
            let ms = frame_ms.get(seq).copied().unwrap_or(0);
            frame_record(seq, ms, &bytes, str::to_string)
        });
    Some(records.collect())
}

/// Re-redact a committed set in place through today's redactor (`THICKAPI_FIXTURE_REWRITE=1`
/// on the `capture_redact` test). Idempotent.
pub fn rewrite_set(set_dir: &Path, ctx: &mut RedactContext) -> std::io::Result<()> {
    let meta_path = set_dir.join(META_FILE);
    if meta_path.exists() {
        let redacted = redact_text(&std::fs::read_to_string(&meta_path)?, ctx);
        match serde_json::from_str::<Value>(&redacted) {
            Ok(v) => std::fs::write(&meta_path, serde_json::to_string_pretty(&v)?)?,
            Err(_) => std::fs::write(&meta_path, redacted)?,
        }
    }
    for sc_dir in scenarios(set_dir) {
        let read = |file: &str| std::fs::read_to_string(sc_dir.join(file)).ok();
        let request = read(REQUEST_FILE)
            .and_then(|t| serde_json::from_str::<RequestRecord>(&t).ok())
            .map(|mut record| {
                redact_request(&mut record, ctx);
                record
            });
        let response = read(RESPONSE_FILE)
            .and_then(|t| serde_json::from_str::<ResponseRecord>(&t).ok())
            .map(|mut record| {
                record.headers.retain(|(k, _)| keep_response_header(k));
                record
            });
        let raw_chunks = read(FRAMES_FILE)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<FrameRecord>(l).ok())
            .map(|rec| (rec.ms, rec.bytes()))
            .collect();
        let mut recording = Recording {
            request,
            response,
            frames: Vec::new(),
        };
        process_frames_for_recording(&mut recording, raw_chunks, ctx);
        recording.write(&sc_dir)?;
        if let Some(events) = read(EVENTS_FILE) {
            let mut out = String::new();
            for line in events.lines().filter(|l| !l.trim().is_empty()) {
                out.push_str(&redact_text(line, ctx));
                out.push('\n');
            }
            std::fs::write(sc_dir.join(EVENTS_FILE), out)?;
        }
    }
    Ok(())
}

fn refuse_leaks(tmp_dir: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let violations = identity_violations(tmp_dir, roots);
    if violations.is_empty() {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(tmp_dir);
    Err(format!(
        "captured set carries identity violations:\n{}",
        violations.join("\n")
    ))
}

/// Run `identity_violations` on `tmp_dir`, then rename it to `dest` only if clean.
/// A leaking tree is deleted and `dest` is left untouched.
pub fn promote_capture_set(tmp_dir: &Path, dest: &Path, roots: &[PathBuf]) -> Result<(), String> {
    refuse_leaks(tmp_dir, roots)?;
    if dest.exists() {
        std::fs::remove_dir_all(dest)
            .map_err(|e| format!("remove existing {} before promote: {e}", dest.display()))?;
    }
    std::fs::rename(tmp_dir, dest)
        .map_err(|e| format!("promote {} -> {}: {e}", tmp_dir.display(), dest.display()))?;
    Ok(())
}

pub(crate) fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    let entries = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", src.display()))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| format!("type {}: {e}", from.display()))?;
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Copy one scenario directory from `tmp_dir` into an existing `dest` set.
/// Never deletes `dest` (unlike [`promote_capture_set`]), so Cursor `--scenario sdlc`
/// cannot replace `{text,tool}`.
pub fn promote_capture_scenario(
    tmp_dir: &Path,
    dest: &Path,
    scenario: &str,
    roots: &[PathBuf],
) -> Result<(), String> {
    refuse_leaks(tmp_dir, roots)?;
    let text = dest.join("text");
    let tool = dest.join("tool");
    if !dest.is_dir() || !text.is_dir() || !tool.is_dir() {
        let _ = std::fs::remove_dir_all(tmp_dir);
        return Err(format!(
            "scenario {scenario} needs an existing {} set with text/ and tool/; will not replace that set",
            dest.display()
        ));
    }
    let src = tmp_dir.join(scenario);
    if !src.is_dir() {
        let _ = std::fs::remove_dir_all(tmp_dir);
        return Err(format!("temp tree has no {scenario}/ directory"));
    }
    let dest_sc = dest.join(scenario);
    if dest_sc.exists() {
        std::fs::remove_dir_all(&dest_sc)
            .map_err(|e| format!("remove existing {}: {e}", dest_sc.display()))?;
    }
    copy_dir(&src, &dest_sc)?;
    let _ = std::fs::remove_dir_all(tmp_dir);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn bridge_set_records_the_bridge_path() {
        assert_eq!(capture_path("claude-bridge"), "bridge");
        assert_eq!(capture_path("claude"), "oauth");
        assert_eq!(capture_path("grok"), "oauth");
        let m = meta("claude-bridge", "2.1.290", "subs/claude/fable", "w", &[]);
        assert_eq!(m["path"], "bridge");
        assert_eq!(m["vendor"], "claude-bridge");
    }

    #[test]
    fn rewrite_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let set_dir = temp.path().join("vendor").join("1.0.0");
        let text_sc = set_dir.join("text");
        std::fs::create_dir_all(&text_sc).unwrap();

        let mut ctx = RedactContext {
            secrets: vec![],
            roots: vec![
                (temp.path().to_string_lossy().into_owned(), "<REPO>"),
                ("C:\\Users\\alice".into(), "<HOME>"),
            ],
            uuids: std::collections::HashMap::new(),
        };

        let leaky_req = json!({
            "method": "POST",
            "url": "https://api.example/C:\\Users\\alice\\api",
            "headers": [
                ["authorization", "Bearer tok-oat01-SECRET123456"],
                ["x-request-id", "12345678-1234-1234-1234-123456789abc"]
            ],
            "body": {
                "path": format!("{}\\file.txt", temp.path().display()),
                "email": "leak@example.com"
            }
        });
        std::fs::write(
            text_sc.join(REQUEST_FILE),
            serde_json::to_string_pretty(&leaky_req).unwrap(),
        )
        .unwrap();

        let leaky_resp = json!({
            "status": 200,
            "headers": [
                ["content-type", "application/json"],
                ["anthropic-organization-id", "4fc37a03-1234-1234-1234-123456789abc"],
                ["x-request-id", "12345678-1234-1234-1234-123456789abc"]
            ]
        });
        std::fs::write(
            text_sc.join(RESPONSE_FILE),
            serde_json::to_string_pretty(&leaky_resp).unwrap(),
        )
        .unwrap();

        let frame = FrameRecord {
            seq: 0,
            ms: 10,
            text: Some("hello from C:\\Users\\alice".into()),
            b64: None,
        };
        std::fs::write(
            text_sc.join(FRAMES_FILE),
            format!("{}\n", serde_json::to_string(&frame).unwrap()),
        )
        .unwrap();
        std::fs::write(
            text_sc.join(EVENTS_FILE),
            "{\"type\":\"text\",\"text\":\"C:\\\\Users\\\\alice\"}\n",
        )
        .unwrap();
        std::fs::write(
            set_dir.join(META_FILE),
            "{\"vendor\":\"test\",\"user\":\"C:\\\\Users\\\\alice\"}",
        )
        .unwrap();

        rewrite_set(&set_dir, &mut ctx).unwrap();
        let roots = vec![temp.path().to_path_buf(), PathBuf::from("C:\\Users\\alice")];
        let violations = identity_violations(&set_dir, &roots);
        assert!(
            violations.is_empty(),
            "violations after rewrite: {:?}",
            violations
        );

        let snapshot = || {
            [
                text_sc.join(REQUEST_FILE),
                text_sc.join(RESPONSE_FILE),
                text_sc.join(FRAMES_FILE),
                text_sc.join(EVENTS_FILE),
                set_dir.join(META_FILE),
            ]
            .map(|p| std::fs::read(p).unwrap())
        };
        let first = snapshot();
        rewrite_set(&set_dir, &mut ctx).unwrap();
        assert_eq!(first, snapshot());
    }

    #[test]
    fn promote_capture_set_deletes_leaky_tmp_and_keeps_dest() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("set");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join(META_FILE), "{\"clean\":true}").unwrap();
        let tmp = root.path().join("set.tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(META_FILE), "ta_deadbeefdeadbeef\n").unwrap();
        let err = promote_capture_set(&tmp, &dest, &[]).expect_err("leaky");
        assert!(err.contains("identity"), "{err}");
        assert!(!tmp.exists(), "leaky temp must be deleted");
        assert_eq!(
            std::fs::read_to_string(dest.join(META_FILE)).unwrap(),
            "{\"clean\":true}"
        );
    }

    #[test]
    fn promote_capture_set_renames_a_clean_tree() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("set");
        let tmp = root.path().join("set.tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join(META_FILE), "{\"vendor\":\"x\"}").unwrap();
        promote_capture_set(&tmp, &dest, &[]).expect("clean");
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::read_to_string(dest.join(META_FILE)).unwrap(),
            "{\"vendor\":\"x\"}"
        );
    }

    #[test]
    fn promote_capture_scenario_keeps_dest_text_and_tool() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("set");
        std::fs::create_dir_all(dest.join("text")).unwrap();
        std::fs::create_dir_all(dest.join("tool")).unwrap();
        std::fs::write(dest.join("text").join("keep.txt"), "text").unwrap();
        std::fs::write(dest.join("tool").join("keep.txt"), "tool").unwrap();
        std::fs::write(dest.join(META_FILE), "{\"keep\":true}").unwrap();
        let tmp = root.path().join("set.tmp");
        std::fs::create_dir_all(tmp.join("sdlc")).unwrap();
        std::fs::write(tmp.join("sdlc").join("frames.jsonl"), "{}\n").unwrap();
        std::fs::write(tmp.join(META_FILE), "{\"scenarios\":[\"sdlc\"]}").unwrap();
        promote_capture_scenario(&tmp, &dest, "sdlc", &[]).expect("merge");
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::read_to_string(dest.join(META_FILE)).unwrap(),
            "{\"keep\":true}"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("text").join("keep.txt")).unwrap(),
            "text"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("tool").join("keep.txt")).unwrap(),
            "tool"
        );
        assert!(dest.join("sdlc").join("frames.jsonl").is_file());
    }

    #[test]
    fn promote_capture_scenario_refuses_a_missing_set() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("set");
        let tmp = root.path().join("set.tmp");
        std::fs::create_dir_all(tmp.join("sdlc")).unwrap();
        std::fs::write(tmp.join("sdlc").join("frames.jsonl"), "{}\n").unwrap();
        let err = promote_capture_scenario(&tmp, &dest, "sdlc", &[]).expect_err("no dest");
        assert!(err.contains("text/"), "{err}");
        assert!(!tmp.exists());
        assert!(!dest.exists());
    }
}
