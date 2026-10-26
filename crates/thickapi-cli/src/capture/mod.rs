//! Capture tooling for `thickapi capture` (SPEC §19.11), out of the daemon (R22 C6, audit R24):
//! the [`Recorder`] tap, redaction ([`redact`]), set checks ([`verify`]) and set writing
//! ([`set`]). `thickapi-core` keeps only the tap trait and record types
//! (`thin::capture`) and the set rules the replay tests share (`thin::fixture_set`).

pub mod redact;
pub mod set;
pub mod verify;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use thickapi_core::thin::capture::{FrameTap, RequestRecord, ResponseRecord};

pub use redact::{redact_text, repo_roots, RedactContext};
pub use set::{
    promote_capture_scenario, promote_capture_set, rewrite_set, write_events, Recording,
};
pub use verify::{check_set, identity_violations, CheckReport};

/// The workspace `create` runs in: the Cursor env cwd and the brief's worktree. Fictional,
/// so every built-in read there is of a missing file and no checkout path reaches the model
/// (a path the model echoes can split across token frames, past per-frame redaction). The
/// server never touches the client's disk; every exec comes back to the adapter.
pub const CREATE_CWD: &str = if cfg!(windows) {
    r"C:\work\smoke-repo"
} else {
    "/work/smoke-repo"
};

/// The capture tap: keeps one exchange in memory, redacting as it goes.
pub struct Recorder {
    started: Instant,
    context: Mutex<RedactContext>,
    raw_chunks: Mutex<Vec<(u64, Vec<u8>)>>,
    recording: Mutex<Recording>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::with_context(RedactContext::for_checkout(&cwd))
    }

    pub fn with_context(ctx: RedactContext) -> Self {
        Self {
            started: Instant::now(),
            context: Mutex::new(ctx),
            raw_chunks: Mutex::new(Vec::new()),
            recording: Mutex::new(Recording::default()),
        }
    }

    /// Extra strings to scrub from everything recorded (a token the adapter read itself).
    pub fn add_secret(&self, secret: impl Into<String>) {
        self.context
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .add_secret(secret);
    }

    /// Redact an event summary with the context the frames were redacted with (call it
    /// after [`Self::take`], so `<UUID-n>` numbering matches): every string leaf,
    /// length-preserving when `preserving` (Connect bodies are redacted that way, so replaying
    /// the redacted frames yields these events back), plain otherwise. A tool call's
    /// arguments can carry the capture cwd (a built-in's absolute path, F1 `create`).
    pub fn redact_summary(
        &self,
        summary: &[serde_json::Value],
        preserving: bool,
    ) -> Vec<serde_json::Value> {
        let mut ctx = self.context.lock().unwrap_or_else(|p| p.into_inner());
        summary
            .iter()
            .map(|v| redact_value(v, &mut ctx, preserving))
            .collect()
    }

    /// Take the exchange recorded so far and reset for the next one.
    pub fn take(&self) -> Recording {
        let mut recording =
            std::mem::take(&mut *self.recording.lock().unwrap_or_else(|p| p.into_inner()));
        let raw_chunks =
            std::mem::take(&mut *self.raw_chunks.lock().unwrap_or_else(|p| p.into_inner()));
        let mut ctx = self.context.lock().unwrap_or_else(|p| p.into_inner());
        set::process_frames_for_recording(&mut recording, raw_chunks, &mut ctx);
        recording
    }
}

fn redact_value(
    value: &serde_json::Value,
    ctx: &mut RedactContext,
    preserving: bool,
) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) if preserving => Value::String(redact::redact_text_preserving(s, ctx)),
        Value::String(s) => Value::String(redact_text(s, ctx)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| redact_value(v, ctx, preserving))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), redact_value(v, ctx, preserving)))
                .collect(),
        ),
        other => other.clone(),
    }
}

impl FrameTap for Recorder {
    fn request(&self, mut record: RequestRecord) {
        let mut ctx = self.context.lock().unwrap_or_else(|p| p.into_inner());
        redact::redact_request(&mut record, &mut ctx);
        let mut guard = self.recording.lock().unwrap_or_else(|p| p.into_inner());
        guard.request = Some(record);
    }

    fn response(&self, status: u16, headers: Vec<(String, String)>) {
        let headers = headers
            .into_iter()
            .filter(|(k, _)| redact::keep_response_header(k))
            .collect();
        let mut guard = self.recording.lock().unwrap_or_else(|p| p.into_inner());
        guard.response = Some(ResponseRecord { status, headers });
    }

    fn frame(&self, chunk: &[u8]) {
        let ms = self.started.elapsed().as_millis() as u64;
        let mut chunks = self.raw_chunks.lock().unwrap_or_else(|p| p.into_inner());
        chunks.push((ms, chunk.to_vec()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::redact::{gunzip_bytes, gzip_bytes, REDACTED, RESPONSE_HEADER_DENYLIST};
    use super::*;
    use serde_json::json;
    use thickapi_core::thin::capture::{load_events, load_frames};
    use thickapi_core::thin::fixture_set::scenarios;

    #[test]
    fn recorder_redacts_bearer_in_headers_body_and_frames() {
        let rec = Recorder::new();
        rec.request(RequestRecord {
            method: "POST".into(),
            url: "https://api.example/v1".into(),
            headers: vec![
                (
                    "authorization".into(),
                    "Bearer tok-oat01-SECRETSECRET".into(),
                ),
                ("accept".into(), "text/event-stream".into()),
            ],
            body: Some(json!({"echo": "token tok-oat01-SECRETSECRET here"})),
            body_b64: None,
        });
        rec.frame(b"data: {\"leak\":\"tok-oat01-SECRETSECRET\"}\n\n");
        rec.frame(&[0x00, 0xff, 0xfe, 0x01]);
        let r = rec.take();
        let req = r.request.unwrap();
        assert_eq!(req.headers[0].1, REDACTED);
        assert_eq!(req.headers[1].1, "text/event-stream");
        assert_eq!(req.body.unwrap()["echo"], "token <redacted> here");
        assert_eq!(r.frames.len(), 2);
        assert!(r.frames[0].text.as_deref().unwrap().contains("<redacted>"));
        assert!(!r.frames[0].text.as_deref().unwrap().contains("SECRET"));
        assert!(r.frames[1].text.is_none());
        assert_eq!(r.frames[1].bytes(), vec![0x00, 0xff, 0xfe, 0x01]);
        assert!(rec.take().frames.is_empty(), "take resets");
    }

    #[test]
    fn recording_round_trips_through_disk() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("text");
        let rec = Recorder::new();
        rec.frame(b"data: a\n\n");
        rec.frame(&[0x80, 0x81]);
        rec.take().write(&dir).unwrap();
        write_events(&dir, &[json!({"type": "finish", "reason": "stop"})]).unwrap();
        let frames = load_frames(&dir).unwrap();
        assert_eq!(frames, vec![b"data: a\n\n".to_vec(), vec![0x80, 0x81]]);
        assert_eq!(load_events(&dir).unwrap()[0]["reason"], "stop");
        assert_eq!(scenarios(temp.path()), vec![dir]);
    }

    /// Events are redacted the way their frames were: length-preserving for Connect (what
    /// replaying the redacted frames yields), plain otherwise. F1 `create`: a built-in's
    /// absolute path in a tool call's arguments.
    #[test]
    fn summary_redaction_matches_frame_redaction() {
        let ctx = || RedactContext {
            secrets: vec![],
            roots: vec![("C:\\Users\\x\\repo".into(), "<REPO>")],
            uuids: std::collections::HashMap::new(),
        };
        let summary = vec![json!({
            "type": "tool_call",
            "name": "write",
            "arguments": {"path": "C:\\Users\\x\\repo\\src\\a.txt", "content": "ok"}
        })];
        let preserved = Recorder::with_context(ctx()).redact_summary(&summary, true);
        let path = preserved[0]["arguments"]["path"].as_str().unwrap();
        assert_eq!(path.len(), "C:\\Users\\x\\repo\\src\\a.txt".len());
        assert!(
            path.starts_with("<REPO>_") && path.ends_with("\\src\\a.txt"),
            "{path}"
        );
        let plain = Recorder::with_context(ctx()).redact_summary(&summary, false);
        assert_eq!(plain[0]["arguments"]["path"], "<REPO>\\src\\a.txt");
        assert_eq!(plain[0]["arguments"]["content"], "ok");
        assert_eq!(plain[0]["name"], "write");
    }

    #[test]
    fn response_headers_keep_only_content_headers() {
        let rec = Recorder::new();
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            ("content-encoding".into(), "gzip".into()),
            ("transfer-encoding".into(), "chunked".into()),
            ("date".into(), "Thu, 17 Sep 2026 00:00:00 GMT".into()),
            ("server".into(), "cloudflare".into()),
            ("x-foo-id".into(), "123".into()),
        ];
        for &h in RESPONSE_HEADER_DENYLIST {
            headers.push((h.into(), "val".into()));
        }
        rec.response(200, headers);
        let r = rec.take();
        let resp = r.response.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.headers.len(), 3);
        assert_eq!(
            resp.headers[0],
            ("content-type".into(), "application/json".into())
        );
        assert_eq!(resp.headers[1], ("content-encoding".into(), "gzip".into()));
        assert_eq!(
            resp.headers[2],
            ("transfer-encoding".into(), "chunked".into())
        );
    }

    #[test]
    fn connect_gzip_frames_redacted_length_preserving() {
        let ctx = RedactContext {
            secrets: vec![],
            roots: vec![("C:\\Users\\x\\repo".into(), "<REPO>")],
            uuids: std::collections::HashMap::new(),
        };
        let rec = Recorder::with_context(ctx);
        rec.response(
            200,
            vec![("content-type".into(), "application/connect+proto".into())],
        );

        let orig_payload =
            b"\x0a\x48prefix C:%5CUsers%5Cx%5Crepo and 64ae2b69-2309-420e-b788-0a0dc4d736f8 suffix";
        let gzipped_payload = gzip_bytes(orig_payload).unwrap();
        let mut frame0 = vec![0x01];
        frame0.extend_from_slice(&(gzipped_payload.len() as u32).to_be_bytes());
        frame0.extend_from_slice(&gzipped_payload);

        let plain_payload = b"simple plain payload frame 1";
        let mut frame1 = vec![0x00];
        frame1.extend_from_slice(&(plain_payload.len() as u32).to_be_bytes());
        frame1.extend_from_slice(plain_payload);

        let mut stream = Vec::new();
        stream.extend_from_slice(&frame0);
        stream.extend_from_slice(&frame1);

        // Split across 3 chunks, where chunk 2 is valid UTF-8
        let chunk0 = &stream[..10];
        let chunk1 = &stream[10..frame0.len()];
        let chunk2 = &stream[frame0.len()..];
        assert!(std::str::from_utf8(chunk2).is_ok());

        rec.frame(chunk0);
        rec.frame(chunk1);
        rec.frame(chunk2);

        let r = rec.take();
        assert_eq!(r.frames.len(), 2);

        // Frame 0 re-parses and gunzips to same length
        let f0_bytes = r.frames[0].bytes();
        assert_eq!(f0_bytes[0], 0x01);
        let f0_len =
            u32::from_be_bytes([f0_bytes[1], f0_bytes[2], f0_bytes[3], f0_bytes[4]]) as usize;
        let f0_payload = &f0_bytes[5..5 + f0_len];
        let unzipped = gunzip_bytes(f0_payload).unwrap();
        assert_eq!(unzipped.len(), orig_payload.len());
        let unzipped_str = String::from_utf8_lossy(&unzipped);
        assert!(!unzipped_str.contains("C:%5CUsers%5Cx%5Crepo"));
        assert!(!unzipped_str.contains("64ae2b69-2309-420e-b788-0a0dc4d736f8"));
        assert!(unzipped_str.contains("<REPO>"));
        assert!(unzipped_str.contains("<UUID-1>"));

        // Frame 1
        let f1_bytes = r.frames[1].bytes();
        assert_eq!(f1_bytes[0], 0x00);
        let f1_len =
            u32::from_be_bytes([f1_bytes[1], f1_bytes[2], f1_bytes[3], f1_bytes[4]]) as usize;
        assert_eq!(&f1_bytes[5..5 + f1_len], plain_payload);
    }
}
