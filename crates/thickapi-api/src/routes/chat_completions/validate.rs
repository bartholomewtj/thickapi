use thickapi_core::config::models::{ModelEntry, Transport};
use thickapi_core::thin::openai::{self, ChatRequest, Content};
use thickapi_core::thin::ThinError;

// ---- validation (SPEC §19.3) ----

const ROLES: &[&str] = &["system", "developer", "user", "assistant", "tool"];

/// Returns the list of ignored-with-header fields.
pub(super) fn validate(
    request: &ChatRequest,
    entry: &ModelEntry,
) -> Result<Vec<&'static str>, ThinError> {
    if request.messages.is_empty() {
        return Err(ThinError::invalid_request(
            "messages must not be empty",
            Some("messages"),
        ));
    }
    if let Some(n) = request.n {
        if n > 1 {
            return Err(ThinError::invalid_request(
                "n > 1 is not supported",
                Some("n"),
            ));
        }
    }
    if request
        .logprobs
        .as_ref()
        .is_some_and(|v| !v.is_null() && v != &serde_json::Value::Bool(false))
    {
        return Err(ThinError::invalid_request(
            "logprobs is not supported",
            Some("logprobs"),
        ));
    }
    if let Some(rf) = &request.response_format {
        let kind = rf.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if kind != "text" && !rf.is_null() {
            return Err(ThinError::invalid_request(
                format!("response_format.type {kind:?} is not supported (only \"text\")"),
                Some("response_format"),
            ));
        }
    }
    let allows_images = entry.input.iter().any(|i| i == "image");
    for (i, m) in request.messages.iter().enumerate() {
        if !ROLES.contains(&m.role.as_str()) {
            return Err(ThinError::invalid_request(
                format!("unknown role {:?}", m.role),
                Some(&format!("messages[{i}].role")),
            ));
        }
        if m.role == "tool" && m.tool_call_id.is_none() {
            return Err(ThinError::invalid_request(
                "tool message requires tool_call_id",
                Some(&format!("messages[{i}].tool_call_id")),
            ));
        }
        if let Some(Content::Parts(parts)) = &m.content {
            for p in parts {
                match p {
                    openai::ContentPart::ImageUrl { .. } if !allows_images => {
                        return Err(ThinError::invalid_request(
                            format!("model {} does not accept image input", entry.id),
                            Some(&format!("messages[{i}].content")),
                        ));
                    }
                    openai::ContentPart::Unknown => {
                        return Err(ThinError::invalid_request(
                            "unsupported content part type",
                            Some(&format!("messages[{i}].content")),
                        ));
                    }
                    _ => {}
                }
            }
        }
        if m.content.is_none()
            && !(m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|t| !t.is_empty()))
        {
            return Err(ThinError::invalid_request(
                "content is required (assistant may omit it only alongside tool_calls)",
                Some(&format!("messages[{i}].content")),
            ));
        }
    }
    for (i, t) in request.tools.iter().enumerate() {
        if t.kind != "function" {
            return Err(ThinError::invalid_request(
                format!("tools[{i}].type {:?} is not supported", t.kind),
                Some(&format!("tools[{i}].type")),
            ));
        }
    }

    let mut ignored = Vec::new();
    // `tool_choice` is honoured on every transport (SPEC §19.3). A malformed value is a 400
    // here, before a bridge spawn; the bridge parses it again when it restricts the catalog.
    openai::ToolChoice::parse(request.tool_choice.as_ref())?;
    if entry.transport == Transport::CliBridge {
        if request.temperature.is_some() {
            ignored.push("temperature");
        }
        if request.top_p.is_some() {
            ignored.push("top_p");
        }
        if request.stop.is_some() {
            ignored.push("stop");
        }
    }
    Ok(ignored)
}
