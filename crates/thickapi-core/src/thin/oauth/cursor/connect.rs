use bytes::Bytes;
use serde_json::Value;

use super::MAX_FRAME_BYTES;

pub fn connect_frame(payload: &[u8], flags: u8) -> Bytes {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Bytes::from(out)
}

// ---------------------------------------------------------------------------------------------
// Connect frame decoding + response protobuf extraction
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Frame {
    pub flags: u8,
    pub payload: Vec<u8>,
}

#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, String> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let flags = match self.buf.first() {
                Some(&f) => f,
                None => break,
            };
            let Some(len_bytes) = self.buf.get(1..5) else {
                break;
            };
            let Ok(len_arr) = <[u8; 4]>::try_from(len_bytes) else {
                break;
            };
            let len = u32::from_be_bytes(len_arr) as usize;
            if len > MAX_FRAME_BYTES {
                return Err(format!("frame of {len} bytes exceeds {MAX_FRAME_BYTES}"));
            }
            if self.buf.len() < 5 + len {
                break;
            }
            let Some(payload) = self.buf.get(5..5 + len).map(<[u8]>::to_vec) else {
                break;
            };
            self.buf.drain(..5 + len);
            frames.push(Frame { flags, payload });
        }
        Ok(frames)
    }

    /// Body ended: leftover bytes mean a truncated frame.
    pub fn finish(&self) -> Result<(), String> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "body ended mid-frame ({} bytes buffered)",
                self.buf.len()
            ))
        }
    }
}

pub(super) fn gunzip(payload: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(payload)
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > MAX_FRAME_BYTES {
        return Err("gzip payload exceeds the frame cap".into());
    }
    Ok(out)
}

/// The `aiserver.v1.ErrorDetails` code Cursor sends when one model is out of plan usage and
/// another would still run (#249, measured 2026-10-05 on `cursor-agent 2026.10.01-14929f9`).
const ROW_LIMIT_ERROR: &str = "ERROR_RATE_LIMITED_CHANGEABLE";

/// A Connect error trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConnectError {
    pub code: String,
    /// Cursor's own `title: detail` from `ErrorDetails` when it sent one (its top-level
    /// `message` is just `Error`), else the top-level message.
    pub message: String,
    /// The limit is true of this model, not the account (`ERROR_RATE_LIMITED_CHANGEABLE`).
    pub row_limit: bool,
}

/// Connect trailer → its error, when it is one.
pub(super) fn parse_connect_error(payload: &[u8]) -> Option<ConnectError> {
    if payload.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_slice(payload).ok()?;
    let err = v.get("error")?;
    let code = err.get("code")?.as_str()?.to_string();
    let debug = err
        .get("details")
        .and_then(Value::as_array)
        .and_then(|details| details.iter().find_map(|d| d.get("debug")));
    let text = |key: &str| {
        debug
            .and_then(|d| d.pointer(&format!("/details/{key}")))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let message = match (text("title"), text("detail")) {
        (Some(title), Some(detail)) => format!("{title}: {detail}"),
        (Some(one), None) | (None, Some(one)) => one.to_string(),
        (None, None) => err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Connect error")
            .to_string(),
    };
    let row_limit =
        debug.and_then(|d| d.get("error")).and_then(Value::as_str) == Some(ROW_LIMIT_ERROR);
    Some(ConnectError {
        code,
        message,
        row_limit,
    })
}
