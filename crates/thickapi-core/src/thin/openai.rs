//! OpenAI chat-completions wire types (SPEC §19.3–§19.4). Lenient on input, exact on output.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub tools: Vec<ToolDef>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningObject>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stop: Option<Value>,
    #[serde(default)]
    pub response_format: Option<Value>,
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub logprobs: Option<Value>,
    #[serde(default)]
    pub top_logprobs: Option<Value>,
    // Ignored without a header (SPEC §19.3).
    #[serde(default)]
    pub user: Option<Value>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub store: Option<Value>,
    #[serde(default)]
    pub seed: Option<Value>,
}

impl ChatRequest {
    /// `reasoning_effort` or `reasoning.effort`; `None` means the client did not ask.
    pub fn effort(&self) -> Option<&str> {
        self.reasoning_effort
            .as_deref()
            .or_else(|| self.reasoning.as_ref().and_then(|r| r.effort.as_deref()))
    }

    /// `max_tokens` and `max_completion_tokens` are aliases.
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    pub fn include_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .is_some_and(|o| o.include_usage)
    }

    pub fn parallel_tool_calls(&self) -> bool {
        self.parallel_tool_calls.unwrap_or(true)
    }
}

/// OpenAI `tool_choice` as the Claude bridge honours it (SPEC §19.3). Absent and `"auto"`
/// are the same. A named object names the **client** tool, not the dialect name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    /// `{type:"function", function:{name}}`. The name is the client's.
    Named(String),
}

impl ToolChoice {
    /// `None` (absent) is [`ToolChoice::Auto`]. A wrong type, an unknown string, or a named
    /// object without `function.name` is `400` with `param: tool_choice`.
    pub fn parse(value: Option<&Value>) -> Result<Self, crate::thin::ThinError> {
        let Some(value) = value else {
            return Ok(Self::Auto);
        };
        match value {
            Value::String(s) => match s.as_str() {
                "auto" => Ok(Self::Auto),
                "none" => Ok(Self::None),
                "required" => Ok(Self::Required),
                other => Err(crate::thin::ThinError::invalid_request(
                    format!("unsupported tool_choice: {other}"),
                    Some("tool_choice"),
                )),
            },
            Value::Object(o) => {
                let kind = o.get("type").and_then(Value::as_str);
                if kind.is_some_and(|k| k != "function") {
                    return Err(crate::thin::ThinError::invalid_request(
                        format!("unsupported tool_choice.type: {kind:?}"),
                        Some("tool_choice"),
                    ));
                }
                let name = o
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| {
                        crate::thin::ThinError::invalid_request(
                            "tool_choice.function.name required",
                            Some("tool_choice"),
                        )
                    })?;
                Ok(Self::Named(name.to_string()))
            }
            _ => Err(crate::thin::ThinError::invalid_request(
                "malformed tool_choice",
                Some("tool_choice"),
            )),
        }
    }

    /// `required`, or a named function: the turn must end in a tool call.
    pub fn forces_tool(&self) -> bool {
        matches!(self, Self::Required | Self::Named(_))
    }

    /// The client tool a named choice demands, if any.
    pub fn named(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name),
            _ => None,
        }
    }
}

/// One reasoning block Pi can replay (SPEC §19.5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum ReasoningDetail {
    #[serde(rename = "reasoning.text")]
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "reasoning.encrypted")]
    Encrypted { data: String },
}

impl ReasoningDetail {
    pub fn has_signature(&self) -> bool {
        match self {
            Self::Text { signature, .. } => signature.as_ref().is_some_and(|s| !s.is_empty()),
            Self::Encrypted { data } => !data.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReasoningObject {
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Content>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallOut>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// Pi replays `reasoning_content` from history; Claude thinking blocks carry a signature.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning_signature: Option<String>,
    /// Pi persists signatures from `reasoning_details` (SPEC §19.5).
    #[serde(default)]
    pub reasoning_details: Option<Vec<ReasoningDetail>>,
    /// Set only by the dialect retry. OpenAI tool messages carry no error flag, so a client
    /// cannot set it (`serde(skip)`) and its messages leave it false and the transcript stays `isError: false`.
    #[serde(skip)]
    pub tool_is_error: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl ChatMessage {
    pub fn has_recoverable_thinking_signature(&self) -> bool {
        if let Some(details) = &self.reasoning_details {
            if details.iter().any(ReasoningDetail::has_signature) {
                return true;
            }
        }
        self.reasoning_signature
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }
}

impl Content {
    /// Concatenated text of every text part.
    pub fn text(&self) -> String {
        match self {
            Content::Text(t) => t.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    pub fn has_image(&self) -> bool {
        matches!(self, Content::Parts(parts) if parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. })))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    /// Anything else is preserved so validation can name it.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// Assistant `tool_calls[]` in both requests (history) and responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallOut {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments string, as OpenAI does it.
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PromptTokensDetails {
    pub cached_tokens: u64,
}

impl Usage {
    pub fn from_parts(input: u64, output: u64, cached: u64) -> Self {
        let prompt = input + cached;
        Self {
            prompt_tokens: prompt,
            completion_tokens: output,
            total_tokens: prompt + output,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: cached,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

// ---- streaming chunks ----

#[derive(Debug, Clone, Serialize)]
pub struct ChatChunk {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: Delta,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<ReasoningDetail>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallOut>>,
}

// ---- non-stream response ----

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionChoice {
    pub index: u32,
    pub message: CompletionMessage,
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompletionMessage {
    pub role: &'static str,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<ReasoningDetail>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallOut>>,
}

/// `call_<24 alnum>` (SPEC §19.4).
/// `true` for ids minted by [`new_call_id`] (`call_` + 24 alphanumerics).
pub fn is_route_call_id(id: &str) -> bool {
    id.len() == 29
        && id.starts_with("call_")
        && id
            .get(5..)
            .is_some_and(|t| t.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The id the client sees: an adapter-minted route-shaped id is kept (it may key adapter state,
/// e.g. Gemini thought signatures); any vendor id is replaced.
pub fn route_call_id(adapter_id: String) -> String {
    if is_route_call_id(&adapter_id) {
        adapter_id
    } else {
        new_call_id()
    }
}

pub fn new_call_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    use rand::Rng;
    let mut rng = rand::thread_rng();
    // SPEC §19.4: `call_tw…` is reserved for TodoWrite rounds. A random mint must not land there.
    loop {
        let tail: String = (0..24)
            .map(|_| {
                let i = rng.gen_range(0..ALPHABET.len());
                ALPHABET.get(i).copied().unwrap_or(b'a') as char
            })
            .collect();
        if !tail.starts_with("tw") {
            return format!("call_{tail}");
        }
    }
}

pub fn new_completion_id() -> String {
    format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_and_parts_content() {
        let req: ChatRequest = serde_json::from_str(
            r#"{"model":"subs/claude/sonnet","messages":[
                {"role":"system","content":"sys"},
                {"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_x","type":"function","function":{"name":"read","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"call_x","content":"ok"}
            ],"reasoning":{"effort":"high"},"max_completion_tokens":100}"#,
        )
        .expect("parse");
        assert_eq!(req.messages.len(), 4);
        assert!(req.messages[1].content.as_ref().unwrap().has_image());
        assert_eq!(req.messages[1].content.as_ref().unwrap().text(), "hi");
        assert!(req.messages[2].content.is_none());
        assert_eq!(req.effort(), Some("high"));
        assert_eq!(req.max_output_tokens(), Some(100));
        assert!(req.parallel_tool_calls());
    }

    #[test]
    fn call_id_shape() {
        let id = new_call_id();
        assert!(id.starts_with("call_"));
        assert_eq!(id.len(), 5 + 24);
        assert!(id[5..].chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn ordinary_call_ids_never_use_the_todowrite_prefix() {
        for _ in 0..64 {
            let id = new_call_id();
            assert!(is_route_call_id(&id), "{id}");
            assert!(!id.starts_with("call_tw"), "{id}");
        }
    }

    #[test]
    fn usage_from_parts_adds_cached_to_prompt() {
        let u = Usage::from_parts(10, 5, 90);
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.total_tokens, 105);
        assert_eq!(u.prompt_tokens_details.unwrap().cached_tokens, 90);
    }
}
