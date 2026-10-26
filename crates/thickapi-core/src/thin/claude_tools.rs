//! Claude Code own tool schemas, fixtures/claude-tools/<cli-version>/tools.json (K2, #220).
//!
//! The dialect (K3) shows Pi tools under the names and argument names the installed
//! claude.exe sends to Anthropic. This module loads the newest captured set and diffs
//! names plus input_schema property sets, so a later CLI that renames an argument fails
//! the test rather than silently changing what the dialect exposes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::thin::fixture_set::{compiled_fixtures_root, newest_set};

/// fixtures/claude-tools/<cli-version>/tools.json.
pub const TOOLS_FILE: &str = "tools.json";
/// Vendor directory under fixtures/, beside the wire sets.
pub const VENDOR: &str = "claude-tools";

/// One captured tool: the name Claude Code sends, its description, and the JSON Schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Set when this tool was not in the live POST (`sdk-tools.d.ts` for the two
    /// `claude -p` omits). Absent on a tool the CLI sent.
    pub source: Option<String>,
}

/// A captured set: which CLI, when, and the tools sorted by name.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeTools {
    pub cli_version: String,
    pub captured_at: String,
    /// "bundle" when the schemas came from the CLI bundled JS, not a live POST.
    pub source: Option<String>,
    pub tools: Vec<ClaudeTool>,
}

impl ClaudeTools {
    /// Parse a tools.json document. Tools come back sorted by name.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let cli_version = string_field(value, "cli_version")?;
        let captured_at = string_field(value, "captured_at")?;
        let source = value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let raw = value
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| "tools.json: tools must be an array".to_string())?;
        let mut tools = Vec::with_capacity(raw.len());
        for tool in raw {
            tools.push(ClaudeTool {
                name: string_field(tool, "name")?,
                description: string_field(tool, "description").unwrap_or_default(),
                input_schema: tool.get("input_schema").cloned().unwrap_or(Value::Null),
                source: tool
                    .get("source")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            cli_version,
            captured_at,
            source,
            tools,
        })
    }

    /// The on-disk document, tools sorted by name.
    pub fn to_value(&self) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|tool| {
                let mut doc = serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                });
                if let (Some(source), Some(obj)) = (&tool.source, doc.as_object_mut()) {
                    obj.insert("source".to_string(), Value::String(source.clone()));
                }
                doc
            })
            .collect();
        let mut doc = serde_json::json!({
            "cli_version": self.cli_version,
            "captured_at": self.captured_at,
            "tools": tools,
        });
        if let Some(source) = &self.source {
            if let Some(obj) = doc.as_object_mut() {
                obj.insert("source".to_string(), Value::String(source.clone()));
            }
        }
        doc
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| format!("parse {}: {err}", path.display()))?;
        Self::from_value(&value)
    }
}

fn string_field(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("tools.json: missing string {key}"))
}

/// The newest fixtures/claude-tools/*/tools.json, by captured_at, else by directory name.
/// A directory whose name ends .tmp does not count (capture check tree).
pub fn newest_tools(fixtures_root: &Path) -> Option<PathBuf> {
    let set = newest_set(fixtures_root, VENDOR)?;
    let file = set.join(TOOLS_FILE);
    file.is_file().then_some(file)
}

/// Load the newest committed set under the checkout fixtures/.
pub fn load_newest() -> Result<ClaudeTools, String> {
    let root = compiled_fixtures_root();
    let path = newest_tools(&root).ok_or_else(|| {
        format!(
            "no {} set under {} (run thickapi capture claude-tools)",
            VENDOR,
            root.display()
        )
    })?;
    ClaudeTools::load(&path)
}

/// Property names of a tool input_schema (properties, else the schema own keys).
pub fn property_names(schema: &Value) -> BTreeSet<String> {
    let object = schema
        .get("properties")
        .and_then(Value::as_object)
        .or_else(|| schema.as_object());
    object
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// Name to property-name set, for the drift diff and the K3 property listing.
pub fn property_sets(tools: &ClaudeTools) -> Vec<(String, BTreeSet<String>)> {
    tools
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), property_names(&tool.input_schema)))
        .collect()
}

/// How fresh differs from committed in tool names and input_schema property sets.
/// Empty means the dialect names have not moved.
pub fn schema_diff(fresh: &ClaudeTools, committed: &ClaudeTools) -> Vec<String> {
    let fresh_map = name_map(fresh);
    let old_map = name_map(committed);
    let mut ordered: Vec<&String> = fresh_map.keys().chain(old_map.keys()).collect();
    ordered.sort();
    ordered.dedup();
    let mut lines = Vec::new();
    for name in ordered {
        match (fresh_map.get(name), old_map.get(name)) {
            (Some(_), None) => lines.push(format!("{name}: added")),
            (None, Some(_)) => lines.push(format!("{name}: removed")),
            (Some(tool), Some(old)) => {
                let gained = property_names(&tool.input_schema);
                let lost_from = property_names(&old.input_schema);
                let added: Vec<&String> = gained.difference(&lost_from).collect();
                let removed: Vec<&String> = lost_from.difference(&gained).collect();
                if !added.is_empty() || !removed.is_empty() {
                    lines.push(format!(
                        "{name}: properties added {added:?}, removed {removed:?}"
                    ));
                }
            }
            (None, None) => {}
        }
    }
    lines
}

fn name_map(tools: &ClaudeTools) -> BTreeMap<String, ClaudeTool> {
    tools
        .tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.clone()))
        .collect()
}

/// Pull Anthropic tools out of a Messages request body. None when this body is not one.
pub fn tools_from_body(body: &Value) -> Option<Vec<ClaudeTool>> {
    let raw = body.get("tools")?.as_array()?;
    let mut tools = Vec::with_capacity(raw.len());
    for tool in raw {
        let name = tool.get("name")?.as_str()?.to_string();
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input_schema = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        tools.push(ClaudeTool {
            name,
            description,
            input_schema,
            source: None,
        });
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Some(tools)
}

/// `claude -p` does not offer these. Their input types ship in `sdk-tools.d.ts`, and the
/// capture pins them so the dialect still has the names the installed CLI defines.
pub const SDK_PINNED: &[(&str, &str)] = &[
    ("TodoWrite", "TodoWriteInput"),
    ("AskUserQuestion", "AskUserQuestionInput"),
];

/// File the pinned tools are marked with (`"source": "sdk-tools.d.ts"`).
pub const SDK_TOOLS_SOURCE: &str = "sdk-tools.d.ts";

/// `sdk-tools.d.ts` beside the resolved `claude.exe` (`../sdk-tools.d.ts` from `bin/`).
pub fn sdk_tools_path(program: &Path) -> Option<PathBuf> {
    let dir = program.parent()?;
    let beside = dir.join("sdk-tools.d.ts");
    if beside.is_file() {
        return Some(beside);
    }
    let parent = dir.parent()?.join("sdk-tools.d.ts");
    parent.is_file().then_some(parent)
}

/// The two tools `claude -p` omits, parsed from `sdk-tools.d.ts`. Each is marked
/// [`SDK_TOOLS_SOURCE`]. A missing interface is an error, not a silent skip.
pub fn sdk_pinned_tools(dts: &str) -> Result<Vec<ClaudeTool>, String> {
    let mut tools = Vec::with_capacity(SDK_PINNED.len());
    for (name, iface) in SDK_PINNED {
        let body = interface_body(dts, iface)
            .ok_or_else(|| format!("sdk-tools.d.ts has no interface {iface}"))?;
        tools.push(ClaudeTool {
            name: (*name).to_string(),
            description: String::new(),
            input_schema: interface_schema(body),
            source: Some(SDK_TOOLS_SOURCE.to_string()),
        });
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tools)
}

/// The `{...}` body of `export interface <name>`, braces balanced, strings respected.
fn interface_body<'a>(dts: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("export interface {name} ");
    let at = dts.find(&marker)?;
    let rest = dts.get(at + marker.len()..)?;
    let open = rest.find('{')?;
    balanced(rest.get(open..)?, '{', '}')
}

/// JSON Schema for one interface body: top-level properties, required, and nested objects.
fn interface_schema(body: &str) -> Value {
    let (properties, required) = properties_of(body);
    let value = Value::Object(properties.into_iter().collect());
    serde_json::json!({
        "type": "object",
        "properties": value,
        "required": required,
    })
}

/// `(properties, required)` for the fields declared directly in `body`.
/// A `BTreeMap`, not `serde_json::Map`: that map keeps only JSON values and drops a
/// key's first byte when the value is not one (the `a` of `answers`).
fn properties_of(body: &str) -> (BTreeMap<String, Value>, Vec<Value>) {
    let mut properties = BTreeMap::new();
    let mut required = Vec::new();
    let mut rest = body;
    while let Some((name, optional, schema, next)) = next_field(rest) {
        if !optional {
            required.push(Value::String(name.clone()));
        }
        properties.insert(name, schema);
        rest = next;
    }
    (properties, required)
}

/// The next `name?:` or `name:` field, its schema, and the text after it.
fn next_field(body: &str) -> Option<(String, bool, Value, &str)> {
    let (name, optional, after, _) = field_start(body)?;
    let (schema, consumed) = field_schema(after)?;
    // Slice the type text, not `body`: `field_start`'s index counts the skipped prefix.
    let next = after.get(consumed..).unwrap_or("");
    Some((name, optional, schema, next))
}

/// The first `name?:` at depth 0, and where in `body` the type text starts.
fn field_start(body: &str) -> Option<(String, bool, &str, usize)> {
    let bytes = body.as_bytes();
    let mut i = 0;
    // A body that includes its own opening brace starts at -1, so that brace does not
    // hide its fields. A slice that starts mid-object (the text after a field) starts at 0.
    let mut depth = if body.trim_start().starts_with('{') {
        -1
    } else {
        0
    };
    while i < bytes.len() {
        if bytes.get(i) == Some(&b'"') || bytes.get(i) == Some(&b'\'') {
            let quote = *bytes.get(i)? as char;
            i += skip_quoted(body.get(i..)?, quote)?;
            continue;
        }
        if bytes.get(i) == Some(&b'/') && bytes.get(i + 1) == Some(&b'*') {
            i += body.get(i..)?.find("*/")? + 2;
            continue;
        }
        if depth == 0 {
            if let Some((name, optional, after)) = field_at(body.get(i..)?) {
                return Some((name, optional, after, i + (body.len() - after.len())));
            }
        }
        match bytes.get(i) {
            Some(b'{' | b'[' | b'(') => depth += 1,
            Some(b'}' | b']' | b')') => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// A field declaration starting at `text[0]`: name, whether it is optional, text after `: `.
fn field_at(text: &str) -> Option<(String, bool, &str)> {
    let text = text.trim_start();
    let name_end = text.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
    if name_end == 0 {
        return None;
    }
    let name = text.get(..name_end)?;
    if !name.chars().next()?.is_ascii_alphabetic() {
        return None;
    }
    let mut rest = text.get(name_end..)?;
    let optional = rest.starts_with('?');
    if optional {
        rest = rest.get(1..)?;
    }
    rest.starts_with(':').then(|| {
        (
            name.to_string(),
            optional,
            rest.get(1..).unwrap_or("").trim_start(),
        )
    })
}

/// Schema for the type starting at `text`, and how many bytes it consumed.
/// A type runs to the next `;` at depth 0, so `T[]` and a trailing comment stay attached.
fn field_schema(text: &str) -> Option<(Value, usize)> {
    let text = text.trim_start();
    let lead = text.len();
    let end = type_end(text)?;
    let ty = text.get(..end)?.trim().trim_end_matches(';').trim();
    // A union of tuples (`[A] | [A, A]`, from minItems/maxItems) collapses to one array
    // of the first element's shape. Done here, once, so it cannot recurse.
    if ty.contains('|') && ty.contains('[') {
        if let Some(first) = union_arm(ty) {
            let items = schema_of(first);
            return Some((
                serde_json::json!({"type": "array", "items": items}),
                lead - text.len() + end,
            ));
        }
    }
    Some((schema_of(ty), lead - text.len() + end))
}

/// Bytes of the type expression starting at `text`, up to and including its `;`.
/// A leading `{` is the type itself, so its inner `;` does not end the expression.
fn type_end(text: &str) -> Option<usize> {
    let mut braces = 0i32;
    let mut brackets = 0i32;
    let mut quote: Option<char> = None;
    let mut escape = false;
    for (idx, c) in text.char_indices() {
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '{' => braces += 1,
            '}' => braces -= 1,
            '[' => brackets += 1,
            ']' => {
                brackets -= 1;
                // A `]` ends the type only when the next byte ends it (`;` or `[]`).
                // A comma is another tuple element; a `|` is the next arm of a union.
                let done = text.get(idx + 1..).is_some_and(|r| {
                    let r = r.trim_start();
                    r.starts_with(';') || r.starts_with("[]")
                });
                if brackets == 0 && braces == 0 && done {
                    let extra = text.get(idx + 1..).is_some_and(|r| r.starts_with("[]"));
                    return Some(idx + 1 + usize::from(extra) * 2);
                }
            }
            ';' if braces == 0 && brackets == 0 => return Some(idx + 1),
            _ => {}
        }
    }
    (!text.is_empty()).then_some(text.len())
}

/// The first element of a tuple-union (`[A] | [A, A]`), as a one-item array type.
fn union_arm(ty: &str) -> Option<&str> {
    let open = ty.find('{')?;
    balanced(ty.get(open..)?, '{', '}')
}

fn schema_of(ty: &str) -> Value {
    let ty = ty.trim().trim_end_matches(';').trim();
    if let Some(inner) = ty.strip_suffix("[]") {
        return serde_json::json!({"type": "array", "items": schema_of(inner.trim())});
    }
    if ty.starts_with('{') && ty.ends_with('}') {
        // properties_of expects the body including its own braces (depth starts at -1).
        let (properties, required) = properties_of(ty);
        return serde_json::json!({"type": "object", "properties": properties, "required": required});
    }
    type_schema(ty)
}

fn type_schema(ty: &str) -> Value {
    let ty = ty.trim();
    if let Some(inner) = ty.strip_suffix("[]") {
        return serde_json::json!({"type": "array", "items": type_schema(inner.trim())});
    }
    if ty.contains('|') && !ty.contains('{') {
        let enums: Vec<Value> = ty
            .split('|')
            .filter_map(|part| {
                let part = part.trim().trim_matches('"');
                (!part.is_empty() && part != "string").then(|| Value::String(part.to_string()))
            })
            .collect();
        if !enums.is_empty() && ty.contains('"') {
            return serde_json::json!({"type": "string", "enum": enums});
        }
    }
    let json_type = match ty {
        "string" => "string",
        "number" => "number",
        "boolean" => "boolean",
        _ => "object",
    };
    serde_json::json!({"type": json_type})
}

/// The bracketed span starting at `text[0]`, including the brackets.
fn balanced(text: &str, open: char, close: char) -> Option<&str> {
    if !text.starts_with(open) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut escape = false;
    while i < bytes.len() {
        let c = *bytes.get(i)?;
        if let Some(q) = quote {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let close_at = text.get(i..)?.find("*/")?;
            i += close_at + 2;
            continue;
        }
        if c == b'"' || c == b'\'' {
            quote = Some(c);
        } else if c == open as u8 {
            depth += 1;
        } else if c == close as u8 {
            depth -= 1;
            if depth == 0 {
                return text.get(..=i);
            }
        }
        i += 1;
    }
    None
}

fn skip_quoted(text: &str, quote: char) -> Option<usize> {
    let mut escape = false;
    for (idx, c) in text.char_indices().skip(1) {
        if escape {
            escape = false;
        } else if c == '\\' {
            escape = true;
        } else if c == quote {
            return Some(idx + c.len_utf8());
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn tool(name: &str, props: &[&str]) -> ClaudeTool {
        let mut properties = Map::new();
        for prop in props {
            properties.insert((*prop).to_string(), serde_json::json!({"type": "string"}));
        }
        ClaudeTool {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object", "properties": properties}),
            source: None,
        }
    }

    fn set(tools: Vec<ClaudeTool>) -> ClaudeTools {
        ClaudeTools {
            cli_version: "2.1.288".into(),
            captured_at: "2026-10-03T00:00:00Z".into(),
            source: None,
            tools,
        }
    }

    #[test]
    fn schema_diff_reports_a_renamed_property_and_a_dropped_tool() {
        let fresh = set(vec![tool("Read", &["file_path", "offset"])]);
        let committed = set(vec![
            tool("Read", &["path", "offset"]),
            tool("MultiEdit", &["edits"]),
        ]);
        let diff = schema_diff(&fresh, &committed);
        assert!(diff
            .iter()
            .any(|l| l.contains("Read") && l.contains("file_path")));
        assert!(diff
            .iter()
            .any(|l| l.contains("MultiEdit") && l.contains("removed")));
        assert!(schema_diff(&committed, &committed).is_empty());
    }

    #[test]
    fn sdk_pinned_tools_reads_the_two_interfaces() {
        let dts = "export interface TodoWriteInput {\n  /** The updated todo list */\n  todos: { content: string; status: \"pending\" | \"in_progress\" | \"completed\"; activeForm: string; }[];\n}\nexport interface AskUserQuestionInput {\n  questions: { question: string; header: string; options: { label: string; description: string; preview?: string; }[]; multiSelect: boolean; }[];\n  answers?: { [k: string]: string };\n}\n";
        let tools = sdk_pinned_tools(dts).unwrap();
        assert_eq!(tools[0].name, "AskUserQuestion");
        assert_eq!(tools[0].source.as_deref(), Some(SDK_TOOLS_SOURCE));
        let props = tools[0]
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap();
        let questions = props.get("questions").unwrap();
        assert_eq!(questions.get("type").and_then(Value::as_str), Some("array"));
        let item_props = questions
            .get("items")
            .and_then(|v| v.get("properties"))
            .and_then(Value::as_object)
            .unwrap();
        assert!(item_props.contains_key("multiSelect"));
        assert!(item_props.contains_key("header"));
        assert!(tools[0].input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "questions"));
        assert!(!tools[0].input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "answers"));
        let todos = tools[1].input_schema["properties"]
            .as_object()
            .unwrap()
            .get("todos")
            .unwrap();
        assert_eq!(todos.get("type").and_then(Value::as_str), Some("array"));
        let status = todos
            .get("items")
            .and_then(|v| v.get("properties"))
            .and_then(|v| v.get("status"))
            .unwrap();
        assert!(status
            .get("enum")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .any(|v| v == "in_progress"));
    }

    #[test]
    fn tools_from_body_sorts_and_ignores_a_body_without_tools() {
        let body = serde_json::json!({
            "tools": [
                {"name": "Write", "description": "w", "input_schema": {"type": "object"}},
                {"name": "Agent", "description": "a", "input_schema": {"properties": {"prompt": {}}}}
            ]
        });
        let tools = tools_from_body(&body).unwrap();
        assert_eq!(tools[0].name, "Agent");
        assert!(property_names(&tools[0].input_schema).contains("prompt"));
        assert!(tools_from_body(&serde_json::json!({"messages": []})).is_none());
    }
}
