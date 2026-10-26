use std::collections::HashSet;

use chrono::Utc;

use crate::adapters::{BackendAdapter, ParsedLine};
use crate::types::{TurnEvent, UsageBreakdown};

pub struct JsonlAccumulator {
    pub text: String,
    pub usage: UsageBreakdown,
    pub cost: f64,
    pub last_events: Vec<serde_json::Value>,
    pub cli_session: Option<String>,
    emitted_tool_ids: HashSet<String>,
    /// The assistant messages seen so far, in order; `text` is these joined by
    /// `MESSAGE_SEPARATOR`. The last one is the message still being written.
    messages: Vec<String>,
}

/// Put between two assistant messages of one turn in `TurnResult.text`.
const MESSAGE_SEPARATOR: &str = "\n\n";

impl Default for JsonlAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonlAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            usage: UsageBreakdown::default(),
            cost: 0.0,
            last_events: Vec::new(),
            cli_session: None,
            emitted_tool_ids: HashSet::new(),
            messages: Vec::new(),
        }
    }

    /// Whether `incoming` only repeats what the turn has already said: the message being
    /// written, the whole text, or every message run together the way a CLI's closing
    /// `result` line reports them. Identical consecutive messages are dropped (SPEC §5).
    fn repeats_known_text(&self, incoming: &str) -> bool {
        if self.messages.last().is_some_and(|last| last == incoming) || incoming == self.text {
            return true;
        }
        let mut rest = incoming;
        for message in &self.messages {
            match rest.strip_prefix(message.as_str()) {
                Some(after) => rest = after.trim_start(),
                None => return false,
            }
        }
        rest.is_empty() && !self.messages.is_empty()
    }

    /// Cursor (and friends) close with a `result` that recaps the turn and then adds a
    /// closing remark. The recap is a repeat; the tail is new text.
    fn trailing_after_known<'a>(&self, incoming: &'a str) -> Option<&'a str> {
        if incoming.is_empty() || self.text.is_empty() {
            return None;
        }
        if let Some(tail) = incoming.strip_prefix(self.text.as_str()) {
            let tail = tail.trim_start_matches(['\r', '\n']);
            if !tail.is_empty() {
                return Some(tail);
            }
        }
        let mut rest = incoming;
        for message in &self.messages {
            {
                let after = rest.strip_prefix(message.as_str())?;
                rest = after.trim_start()
            }
        }
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    }

    /// A CLI line carries either a longer snapshot of the message being written or a new
    /// message. A turn that speaks, calls a tool and speaks again has several messages, and
    /// the result keeps all of them: the last one used to overwrite the rest, so the text a
    /// client got back was only the closing remark (C24). The deltas emitted here always
    /// concatenate to `text`.
    fn push_assistant_text<E: Fn(TurnEvent)>(&mut self, text: String, emit: &E) -> Vec<TurnEvent> {
        if text.is_empty() || self.repeats_known_text(&text) {
            return vec![];
        }
        let grown = self
            .messages
            .last()
            .and_then(|last| text.strip_prefix(last.as_str()))
            .filter(|tail| !tail.is_empty())
            .map(str::to_string);
        if let (Some(tail), Some(last)) = (grown, self.messages.last_mut()) {
            last.push_str(&tail);
            self.text.push_str(&tail);
            let ev = TurnEvent::AssistantDelta { text: tail };
            emit(ev.clone());
            return vec![ev];
        }
        if let Some(tail) = self.trailing_after_known(&text) {
            let tail = tail.to_string();
            let delta = format!("{MESSAGE_SEPARATOR}{tail}");
            self.messages.push(tail);
            self.text.push_str(&delta);
            let ev = TurnEvent::AssistantDelta { text: delta };
            emit(ev.clone());
            return vec![ev];
        }
        let delta = if self.messages.is_empty() {
            text.clone()
        } else {
            format!("{MESSAGE_SEPARATOR}{text}")
        };
        self.messages.push(text);
        self.text.push_str(&delta);
        let ev = TurnEvent::AssistantDelta { text: delta };
        emit(ev.clone());
        vec![ev]
    }

    pub fn process_line<E: Fn(TurnEvent)>(
        &mut self,
        adapter: &dyn BackendAdapter,
        line: &str,
        emit: &E,
    ) -> Vec<TurnEvent> {
        let parsed = adapter.parse_line(line);
        let mut emitted = Vec::new();
        for item in parsed {
            match item {
                ParsedLine::Raw(value) => {
                    self.last_events.push(value.clone());
                    if self.last_events.len() > 32 {
                        self.last_events.remove(0);
                    }
                    if let Some(session) = adapter.session_followed(&value) {
                        self.cli_session = Some(session);
                    }
                    // Assistant text is emitted via ParsedLine::AssistantDelta only.
                }
                ParsedLine::AssistantDelta(text) => {
                    emitted.extend(self.push_assistant_text(text, emit));
                }
                ParsedLine::ToolCall {
                    tool_call_id,
                    tool,
                    args,
                } => {
                    if !self.emitted_tool_ids.insert(tool_call_id.clone()) {
                        continue;
                    }
                    let ev = TurnEvent::ToolCall {
                        tool,
                        tool_call_id,
                        args,
                        label: None,
                        ok: None,
                        result_snippet: None,
                        duration_ms: None,
                        started_at: Utc::now().to_rfc3339(),
                        ended_at: None,
                    };
                    emitted.push(ev.clone());
                    emit(ev);
                }
                ParsedLine::Usage {
                    input,
                    output,
                    cache_read,
                    cache_write,
                    reasoning,
                    cost,
                } => {
                    self.usage.input += input;
                    self.usage.output += output;
                    self.usage.cache_read += cache_read;
                    self.usage.cache_write += cache_write;
                    self.usage.reasoning += reasoning;
                    self.cost += cost;
                    let ev = TurnEvent::Usage {
                        input,
                        output,
                        cache_read,
                        cache_write,
                        reasoning,
                        cost,
                        cost_components: serde_json::json!({}),
                    };
                    emitted.push(ev.clone());
                    emit(ev);
                }
                ParsedLine::SessionInit(value) => {
                    self.last_events.push(value.clone());
                    if let Some(session) = adapter.session_followed(&value) {
                        self.cli_session = Some(session);
                    }
                }
            }
        }
        emitted
    }
}
