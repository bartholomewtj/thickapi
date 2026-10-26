//! Anthropic Messages content-block state machine (SPEC §19.8.1 / §19.15).
//!
//! OAuth SSE and the Claude-bridge stream-json parsers stay different (event names,
//! `never_started`, init/result/isolation, drain-and-tap). Both feed
//! `content_block_start` / `content_block_delta` / `content_block_stop` here so
//! text / thinking / tool_use / unknown-skip is one implementation.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::thin::openai::ReasoningDetail;
use crate::thin::ThinEvent;

/// Open content block at one Anthropic `index`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text,
    Thinking {
        text: String,
        signature: String,
        redacted: bool,
    },
    ToolUse {
        id: String,
        /// Resolved name: the client tool, or the Claude name of a dialect row.
        name: String,
        /// The name on the wire, prefix included.
        wire_name: String,
        json: String,
    },
    /// Unknown content-block type (C8): occupy the index so later deltas do not 502.
    Ignored,
}

impl Block {
    fn is_open(&self) -> bool {
        !matches!(self, Block::Ignored)
    }

    /// `None` for [`Block::Ignored`], which is never open.
    fn kind(&self) -> Option<BlockKind> {
        match self {
            Block::Text => Some(BlockKind::Text),
            Block::Thinking { .. } => Some(BlockKind::Thinking),
            Block::ToolUse { .. } => Some(BlockKind::ToolUse),
            Block::Ignored => None,
        }
    }
}

/// What an open block is, for a caller deciding whether it is safe to close it (#145).
/// Redacted thinking is `Thinking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Text => "text",
            BlockKind::Thinking => "thinking",
            BlockKind::ToolUse => "tool_use",
        }
    }
}

/// How the outer parser treats isolation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Strictness {
    /// Messages API SSE: missing/duplicate/mismatch is silent; unknown blocks skip without 502;
    /// bad tool JSON becomes `{}`.
    Lenient,
    /// Bridge isolation: duplicate index, missing index, delta mismatch, unknown tool, empty
    /// tool id, empty redacted data, and invalid tool JSON are protocol errors. Tool calls are
    /// returned for the caller to buffer, not emitted as [`ThinEvent::ToolCall`].
    Strict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    DuplicateIndex(u64),
    UnknownTool(String),
    MissingToolId,
    EmptyRedacted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    UnknownIndex(u64),
    Mismatch { delta_type: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    UnknownIndex(u64),
    InvalidToolJson { name: String },
    NonObjectToolJson { name: String },
}

#[derive(Debug, Default)]
pub struct StartOutcome {
    pub events: Vec<ThinEvent>,
    pub skipped: bool,
    pub skipped_kind: Option<String>,
}

#[derive(Debug, Default)]
pub struct DeltaOutcome {
    pub events: Vec<ThinEvent>,
    pub skipped: bool,
}

#[derive(Debug, Default)]
pub struct StopOutcome {
    pub events: Vec<ThinEvent>,
    pub tool: Option<ClosedTool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedTool {
    pub id: String,
    pub name: String,
    /// The name on the wire, prefix included. Empty when the start had none.
    pub wire_name: String,
    pub arguments: String,
}

/// `index` → [`Block`], start/delta/stop → [`ThinEvent`]s.
#[derive(Debug, Default)]
pub struct BlockMachine {
    map: BTreeMap<u64, Block>,
}

impl BlockMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn has_open(&self) -> bool {
        self.map.values().any(Block::is_open)
    }

    /// The open blocks, by index, with their kind. Skipped (unknown) blocks are not open.
    pub fn open_indices(&self) -> Vec<(u64, BlockKind)> {
        self.map
            .iter()
            .filter_map(|(i, b)| b.kind().map(|k| (*i, k)))
            .collect()
    }

    pub fn insert(&mut self, index: u64, block: Block) {
        self.map.insert(index, block);
    }

    pub fn get(&self, index: u64) -> Option<&Block> {
        self.map.get(&index)
    }

    /// Open a content block. `resolve_tool` maps the wire tool name to the client name
    /// (`None` = unknown). Lenient callers always return `Some`.
    pub fn start(
        &mut self,
        index: u64,
        content_block: &Value,
        resolve_tool: impl FnOnce(&str) -> Option<String>,
        strictness: Strictness,
    ) -> Result<StartOutcome, StartError> {
        if strictness == Strictness::Strict && self.map.contains_key(&index) {
            return Err(StartError::DuplicateIndex(index));
        }
        let kind = content_block.get("type").and_then(Value::as_str);
        match kind {
            Some("text") => {
                let mut events = Vec::new();
                if let Some(t) = nonempty(content_block.get("text")) {
                    events.push(ThinEvent::TextDelta(t.to_string()));
                }
                self.map.insert(index, Block::Text);
                Ok(StartOutcome {
                    events,
                    ..StartOutcome::default()
                })
            }
            Some("thinking") => {
                let mut events = Vec::new();
                let text = nonempty(content_block.get("thinking"))
                    .unwrap_or_default()
                    .to_string();
                if !text.is_empty() {
                    events.push(thinking_delta(&text));
                }
                let signature = content_block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.map.insert(
                    index,
                    Block::Thinking {
                        text,
                        signature,
                        redacted: false,
                    },
                );
                Ok(StartOutcome {
                    events,
                    ..StartOutcome::default()
                })
            }
            Some("redacted_thinking") => {
                let data = content_block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if data.is_empty() && strictness == Strictness::Strict {
                    return Err(StartError::EmptyRedacted);
                }
                let mut events = Vec::new();
                if !data.is_empty() && strictness == Strictness::Lenient {
                    events.push(ThinEvent::ReasoningDelta {
                        text: String::new(),
                        signature: None,
                        details: Some(vec![ReasoningDetail::Encrypted {
                            data: data.to_string(),
                        }]),
                    });
                }
                self.map.insert(
                    index,
                    Block::Thinking {
                        text: String::new(),
                        signature: data.to_string(),
                        redacted: true,
                    },
                );
                Ok(StartOutcome {
                    events,
                    ..StartOutcome::default()
                })
            }
            Some("tool_use") => {
                let wire = content_block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name =
                    resolve_tool(wire).ok_or_else(|| StartError::UnknownTool(wire.into()))?;
                let id = content_block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() && strictness == Strictness::Strict {
                    return Err(StartError::MissingToolId);
                }
                self.map.insert(
                    index,
                    Block::ToolUse {
                        id: id.to_string(),
                        name,
                        wire_name: wire.to_string(),
                        json: initial_tool_json(content_block),
                    },
                );
                Ok(StartOutcome::default())
            }
            other => {
                let skipped_kind = Some(other.unwrap_or("<missing>").to_string());
                tracing::debug!(
                    kind = skipped_kind.as_deref().unwrap_or("<missing>"),
                    "skipped unknown Claude content block"
                );
                self.map.insert(index, Block::Ignored);
                Ok(StartOutcome {
                    skipped: true,
                    skipped_kind,
                    events: Vec::new(),
                })
            }
        }
    }

    /// Apply a `content_block_delta`. Unknown delta types are C8 skip (even when the index is
    /// missing) so a new vocabulary token does not 502. `grow` appends signature / tool JSON
    /// (OAuth size-caps via [`crate::thin::oauth::vendor::MapperCore::accumulate`]).
    pub fn delta(
        &mut self,
        index: u64,
        delta: &Value,
        strictness: Strictness,
        grow: &mut impl FnMut(&mut String, &str) -> bool,
    ) -> Result<DeltaOutcome, DeltaError> {
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if !matches!(
            delta_type,
            "text_delta" | "thinking_delta" | "signature_delta" | "input_json_delta"
        ) {
            return Ok(DeltaOutcome {
                skipped: true,
                events: Vec::new(),
            });
        }
        let Some(block) = self.map.get_mut(&index) else {
            return match strictness {
                Strictness::Lenient => Ok(DeltaOutcome::default()),
                Strictness::Strict => Err(DeltaError::UnknownIndex(index)),
            };
        };
        if matches!(block, Block::Ignored) {
            return Ok(DeltaOutcome::default());
        }
        let field = |key: &str| -> Result<&str, DeltaError> {
            match delta.get(key).and_then(Value::as_str) {
                Some(s) => Ok(s),
                None if strictness == Strictness::Strict => Err(DeltaError::Mismatch {
                    delta_type: delta_type.to_string(),
                }),
                None => Ok(""),
            }
        };
        match (delta_type, block) {
            ("text_delta", Block::Text) => {
                let text = field("text")?;
                let mut events = Vec::new();
                if !text.is_empty() {
                    events.push(ThinEvent::TextDelta(text.to_string()));
                }
                Ok(DeltaOutcome {
                    events,
                    skipped: false,
                })
            }
            ("thinking_delta", Block::Thinking { text, .. }) => {
                let piece = field("thinking")?;
                let mut events = Vec::new();
                if !piece.is_empty() {
                    text.push_str(piece);
                    events.push(thinking_delta(piece));
                }
                Ok(DeltaOutcome {
                    events,
                    skipped: false,
                })
            }
            ("signature_delta", Block::Thinking { signature, .. }) => {
                let piece = field("signature")?;
                grow(signature, piece);
                Ok(DeltaOutcome::default())
            }
            ("input_json_delta", Block::ToolUse { json, .. }) => {
                let piece = field("partial_json")?;
                grow(json, piece);
                Ok(DeltaOutcome::default())
            }
            _ => match strictness {
                Strictness::Lenient => Ok(DeltaOutcome::default()),
                Strictness::Strict => Err(DeltaError::Mismatch {
                    delta_type: delta_type.to_string(),
                }),
            },
        }
    }

    pub fn stop(&mut self, index: u64, strictness: Strictness) -> Result<StopOutcome, StopError> {
        let Some(block) = self.map.remove(&index) else {
            return match strictness {
                Strictness::Lenient => Ok(StopOutcome::default()),
                Strictness::Strict => Err(StopError::UnknownIndex(index)),
            };
        };
        match block {
            Block::Text | Block::Ignored => Ok(StopOutcome::default()),
            Block::Thinking {
                text,
                signature,
                redacted,
            } => {
                let mut events = Vec::new();
                match strictness {
                    Strictness::Lenient if !signature.is_empty() && !redacted => {
                        events.push(ThinEvent::ReasoningDelta {
                            text: String::new(),
                            signature: Some(signature.clone()),
                            details: Some(vec![ReasoningDetail::Text {
                                text,
                                signature: Some(signature),
                            }]),
                        });
                    }
                    Strictness::Strict if !signature.is_empty() => {
                        events.push(ThinEvent::ReasoningDelta {
                            text: String::new(),
                            signature: Some(signature),
                            details: None,
                        });
                    }
                    _ => {}
                }
                Ok(StopOutcome { events, tool: None })
            }
            Block::ToolUse {
                id,
                name,
                wire_name,
                json,
            } => {
                let arguments = tool_arguments(&json, &name, strictness)?;
                let mut events = Vec::new();
                if strictness == Strictness::Lenient {
                    events.push(ThinEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
                Ok(StopOutcome {
                    events,
                    tool: Some(ClosedTool {
                        id,
                        name,
                        wire_name,
                        arguments,
                    }),
                })
            }
        }
    }
}

fn nonempty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|t| !t.is_empty())
}

fn thinking_delta(text: &str) -> ThinEvent {
    ThinEvent::ReasoningDelta {
        text: text.to_string(),
        signature: None,
        details: None,
    }
}

fn initial_tool_json(content_block: &Value) -> String {
    content_block
        .get("input")
        .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        .map(|v| v.to_string())
        .unwrap_or_default()
}

fn tool_arguments(json: &str, name: &str, strictness: Strictness) -> Result<String, StopError> {
    if json.trim().is_empty() {
        return Ok("{}".to_string());
    }
    match serde_json::from_str::<Value>(json) {
        Ok(v) if v.is_object() => Ok(v.to_string()),
        Ok(_) if strictness == Strictness::Strict => Err(StopError::NonObjectToolJson {
            name: name.to_string(),
        }),
        Err(_) if strictness == Strictness::Strict => Err(StopError::InvalidToolJson {
            name: name.to_string(),
        }),
        _ => Ok("{}".to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grow(dst: &mut String, piece: &str) -> bool {
        dst.push_str(piece);
        true
    }

    fn resolve_ok(wire: &str) -> Option<String> {
        Some(wire.to_string())
    }

    #[test]
    fn text_start_delta_stop() {
        let mut m = BlockMachine::new();
        let out = m
            .start(
                0,
                &json!({"type": "text", "text": "Hi"}),
                resolve_ok,
                Strictness::Lenient,
            )
            .unwrap();
        assert_eq!(out.events.len(), 1);
        assert!(matches!(&out.events[0], ThinEvent::TextDelta(t) if t == "Hi"));
        let out = m
            .delta(
                0,
                &json!({"type": "text_delta", "text": " there"}),
                Strictness::Lenient,
                &mut grow,
            )
            .unwrap();
        assert!(matches!(&out.events[0], ThinEvent::TextDelta(t) if t == " there"));
        let out = m.stop(0, Strictness::Lenient).unwrap();
        assert!(out.events.is_empty());
        assert!(!m.has_open());
    }

    #[test]
    fn thinking_streams_then_signs_on_stop() {
        let mut m = BlockMachine::new();
        m.start(
            0,
            &json!({"type": "thinking", "thinking": "", "signature": ""}),
            resolve_ok,
            Strictness::Lenient,
        )
        .unwrap();
        let out = m
            .delta(
                0,
                &json!({"type": "thinking_delta", "thinking": "plan"}),
                Strictness::Lenient,
                &mut grow,
            )
            .unwrap();
        assert!(matches!(&out.events[0], ThinEvent::ReasoningDelta { text, .. } if text == "plan"));
        m.delta(
            0,
            &json!({"type": "signature_delta", "signature": "sig"}),
            Strictness::Lenient,
            &mut grow,
        )
        .unwrap();
        let out = m.stop(0, Strictness::Lenient).unwrap();
        match &out.events[0] {
            ThinEvent::ReasoningDelta {
                signature, details, ..
            } => {
                assert_eq!(signature.as_deref(), Some("sig"));
                assert!(details.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn thinking_start_emits_initial_text() {
        let mut m = BlockMachine::new();
        let out = m
            .start(
                0,
                &json!({"type": "thinking", "thinking": "already", "signature": ""}),
                resolve_ok,
                Strictness::Strict,
            )
            .unwrap();
        assert!(
            matches!(&out.events[0], ThinEvent::ReasoningDelta { text, .. } if text == "already")
        );
    }

    #[test]
    fn tool_use_lenient_emits_tool_call() {
        let mut m = BlockMachine::new();
        m.start(
            1,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}),
            resolve_ok,
            Strictness::Lenient,
        )
        .unwrap();
        m.delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": "{\"path\":\"a\"}"}),
            Strictness::Lenient,
            &mut grow,
        )
        .unwrap();
        let out = m.stop(1, Strictness::Lenient).unwrap();
        assert!(matches!(
            &out.events[0],
            ThinEvent::ToolCall { name, arguments, .. }
                if name == "read" && arguments.contains("path")
        ));
        assert_eq!(out.tool.as_ref().unwrap().id, "toolu_1");
    }

    #[test]
    fn tool_use_strict_buffers_without_emitting() {
        let mut m = BlockMachine::new();
        m.start(
            1,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a"}}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        let out = m.stop(1, Strictness::Strict).unwrap();
        assert!(out.events.is_empty());
        let tool = out.tool.unwrap();
        assert_eq!(tool.name, "read");
        assert!(tool.arguments.contains("path"));
    }

    #[test]
    fn unknown_block_is_skipped_not_an_error() {
        let mut m = BlockMachine::new();
        let out = m
            .start(
                9,
                &json!({"type": "search_result", "title": "x"}),
                resolve_ok,
                Strictness::Strict,
            )
            .unwrap();
        assert!(out.skipped);
        assert_eq!(out.skipped_kind.as_deref(), Some("search_result"));
        assert!(!m.has_open());
        let out = m
            .delta(
                9,
                &json!({"type": "text_delta", "text": "nope"}),
                Strictness::Strict,
                &mut grow,
            )
            .unwrap();
        assert!(out.events.is_empty());
        m.stop(9, Strictness::Strict).unwrap();
    }

    #[test]
    fn open_indices_names_each_open_block_and_its_kind() {
        let mut m = BlockMachine::new();
        assert!(m.open_indices().is_empty());
        m.start(
            0,
            &json!({"type": "thinking", "thinking": "", "signature": ""}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        m.start(
            1,
            &json!({"type": "text", "text": ""}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        m.start(
            2,
            &json!({"type": "redacted_thinking", "data": "enc"}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        m.start(
            3,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        m.start(
            4,
            &json!({"type": "search_result", "title": "x"}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        assert_eq!(
            m.open_indices(),
            vec![
                (0, BlockKind::Thinking),
                (1, BlockKind::Text),
                (2, BlockKind::Thinking),
                (3, BlockKind::ToolUse),
            ],
            "the skipped block at 4 is not open"
        );
        m.stop(1, Strictness::Strict).unwrap();
        m.stop(3, Strictness::Strict).unwrap();
        assert_eq!(
            m.open_indices(),
            vec![(0, BlockKind::Thinking), (2, BlockKind::Thinking)]
        );
        assert_eq!(BlockKind::ToolUse.as_str(), "tool_use");
    }

    #[test]
    fn unknown_delta_type_skips_without_needing_an_index() {
        let mut m = BlockMachine::new();
        let out = m
            .delta(
                3,
                &json!({"type": "citations_delta"}),
                Strictness::Strict,
                &mut grow,
            )
            .unwrap();
        assert!(out.skipped);
    }

    #[test]
    fn strict_unknown_tool_and_empty_id_are_errors() {
        let mut m = BlockMachine::new();
        let err = m
            .start(
                0,
                &json!({"type": "tool_use", "id": "t", "name": "Bash", "input": {}}),
                |_| None,
                Strictness::Strict,
            )
            .unwrap_err();
        assert_eq!(err, StartError::UnknownTool("Bash".into()));
        let err = m
            .start(
                0,
                &json!({"type": "tool_use", "id": "", "name": "read", "input": {}}),
                resolve_ok,
                Strictness::Strict,
            )
            .unwrap_err();
        assert_eq!(err, StartError::MissingToolId);
    }

    #[test]
    fn lenient_coerces_bad_tool_json_strict_does_not() {
        let mut m = BlockMachine::new();
        m.insert(
            0,
            Block::ToolUse {
                id: "t".into(),
                name: "read".into(),
                wire_name: "mcp__pi__read".into(),
                json: "not-json".into(),
            },
        );
        let out = m.stop(0, Strictness::Lenient).unwrap();
        assert_eq!(out.tool.unwrap().arguments, "{}");

        m.insert(
            1,
            Block::ToolUse {
                id: "t".into(),
                name: "read".into(),
                wire_name: "mcp__pi__read".into(),
                json: "not-json".into(),
            },
        );
        assert!(matches!(
            m.stop(1, Strictness::Strict),
            Err(StopError::InvalidToolJson { .. })
        ));
    }

    #[test]
    fn redacted_lenient_emits_on_start_strict_requires_data() {
        let mut m = BlockMachine::new();
        let out = m
            .start(
                0,
                &json!({"type": "redacted_thinking", "data": "enc"}),
                resolve_ok,
                Strictness::Lenient,
            )
            .unwrap();
        assert!(matches!(
            &out.events[0],
            ThinEvent::ReasoningDelta {
                details: Some(_),
                ..
            }
        ));
        let out = m.stop(0, Strictness::Lenient).unwrap();
        assert!(out.events.is_empty(), "redacted does not re-emit on stop");

        let err = m
            .start(
                1,
                &json!({"type": "redacted_thinking", "data": ""}),
                resolve_ok,
                Strictness::Strict,
            )
            .unwrap_err();
        assert_eq!(err, StartError::EmptyRedacted);
        m.start(
            1,
            &json!({"type": "redacted_thinking", "data": "enc"}),
            resolve_ok,
            Strictness::Strict,
        )
        .unwrap();
        let out = m.stop(1, Strictness::Strict).unwrap();
        assert!(matches!(
            &out.events[0],
            ThinEvent::ReasoningDelta { signature: Some(s), details: None, .. } if s == "enc"
        ));
    }
}
