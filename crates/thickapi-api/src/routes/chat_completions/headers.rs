use axum::http::{HeaderMap, HeaderValue};

/// `x-thickapi-path: <path>[;reason=…][;probe=…]` and optional `x-thickapi-fallback` (SPEC §19.15 R4).
pub(super) fn insert_path_headers(
    headers: &mut HeaderMap,
    path: &str,
    fallback: Option<&str>,
    reason: Option<&str>,
    probe: Option<&str>,
) {
    let mut value = path.to_string();
    if let Some(r) = reason {
        value.push_str(";reason=");
        value.push_str(r);
    }
    if let Some(p) = probe {
        value.push_str(";probe=");
        value.push_str(p);
    }
    if let Ok(v) = HeaderValue::from_str(&value) {
        headers.insert("x-thickapi-path", v);
    }
    if let Some(f) = fallback {
        if let Ok(v) = HeaderValue::from_str(f) {
            headers.insert("x-thickapi-fallback", v);
        }
    }
}

pub(super) fn path_reason_from(why: &str) -> Option<&'static str> {
    let w = why.to_ascii_lowercase();
    if w.contains("token expired") {
        Some("token_expired")
    } else if w.contains("direct blocked") || w.starts_with("blocked:") {
        Some("blocked")
    } else if w.contains("mode = off") {
        Some("mode")
    } else if w.contains("cli_bridge") {
        Some("row")
    } else {
        None
    }
}

pub(super) fn iso8601(unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}
