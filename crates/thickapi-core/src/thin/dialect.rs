//! Claude Code dialect (SPEC §19.7 Dialect, K3).
//!
//! Pi's tools are shown to `claude.exe` under Claude Code names and schemas, and calls are
//! translated back. `claude.exe` still never executes a tool: the proposal server only lists
//! them. Activation is per request and structural. `off` is not this module's job; the bridge
//! simply does not call it, and today's catalog, transcript and publish stay byte for byte.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::thin::openai::ToolDef;

/// `router.yaml` `thin.vendors.claude.bridge.dialect`.
pub const MODE_CLAUDE_CODE: &str = "claude_code";
pub const MODE_OFF: &str = "off";

/// One Claude-facing tool. `to_pi` / `to_claude` are the row's functions, selected by `name`.
struct Row {
    /// Claude Code's name, as the K2 fixture records it.
    name: &'static str,
    /// The Pi tool this row replaces when it is active.
    pi_tool: &'static str,
    /// JSON-pointer-ish paths that must exist in Pi's schema (`edits[].oldText`).
    requires: &'static [&'static str],
    /// Claude Code's description, trimmed where Pi's behaviour differs.
    description: &'static str,
    /// Only the arguments Pi honours.
    schema: fn() -> Value,
}

/// A row that passed activation for this request.
#[derive(Debug, Clone)]
pub struct ActiveRow {
    /// Claude Code name (`Edit`).
    pub name: &'static str,
    /// Pi tool it replaces (`edit`).
    pub pi_tool: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    /// Catalogue ids the daemon loaded (`subs/claude/sonnet`). `Agent`'s `model` alias
    /// resolves through these; every other row ignores them. Empty when the caller has no
    /// catalogue (unit tests, replay).
    models: Arc<BTreeSet<String>>,
}

impl ActiveRow {
    pub fn description(&self) -> &'static str {
        self.description
    }

    pub fn schema(&self) -> Value {
        (self.schema)()
    }

    /// Claude args to one Pi call. `Err` names the bad or missing argument.
    ///
    /// `TodoWrite` is not one call. [`to_pi`] returns its round-1 `create` (or `list` when
    /// `todos` is empty). Publish uses [`to_pi_calls`] so the list call goes out with it.
    pub fn to_pi(&self, args: &Value) -> Result<Value, String> {
        translate_to_pi(self.name, args, &self.models)
    }

    /// Claude args to the Pi calls of one assistant message. Every row but `TodoWrite` is one
    /// call. `TodoWrite` is round 1: `list`, and `create` when `todos` is not empty.
    pub fn to_pi_calls(&self, args: &Value) -> Result<Vec<Value>, String> {
        if self.name == "TodoWrite" {
            return todo_round1(args);
        }
        self.to_pi(args).map(|one| vec![one])
    }

    /// A Pi-shaped history call to Claude args. `None` when it has no Claude form.
    pub fn to_claude(&self, args: &Value) -> Option<Value> {
        translate_to_claude(self.name, args)
    }
}

/// What the catalog hook resolves a transport name to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogTarget {
    /// A Pi tool, passed through under its own name.
    Client(String),
    /// An active dialect row. Publish runs `to_pi`; the client sees `pi_tool`.
    Dialect {
        row: &'static str,
        pi_tool: &'static str,
    },
}

fn rows() -> &'static [Row] {
    &[
        Row {
            name: "Read",
            pi_tool: "read",
            requires: &[
                "/properties/path",
                "/properties/offset",
                "/properties/limit",
            ],
            description: READ_DESCRIPTION,
            schema: read_schema,
        },
        Row {
            name: "Write",
            pi_tool: "write",
            requires: &["/properties/path", "/properties/content"],
            description: WRITE_DESCRIPTION,
            schema: write_schema,
        },
        Row {
            name: "Edit",
            pi_tool: "edit",
            requires: &[
                "/properties/path",
                "/properties/edits/items/properties/oldText",
                "/properties/edits/items/properties/newText",
            ],
            description: EDIT_DESCRIPTION,
            schema: edit_schema,
        },
        Row {
            name: "Bash",
            pi_tool: "bash",
            requires: &["/properties/command", "/properties/timeout"],
            description: BASH_DESCRIPTION,
            schema: bash_schema,
        },
        Row {
            name: "Glob",
            pi_tool: "find",
            requires: &["/properties/pattern", "/properties/path"],
            description: GLOB_DESCRIPTION,
            schema: glob_schema,
        },
        Row {
            name: "Grep",
            pi_tool: "grep",
            requires: &[
                "/properties/pattern",
                "/properties/path",
                "/properties/glob",
                "/properties/ignoreCase",
                "/properties/context",
                "/properties/limit",
            ],
            description: GREP_DESCRIPTION,
            schema: grep_schema,
        },
        Row {
            name: "Agent",
            pi_tool: "subagent",
            requires: &[
                "/properties/prompt",
                "/properties/description",
                "/properties/subagent_type",
                "/properties/run_in_background",
                "/properties/model",
            ],
            description: AGENT_DESCRIPTION,
            schema: agent_schema,
        },
        Row {
            name: "AskUserQuestion",
            pi_tool: "askuserquestion",
            requires: &[
                "/properties/questions",
                "/properties/questions/items/properties/question",
                "/properties/questions/items/properties/header",
                "/properties/questions/items/properties/multiSelect",
                "/properties/questions/items/properties/options",
                "/properties/questions/items/properties/options/items/properties/label",
            ],
            description: ASK_DESCRIPTION,
            schema: ask_schema,
        },
        Row {
            name: "TodoWrite",
            pi_tool: "todo",
            // The current Pi `todo` (`@juicesharp/rpiv-todo`). The smoke fixture's older
            // list/add/toggle shape does not have `todos[]`, so the row stays inactive there.
            requires: &[
                "/properties/action",
                "/properties/todos",
                "/properties/todos/items/properties/subject",
                "/properties/todos/items/properties/content",
                "/properties/todos/items/properties/activeForm",
                "/properties/todos/items/properties/metadata",
                "/properties/id",
                "/properties/status",
            ],
            description: TODO_DESCRIPTION,
            schema: todo_schema,
        },
    ]
}

/// Catalogue ids the daemon loaded, for `Agent`'s `model` map. Disabled rows are absent:
/// a key the daemon will not serve is not a model this row may name.
pub fn catalogue_ids(models: &crate::config::ModelsConfig) -> Vec<String> {
    models
        .models
        .iter()
        .filter(|row| row.enabled)
        .map(|row| row.id.clone())
        .collect()
}

/// Rows active for this request, in table order. A skip is logged once per process per reason.
/// `models` is empty: `Agent`'s `model` alias is then dropped rather than guessed.
pub fn activate(tools: &[ToolDef]) -> Vec<ActiveRow> {
    activate_with(tools, &[])
}

/// [`activate`], plus the catalogue ids the daemon has loaded. Only `Agent` reads them:
/// `sonnet` / `opus` / `fable` / `haiku` become `subs/claude/<x>` when that id is here.
pub fn activate_with(tools: &[ToolDef], models: &[&str]) -> Vec<ActiveRow> {
    let names: BTreeSet<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
    let loaded: Arc<BTreeSet<String>> =
        Arc::new(models.iter().map(|id| (*id).to_string()).collect());
    let mut active = Vec::new();
    for row in rows() {
        if names.contains(row.name) {
            note_skip(row.name, "name collides with a Pi tool");
            continue;
        }
        let Some(tool) = tools.iter().find(|t| t.function.name == row.pi_tool) else {
            note_skip(row.name, "pi tool absent");
            continue;
        };
        let schema = tool.function.parameters.as_ref();
        if let Some(missing) = row.requires.iter().find(|p| !path_present(schema, p)) {
            note_skip(row.name, &format!("requires {missing} missing"));
            continue;
        }
        active.push(ActiveRow {
            name: row.name,
            pi_tool: row.pi_tool,
            description: row.description,
            schema: row.schema,
            models: Arc::clone(&loaded),
        });
    }
    active
}

/// The active row for a Claude name, if this request turned it on.
pub fn row_by_name<'a>(active: &'a [ActiveRow], name: &str) -> Option<&'a ActiveRow> {
    active.iter().find(|r| r.name == name)
}

/// The active row that replaces this Pi tool, if any.
pub fn row_for_pi<'a>(active: &'a [ActiveRow], pi_tool: &str) -> Option<&'a ActiveRow> {
    active.iter().find(|r| r.pi_tool == pi_tool)
}

/// One paragraph naming the active mappings, plus the one-call line when the client asked
/// for it. Empty when neither applies.
pub fn prompt_note(active: &[ActiveRow]) -> String {
    prompt_note_with(active, false)
}

/// `one_call` appends "Call at most one tool per message." (`parallel_tool_calls: false`).
pub fn prompt_note_with(active: &[ActiveRow], one_call: bool) -> String {
    let mut parts = Vec::new();
    if !active.is_empty() {
        let maps: Vec<String> = active
            .iter()
            .map(|r| format!("`{}` is `mcp__pi__{}`", r.pi_tool, r.name))
            .collect();
        parts.push(format!(
            "Tool names here: {}. Call the mcp__pi__ name; Pi records the call under the first name.",
            maps.join(", ")
        ));
    }
    if one_call {
        parts.push("Call at most one tool per message.".to_string());
    }
    parts.join(" ")
}

/// `true` when every pointer-ish path exists. `edits[]` walks array `items`.
fn path_present(schema: Option<&Value>, path: &str) -> bool {
    let Some(mut cur) = schema else {
        return false;
    };
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if let Some(name) = seg.strip_suffix("[]") {
            cur = match cur.get(name).and_then(|v| v.get("items")) {
                Some(next) => next,
                None => return false,
            };
        } else {
            cur = match cur.get(seg) {
                Some(next) => next,
                None => return false,
            };
        }
    }
    true
}

fn error_notes() -> &'static Mutex<std::collections::BTreeMap<String, String>> {
    static NOTES: Mutex<std::collections::BTreeMap<String, String>> =
        Mutex::new(std::collections::BTreeMap::new());
    &NOTES
}

fn client_tool_notes() -> &'static Mutex<std::collections::BTreeMap<String, Vec<String>>> {
    static NOTES: Mutex<std::collections::BTreeMap<String, Vec<String>>> =
        Mutex::new(std::collections::BTreeMap::new());
    &NOTES
}

fn vendor_tool_notes() -> &'static Mutex<std::collections::BTreeMap<String, Vec<String>>> {
    static NOTES: Mutex<std::collections::BTreeMap<String, Vec<String>>> =
        Mutex::new(std::collections::BTreeMap::new());
    &NOTES
}

fn with_notes<T>(
    slot: &'static Mutex<std::collections::BTreeMap<String, Vec<String>>>,
    f: impl FnOnce(&mut std::collections::BTreeMap<String, Vec<String>>) -> T,
) -> T {
    match slot.lock() {
        Ok(mut notes) => f(&mut notes),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// Client-side tool names the route handed out, in order. A non-dialect vendor's client
/// name is the vendor name. The access line prefers [`replace_tools`] when the bridge set one.
pub fn note_tools(request_id: &str, names: &[&str]) {
    if names.is_empty() {
        return;
    }
    with_notes(client_tool_notes(), |notes| {
        notes
            .entry(request_id.to_string())
            .or_default()
            .extend(names.iter().map(|n| (*n).to_string()));
    });
}

/// Vendor-side names the model emitted (`Edit`, `TodoWrite`), replacing any earlier note.
/// The bridge calls this at publish, before the route sees the translated Pi names, so
/// `tools_out` stays the vendor list even when the client saw `todo`.
pub fn replace_tools(request_id: &str, names: &[String]) {
    with_notes(vendor_tool_notes(), |notes| {
        if names.is_empty() {
            notes.remove(request_id);
        } else {
            notes.insert(request_id.to_string(), names.to_vec());
        }
    });
}

/// Bridge spawn to first stream event, in ms. The latest note for a request wins, so a
/// dialect re-spawn replaces the first attempt's time rather than adding to it.
pub fn note_spawn_ms(request_id: &str, ms: u64) {
    with_spawn_notes(|notes| {
        notes.insert(request_id.to_string(), ms);
    });
}

fn spawn_notes() -> &'static Mutex<std::collections::BTreeMap<String, u64>> {
    static NOTES: Mutex<std::collections::BTreeMap<String, u64>> =
        Mutex::new(std::collections::BTreeMap::new());
    &NOTES
}

fn with_spawn_notes<T>(f: impl FnOnce(&mut std::collections::BTreeMap<String, u64>) -> T) -> T {
    match spawn_notes().lock() {
        Ok(mut notes) => f(&mut notes),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// `dialect_error=<Tool>:<reason>` for the access line. The latest note for a request wins,
/// so a retried failure and the fatal one both appear, the fatal one on the end line.
pub fn note_error(request_id: &str, tool: &str, reason: &str) {
    let value = format!("{tool}:{reason}");
    match error_notes().lock() {
        Ok(mut notes) => {
            notes.insert(request_id.to_string(), value);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(request_id.to_string(), value);
        }
    }
}

/// The latest dialect error for this request, removed so the map does not grow. The access
/// line is emitted once per request, and a fatal note replaces the retried one before that.
pub fn take_error(request_id: &str) -> Option<String> {
    let mut notes = match error_notes().lock() {
        Ok(notes) => notes,
        Err(poisoned) => poisoned.into_inner(),
    };
    notes.remove(request_id)
}

/// Vendor-side names noted for this request, comma-joined, removed so the maps do not grow.
/// The bridge's list wins; otherwise the client names. `None` when the model emitted no
/// tool call (the access line then prints `-`).
pub fn take_tools(request_id: &str) -> Option<String> {
    let vendor = with_notes(vendor_tool_notes(), |notes| notes.remove(request_id));
    let client = with_notes(client_tool_notes(), |notes| notes.remove(request_id));
    let names = vendor.filter(|n| !n.is_empty()).or(client)?;
    if names.is_empty() {
        None
    } else {
        Some(names.join(","))
    }
}

/// Spawn-to-first-event ms noted for this request, removed so the map does not grow.
/// `None` when this path did not spawn a bridge child (the access line then prints `null`).
pub fn take_spawn_ms(request_id: &str) -> Option<u64> {
    with_spawn_notes(|notes| notes.remove(request_id))
}

fn note_skip(tool: &str, reason: &str) {
    static SEEN: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let key = format!("{tool}:{reason}");
    let first = match SEEN.lock() {
        Ok(mut seen) => seen.insert(key.clone()),
        Err(poisoned) => poisoned.into_inner().insert(key.clone()),
    };
    if first {
        tracing::info!(dialect_skip = %key, "dialect row inactive");
    }
}

fn translate_to_pi(name: &str, args: &Value, models: &BTreeSet<String>) -> Result<Value, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "arguments must be an object".to_string())?;
    match name {
        "Read" => read_to_pi(obj),
        "Write" => write_to_pi(obj),
        "Edit" => edit_to_pi(obj),
        "Bash" => bash_to_pi(obj),
        "Glob" => glob_to_pi(obj),
        "Grep" => grep_to_pi(obj),
        "Agent" => agent_to_pi(obj, models),
        "AskUserQuestion" => ask_to_pi(obj),
        // Round 1's representative call. An empty list is `list`; otherwise `create`.
        // Publish expands both through [`todo_round1`].
        "TodoWrite" => todo_to_pi(obj),
        _ => Err(format!("no dialect row named {name}")),
    }
}

fn translate_to_claude(name: &str, args: &Value) -> Option<Value> {
    let obj = args.as_object()?;
    match name {
        "Read" => read_to_claude(obj),
        "Write" => write_to_claude(obj),
        "Edit" => edit_to_claude(obj),
        "Bash" => bash_to_claude(obj),
        "Glob" => glob_to_claude(obj),
        "Grep" => grep_to_claude(obj),
        "Agent" => agent_to_claude(obj),
        "AskUserQuestion" => ask_to_claude(obj),
        // A raw Pi `todo` call (a Grok turn, or a round that was not folded) has no Claude
        // form. The fold in `transcript` rebuilds TodoWrite from round metadata instead.
        "TodoWrite" => None,
        _ => None,
    }
}

fn insert(out: &mut Value, key: &str, value: Value) {
    if let Some(obj) = out.as_object_mut() {
        obj.insert(key.to_string(), value);
    }
}

fn require_string<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, String> {
    match obj.get(key) {
        None => Err(format!("missing required {key}")),
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_string<'a>(
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn optional_bool(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

fn optional_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|v| n.as_f64().is_some_and(|f| f == *v as f64))
            .map(Some)
            .ok_or_else(|| format!("{key} must be a non-negative integer")),
        Some(_) => Err(format!("{key} must be a non-negative integer")),
    }
}

fn read_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let path = require_string(obj, "file_path")?;
    let offset = optional_u64(obj, "offset")?;
    let limit = optional_u64(obj, "limit")?;
    let mut out = json!({"path": path});
    if let Some(offset) = offset {
        insert(&mut out, "offset", json!(offset));
    }
    if let Some(limit) = limit {
        insert(&mut out, "limit", json!(limit));
    }
    Ok(out)
}

fn read_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let path = obj.get("path")?.as_str()?;
    let mut out = json!({"file_path": path});
    copy_u64(obj, "offset", &mut out, "offset")?;
    copy_u64(obj, "limit", &mut out, "limit")?;
    Some(out)
}

fn write_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    Ok(json!({
        "path": require_string(obj, "file_path")?,
        "content": require_string(obj, "content")?,
    }))
}

fn write_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    Some(json!({
        "file_path": obj.get("path")?.as_str()?,
        "content": obj.get("content")?.as_str()?,
    }))
}

fn edit_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    Ok(json!({
        "path": require_string(obj, "file_path")?,
        "edits": [{
            "oldText": require_string(obj, "old_string")?,
            "newText": require_string(obj, "new_string")?,
        }],
    }))
}

/// Reverse is exact only for a one-block edit. Two blocks have no Claude form.
fn edit_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let path = obj.get("path")?.as_str()?;
    let edits = obj.get("edits")?.as_array()?;
    let [only] = edits.as_slice() else {
        return None;
    };
    let block = only.as_object()?;
    if block.len() != 2 {
        return None;
    }
    Some(json!({
        "file_path": path,
        "old_string": block.get("oldText")?.as_str()?,
        "new_string": block.get("newText")?.as_str()?,
    }))
}

fn bash_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let command = require_string(obj, "command")?;
    let mut out = json!({"command": command});
    if let Some(ms) = optional_u64(obj, "timeout")? {
        let seconds = ms.div_ceil(1000);
        insert(&mut out, "timeout", json!(seconds));
    }
    Ok(out)
}

fn bash_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let command = obj.get("command")?.as_str()?;
    let mut out = json!({"command": command});
    if obj.contains_key("timeout") {
        let seconds = obj.get("timeout")?.as_u64()?;
        insert(&mut out, "timeout", json!(seconds.saturating_mul(1000)));
    }
    Some(out)
}

fn glob_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let pattern = require_string(obj, "pattern")?;
    let mut out = json!({"pattern": pattern});
    if let Some(path) = optional_string(obj, "path")? {
        insert(&mut out, "path", json!(path));
    }
    Ok(out)
}

fn glob_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let pattern = obj.get("pattern")?.as_str()?;
    let mut out = json!({"pattern": pattern});
    if let Some(path) = obj.get("path").and_then(Value::as_str) {
        insert(&mut out, "path", json!(path));
    }
    Some(out)
}

fn grep_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let pattern = require_string(obj, "pattern")?;
    let mut out = json!({"pattern": pattern});
    if let Some(path) = optional_string(obj, "path")? {
        insert(&mut out, "path", json!(path));
    }
    if let Some(glob) = optional_string(obj, "glob")? {
        insert(&mut out, "glob", json!(glob));
    }
    if let Some(ignore) = optional_bool(obj, "-i")? {
        insert(&mut out, "ignoreCase", json!(ignore));
    }
    if let Some(context) = optional_u64(obj, "-C")? {
        insert(&mut out, "context", json!(context));
    }
    if let Some(limit) = optional_u64(obj, "head_limit")? {
        insert(&mut out, "limit", json!(limit));
    }
    Ok(out)
}

fn grep_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let pattern = obj.get("pattern")?.as_str()?;
    let mut out = json!({"pattern": pattern});
    copy_str(obj, "path", &mut out, "path")?;
    copy_str(obj, "glob", &mut out, "glob")?;
    copy_bool(obj, "ignoreCase", &mut out, "-i")?;
    copy_u64(obj, "context", &mut out, "-C")?;
    copy_u64(obj, "limit", &mut out, "head_limit")?;
    Some(out)
}

/// Claude family aliases the Agent row may map. Anything else is dropped, not guessed.
const AGENT_MODEL_ALIASES: &[&str] = &["sonnet", "opus", "fable", "haiku"];

fn agent_model_id(alias: &str) -> String {
    format!("subs/claude/{alias}")
}

/// `model` becomes `subs/claude/<alias>` only when that id is in the loaded catalogue.
/// A missing row, or any other value, drops the key. Other Claude-only args are not read.
fn agent_to_pi(
    obj: &serde_json::Map<String, Value>,
    models: &BTreeSet<String>,
) -> Result<Value, String> {
    let mut out = json!({
        "description": require_string(obj, "description")?,
        "prompt": require_string(obj, "prompt")?,
        "subagent_type": require_string(obj, "subagent_type")?,
    });
    if let Some(background) = optional_bool(obj, "run_in_background")? {
        insert(&mut out, "run_in_background", json!(background));
    }
    if let Some(alias) = optional_string(obj, "model")? {
        if AGENT_MODEL_ALIASES.contains(&alias) {
            let id = agent_model_id(alias);
            if models.contains(&id) {
                insert(&mut out, "model", json!(id));
            }
        }
    }
    Ok(out)
}

/// Exact for a call this row produced: a mapped `subs/claude/<alias>` comes back as the alias.
/// A model the forward map dropped is absent, so it is not invented here.
fn agent_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let mut out = json!({
        "description": obj.get("description")?.as_str()?,
        "prompt": obj.get("prompt")?.as_str()?,
        "subagent_type": obj.get("subagent_type")?.as_str()?,
    });
    copy_bool(obj, "run_in_background", &mut out, "run_in_background")?;
    if let Some(model) = obj.get("model") {
        let id = model.as_str()?;
        let alias = AGENT_MODEL_ALIASES
            .iter()
            .copied()
            .find(|alias| agent_model_id(alias) == id)?;
        insert(&mut out, "model", json!(alias));
    }
    Some(out)
}

fn ask_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let questions = ask_questions(obj.get("questions"))?;
    Ok(json!({"questions": questions}))
}

fn ask_to_claude(obj: &serde_json::Map<String, Value>) -> Option<Value> {
    let questions = ask_questions(obj.get("questions")).ok()?;
    Some(json!({"questions": questions}))
}

/// Identity on `questions`, capped at 4. Extra keys (`preview`, `annotations`) are not copied.
fn ask_questions(value: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Err("missing required questions".to_string());
    };
    if items.is_empty() {
        return Err("questions must contain 1 to 4 items".to_string());
    }
    if items.len() > 4 {
        return Err("questions is capped at 4".to_string());
    }
    items.iter().map(ask_question).collect()
}

fn ask_question(value: &Value) -> Result<Value, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "questions[] must be an object".to_string())?;
    let mut out = json!({"question": require_string(obj, "question")?});
    if let Some(header) = optional_string(obj, "header")? {
        insert(&mut out, "header", json!(header));
    }
    if let Some(multi) = optional_bool(obj, "multiSelect")? {
        insert(&mut out, "multiSelect", json!(multi));
    }
    let options = obj
        .get("options")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing required options".to_string())?;
    if options.is_empty() {
        return Err("options must contain at least one item".to_string());
    }
    let mapped = options
        .iter()
        .map(|option| {
            let option = option
                .as_object()
                .ok_or_else(|| "options[] must be an object".to_string())?;
            let mut out = json!({"label": require_string(option, "label")?});
            if let Some(description) = optional_string(option, "description")? {
                insert(&mut out, "description", json!(description));
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>, String>>()?;
    insert(&mut out, "options", Value::Array(mapped));
    Ok(out)
}

fn copy_str(
    obj: &serde_json::Map<String, Value>,
    from: &str,
    out: &mut Value,
    to: &str,
) -> Option<()> {
    if let Some(v) = obj.get(from) {
        insert(out, to, Value::String(v.as_str()?.to_string()));
    }
    Some(())
}

fn copy_bool(
    obj: &serde_json::Map<String, Value>,
    from: &str,
    out: &mut Value,
    to: &str,
) -> Option<()> {
    if let Some(v) = obj.get(from) {
        insert(out, to, Value::Bool(v.as_bool()?));
    }
    Some(())
}

fn copy_u64(
    obj: &serde_json::Map<String, Value>,
    from: &str,
    out: &mut Value,
    to: &str,
) -> Option<()> {
    if let Some(v) = obj.get(from) {
        insert(out, to, json!(v.as_u64()?));
    }
    Some(())
}

/// Claude Code's own TodoWrite result text (CLI `2.1.288`, `TodoWriteTool` success path).
/// `isError` stays false on this wire, so an error is prefixed `Error:` instead.
pub const TODO_SUCCESS: &str = "Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable";

/// Prefix of a TodoWrite round id (`call_` + `tw`). The tail is 24 alnum: `tw`, a 19-char
/// base, the round digit and a 2-digit index. Ordinary ids never start with this prefix.
pub const TODO_ID_MARK: &str = "call_tw";

/// Characters of the base inside a TodoWrite round id.
const TODO_BASE_LEN: usize = 19;

/// Round 1. Index 0 is `list` when `todos` is not empty; the `create` is index 1. An empty
/// `todos` is `list` only, at index 0.
pub const TODO_ROUND_LIST: u8 = 1;

/// Round 2: deletes of ids the list named and R1 did not create, then updates of created
/// items whose `tw_status` is not `pending`. Both are independent, so one batch is safe.
pub const TODO_ROUND_APPLY: u8 = 2;

/// One Pi call the bridge publishes for a TodoWrite round.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoPiCall {
    pub id: String,
    pub arguments: Value,
}

/// `true` for `call_tw` + 19 alnum + round digit + 2-digit index (29 chars, a route id).
pub fn is_todo_call_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix(TODO_ID_MARK) else {
        return false;
    };
    let mut chars = rest.chars();
    let base_ok =
        (0..TODO_BASE_LEN).all(|_| chars.next().is_some_and(|c| c.is_ascii_alphanumeric()));
    let round_ok = chars.next().is_some_and(|c| c.is_ascii_digit());
    let index_ok = (0..2).all(|_| chars.next().is_some_and(|c| c.is_ascii_digit()));
    base_ok && round_ok && index_ok && chars.next().is_none()
}

/// `call_tw` + 19-char base + round digit + 2-digit index. Still `call_` + 24 alnum.
pub fn todo_call_id(base: &str, round: u8, index: usize) -> String {
    let mut chars: Vec<char> = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(TODO_BASE_LEN)
        .collect();
    chars.resize(TODO_BASE_LEN, '0');
    let index = index.min(99);
    let mut out = String::with_capacity(29);
    out.push_str(TODO_ID_MARK);
    out.extend(chars);
    out.push(char::from(b'0' + round.min(9)));
    out.push(char::from(b'0' + (index / 10) as u8));
    out.push(char::from(b'0' + (index % 10) as u8));
    out
}

/// The 19-char base of a TodoWrite round id. `None` for an ordinary id.
pub fn todo_call_base(id: &str) -> Option<&str> {
    let rest = id.strip_prefix(TODO_ID_MARK)?;
    is_todo_call_id(id).then_some(rest.get(..TODO_BASE_LEN)?)
}

/// Round digit of a TodoWrite round id.
pub fn todo_call_round(id: &str) -> Option<u8> {
    let rest = id.strip_prefix(TODO_ID_MARK)?;
    is_todo_call_id(id).then(|| {
        rest.chars()
            .nth(TODO_BASE_LEN)
            .and_then(|c| c.to_digit(10))
            .unwrap_or(0) as u8
    })
}

/// Index of a TodoWrite round id.
pub fn todo_call_index(id: &str) -> Option<usize> {
    let rest = id.strip_prefix(TODO_ID_MARK)?;
    is_todo_call_id(id).then(|| {
        rest.get(TODO_BASE_LEN + 1..)
            .and_then(|tail| tail.parse().ok())
            .unwrap_or(0)
    })
}

/// A base that cannot collide with [`crate::thin::openai::new_call_id`]: those never start
/// `tw`, and this one is taken from the client id with `tw` forced in front by the format.
fn todo_base_from(client_id: &str) -> String {
    let raw: String = client_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let raw = raw
        .strip_prefix("call")
        .unwrap_or(raw.as_str())
        .trim_start_matches(['t', 'w', 'T', 'W']);
    let mut base: String = raw.chars().take(TODO_BASE_LEN).collect();
    while base.len() < TODO_BASE_LEN {
        base.push('0');
    }
    base
}

/// Round 1 of one TodoWrite. `todos: []` is `list` only. Otherwise `list` then `create`,
/// and the create items carry `tw`, `tw_i` and `tw_status` so round 2 needs no daemon state.
pub fn todo_round1(args: &Value) -> Result<Vec<Value>, String> {
    let items = todo_items(args)?;
    let mut calls = vec![json!({"action": "list"})];
    if !items.is_empty() {
        calls.push(todo_create(&items)?);
    }
    Ok(calls)
}

/// Round 1 with the ids the client will see. The base comes from Claude's tool-use id, so
/// the same proposal always publishes the same ids. The create's `tw` metadata is that base.
pub fn todo_round1_calls(client_id: &str, args: &Value) -> Result<Vec<TodoPiCall>, String> {
    let base = todo_base_from(client_id);
    if base.len() != TODO_BASE_LEN {
        return Err(format!(
            "todo base {base:?} is not {TODO_BASE_LEN} from {client_id}"
        ));
    }
    let calls = todo_round1(args)?;
    Ok(calls
        .into_iter()
        .enumerate()
        .map(|(i, arguments)| TodoPiCall {
            id: todo_call_id(&base, TODO_ROUND_LIST, i),
            arguments: stamp_todo_create(&arguments, &base),
        })
        .collect())
}

fn todo_items(args: &Value) -> Result<Vec<TodoItem>, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "arguments must be an object".to_string())?;
    let Some(items) = obj.get("todos").and_then(Value::as_array) else {
        return Err("missing required todos".to_string());
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| todo_item(i, item))
        .collect()
}

struct TodoItem {
    content: String,
    status: String,
    active_form: String,
}

fn todo_item(index: usize, value: &Value) -> Result<TodoItem, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("todos[{index}] must be an object"))?;
    let content = require_string(obj, "content")?.to_string();
    let status = require_string(obj, "status")?.to_string();
    if !matches!(status.as_str(), "pending" | "in_progress" | "completed") {
        return Err(format!(
            "todos[{index}].status must be pending, in_progress or completed"
        ));
    }
    let active_form = require_string(obj, "activeForm")?.to_string();
    Ok(TodoItem {
        content,
        status,
        active_form,
    })
}

/// The representative Pi call: `create` when there is something to create, else `list`.
fn todo_to_pi(obj: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let mut calls = todo_round1(&Value::Object(obj.clone()))?;
    let create = calls
        .iter()
        .position(|c| c.get("action").and_then(Value::as_str) == Some("create"));
    match create {
        Some(i) => Ok(calls.swap_remove(i)),
        None => calls
            .pop()
            .ok_or_else(|| "TodoWrite produced no call".to_string()),
    }
}

fn todo_create(items: &[TodoItem]) -> Result<Value, String> {
    // The base is filled in by [`todo_round1_calls`]. A bare `to_pi` (the dialect-error
    // check) still has to be a valid create, so it uses a fixed base.
    let base = "0000000000000000000";
    let todos: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            json!({
                "subject": item.content,
                "activeForm": item.active_form,
                "metadata": {
                    "tw": base,
                    "tw_i": i,
                    "tw_status": item.status,
                }
            })
        })
        .collect();
    Ok(json!({"action": "create", "todos": todos}))
}

/// One item of a round-1 `create`, in `tw_i` order, as the fold and round 2 both read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoCreated {
    pub index: usize,
    pub subject: String,
    pub active_form: String,
    pub status: String,
    /// Set once the create result names the id. Absent until then, and on an error result.
    pub id: Option<u64>,
}

/// Items of a round-1 `create`, ordered by `tw_i`. `None` when this call is not that create.
pub fn todo_created_items(arguments: &Value) -> Option<Vec<TodoCreated>> {
    let todos = arguments.get("todos")?.as_array()?;
    if arguments.get("action").and_then(Value::as_str) != Some("create") {
        return None;
    }
    let mut items = Vec::new();
    for (fallback, item) in todos.iter().enumerate() {
        let meta = item.get("metadata")?.as_object()?;
        let index = meta
            .get("tw_i")
            .and_then(Value::as_u64)
            .unwrap_or(fallback as u64) as usize;
        items.push(TodoCreated {
            index,
            subject: item
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            active_form: item
                .get("activeForm")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: meta
                .get("tw_status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            id: None,
        });
    }
    items.sort_by_key(|item| item.index);
    Some(items)
}

/// The base stamped into a round-1 create. `None` when the call is not one.
pub fn todo_create_base(arguments: &Value) -> Option<String> {
    arguments
        .get("todos")?
        .as_array()?
        .first()?
        .get("metadata")?
        .get("tw")?
        .as_str()
        .map(str::to_string)
}

/// Ids a `todo` `list` result names, anchored the way `formatListLine` writes them:
/// `[status] #<id> <subject>`. A subject that itself contains `#12` is not an id.
/// `No tasks` and any other shape yield nothing.
pub fn todo_list_ids(text: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix('[') else {
            continue;
        };
        let Some((_, after)) = rest.split_once(']') else {
            continue;
        };
        let Some(token) = after.trim().strip_prefix('#') else {
            continue;
        };
        let digits: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || token.as_bytes().get(digits.len()) != Some(&b' ') {
            continue;
        }
        if let Ok(id) = digits.parse() {
            ids.push(id);
        }
    }
    ids
}

/// Ids a `create` result names (`Created #<id>: <subject> (pending)`), in order.
pub fn todo_created_ids(text: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("Created #") else {
            continue;
        };
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        if let Ok(id) = digits.parse() {
            ids.push(id);
        }
    }
    ids
}

/// `true` when a tool result is an error: the text starts `Error:`, or the message flag is set.
pub fn todo_result_is_error(text: &str, flagged: bool) -> bool {
    flagged || text.starts_with("Error:")
}

/// Round 2 for one completed round 1. Deletes first (every listed id R1 did not create),
/// then updates for created items whose target status is not `pending`. Empty means the
/// fold happens at once and the daemon does not answer a second round.
pub fn todo_round2(base: &str, listed: &[u64], created: &[TodoCreated]) -> Vec<TodoPiCall> {
    let created_ids: Vec<u64> = created.iter().filter_map(|item| item.id).collect();
    let mut calls = Vec::new();
    for id in listed {
        if created_ids.contains(id) {
            continue;
        }
        calls.push(json!({"action": "delete", "id": id}));
    }
    for item in created {
        if item.status == "pending" {
            continue;
        }
        let Some(id) = item.id else {
            continue;
        };
        calls.push(json!({"action": "update", "id": id, "status": item.status}));
    }
    calls
        .into_iter()
        .enumerate()
        .map(|(i, arguments)| TodoPiCall {
            id: todo_call_id(base, TODO_ROUND_APPLY, i),
            arguments,
        })
        .collect()
}

/// Round 2 for a request whose trailing tool results complete a round 1, when that round
/// 2 is not empty. `None` when the trailing results are not such a round 1 (so the bridge
/// spawns), or when round 2 would be empty (so the next spawn folds at once).
///
/// A mixed batch counts: the trailing results need only *contain* a complete round 1, not
/// consist of one. The other results wait in the history and fold into the next spawn.
pub fn pending_todo_round2(
    messages: &[crate::thin::openai::ChatMessage],
) -> Option<Vec<TodoPiCall>> {
    let (base, listed, created, saw_round2) = trailing_round1(messages)?;
    if saw_round2 {
        return None;
    }
    let calls = todo_round2(&base, &listed, &created);
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// The latest assistant `todo` round and the tool results that answer it, when those results
/// are among the trailing tool messages and the round is a round 1.
fn trailing_round1(
    messages: &[crate::thin::openai::ChatMessage],
) -> Option<(String, Vec<u64>, Vec<TodoCreated>, bool)> {
    let split = messages.iter().rposition(|msg| msg.role != "tool")?;
    if split + 1 >= messages.len() {
        return None;
    }
    let assistant = messages.get(split)?;
    if assistant.role != "assistant" {
        return None;
    }
    let calls = assistant.tool_calls.as_deref()?;
    let mut base: Option<String> = None;
    let mut created = Vec::new();
    let mut round_ids = Vec::new();
    let mut saw_round2 = false;
    for call in calls {
        if call.function.name != "todo" || !is_todo_call_id(&call.id) {
            continue;
        }
        let call_base = todo_call_base(&call.id)?.to_string();
        if base.as_ref().is_some_and(|have| have != &call_base) {
            return None;
        }
        base = Some(call_base);
        if todo_call_round(&call.id) == Some(TODO_ROUND_APPLY) {
            saw_round2 = true;
        }
        let arguments = serde_json::from_str::<Value>(&call.function.arguments).ok()?;
        if let Some(items) = todo_created_items(&arguments) {
            created = items;
        }
        round_ids.push(call.id.clone());
    }
    let base = base?;
    if round_ids.is_empty() {
        return None;
    }
    let results = messages.get(split + 1..)?;
    let mut answered = std::collections::BTreeSet::new();
    let mut listed = Vec::new();
    let mut list_seen = false;
    for msg in results {
        let Some(id) = msg.tool_call_id.as_deref() else {
            continue;
        };
        if !round_ids.iter().any(|round| round == id) {
            continue;
        }
        answered.insert(id.to_string());
        let text = msg.content.as_ref().map(|c| c.text()).unwrap_or_default();
        if todo_result_is_error(&text, msg.tool_is_error) {
            return None;
        }
        if todo_call_round(id) == Some(TODO_ROUND_LIST) && todo_call_index(id) == Some(0) {
            listed = todo_list_ids(&text);
            list_seen = true;
        }
        if todo_call_round(id) == Some(TODO_ROUND_LIST) && todo_call_index(id) == Some(1) {
            let ids = todo_created_ids(&text);
            for (item, created_id) in created.iter_mut().zip(ids) {
                item.id = Some(created_id);
            }
        }
    }
    if !list_seen || answered.len() != round_ids.len() {
        return None;
    }
    Some((base, listed, created, saw_round2))
}

/// The Claude args a fold rebuilds from round-1 create metadata. Order is `tw_i`.
pub fn todo_claude_args(created: &[TodoCreated]) -> Value {
    let todos: Vec<Value> = created
        .iter()
        .map(|item| {
            json!({
                "content": item.subject,
                "status": item.status,
                "activeForm": item.active_form,
            })
        })
        .collect();
    json!({"todos": todos})
}

/// Rewrite a round-1 `create` so its metadata base matches the published call id.
pub fn stamp_todo_create(arguments: &Value, base: &str) -> Value {
    let mut out = arguments.clone();
    let Some(todos) = out.get_mut("todos").and_then(Value::as_array_mut) else {
        return out;
    };
    for item in todos {
        if let Some(meta) = item.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.insert("tw".to_string(), json!(base));
        }
    }
    out
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn read_schema() -> Value {
    object_schema(
        json!({
            "file_path": {"type": "string", "description": "The path to the file to read"},
            "offset": {"type": "integer", "minimum": 1, "description": "The line number to start reading from (1-based). Only provide if the file is too large to read at once"},
            "limit": {"type": "integer", "exclusiveMinimum": 0, "description": "The number of lines to read. Only provide if the file is too large to read at once"},
        }),
        &["file_path"],
    )
}

fn write_schema() -> Value {
    object_schema(
        json!({
            "file_path": {"type": "string", "description": "The path to the file to write"},
            "content": {"type": "string", "description": "The content to write to the file"},
        }),
        &["file_path", "content"],
    )
}

fn edit_schema() -> Value {
    object_schema(
        json!({
            "file_path": {"type": "string", "description": "The path to the file to modify"},
            "old_string": {"type": "string", "description": "The text to replace"},
            "new_string": {"type": "string", "description": "The text to replace it with (must be different from old_string)"},
        }),
        &["file_path", "old_string", "new_string"],
    )
}

fn bash_schema() -> Value {
    object_schema(
        json!({
            "command": {"type": "string", "description": "The command to execute"},
            "timeout": {"type": "integer", "exclusiveMinimum": 0, "description": "Optional timeout in milliseconds"},
        }),
        &["command"],
    )
}

fn glob_schema() -> Value {
    object_schema(
        json!({
            "pattern": {"type": "string", "description": "The glob pattern to match files against"},
            "path": {"type": "string", "description": "The directory to search in. Omit to use the current working directory"},
        }),
        &["pattern"],
    )
}

fn grep_schema() -> Value {
    object_schema(
        json!({
            "pattern": {"type": "string", "description": "The regular expression pattern to search for in file contents"},
            "path": {"type": "string", "description": "File or directory to search in. Defaults to the current working directory"},
            "glob": {"type": "string", "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")"},
            "-i": {"type": "boolean", "description": "Case insensitive search"},
            "-C": {"type": "integer", "minimum": 0, "description": "Number of lines to show before and after each match"},
            "head_limit": {"type": "integer", "minimum": 0, "description": "Limit output to the first N matches"},
        }),
        &["pattern"],
    )
}

fn agent_schema() -> Value {
    object_schema(
        json!({
            "description": {"type": "string", "description": "A short (3-5 word) description of the task"},
            "prompt": {"type": "string", "description": "The task for the agent to perform"},
            "subagent_type": {"type": "string", "description": "The type of specialized agent to use: general-purpose, Explore, Plan, or a custom type"},
            "model": {
                "type": "string",
                "enum": ["sonnet", "opus", "fable", "haiku"],
                "description": "Optional model override. Mapped to the loaded catalogue, or dropped when that row is absent",
            },
            "run_in_background": {"type": "boolean", "description": "Set to true to run in the background and return an agent id immediately"},
        }),
        &["description", "prompt", "subagent_type"],
    )
}

fn todo_schema() -> Value {
    object_schema(
        json!({
            "todos": {
                "type": "array",
                "description": "The complete todo list. This replaces the current list.",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "minLength": 1, "description": "The imperative task description"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "pending, in_progress, or completed"},
                        "activeForm": {"type": "string", "minLength": 1, "description": "Present continuous form shown while the task is in progress (e.g. 'writing tests')"}
                    },
                    "required": ["content", "status", "activeForm"],
                    "additionalProperties": false
                }
            }
        }),
        &["todos"],
    )
}

fn ask_schema() -> Value {
    let questions = json!({
        "type": "array",
        "maxItems": 4,
        "description": "Questions to ask the user (1 to 4). The UI answers one option per question",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["question", "options"],
            "properties": {
                "question": {"type": "string", "description": "The full question text to display"},
                "header": {"type": "string", "description": "Short label for the question, e.g. 'Scope'"},
                "multiSelect": {"type": "boolean", "description": "Accepted but ignored: the UI is single-select"},
                "options": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["label"],
                        "properties": {
                            "label": {"type": "string", "description": "Display label for the option"},
                            "description": {"type": "string", "description": "Optional description shown below the label"},
                        },
                    },
                },
            },
        },
    });
    object_schema(json!({"questions": questions}), &["questions"])
}

const READ_DESCRIPTION: &str = "\
Reads a file from the local filesystem.\n\n\
- `file_path` is the path to read.\n\
- Reads up to 2000 lines by default, and output is truncated at 2000 lines or 50KB.\n\
- When you already know which part of the file you need, only read that part. `offset` is 1-based.\n\
- Results do not include cat -n line prefixes.\n\
- Reads images (PNG, JPG, and other common formats) and presents them visually.\n\
- PDF page ranges are not available on this tool.\n\
- Reading a directory or a missing file returns an error rather than content.";

const WRITE_DESCRIPTION: &str = "\
Writes a file to the local filesystem, overwriting if one exists. Parent directories are created.\n\n\
When to use: creating a new file, or fully replacing one you have already read. For partial changes, use Edit instead.";

const EDIT_DESCRIPTION: &str = "\
Performs exact string replacement in a file.\n\n\
- `old_string` must match the file exactly, including indentation, and be unique. The edit fails otherwise.\n\
- One replacement per call. There is no replace_all: a second occurrence is a second call.\n\
- Do not include a large unchanged region just to connect distant changes.";

const BASH_DESCRIPTION: &str = "\
Executes a bash command and returns its output.\n\n\
- Prefer the dedicated file tools over running `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` here.\n\
- `timeout` is in milliseconds. There is no default timeout.\n\
- Background execution is not available on this tool. Do not append `&`.";

const GLOB_DESCRIPTION: &str = "\
Fast file pattern matching. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". Returns matching file paths.\n\n\
Results are not sorted by modification time. A pattern with no `/` matches the basename; a pattern containing `/` is matched against the full path and is auto-prefixed with `**/` when it does not already start with one.";

const GREP_DESCRIPTION: &str = "\
Search file contents for a pattern.\n\n\
- Full regex syntax (e.g. \"log.*Error\", \"function\\s+\\w+\"). Escape literal braces (`interface\\{\\}`).\n\
- Filter with `glob` (e.g. \"**/*.tsx\").\n\
- Output is `path:line: text`. There is no files-only or count mode, and no multiline mode.\n\
- `-C` shows that many lines before and after each match. `-i` is case insensitive. `head_limit` caps the number of matches.";

const AGENT_DESCRIPTION: &str = "\
Launch a new agent to handle a complex, multi-step task. The agent runs as a Pi subagent.\n\n\
- `subagent_type` is `general-purpose`, `Explore`, `Plan`, or a custom type the seat has defined.\n\
- `model` is `sonnet`, `opus`, `fable` or `haiku`. It is used only when that catalogue row is loaded; otherwise the agent keeps its own model.\n\
- `run_in_background` returns an agent id immediately. The result is the subagent's report, not a Claude Code agent transcript.\n\
- Isolation, resume and a fork of the parent model are not available on this tool.";

const TODO_DESCRIPTION: &str = "\
Replace the whole todo list in one call. Pass every item, including ones already pending or completed; an item left out is removed. Each item needs content (imperative), status (pending, in_progress or completed) and activeForm (present continuous, shown while the item is in progress). Pass an empty list to clear the list.";

const ASK_DESCRIPTION: &str = "\
Ask the user 1 to 4 clarifying questions, each with options plus a free-text choice.\n\n\
- `questions` is capped at 4. Each question needs `question` and at least one `options` entry with a `label`.\n\
- `multiSelect` is accepted but the UI is single-select: one option, or the free-text answer, per question.\n\
- The result is the user's answers, one line per question. With no interactive UI the tool returns an error and the turn should state its assumptions and continue.";

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::thin::openai::FunctionDef;

    fn tool(name: &str, parameters: Value) -> ToolDef {
        ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: None,
                parameters: Some(parameters),
                strict: None,
            },
        }
    }

    fn pi_tools() -> Vec<ToolDef> {
        let text = include_str!("../../../../fixtures/thin/pi-tool-catalog-smoke.json");
        serde_json::from_str(text).expect("pi catalog")
    }

    fn row(name: &str) -> ActiveRow {
        let active = activate(&pi_tools());
        active.iter().find(|r| r.name == name).expect(name).clone()
    }

    #[test]
    fn tools_out_prefers_the_vendor_list_and_spawn_ms_is_taken_once() {
        let id = "chatcmpl-k9-notes";
        note_tools(id, &["edit", "todo"]);
        replace_tools(id, &["Edit".into(), "TodoWrite".into()]);
        note_spawn_ms(id, 42);
        // A later note replaces, the way a dialect re-spawn does.
        note_spawn_ms(id, 17);
        assert_eq!(take_tools(id).as_deref(), Some("Edit,TodoWrite"));
        assert_eq!(take_spawn_ms(id), Some(17));
        assert_eq!(take_tools(id), None);
        assert_eq!(take_spawn_ms(id), None);

        let plain = "chatcmpl-k9-plain";
        note_tools(plain, &["read"]);
        assert_eq!(take_tools(plain).as_deref(), Some("read"));
    }

    #[test]
    fn pi_catalog_activates_every_v1_row() {
        let active = activate(&pi_tools());
        let names: Vec<&str> = active.iter().map(|r| r.name).collect();
        assert_eq!(names, ["Read", "Write", "Edit", "Bash", "Glob", "Grep"]);
    }

    #[test]
    fn missing_requires_path_leaves_the_row_off() {
        let tools = vec![tool(
            "read",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];
        assert!(activate(&tools).is_empty());
    }

    #[test]
    fn a_pi_tool_already_named_read_disables_that_row() {
        let mut tools = pi_tools();
        tools.push(tool("Read", json!({"type": "object", "properties": {}})));
        let active = activate(&tools);
        assert!(active.iter().all(|r| r.name != "Read"));
        assert!(active.iter().any(|r| r.name == "Edit"));
    }

    #[test]
    fn read_round_trips() {
        let row = row("Read");
        let claude = json!({"file_path": "src/a.rs", "offset": 2, "limit": 10});
        let pi = row.to_pi(&claude).unwrap();
        assert_eq!(pi, json!({"path": "src/a.rs", "offset": 2, "limit": 10}));
        assert_eq!(row.to_claude(&pi).unwrap(), claude);
    }

    #[test]
    fn read_rejects_a_missing_file_path() {
        let err = row("Read").to_pi(&json!({"offset": 1})).unwrap_err();
        assert!(err.contains("file_path"), "{err}");
    }

    #[test]
    fn write_round_trips() {
        let row = row("Write");
        let claude = json!({"file_path": "a.txt", "content": "hi"});
        let pi = row.to_pi(&claude).unwrap();
        assert_eq!(pi, json!({"path": "a.txt", "content": "hi"}));
        assert_eq!(row.to_claude(&pi).unwrap(), claude);
    }

    #[test]
    fn edit_round_trips_one_block_and_refuses_two() {
        let row = row("Edit");
        let claude = json!({"file_path": "a.rs", "old_string": "foo", "new_string": "bar"});
        let pi = row.to_pi(&claude).unwrap();
        assert_eq!(
            pi,
            json!({"path": "a.rs", "edits": [{"oldText": "foo", "newText": "bar"}]})
        );
        assert_eq!(row.to_claude(&pi).unwrap(), claude);
        let two = json!({
            "path": "a.rs",
            "edits": [
                {"oldText": "a", "newText": "b"},
                {"oldText": "c", "newText": "d"}
            ]
        });
        assert!(row.to_claude(&two).is_none());
    }

    #[test]
    fn edit_schema_has_no_replace_all() {
        let schema = row("Edit").schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(!props.contains_key("replace_all"));
    }

    #[test]
    fn bash_timeout_ceils_milliseconds_to_seconds() {
        let row = row("Bash");
        let pi = row
            .to_pi(&json!({"command": "ls", "timeout": 1001}))
            .unwrap();
        assert_eq!(pi, json!({"command": "ls", "timeout": 2}));
        assert_eq!(
            row.to_claude(&pi).unwrap(),
            json!({"command": "ls", "timeout": 2000})
        );
        assert_eq!(
            row.to_pi(&json!({"command": "ls"})).unwrap(),
            json!({"command": "ls"})
        );
        let schema = row.schema();
        assert!(schema["properties"].get("description").is_none());
        assert!(schema["properties"].get("run_in_background").is_none());
    }

    #[test]
    fn glob_and_grep_round_trip() {
        let glob = row("Glob");
        let g = json!({"pattern": "**/*.rs", "path": "src"});
        assert_eq!(glob.to_claude(&glob.to_pi(&g).unwrap()).unwrap(), g);
        let grep = row("Grep");
        let q = json!({
            "pattern": "fn ",
            "path": "src",
            "glob": "*.rs",
            "-i": true,
            "-C": 2,
            "head_limit": 5
        });
        let pi = grep.to_pi(&q).unwrap();
        assert_eq!(
            pi,
            json!({
                "pattern": "fn ",
                "path": "src",
                "glob": "*.rs",
                "ignoreCase": true,
                "context": 2,
                "limit": 5
            })
        );
        assert_eq!(grep.to_claude(&pi).unwrap(), q);
        let schema = grep.schema();
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(
            props.keys().cloned().collect::<Vec<_>>(),
            ["-C", "-i", "glob", "head_limit", "path", "pattern"]
        );
    }

    #[test]
    fn prompt_note_names_only_active_rows() {
        let note = prompt_note(&activate(&pi_tools()));
        assert!(note.contains("`edit` is `mcp__pi__Edit`"), "{note}");
        assert!(note.starts_with("Tool names here:"));
        assert!(prompt_note(&[]).is_empty());
    }

    fn subagent_tool() -> ToolDef {
        tool(
            "subagent",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "description": {"type": "string"},
                    "subagent_type": {"type": "string"},
                    "model": {"type": "string"},
                    "thinking": {"type": "string"},
                    "run_in_background": {"type": "boolean"},
                    "resume": {"type": "string"}
                },
                "required": ["prompt", "description", "subagent_type"]
            }),
        )
    }

    fn ask_tool() -> ToolDef {
        tool(
            "askuserquestion",
            json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"},
                                "header": {"type": "string"},
                                "multiSelect": {"type": "boolean"},
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string"},
                                            "description": {"type": "string"}
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "required": ["questions"]
            }),
        )
    }

    fn row_with(name: &str, models: &[&str]) -> ActiveRow {
        let tools = match name {
            "Agent" => vec![subagent_tool()],
            "AskUserQuestion" => vec![ask_tool()],
            _ => panic!("{name}"),
        };
        let active = activate_with(&tools, models);
        active
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} inactive"))
            .clone()
    }

    #[test]
    fn agent_and_ask_need_their_pi_tools() {
        let active = activate(&[subagent_tool(), ask_tool()]);
        let names: Vec<&str> = active.iter().map(|r| r.name).collect();
        assert_eq!(names, ["Agent", "AskUserQuestion"]);
        assert!(activate(&pi_tools()).iter().all(|r| r.name != "Agent"));
        assert!(activate(&[tool(
            "subagent",
            json!({"type": "object", "properties": {"prompt": {"type": "string"}}})
        )])
        .is_empty());
    }

    #[test]
    fn agent_maps_a_loaded_model_and_drops_the_rest() {
        let row = row_with("Agent", &["subs/claude/sonnet", "subs/claude/opus"]);
        let claude = json!({
            "description": "Read the dialect",
            "prompt": "Find the Agent row",
            "subagent_type": "Explore",
            "model": "sonnet",
            "run_in_background": true
        });
        let pi = row.to_pi(&claude).unwrap();
        assert_eq!(
            pi,
            json!({
                "description": "Read the dialect",
                "prompt": "Find the Agent row",
                "subagent_type": "Explore",
                "model": "subs/claude/sonnet",
                "run_in_background": true
            })
        );
        assert_eq!(row.to_claude(&pi).unwrap(), claude);

        let dropped = row
            .to_pi(&json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "Plan",
                "model": "haiku"
            }))
            .unwrap();
        assert!(dropped.get("model").is_none(), "{dropped}");
        assert!(row.to_claude(&dropped).unwrap().get("model").is_none());

        let other = row
            .to_pi(&json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "general-purpose",
                "model": "gpt-5",
                "isolation": "worktree"
            }))
            .unwrap();
        assert_eq!(
            other,
            json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "general-purpose"
            })
        );
        assert!(row.to_claude(&other).unwrap().get("isolation").is_none());
    }

    #[test]
    fn agent_without_a_catalogue_drops_every_model() {
        let row = row_with("Agent", &[]);
        let pi = row
            .to_pi(&json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "Explore",
                "model": "fable"
            }))
            .unwrap();
        assert!(pi.get("model").is_none(), "{pi}");
        let schema = row.schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(!props.contains_key("isolation"));
        assert!(!props.contains_key("resume"));
        assert!(row.description().contains("runs as a Pi subagent"));
    }

    #[test]
    fn ask_round_trips_and_caps_at_four() {
        let row = row_with("AskUserQuestion", &[]);
        let claude = json!({
            "questions": [{
                "question": "Which file?",
                "header": "File",
                "multiSelect": true,
                "options": [
                    {"label": "dialect.rs", "description": "The table"},
                    {"label": "other"}
                ]
            }],
            "annotations": {"source": "ignored"}
        });
        let pi = row.to_pi(&claude).unwrap();
        assert_eq!(
            pi,
            json!({
                "questions": [{
                    "question": "Which file?",
                    "header": "File",
                    "multiSelect": true,
                    "options": [
                        {"label": "dialect.rs", "description": "The table"},
                        {"label": "other"}
                    ]
                }]
            })
        );
        assert_eq!(row.to_claude(&pi).unwrap(), pi);
        let five = json!({"questions": [
            {"question": "a", "options": [{"label": "1"}]},
            {"question": "b", "options": [{"label": "1"}]},
            {"question": "c", "options": [{"label": "1"}]},
            {"question": "d", "options": [{"label": "1"}]},
            {"question": "e", "options": [{"label": "1"}]}
        ]});
        let err = row.to_pi(&five).unwrap_err();
        assert!(err.contains("capped at 4"), "{err}");
        let schema = row.schema();
        assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
        assert!(schema["properties"].get("annotations").is_none());
        assert!(schema["properties"].get("answers").is_none());
        assert!(schema["properties"].get("metadata").is_none());
        assert!(row.description().contains("single-select"));
    }

    fn todo_tool() -> ToolDef {
        tool(
            "todo",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string"},
                    "subject": {"type": "string"},
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": {"type": "string"},
                                "content": {"type": "string"},
                                "activeForm": {"type": "string"},
                                "metadata": {"type": "object"}
                            }
                        }
                    },
                    "id": {"type": "number"},
                    "status": {"type": "string"}
                },
                "required": ["action"]
            }),
        )
    }

    fn todo_row() -> ActiveRow {
        activate(&[todo_tool()]).into_iter().next().expect("row")
    }

    fn item(content: &str, status: &str) -> Value {
        json!({"content": content, "status": status, "activeForm": format!("doing {content}")})
    }

    #[test]
    fn todowrite_activates_only_with_the_current_todo_schema() {
        let row = todo_row();
        assert_eq!(row.name, "TodoWrite");
        assert_eq!(row.pi_tool, "todo");
        let schema = row.schema();
        assert_eq!(schema["required"], json!(["todos"]));
        assert!(row.description().contains("Replace the whole todo list"));
        // The smoke fixture's older list/add/toggle shape has no `todos[]`.
        assert!(activate(&[tool(
            "todo",
            json!({"type": "object", "properties": {"action": {"type": "string"}}})
        )])
        .is_empty());
    }

    #[test]
    fn empty_todos_is_a_list_only_round() {
        let calls = todo_row().to_pi_calls(&json!({"todos": []})).unwrap();
        assert_eq!(calls, vec![json!({"action": "list"})]);
        let published = todo_round1_calls("toolu_01", &json!({"todos": []})).unwrap();
        assert_eq!(published.len(), 1);
        assert!(is_todo_call_id(&published[0].id));
        assert_eq!(todo_call_round(&published[0].id), Some(TODO_ROUND_LIST));
        assert_eq!(todo_call_index(&published[0].id), Some(0));
        assert_eq!(published[0].arguments, json!({"action": "list"}));
    }

    #[test]
    fn one_pending_item_needs_no_round2_on_an_empty_list() {
        let args = json!({"todos": [item("Read SPEC", "pending")]});
        let published = todo_round1_calls("toolu_02", &args).unwrap();
        assert_eq!(published.len(), 2);
        let created = todo_created_items(&published[1].arguments).unwrap();
        assert_eq!(created[0].status, "pending");
        assert_eq!(created[0].subject, "Read SPEC");
        let mut created = created;
        created[0].id = Some(7);
        let round2 = todo_round2(todo_call_base(&published[0].id).unwrap(), &[], &created);
        assert!(round2.is_empty(), "{round2:?}");
    }

    #[test]
    fn mixed_statuses_delete_the_old_ids_and_update_the_new_ones() {
        let args = json!({"todos": [
            item("Read SPEC", "completed"),
            item("Write the row", "in_progress"),
            item("Open the PR", "pending")
        ]});
        let published = todo_round1_calls("toolu_03", &args).unwrap();
        let mut created = todo_created_items(&published[1].arguments).unwrap();
        for (i, item) in created.iter_mut().enumerate() {
            item.id = Some(10 + i as u64);
        }
        let round2 = todo_round2(todo_call_base(&published[0].id).unwrap(), &[3, 4], &created);
        assert_eq!(round2.len(), 4, "{round2:?}");
        assert_eq!(round2[0].arguments, json!({"action": "delete", "id": 3}));
        assert_eq!(round2[1].arguments, json!({"action": "delete", "id": 4}));
        assert_eq!(
            round2[2].arguments,
            json!({"action": "update", "id": 10, "status": "completed"})
        );
        assert_eq!(
            round2[3].arguments,
            json!({"action": "update", "id": 11, "status": "in_progress"})
        );
        assert!(round2.iter().all(|c| is_todo_call_id(&c.id)));
        assert!(round2
            .iter()
            .all(|c| todo_call_round(&c.id) == Some(TODO_ROUND_APPLY)));
    }

    #[test]
    fn list_ids_are_anchored_at_the_status_bracket() {
        let text = "[pending] #3 Read #12 carefully (reading)\n[completed] #4 Done ⛓ #3";
        assert_eq!(todo_list_ids(text), vec![3, 4]);
        assert!(todo_list_ids("No tasks").is_empty());
        assert_eq!(
            todo_created_ids(
                "Created #10: Read SPEC (pending)\nCreated #11: Write the row (pending)"
            ),
            vec![10, 11]
        );
    }

    #[test]
    fn a_raw_todo_call_has_no_claude_form() {
        let row = todo_row();
        assert!(row.to_claude(&json!({"action": "list"})).is_none());
        let err = row
            .to_pi(&json!({"todos": [{"content": "x"}]}))
            .unwrap_err();
        assert!(err.contains("status"), "{err}");
    }

    #[test]
    fn round_ids_are_deterministic_and_not_ordinary() {
        let a = todo_round1_calls("toolu_same", &json!({"todos": [item("a", "pending")]})).unwrap();
        let b = todo_round1_calls("toolu_same", &json!({"todos": [item("a", "pending")]})).unwrap();
        assert_eq!(a, b);
        let direct = todo_call_id("oolusame00000000", TODO_ROUND_LIST, 0);
        assert_eq!(direct.len(), 29, "{direct}");
        assert!(a.iter().all(|c| c.id.starts_with("call_tw")), "{:?}", a);
        assert!(
            a.iter()
                .all(|c| crate::thin::openai::is_route_call_id(&c.id)),
            "{:?}",
            a.iter().map(|c| (&c.id, c.id.len())).collect::<Vec<_>>()
        );
        assert!(!is_todo_call_id("call_ordinary0000000000000000"));
    }
}
