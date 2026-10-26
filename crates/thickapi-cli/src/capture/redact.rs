//! Capture redaction (SPEC §19.11): secrets the tap saw, `%USERPROFILE%` / repo roots →
//! `<HOME>` / `<REPO>` in every slash form, every `Text` entry of
//! [`fixture_set::TOKEN_PATTERNS`] → `<redacted>`, and UUIDs → stable `<UUID-n>`. Binary bodies
//! (Cursor's Connect frames) are redacted length-preserving so the framing still parses.

use std::path::{Path, PathBuf};

use base64::Engine;
use regex::{Captures, Regex};
use serde_json::Value;
use thickapi_core::thin::capture::RequestRecord;
use thickapi_core::thin::fixture_set::{self, Scope};

/// Header names whose values are secrets: redacted in the record and scrubbed from frames.
/// `chatgpt-account-id` (Codex) names the account, not a secret, but it is identity.
pub const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "x-api-key",
    "x-goog-api-key",
    "chatgpt-account-id",
];
pub const REDACTED: &str = "<redacted>";

/// Response headers a set never keeps (the record keeps only the content headers; see
/// [`keep_response_header`]). Named so the denylist the SPEC cites stays checkable.
pub const RESPONSE_HEADER_DENYLIST: &[&str] = &[
    "anthropic-organization-id",
    "anthropic-workspace-id",
    "anthropic-ratelimit-requests-limit",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-requests-reset",
    "anthropic-ratelimit-tokens-limit",
    "anthropic-ratelimit-tokens-remaining",
    "anthropic-ratelimit-tokens-reset",
    "anthropic-ratelimit-input-tokens-limit",
    "anthropic-ratelimit-input-tokens-remaining",
    "anthropic-ratelimit-input-tokens-reset",
    "anthropic-ratelimit-output-tokens-limit",
    "anthropic-ratelimit-output-tokens-remaining",
    "anthropic-ratelimit-output-tokens-reset",
    "request-id",
    "x-request-id",
    "cf-ray",
    "traceparent",
    "server-timing",
    "set-cookie",
];

/// A recorded response keeps only `content-type`, `content-encoding` and `transfer-encoding`.
pub fn keep_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "content-type" | "content-encoding" | "transfer-encoding"
    )
}

fn caps_str<'a>(caps: &'a Captures<'a>, i: usize) -> &'a str {
    caps.get(i).map(|m| m.as_str()).unwrap_or("")
}

fn uuid_regex() -> &'static Regex {
    static UUID: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    UUID.get_or_init(|| {
        let (.., re) = fixture_set::compiled()
            .iter()
            .find(|(name, ..)| *name == "response header UUID")
            .unwrap_or_else(|| unreachable!("TOKEN_PATTERNS names the header UUID"));
        re.clone()
    })
}

pub(crate) fn strip_unc(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p.to_path_buf()
    }
}

/// Walk up from `start` to the first dir with `.git`. That dir is one root.
/// If `.git` is a file (`gitdir: X/.git/worktrees/N`), `X` is a second root (the main checkout).
/// Canonicalise and strip the `\\?\` prefix. Do not spawn `git`.
pub fn repo_roots(start: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        let git_candidate = current.join(".git");
        if git_candidate.exists() {
            let root_canon = dunce::canonicalize(&current).unwrap_or_else(|_| strip_unc(&current));
            roots.push(strip_unc(&root_canon));
            if git_candidate.is_file() {
                if let Ok(content) = std::fs::read_to_string(&git_candidate) {
                    for line in content.lines() {
                        let line = line.trim();
                        if let Some(rest) = line.strip_prefix("gitdir:") {
                            let gitdir = rest.trim();
                            let gitdir_path = if Path::new(gitdir).is_relative() {
                                current.join(gitdir)
                            } else {
                                PathBuf::from(gitdir)
                            };
                            for ancestor in gitdir_path.ancestors() {
                                if ancestor
                                    .file_name()
                                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case(".git"))
                                    .unwrap_or(false)
                                {
                                    if let Some(main_root) = ancestor.parent() {
                                        let main_canon = dunce::canonicalize(main_root)
                                            .unwrap_or_else(|_| strip_unc(main_root));
                                        roots.push(strip_unc(&main_canon));
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            break;
        }
        if !current.pop() {
            break;
        }
    }
    let mut deduplicated = Vec::new();
    for r in roots {
        if !deduplicated.contains(&r) {
            deduplicated.push(r);
        }
    }
    deduplicated
}

pub fn path_to_pattern(path_str: &str) -> String {
    let clean = path_str.strip_prefix(r"\\?\").unwrap_or(path_str);
    let parts: Vec<&str> = clean.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return regex::escape(clean);
    }
    let escaped_parts: Vec<String> = parts.iter().map(|p| regex::escape(p)).collect();
    escaped_parts.join(r"(?:\\+|/|%5[Cc])")
}

pub(crate) fn connect_frame_header(stream: &[u8]) -> Option<(u8, usize)> {
    let header = stream.get(..5)?;
    let flags = *header.first()?;
    let len_bytes = header.get(1..5)?;
    let len_arr: [u8; 4] = len_bytes.try_into().ok()?;
    Some((flags, u32::from_be_bytes(len_arr) as usize))
}

pub fn parse_connect_frames(mut stream: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    let mut frames = Vec::new();
    if stream.is_empty() {
        return None;
    }
    while !stream.is_empty() {
        let (flags, len) = connect_frame_header(stream)?;
        if stream.len() < 5 + len {
            return None;
        }
        let payload = stream.get(5..5 + len)?.to_vec();
        frames.push((flags, payload));
        stream = stream.get(5 + len..)?;
    }
    Some(frames)
}

/// Re-frame Connect `(flags, payload)` pairs into one byte stream.
pub(crate) fn encode_connect(frames: Vec<(u8, Vec<u8>)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (flags, payload) in frames {
        out.push(flags);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out
}

pub(crate) fn gunzip_bytes(payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(payload);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

pub(crate) fn gzip_bytes(payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(payload)?;
    encoder.finish()
}

pub struct RedactContext {
    pub secrets: Vec<String>,
    pub roots: Vec<(String, &'static str)>,
    pub uuids: std::collections::HashMap<String, usize>,
}

impl RedactContext {
    pub fn for_checkout(start: &Path) -> Self {
        let mut roots = Vec::new();
        let home = std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .ok()
            .or_else(dirs::home_dir);
        if let Some(h) = home {
            let h_clean = strip_unc(&dunce::canonicalize(&h).unwrap_or(h));
            roots.push((h_clean.to_string_lossy().into_owned(), "<HOME>"));
        }
        for r in repo_roots(start) {
            roots.push((r.to_string_lossy().into_owned(), "<REPO>"));
        }
        roots.sort_by_key(|a| std::cmp::Reverse(a.0.len()));
        Self {
            secrets: Vec::new(),
            roots,
            uuids: std::collections::HashMap::new(),
        }
    }

    pub fn add_secret(&mut self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.len() >= 8 && !self.secrets.contains(&secret) {
            self.secrets.push(secret);
            self.secrets.sort_by_key(|a| std::cmp::Reverse(a.len()));
        }
    }

    pub fn uuid_placeholder(&mut self, uuid_str: &str) -> String {
        let lower = uuid_str.to_ascii_lowercase();
        let next_n = self.uuids.len() + 1;
        let n = *self.uuids.entry(lower).or_insert(next_n);
        format!("<UUID-{n}>")
    }
}

pub fn pad_or_truncate(placeholder: &str, len: usize) -> String {
    match placeholder.get(..len) {
        Some(head) => head.to_string(),
        None => format!("{placeholder}{}", "_".repeat(len - placeholder.len())),
    }
}

/// Replace every leak in `text`. `preserve` keeps each match's byte length (binary bodies).
fn redact(text: &str, ctx: &mut RedactContext, preserve: bool) -> String {
    let fill = |placeholder: &str, found: &str| {
        if preserve {
            pad_or_truncate(placeholder, found.len())
        } else {
            placeholder.to_string()
        }
    };
    let mut out = text.to_string();
    for s in ctx.secrets.iter().filter(|s| !s.is_empty()) {
        out = out.replace(s.as_str(), &fill(REDACTED, s));
    }
    for (root_path, placeholder) in &ctx.roots {
        let pat = path_to_pattern(root_path);
        if let Ok(re) = Regex::new(&format!("(?i){pat}")) {
            let with = |caps: &Captures| fill(placeholder, caps_str(caps, 0));
            out = re.replace_all(&out, with).into_owned();
        }
    }
    for (_, scope, re) in fixture_set::compiled() {
        if *scope == Scope::Text {
            let with = |caps: &Captures| fill(REDACTED, caps_str(caps, 0));
            out = re.replace_all(&out, with).into_owned();
        }
    }
    uuid_regex()
        .replace_all(&out, |caps: &Captures| {
            let found = caps_str(caps, 0);
            fill(&ctx.uuid_placeholder(found), found)
        })
        .into_owned()
}

pub fn redact_text(text: &str, ctx: &mut RedactContext) -> String {
    redact(text, ctx, false)
}

pub fn redact_text_preserving(text: &str, ctx: &mut RedactContext) -> String {
    redact(text, ctx, true)
}

fn decode_printable_char(bytes: &[u8]) -> Option<(char, usize)> {
    let b = *bytes.first()?;
    if (0x20..=0x7e).contains(&b) || b == b'\t' || b == b'\n' || b == b'\r' {
        return Some((b as char, 1));
    }
    if b >= 0x80 {
        let max_len = bytes.len().min(4);
        for len in 2..=max_len {
            let Some(slice) = bytes.get(..len) else {
                break;
            };
            if let Ok(s) = std::str::from_utf8(slice) {
                if let Some(c) = s.chars().next() {
                    if c.len_utf8() == len && !c.is_control() {
                        return Some((c, len));
                    }
                }
            }
        }
    }
    None
}

fn find_printable_runs(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((_, len)) = bytes.get(i..).and_then(decode_printable_char) {
            let start = i;
            i += len;
            while i < bytes.len() {
                if let Some((_, next_len)) = bytes.get(i..).and_then(decode_printable_char) {
                    i += next_len;
                } else {
                    break;
                }
            }
            if i - start >= 8 {
                runs.push((start, i));
            }
        } else {
            i += 1;
        }
    }
    runs
}

pub fn redact_bytes_preserving(bytes: &[u8], ctx: &mut RedactContext) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for (start, end) in find_printable_runs(bytes) {
        if let Some(run) = bytes.get(start..end) {
            if let Ok(run_str) = std::str::from_utf8(run) {
                let redacted = redact_text_preserving(run_str, ctx);
                if redacted.len() == end - start {
                    if let Some(dst) = out.get_mut(start..end) {
                        dst.copy_from_slice(redacted.as_bytes());
                    }
                }
            }
        }
    }
    out
}

pub fn redact_connect(stream: &[u8], ctx: &mut RedactContext) -> Option<Vec<(u8, Vec<u8>)>> {
    let frames = parse_connect_frames(stream)?;
    let mut out = Vec::with_capacity(frames.len());
    for (flags, payload) in frames {
        let new_payload = if (flags & 0x01) != 0 {
            let unzipped = gunzip_bytes(&payload).ok()?;
            let redacted_unzipped = redact_bytes_preserving(&unzipped, ctx);
            gzip_bytes(&redacted_unzipped).ok()?
        } else {
            redact_bytes_preserving(&payload, ctx)
        };
        out.push((flags, new_payload));
    }
    Some(out)
}

/// Redact one request record in place: secret headers become `<redacted>` (their bearer is
/// scrubbed from everything later), every other header, the URL and the body pass the
/// redactor, and a binary body keeps its Connect framing.
pub fn redact_request(record: &mut RequestRecord, ctx: &mut RedactContext) {
    for (name, value) in record.headers.iter_mut() {
        if SECRET_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            let secret = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
                .unwrap_or(value)
                .to_string();
            ctx.add_secret(secret);
            *value = REDACTED.to_string();
        } else {
            *value = redact_text(value, ctx);
        }
    }
    record.url = redact_text(&record.url, ctx);
    if let Some(body) = record.body.take() {
        let text = redact_text(&body.to_string(), ctx);
        record.body = Some(serde_json::from_str(&text).unwrap_or(Value::String(text)));
    }
    if let Some(b64) = record.body_b64.take() {
        let engine = base64::engine::general_purpose::STANDARD;
        if let Ok(bytes) = engine.decode(&b64) {
            let new_bytes = match redact_connect(&bytes, ctx) {
                Some(frames) => encode_connect(frames),
                None => redact_bytes_preserving(&bytes, ctx),
            };
            record.body_b64 = Some(engine.encode(&new_bytes));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn redact_text_paths_tokens_emails_uuids() {
        let mut ctx = RedactContext {
            secrets: vec![],
            roots: vec![
                ("C:\\Users\\user\\repo".into(), "<REPO>"),
                ("C:\\Users\\user".into(), "<HOME>"),
            ],
            uuids: std::collections::HashMap::new(),
        };
        // 1. HOME/REPO in all 4 forms, repo under home gives <REPO>
        let text = "a C:\\Users\\user\\repo b C:/Users/user/repo c C:%5CUsers%5Cuser%5Crepo d C:\\\\Users\\\\user\\\\repo e C:\\Users\\user\\other f";
        let redacted = redact_text(text, &mut ctx);
        assert_eq!(
            redacted,
            "a <REPO> b <REPO> c <REPO> d <REPO> e <HOME>\\other f"
        );

        // 2. Google refresh tokens
        let text2 = "token: 1//0gABCDEFGHIJ1234567890";
        assert_eq!(redact_text(text2, &mut ctx), "token: <redacted>");

        // 3. Email
        let text3 = "contact a.b@example.com please";
        assert_eq!(redact_text(text3, &mut ctx), "contact <redacted> please");

        // 4. JWT. Assembled at runtime so no contiguous header.payload literal
        // sits in this source: draftr's push gate scans context lines too, so
        // a literal here would fail every later diff that shows this line.
        let text4 = format!(
            "token={}.{}.{}",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "FAKESignatureNotARealJWT"
        );
        assert_eq!(redact_text(&text4, &mut ctx), "token=<redacted>");

        // 5. UUIDs: same UUID twice gives <UUID-1>, second gives <UUID-2>
        let u1 = "11111111-1111-1111-1111-111111111111";
        let u2 = "22222222-2222-2222-2222-222222222222";
        let text5 = format!("{u1} and {u1} then {u2}");
        assert_eq!(
            redact_text(&text5, &mut ctx),
            "<UUID-1> and <UUID-1> then <UUID-2>"
        );

        // 6. Second call on same context continues numbering
        let u3 = "33333333-3333-3333-3333-333333333333";
        let text6 = format!("third {u3} and first {u1}");
        assert_eq!(
            redact_text(&text6, &mut ctx),
            "third <UUID-3> and first <UUID-1>"
        );
    }

    #[test]
    fn preserving_redaction_keeps_byte_length() {
        let mut ctx = RedactContext {
            secrets: vec![],
            roots: vec![("C:\\Users\\x\\repo".into(), "<REPO>")],
            uuids: std::collections::HashMap::new(),
        };
        let text = "at C:%5CUsers%5Cx%5Crepo id 64ae2b69-2309-420e-b788-0a0dc4d736f8 ok";
        let out = redact_text_preserving(text, &mut ctx);
        assert_eq!(out.len(), text.len(), "{out}");
        assert!(out.contains("<REPO>") && out.contains("<UUID-1>"), "{out}");
        assert_eq!(pad_or_truncate("<redacted>", 4), "<red");
        assert_eq!(pad_or_truncate("<ab>", 6), "<ab>__");
    }
}
