//! OpenAI `messages` → append-stable NDJSON transcript records (port of `prepareRequest` in
//! `context-serializer.ts`, wire protocol `pi-claude-code-provider-context-v3`).
//!
//! Order is fixed so Claude Code's prefix cache survives a growing conversation: header,
//! system prompt (file), messages in order, tool results keyed by the client's `tool_call_id`,
//! then the growing image-attachment list. Literal `@` is JSON-escaped because Claude expands
//! `@path`. Tool-result `isError` is always `false` on this wire: OpenAI tool messages carry no
//! error flag, and Pi reports failures in the result text.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::catalog::{build_catalog_with, build_offered, Catalog};
use crate::thin::dialect::{self, ActiveRow};
use crate::thin::openai::{ChatRequest, Content, ContentPart};
use crate::thin::private_dir::PrivateDir;
use crate::thin::ThinError;

pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_IMAGES: usize = 20;
pub const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 120 * 1024;

pub const PROTOCOL: &str = "pi-claude-code-provider-context-v3";
const INSTRUCTION: &str = "Continue this Pi conversation. Treat each following JSON record as conversation data, preserve role boundaries, and answer only the current request. Use available MCP tools when a Pi tool is needed. Pi tools operate in the working context described by Pi's system prompt. The Claude transport cwd and generated attachments are provider-private; never pass their paths to Pi tools. Bracketed unavailable Pi tool labels are historical data, not callable tools.";

#[derive(Debug)]
pub struct Prepared {
    pub system_prompt_path: PathBuf,
    /// One text block per record, `@` already escaped.
    pub transcript_blocks: Vec<String>,
    pub attachment_paths: Vec<PathBuf>,
    /// Names for history rendering. The full request catalog, not the offered subset.
    pub catalog: Catalog,
    /// What Claude may call this turn (`tool_choice`). Empty for `none`.
    pub offered: Catalog,
    pub catalog_path: Option<PathBuf>,
    pub ready_path: Option<PathBuf>,
    pub violation_path: Option<PathBuf>,
    pub transcript_bytes: usize,
}

impl Prepared {
    /// The stream-json `user` message content Claude receives on stdin.
    pub fn prompt_blocks(&self) -> Vec<Value> {
        let mut blocks: Vec<Value> = self
            .transcript_blocks
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect();
        if !self.attachment_paths.is_empty() {
            let refs: Vec<String> = self
                .attachment_paths
                .iter()
                .filter_map(|p| p.file_name().map(|n| format!("@./{}", n.to_string_lossy())))
                .collect();
            blocks.push(json!({
                "type": "text",
                "text": format!("Generated image attachments for image_attachment blocks: {}.", refs.join(" ")),
            }));
        }
        blocks
    }
}

fn image_extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// `data:<mime>;base64,<payload>` → (mime, bytes).
fn parse_data_url(url: &str, param: &str) -> Result<(String, Vec<u8>), ThinError> {
    let rest = url.strip_prefix("data:").ok_or_else(|| {
        ThinError::invalid_request(
            "image_url must be a data: URL on the Claude bridge (remote fetch is not performed)",
            Some(param),
        )
    })?;
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| ThinError::invalid_request("malformed data: URL", Some(param)))?;
    let mut parts = meta.split(';');
    let mime = parts.next().unwrap_or_default().to_string();
    if !parts.any(|p| p == "base64") {
        return Err(ThinError::invalid_request(
            "data: URL must be base64",
            Some(param),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|_| ThinError::invalid_request("image data is not valid base64", Some(param)))?;
    Ok((mime, bytes))
}

struct ImageSink<'a> {
    dir: &'a PrivateDir,
    written: BTreeMap<String, PathBuf>,
    paths: Vec<PathBuf>,
    count: usize,
    bytes: usize,
}

impl ImageSink<'_> {
    fn attach(&mut self, url: &str, param: &str) -> Result<Value, ThinError> {
        self.count += 1;
        if self.count > MAX_IMAGES {
            return Err(ThinError::invalid_request(
                format!("at most {MAX_IMAGES} images are supported"),
                Some(param),
            ));
        }
        let (mime, bytes) = parse_data_url(url, param)?;
        let ext = image_extension(&mime).ok_or_else(|| {
            ThinError::invalid_request(format!("unsupported image type: {mime}"), Some(param))
        })?;
        if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
            return Err(ThinError::invalid_request(
                format!("image size must be between 1 byte and {MAX_IMAGE_BYTES} bytes"),
                Some(param),
            ));
        }
        let digest = hex::encode(Sha256::digest(&bytes));
        let name = format!("image-{digest}.{ext}");
        if !self.written.contains_key(&name) {
            self.bytes += bytes.len();
            if self.bytes > MAX_TOTAL_IMAGE_BYTES {
                return Err(ThinError::invalid_request(
                    format!("aggregate image size exceeds {MAX_TOTAL_IMAGE_BYTES} bytes"),
                    Some(param),
                ));
            }
            let path = self
                .dir
                .write_file(&name, &bytes)
                .map_err(|e| ThinError::server(format!("write image attachment: {e}")))?;
            self.written.insert(name.clone(), path.clone());
            self.paths.push(path);
        }
        Ok(json!({"type": "image_attachment", "attachment": name, "mimeType": mime}))
    }
}

fn content_blocks(
    content: &Content,
    images: Option<&mut ImageSink<'_>>,
    param: &str,
) -> Result<Vec<Value>, ThinError> {
    match content {
        Content::Text(text) => Ok(vec![json!({"type": "text", "text": text})]),
        Content::Parts(parts) => {
            let mut out = Vec::with_capacity(parts.len());
            let mut images = images;
            for (i, part) in parts.iter().enumerate() {
                match part {
                    ContentPart::Text { text } => out.push(json!({"type": "text", "text": text})),
                    ContentPart::ImageUrl { image_url } => {
                        let sink = images.as_deref_mut().ok_or_else(|| {
                            ThinError::invalid_request(
                                "image_url parts are only accepted in user and tool messages",
                                Some(&format!("{param}[{i}]")),
                            )
                        })?;
                        out.push(sink.attach(&image_url.url, &format!("{param}[{i}]"))?);
                    }
                    ContentPart::Unknown => {
                        return Err(ThinError::invalid_request(
                            "unsupported content part type",
                            Some(&format!("{param}[{i}].type")),
                        ))
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Serialise the request into the private directory. Fails closed on any shape problem.
pub fn prepare(
    req: &ChatRequest,
    dir: &PrivateDir,
    active: &[ActiveRow],
) -> Result<Prepared, ThinError> {
    let choice = crate::thin::openai::ToolChoice::parse(req.tool_choice.as_ref())?;
    // Offered tools are what Claude may call. History keeps the full catalog's names, so a
    // `none` or named choice does not turn an earlier call into an unavailable label.
    let offered = build_offered(&req.tools, active, &choice)?;
    let catalog = build_catalog_with(&req.tools, active)?;

    // System prompt: every system/developer message, in order, blank-line joined.
    let mut system = String::new();
    for msg in &req.messages {
        if msg.role == "system" || msg.role == "developer" {
            if let Some(content) = &msg.content {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&content.text());
            }
        }
    }
    let note = dialect::prompt_note_with(active, !req.parallel_tool_calls());
    if !note.is_empty() {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(&note);
    }
    if system.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(ThinError::invalid_request(
            format!(
                "system prompt is {} bytes; the Claude bridge limit is {MAX_SYSTEM_PROMPT_BYTES}",
                system.len()
            ),
            Some("messages"),
        ));
    }
    let system_prompt_path = dir
        .write_file("system-prompt.txt", system.as_bytes())
        .map_err(|e| ThinError::server(format!("write system prompt: {e}")))?;

    let mut images = ImageSink {
        dir,
        written: BTreeMap::new(),
        paths: Vec::new(),
        count: 0,
        bytes: 0,
    };
    // tool_call_id → transport name, from the assistant message that issued it.
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut messages: Vec<Value> = Vec::new();
    // TodoWrite rounds fold into one toolCall. A round spans an assistant message and the
    // tool results that follow it, so the walk keeps the open round until those arrive.
    let mut rounds = TodoRounds::new(catalog.active.iter().any(|r| r.name == "TodoWrite"));

    for (i, msg) in req.messages.iter().enumerate() {
        let param = format!("messages[{i}]");
        match msg.role.as_str() {
            "system" | "developer" => {}
            "user" => {
                let content = msg.content.as_ref().ok_or_else(|| {
                    ThinError::invalid_request(
                        "user message needs content",
                        Some(&format!("{param}.content")),
                    )
                })?;
                let blocks = match content {
                    Content::Text(text) => Value::String(text.clone()),
                    parts => Value::Array(content_blocks(
                        parts,
                        Some(&mut images),
                        &format!("{param}.content"),
                    )?),
                };
                messages.push(json!({"role": "user", "content": blocks}));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(reasoning) = &msg.reasoning_content {
                    if !reasoning.is_empty() {
                        blocks.push(json!({"type": "thinking", "thinking": reasoning}));
                    }
                }
                if let Some(content) = &msg.content {
                    for block in content_blocks(content, None, &format!("{param}.content"))? {
                        if block.get("type").and_then(Value::as_str) == Some("text")
                            && block
                                .get("text")
                                .and_then(Value::as_str)
                                .is_some_and(str::is_empty)
                        {
                            continue;
                        }
                        blocks.push(block);
                    }
                }
                for (j, call) in msg.tool_calls.iter().flatten().enumerate() {
                    if call.id.trim().is_empty() {
                        return Err(ThinError::invalid_request(
                            "tool_calls[].id must be a nonempty string",
                            Some(&format!("{param}.tool_calls[{j}].id")),
                        ));
                    }
                    let arguments: Value = if call.function.arguments.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&call.function.arguments).map_err(|_| {
                            ThinError::invalid_request(
                                "tool_calls[].function.arguments must be a JSON object string",
                                Some(&format!("{param}.tool_calls[{j}].function.arguments")),
                            )
                        })?
                    };
                    if !arguments.is_object() {
                        return Err(ThinError::invalid_request(
                            "tool_calls[].function.arguments must encode a JSON object",
                            Some(&format!("{param}.tool_calls[{j}].function.arguments")),
                        ));
                    }
                    // A round's own calls are not rendered. The fold emits one TodoWrite.
                    if rounds.absorb_call(&call.id, &call.function.name, &arguments) {
                        call_names.insert(call.id.clone(), rounds.transport_name());
                        continue;
                    }
                    let (name, arguments) =
                        render_history_call(&catalog, &call.function.name, &arguments);
                    call_names.insert(call.id.clone(), name.clone());
                    blocks.push(json!({
                        "type": "toolCall",
                        "id": call.id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
                if let Some(folded) = rounds.take_assistant() {
                    blocks.insert(0, folded);
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let id = msg
                    .tool_call_id
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        ThinError::invalid_request(
                            "tool message needs tool_call_id",
                            Some(&format!("{param}.tool_call_id")),
                        )
                    })?;
                let name = call_names
                    .get(id)
                    .cloned()
                    .or_else(|| {
                        msg.name
                            .as_deref()
                            .map(|n| render_history_name(&catalog, n))
                    })
                    .ok_or_else(|| {
                        ThinError::invalid_request(
                            format!("tool message {id} has no preceding assistant tool_call"),
                            Some(&format!("{param}.tool_call_id")),
                        )
                    })?;
                let content = match &msg.content {
                    Some(content) => {
                        content_blocks(content, Some(&mut images), &format!("{param}.content"))?
                    }
                    None => vec![json!({"type": "text", "text": ""})],
                };
                // A round's results collapse into one TodoWrite result, emitted with the last
                // one. An error in either round becomes that result, prefixed `Error:`.
                if rounds.absorb_result(id, &content, msg.tool_is_error) {
                    if let Some(mut folded) = rounds.take_result() {
                        // No round 2: the toolCall was held back. It goes out as its own
                        // assistant record, then the result. Round 2 emits the toolCall itself.
                        if let Some(call) =
                            folded.as_object_mut().and_then(|o| o.remove("__toolCall"))
                        {
                            messages.push(json!({"role": "assistant", "content": [call]}));
                        }
                        messages.push(folded);
                    }
                    continue;
                }
                messages.push(json!({
                    "role": "toolResult",
                    "toolCallId": id,
                    "toolName": name,
                    "content": content,
                    "isError": msg.tool_is_error,
                }));
            }
            other => {
                return Err(ThinError::invalid_request(
                    format!("unsupported message role: {other}"),
                    Some(&format!("{param}.role")),
                ))
            }
        }
    }

    let (catalog_path, ready_path, violation_path) = if offered.is_empty() {
        (None, None, None)
    } else {
        let path = dir
            .write_file("tools.json", offered.json_text().as_bytes())
            .map_err(|e| ThinError::server(format!("write tool catalog: {e}")))?;
        (
            Some(path),
            Some(dir.path().join("mcp-ready")),
            Some(dir.path().join("mcp-execution-attempt")),
        )
    };

    let header = json!({
        "protocol": PROTOCOL,
        "instruction": INSTRUCTION,
        "toolNameMap": offered.public_map,
    });
    let mut records = vec![serde_json::to_string(&header).unwrap_or_default()];
    records.extend(
        messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default()),
    );
    let transcript_blocks: Vec<String> =
        records.iter().map(|r| r.replace('@', "\\u0040")).collect();
    let transcript_bytes = transcript_blocks
        .iter()
        .map(|b| b.len() + 1)
        .sum::<usize>()
        .saturating_sub(1);
    if transcript_bytes > MAX_TRANSCRIPT_BYTES {
        return Err(ThinError::invalid_request(
            format!("serialized conversation exceeds {MAX_TRANSCRIPT_BYTES} bytes"),
            Some("messages"),
        ));
    }

    Ok(Prepared {
        system_prompt_path,
        transcript_blocks,
        attachment_paths: images.paths,
        catalog,
        offered,
        catalog_path,
        ready_path,
        violation_path,
        transcript_bytes,
    })
}

/// One TodoWrite as Claude should see it: the round-1 calls (and round 2, once it has
/// run) collapsed into a single `toolCall` and a single `toolResult`.
struct TodoRounds {
    on: bool,
    open: Option<OpenRound>,
}

struct OpenRound {
    /// `call_tw` base. The folded id is this base's round-1 index 0, which is always the list.
    base: String,
    id: String,
    created: Vec<dialect::TodoCreated>,
    /// Ids the round-1 `list` result named. Empty until that result arrives, and when it said
    /// `No tasks`.
    listed: Vec<u64>,
    /// The list result has arrived, so `listed` is complete rather than not-yet-read.
    list_seen: bool,
    /// Ids still waiting for a tool result, in the order the assistant message named them.
    pending: Vec<String>,
    /// Set by the first error result. Prefixed `Error:` when the text does not already say so.
    error: Option<String>,
    /// Round 2's calls were in this assistant message, so the toolCall emits with it.
    saw_round2: bool,
}

impl TodoRounds {
    fn new(on: bool) -> Self {
        Self { on, open: None }
    }

    fn transport_name(&self) -> String {
        format!("{}TodoWrite", super::catalog::TRANSPORT_PREFIX)
    }

    /// `true` when this call belongs to a TodoWrite round and must not be rendered itself.
    fn absorb_call(&mut self, id: &str, name: &str, arguments: &Value) -> bool {
        if !self.on || name != "todo" || !dialect::is_todo_call_id(id) {
            return false;
        }
        let Some(base) = dialect::todo_call_base(id).map(str::to_string) else {
            return false;
        };
        let round = dialect::todo_call_round(id).unwrap_or(0);
        if self.open.as_ref().is_some_and(|open| open.base != base) {
            // A new base starts a new round. The previous one had no results yet, so it stays
            // unrendered: a round without its results is not a TodoWrite Claude can answer.
            self.open = None;
        }
        let open = self.open.get_or_insert_with(|| OpenRound {
            id: dialect::todo_call_id(&base, dialect::TODO_ROUND_LIST, 0),
            base: base.clone(),
            created: Vec::new(),
            listed: Vec::new(),
            list_seen: false,
            pending: Vec::new(),
            error: None,
            saw_round2: false,
        });
        open.pending.push(id.to_string());
        if let Some(items) = dialect::todo_created_items(arguments) {
            open.created = items;
        }
        if round == dialect::TODO_ROUND_APPLY {
            open.saw_round2 = true;
        }
        true
    }

    /// The folded toolCall. Round 1 holds it: the results decide whether round 2 follows, and
    /// the toolCall is emitted with round 2's assistant message (or, when there is no round 2,
    /// just before the folded result). Round 2's own message emits it, since its calls are the
    /// ones being swallowed now.
    fn take_assistant(&mut self) -> Option<Value> {
        let open = self.open.as_mut()?;
        if open.saw_round2 {
            return Some(self.tool_call());
        }
        None
    }

    fn tool_call(&self) -> Value {
        let open = self.open.as_ref();
        let arguments = open
            .map(|open| dialect::todo_claude_args(&open.created))
            .unwrap_or_else(|| json!({"todos": []}));
        let id = open.map(|open| open.id.clone()).unwrap_or_default();
        json!({
            "type": "toolCall",
            "id": id,
            "name": self.transport_name(),
            "arguments": arguments,
        })
    }

    /// `true` when this result belongs to the open round.
    fn absorb_result(&mut self, id: &str, content: &[Value], flagged: bool) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if !open.pending.iter().any(|pending| pending == id) {
            return false;
        }
        open.pending.retain(|pending| pending != id);
        let text = content_text(content);
        if open.error.is_none() && dialect::todo_result_is_error(&text, flagged) {
            open.error = Some(error_text(&text));
        }
        if dialect::todo_call_round(id) == Some(dialect::TODO_ROUND_LIST)
            && dialect::todo_call_index(id) == Some(0)
        {
            open.listed = dialect::todo_list_ids(&text);
            open.list_seen = true;
        }
        if dialect::todo_call_round(id) == Some(dialect::TODO_ROUND_LIST)
            && dialect::todo_call_index(id) == Some(1)
        {
            let ids = dialect::todo_created_ids(&text);
            for (item, created) in open.created.iter_mut().zip(ids) {
                item.id = Some(created);
            }
        }
        true
    }

    /// The folded toolResult, once every call of the open round has a result and round 2 will
    /// not follow. A round-1 result set whose round 2 would be empty folds here. When round 2
    /// is still to come, nothing is emitted and the round stays open for the next message.
    fn take_result(&mut self) -> Option<Value> {
        let open = self.open.as_ref()?;
        if !open.pending.is_empty() {
            return None;
        }
        if !open.saw_round2 && round2_would_run(open) {
            return None;
        }
        let mut result = self.tool_result();
        // No round 2: the toolCall was held. Emit it as its own assistant record first by
        // returning only the result here; the caller inserts the toolCall alongside.
        if !open.saw_round2 {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("__toolCall".into(), self.tool_call());
            }
        }
        self.open = None;
        Some(result)
    }

    fn tool_result(&self) -> Value {
        let open = self.open.as_ref();
        let text = open
            .and_then(|open| open.error.clone())
            .unwrap_or_else(|| dialect::TODO_SUCCESS.to_string());
        let id = open.map(|open| open.id.clone()).unwrap_or_default();
        json!({
            "role": "toolResult",
            "toolCallId": id,
            "toolName": self.transport_name(),
            "content": [{"type": "text", "text": text}],
            "isError": false,
        })
    }
}

fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn error_text(text: &str) -> String {
    if text.starts_with("Error:") {
        text.to_string()
    } else {
        format!("Error: {text}")
    }
}

/// Round 2 runs when the list names an id the create did not, or a created item is not
/// pending. An error, or a list result that never arrived, does not start a second round.
fn round2_would_run(open: &OpenRound) -> bool {
    if open.error.is_some() || !open.list_seen {
        return false;
    }
    let created_ids: Vec<u64> = open.created.iter().filter_map(|item| item.id).collect();
    let stale = open.listed.iter().any(|id| !created_ids.contains(id));
    let update = open
        .created
        .iter()
        .any(|item| item.status != "pending" && item.id.is_some());
    stale || update
}

/// A Pi-shaped history call as Claude should see it. Dialect off is exactly the previous
/// rendering: [`Catalog::historical_name`]. Dialect on uses the Claude form when the call has
/// one, and otherwise the Pi transport name, which spike 1 showed Claude accepts even when it
/// is absent from the catalog. The opaque digest stays only for an unsafe name.
fn render_history_call(catalog: &Catalog, pi_name: &str, arguments: &Value) -> (String, Value) {
    if catalog.active.is_empty() {
        return (catalog.historical_name(pi_name), arguments.clone());
    }
    // A retry appends every call already in transport form (`mcp__pi__Edit`, and a sibling
    // `mcp__pi__Read`). Those names are not Pi tool names, so they must not be prefixed again.
    if pi_name.starts_with(super::catalog::TRANSPORT_PREFIX) {
        return (pi_name.to_string(), arguments.clone());
    }
    if let Some(row) = dialect::row_for_pi(&catalog.active, pi_name) {
        if let Some(claude) = row.to_claude(arguments) {
            return (
                format!("{}{}", super::catalog::TRANSPORT_PREFIX, row.name),
                claude,
            );
        }
    }
    (render_history_name(catalog, pi_name), arguments.clone())
}

fn render_history_name(catalog: &Catalog, pi_name: &str) -> String {
    if catalog.active.is_empty() {
        return catalog.historical_name(pi_name);
    }
    if super::catalog::is_safe_name(pi_name) {
        format!("{}{pi_name}", super::catalog::TRANSPORT_PREFIX)
    } else {
        catalog.historical_name(pi_name)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn request(body: Value) -> ChatRequest {
        serde_json::from_value(body).expect("request")
    }

    fn setup() -> (tempfile::TempDir, PrivateDir) {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = PrivateDir::create(root.path(), "t").expect("private dir");
        (root, dir)
    }

    #[test]
    fn header_then_messages_with_tool_names_mapped() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "subs/claude/sonnet",
            "messages": [
                {"role": "system", "content": "You are Pi."},
                {"role": "user", "content": "read a@b.txt"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a@b.txt\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "hello"},
                {"role": "user", "content": [{"type": "text", "text": "thanks"}]}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        assert_eq!(
            std::fs::read_to_string(&prepared.system_prompt_path).unwrap(),
            "You are Pi."
        );
        assert_eq!(prepared.transcript_blocks.len(), 5);
        let header: Value = serde_json::from_str(&prepared.transcript_blocks[0]).unwrap();
        assert_eq!(header["protocol"], PROTOCOL);
        assert_eq!(header["toolNameMap"][0]["transportName"], "mcp__pi__read");
        assert!(
            !prepared.transcript_blocks.iter().any(|b| b.contains('@')),
            "@ must be escaped"
        );
        let call: Value =
            serde_json::from_str(&prepared.transcript_blocks[2].replace("\\u0040", "@")).unwrap();
        assert_eq!(call["content"][0]["name"], "mcp__pi__read");
        assert_eq!(call["content"][0]["arguments"]["path"], "a@b.txt");
        let result: Value = serde_json::from_str(&prepared.transcript_blocks[3]).unwrap();
        assert_eq!(result["role"], "toolResult");
        assert_eq!(result["toolName"], "mcp__pi__read");
        assert_eq!(result["isError"], false);
        assert!(prepared.catalog_path.is_some());
        assert!(prepared.ready_path.as_ref().unwrap().ends_with("mcp-ready"));
        assert_eq!(prepared.prompt_blocks().len(), 5);
    }

    #[test]
    fn no_tools_means_no_catalog_files() {
        let (_root, dir) = setup();
        let req = request(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        assert!(prepared.catalog_path.is_none());
        assert!(prepared.ready_path.is_none());
        assert!(!dir.path().join("tools.json").exists());
    }

    #[test]
    fn dialect_off_is_the_pass_through_transcript() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "You are Pi."},
                {"role": "assistant", "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "edit", "arguments": "{\"path\":\"a.rs\",\"edits\":[{\"oldText\":\"x\",\"newText\":\"y\"}]}"}}
                ]}
            ],
            "tools": [{"type": "function", "function": {
                "name": "edit",
                "description": "edit",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "edits": {"type": "array", "items": {"type": "object", "properties": {"oldText": {"type": "string"}, "newText": {"type": "string"}}}}}}
            }}]
        }));
        let off = prepare(&req, &dir, &[]).expect("off");
        let prompt = std::fs::read_to_string(&off.system_prompt_path).unwrap();
        assert_eq!(prompt, "You are Pi.");
        assert!(matches!(
            off.catalog.transport_target.get("mcp__pi__edit"),
            Some(crate::thin::dialect::CatalogTarget::Client(name)) if name == "edit"
        ));
        let call: Value = serde_json::from_str(&off.transcript_blocks[1]).unwrap();
        assert_eq!(call["content"][0]["name"], "mcp__pi__edit");
        assert_eq!(call["content"][0]["arguments"]["path"], "a.rs");
        let on = dialect::activate(&req.tools);
        assert_eq!(on.len(), 1);
        assert_eq!(on[0].name, "Edit");
    }

    #[test]
    fn historical_tool_not_in_catalog_is_bracketed() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "old_tool", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c", "content": "x"}
            ]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        let call: Value = serde_json::from_str(&prepared.transcript_blocks[1]).unwrap();
        assert!(call["content"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("[unavailable Pi tool "));
    }

    #[test]
    fn a_retry_call_keeps_its_transport_name_and_claude_args() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "edit it"}],
            "tools": [{"type": "function", "function": {
                "name": "edit",
                "parameters": {"type": "object", "properties": {
                    "path": {"type": "string"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {
                        "oldText": {"type": "string"},
                        "newText": {"type": "string"}
                    }}}
                }}
            }}]
        }));
        let active = dialect::activate(&req.tools);
        let thin = crate::thin::ThinRequest {
            request: req,
            entry: crate::config::models::ModelEntry::bridge_probe("m"),
            effort: None,
            request_id: "retry".into(),
            path: "bridge",
            fallback: None,
            cursor_cwd: None,
        };
        let err = super::super::DialectArgError {
            tool: "Edit".into(),
            reason: "missing required file_path".into(),
            calls: vec![
                crate::thin::claude_bridge::DialectRetryCall {
                    id: "toolu_bad".into(),
                    name: "mcp__pi__Edit".into(),
                    arguments: "{\"path\":\"a.rs\"}".into(),
                    bad: true,
                },
                crate::thin::claude_bridge::DialectRetryCall {
                    id: "toolu_ok".into(),
                    name: "mcp__pi__Read".into(),
                    arguments: "{\"file_path\":\"a.rs\"}".into(),
                    bad: false,
                },
            ],
            text: "Trying.".into(),
        };
        let retry = super::super::with_dialect_error(&thin, &err);
        let prepared = prepare(&retry.request, &dir, &active).expect("prepare");
        let call: Value = serde_json::from_str(&prepared.transcript_blocks[2]).unwrap();
        let blocks = call["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["name"], "mcp__pi__Edit");
        assert_eq!(blocks[1]["arguments"]["path"], "a.rs");
        assert_eq!(blocks[2]["name"], "mcp__pi__Read");
        assert_eq!(blocks[2]["arguments"]["file_path"], "a.rs");
        let bad: Value = serde_json::from_str(&prepared.transcript_blocks[3]).unwrap();
        assert_eq!(bad["toolName"], "mcp__pi__Edit");
        assert_eq!(bad["isError"], true);
        assert!(bad["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("file_path"));
        let other: Value = serde_json::from_str(&prepared.transcript_blocks[4]).unwrap();
        assert_eq!(other["toolName"], "mcp__pi__Read");
        assert_eq!(other["isError"], true);
        assert!(other["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Not run"));
    }

    #[test]
    fn dialect_on_renders_an_unknown_safe_name_as_a_pi_transport_name() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "old_tool", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c", "content": "x"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "edit",
                "parameters": {"type": "object", "properties": {
                    "path": {"type": "string"},
                    "edits": {"type": "array", "items": {"type": "object", "properties": {
                        "oldText": {"type": "string"},
                        "newText": {"type": "string"}
                    }}}
                }}
            }}]
        }));
        let active = dialect::activate(&req.tools);
        let prepared = prepare(&req, &dir, &active).expect("prepare");
        let call: Value = serde_json::from_str(&prepared.transcript_blocks[1]).unwrap();
        assert_eq!(call["content"][0]["name"], "mcp__pi__old_tool");
        let result: Value = serde_json::from_str(&prepared.transcript_blocks[2]).unwrap();
        assert_eq!(result["toolName"], "mcp__pi__old_tool");
    }

    #[test]
    fn historical_unsafe_name_stays_bracketed() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c", "type": "function", "function": {"name": "old tool!", "arguments": "{}"}}
                ]}
            ]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        let call: Value = serde_json::from_str(&prepared.transcript_blocks[1]).unwrap();
        assert!(call["content"][0]["name"]
            .as_str()
            .unwrap()
            .starts_with("[unavailable Pi tool "));
    }

    #[test]
    fn tool_message_without_prior_call_is_400() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [{"role": "tool", "tool_call_id": "nope", "content": "x"}]
        }));
        let err = prepare(&req, &dir, &[]).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.param(), Some("messages[0].tool_call_id"));
    }

    #[test]
    fn reasoning_content_becomes_thinking_block() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [{"role": "assistant", "content": "ans", "reasoning_content": "hmm"}]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        let msg: Value = serde_json::from_str(&prepared.transcript_blocks[1]).unwrap();
        assert_eq!(msg["content"][0]["type"], "thinking");
        assert_eq!(msg["content"][1]["type"], "text");
    }

    #[test]
    fn data_url_image_written_once_and_listed_last() {
        let (_root, dir) = setup();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfake");
        let url = format!("data:image/png;base64,{png}");
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "see"},
                    {"type": "image_url", "image_url": {"url": url}},
                    {"type": "image_url", "image_url": {"url": url}}
                ]}
            ]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        assert_eq!(prepared.attachment_paths.len(), 1);
        assert!(prepared.attachment_paths[0].is_file());
        let blocks = prepared.prompt_blocks();
        assert!(blocks.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with("Generated image attachments"));
        assert!(blocks.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("@./image-"));
    }

    #[test]
    fn tool_result_image_is_an_attachment_on_the_tool_result() {
        let (_root, dir) = setup();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfake");
        let url = format!("data:image/png;base64,{png}");
        let req = request(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_read_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"blue-16.png\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_read_1", "content": [
                    {"type": "text", "text": "Read image file blue-16.png"},
                    {"type": "image_url", "image_url": {"url": url}}
                ]},
                {"role": "user", "content": "what colour was the image you read?"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object"}}}]
        }));
        let prepared = prepare(&req, &dir, &[]).expect("prepare");
        assert_eq!(prepared.attachment_paths.len(), 1);
        let result: Value = serde_json::from_str(&prepared.transcript_blocks[2]).unwrap();
        assert_eq!(result["role"], "toolResult");
        assert_eq!(result["content"][1]["type"], "image_attachment");
        let name = result["content"][1]["attachment"].as_str().unwrap();
        assert!(name.starts_with("image-") && name.ends_with(".png"));
        assert!(prepared.prompt_blocks().last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains(&format!("@./{name}")));
    }

    #[test]
    fn remote_image_url_is_400() {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/x.png"}}
            ]}]
        }));
        let err = prepare(&req, &dir, &[]).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.param(), Some("messages[0].content[0]"));
    }

    fn todo_tool() -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "todo",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string"},
                        "todos": {"type": "array", "items": {"type": "object", "properties": {
                            "subject": {"type": "string"},
                            "content": {"type": "string"},
                            "activeForm": {"type": "string"},
                            "metadata": {"type": "object"}
                        }}},
                        "id": {"type": "number"},
                        "status": {"type": "string"}
                    },
                    "required": ["action"]
                }
            }
        })
    }

    fn records(messages: Value) -> Vec<Value> {
        let (_root, dir) = setup();
        let req = request(json!({
            "model": "m",
            "messages": messages,
            "tools": [todo_tool()]
        }));
        let active = dialect::activate(&req.tools);
        let prepared = prepare(&req, &dir, &active).expect("prepare");
        prepared
            .transcript_blocks
            .iter()
            .skip(1)
            .map(|b| serde_json::from_str(b).unwrap())
            .collect()
    }

    fn round1(items: Value) -> (String, String, Value) {
        let calls = dialect::todo_round1_calls("toolu_fold", &json!({"todos": items})).unwrap();
        let list = &calls[0];
        if calls.len() == 1 {
            return (list.id.clone(), String::new(), json!({"todos": []}));
        }
        let create = &calls[1];
        (list.id.clone(), create.id.clone(), create.arguments.clone())
    }

    #[test]
    fn empty_todos_fold_after_the_deletes() {
        let (list_id, _, _) = round1(json!([]));
        let base = dialect::todo_call_base(&list_id).unwrap();
        let delete_id = dialect::todo_call_id(base, dialect::TODO_ROUND_APPLY, 0);
        let rows = records(json!([
            {"role": "assistant", "tool_calls": [
                {"id": list_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"list\"}"}}
            ]},
            {"role": "tool", "tool_call_id": list_id, "content": "[pending] #3 Old task"},
            {"role": "assistant", "tool_calls": [
                {"id": delete_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"delete\",\"id\":3}"}}
            ]},
            {"role": "tool", "tool_call_id": delete_id, "content": "Deleted #3: Old task"}
        ]));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["content"][0]["name"], "mcp__pi__TodoWrite");
        assert_eq!(rows[0]["content"][0]["arguments"], json!({"todos": []}));
        assert_eq!(rows[0]["content"][0]["id"], list_id);
        assert_eq!(rows[1]["toolName"], "mcp__pi__TodoWrite");
        assert_eq!(rows[1]["content"][0]["text"], dialect::TODO_SUCCESS);
        assert_eq!(rows[1]["isError"], false);
    }

    #[test]
    fn one_pending_on_an_empty_list_folds_without_a_second_round() {
        let items =
            json!([{"content": "Read SPEC", "status": "pending", "activeForm": "reading SPEC"}]);
        let (list_id, create_id, create_args) = round1(items);
        let rows = records(json!([
            {"role": "assistant", "tool_calls": [
                {"id": list_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"list\"}"}},
                {"id": create_id, "type": "function", "function": {"name": "todo", "arguments": create_args.to_string()}}
            ]},
            {"role": "tool", "tool_call_id": list_id, "content": "No tasks"},
            {"role": "tool", "tool_call_id": create_id, "content": "Created #7: Read SPEC (pending)"}
        ]));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            rows[0]["content"][0]["arguments"],
            json!({"todos": [{"content": "Read SPEC", "status": "pending", "activeForm": "reading SPEC"}]})
        );
        assert_eq!(rows[1]["content"][0]["text"], dialect::TODO_SUCCESS);
    }

    #[test]
    fn an_error_in_round1_becomes_the_todowrite_result() {
        let items =
            json!([{"content": "Read SPEC", "status": "pending", "activeForm": "reading SPEC"}]);
        let (list_id, create_id, create_args) = round1(items);
        let rows = records(json!([
            {"role": "assistant", "tool_calls": [
                {"id": list_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"list\"}"}},
                {"id": create_id, "type": "function", "function": {"name": "todo", "arguments": create_args.to_string()}}
            ]},
            {"role": "tool", "tool_call_id": list_id, "content": "No tasks"},
            {"role": "tool", "tool_call_id": create_id, "content": "Error: store locked"}
        ]));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1]["content"][0]["text"], "Error: store locked");
        assert_eq!(rows[1]["isError"], false);
    }

    #[test]
    fn the_fold_is_deterministic() {
        let items = json!([
            {"content": "Read SPEC", "status": "completed", "activeForm": "reading SPEC"},
            {"content": "Write the row", "status": "in_progress", "activeForm": "writing the row"}
        ]);
        let (list_id, create_id, create_args) = round1(items);
        let base = dialect::todo_call_base(&list_id).unwrap().to_string();
        let delete_id = dialect::todo_call_id(&base, dialect::TODO_ROUND_APPLY, 0);
        let update_id = dialect::todo_call_id(&base, dialect::TODO_ROUND_APPLY, 1);
        let messages = json!([
            {"role": "user", "content": "track it"},
            {"role": "assistant", "content": "Updating.", "tool_calls": [
                {"id": list_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"list\"}"}},
                {"id": create_id, "type": "function", "function": {"name": "todo", "arguments": create_args.to_string()}},
                {"id": "call_read1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"SPEC.md\"}"}}
            ]},
            {"role": "tool", "tool_call_id": list_id, "content": "[pending] #3 Old #12 task"},
            {"role": "tool", "tool_call_id": create_id, "content": "Created #10: Read SPEC (pending)\nCreated #11: Write the row (pending)"},
            {"role": "tool", "tool_call_id": "call_read1", "content": "spec"},
            {"role": "assistant", "tool_calls": [
                {"id": delete_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"delete\",\"id\":3}"}},
                {"id": update_id, "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"update\",\"id\":10,\"status\":\"completed\"}"}}
            ]},
            {"role": "tool", "tool_call_id": delete_id, "content": "Deleted #3: Old #12 task"},
            {"role": "tool", "tool_call_id": update_id, "content": "Updated #10 (pending → completed)"}
        ]);
        let once = records(messages.clone());
        let twice = records(messages);
        assert_eq!(once, twice);
        let call = once
            .iter()
            .find(|r| r["content"][0]["name"] == "mcp__pi__TodoWrite")
            .unwrap();
        assert_eq!(
            call["content"][0]["arguments"]["todos"],
            json!([
                {"content": "Read SPEC", "status": "completed", "activeForm": "reading SPEC"},
                {"content": "Write the row", "status": "in_progress", "activeForm": "writing the row"}
            ])
        );
        let read = once.iter().find(|r| {
            r["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["name"] == "mcp__pi__read"))
        });
        assert!(
            read.is_some(),
            "the sibling call must survive the fold: {once:?}"
        );
    }

    #[test]
    fn a_raw_todo_history_is_not_folded() {
        let rows = records(json!([
            {"role": "assistant", "tool_calls": [
                {"id": "call_grok00000000000000001", "type": "function", "function": {"name": "todo", "arguments": "{\"action\":\"list\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_grok00000000000000001", "content": "No tasks"}
        ]));
        assert_eq!(rows[0]["content"][0]["name"], "mcp__pi__todo");
        assert_eq!(rows[1]["toolName"], "mcp__pi__todo");
    }
}
