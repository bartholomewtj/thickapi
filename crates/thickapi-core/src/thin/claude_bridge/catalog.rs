//! OpenAI `tools` → MCP proposal catalog (port of `serializeToolCatalog` in
//! `context-serializer.ts`). Safe names are kept; others get deterministic aliases. Claude sees
//! every tool as `mcp__pi__<alias>`; the mapper translates back to the client's name.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::thin::dialect::{self, ActiveRow, CatalogTarget};
use crate::thin::openai::{ToolChoice, ToolDef};
use crate::thin::ThinError;

pub const MAX_TOOLS: usize = 256;
pub const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
pub const TRANSPORT_PREFIX: &str = "mcp__pi__";

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// `[{name, description, inputSchema}]` as the proposal server serves it.
    pub entries: Vec<Value>,
    /// `mcp__pi__<alias>` → client tool name. A dialect row maps to its Pi tool, so publish
    /// can still name the client tool after `to_pi`.
    pub transport_to_client: BTreeMap<String, String>,
    /// `mcp__pi__<alias>` → client name or dialect row. Empty means every name is a client name
    /// (dialect off, and `replay`).
    pub transport_target: BTreeMap<String, CatalogTarget>,
    /// client tool name → `mcp__pi__<alias>` of the tool Claude should call for it.
    pub client_to_transport: BTreeMap<String, String>,
    /// `[{transportName, piName}]` published in the transcript header.
    pub public_map: Vec<Value>,
    /// Rows that replaced their Pi tool in this catalog. Empty when dialect is off.
    pub active: Vec<ActiveRow>,
}

impl Catalog {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The tool set Claude's init record must report (SPEC §19.7 init validation).
    pub fn expected_tools(&self) -> BTreeSet<String> {
        self.transport_to_client.keys().cloned().collect()
    }

    pub fn json_text(&self) -> String {
        let mut text = serde_json::to_string(&self.entries).unwrap_or_else(|_| "[]".into());
        text.push('\n');
        text
    }

    /// Historical transport name for a tool that is no longer in the active catalog.
    pub fn historical_name(&self, client_name: &str) -> String {
        self.client_to_transport
            .get(client_name)
            .cloned()
            .unwrap_or_else(|| unavailable_historical_name(client_name))
    }
}

pub fn build_catalog(tools: &[ToolDef]) -> Result<Catalog, ThinError> {
    build_catalog_with(tools, &[])
}

/// Tools Claude may call this turn. `none` offers nothing (history still renders through the
/// full catalog). A named choice keeps that one client tool; an unknown name is a 400.
fn offered_tools<'a>(
    tools: &'a [ToolDef],
    choice: &ToolChoice,
) -> Result<Vec<&'a ToolDef>, ThinError> {
    match choice {
        ToolChoice::None => Ok(Vec::new()),
        ToolChoice::Named(name) => {
            let tool = tools
                .iter()
                .find(|t| t.function.name == *name)
                .ok_or_else(|| {
                    ThinError::invalid_request(
                        format!("tool_choice names unknown tool {name:?}"),
                        Some("tool_choice"),
                    )
                })?;
            Ok(vec![tool])
        }
        ToolChoice::Auto | ToolChoice::Required => Ok(tools.iter().collect()),
    }
}

/// `active` rows replace their Pi tool. Pass `&[]` for dialect off, which is byte for byte
/// today's catalog.
pub fn build_catalog_with(tools: &[ToolDef], active: &[ActiveRow]) -> Result<Catalog, ThinError> {
    build_offered(tools, active, &ToolChoice::Auto)
}

/// The catalog Claude may call this turn. `none` is empty (init `tools: []`, no MCP server).
/// A named choice keeps that one client tool, presented through the dialect when a row maps
/// it. History rendering uses [`build_catalog_with`] on the full list, so a tool this choice
/// hid still has its historical name.
pub fn build_offered(
    tools: &[ToolDef],
    active: &[ActiveRow],
    choice: &ToolChoice,
) -> Result<Catalog, ThinError> {
    let offered = offered_tools(tools, choice)?;
    build_from(&offered, active)
}

fn build_from(tools: &[&ToolDef], active: &[ActiveRow]) -> Result<Catalog, ThinError> {
    if tools.len() > MAX_TOOLS {
        return Err(ThinError::invalid_request(
            format!("at most {MAX_TOOLS} tools are supported on the Claude bridge"),
            Some("tools"),
        ));
    }
    let mut sorted: Vec<&ToolDef> = Vec::with_capacity(tools.len());
    for (i, tool) in tools.iter().copied().enumerate() {
        if tool.kind != "function" {
            return Err(ThinError::invalid_request(
                format!("tools[{i}].type must be \"function\""),
                Some(&format!("tools[{i}].type")),
            ));
        }
        if tool.function.name.trim().is_empty() {
            return Err(ThinError::invalid_request(
                format!("tools[{i}].function.name must be a nonempty string"),
                Some(&format!("tools[{i}].function.name")),
            ));
        }
        if let Some(params) = &tool.function.parameters {
            if !params.is_object() {
                return Err(ThinError::invalid_request(
                    format!("tools[{i}].function.parameters must be an object"),
                    Some(&format!("tools[{i}].function.parameters")),
                ));
            }
        }
        sorted.push(tool);
    }
    sorted.sort_by(|a, b| a.function.name.cmp(&b.function.name));
    if let Some(dup) = sorted.windows(2).find_map(|w| match w {
        [a, b] if a.function.name == b.function.name => Some(a),
        _ => None,
    }) {
        return Err(ThinError::invalid_request(
            format!("duplicate tool name: {}", dup.function.name),
            Some("tools"),
        ));
    }

    let mut used = BTreeSet::new();
    let mut catalog = Catalog {
        active: active.to_vec(),
        ..Catalog::default()
    };
    for tool in sorted {
        if let Some(row) = dialect::row_for_pi(active, &tool.function.name) {
            push_dialect_row(&mut catalog, &mut used, tool, row);
        } else {
            push_client_tool(&mut catalog, &mut used, tool);
        }
    }
    if catalog.json_text().len() > MAX_CATALOG_BYTES {
        return Err(ThinError::invalid_request(
            format!("tool catalog exceeds {MAX_CATALOG_BYTES} bytes"),
            Some("tools"),
        ));
    }
    Ok(catalog)
}

fn push_client_tool(catalog: &mut Catalog, used: &mut BTreeSet<String>, tool: &ToolDef) {
    let alias = safe_tool_name(&tool.function.name, used);
    let transport = format!("{TRANSPORT_PREFIX}{alias}");
    catalog
        .transport_to_client
        .insert(transport.clone(), tool.function.name.clone());
    catalog.transport_target.insert(
        transport.clone(),
        CatalogTarget::Client(tool.function.name.clone()),
    );
    catalog
        .client_to_transport
        .insert(tool.function.name.clone(), transport.clone());
    catalog.public_map.push(json!({
        "transportName": transport,
        "piName": tool.function.name,
    }));
    catalog.entries.push(json!({
        "name": alias,
        "description": tool.function.description.clone().unwrap_or_default(),
        "inputSchema": tool
            .function
            .parameters
            .clone()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    }));
}

/// The row replaces the Pi tool: Claude sees `mcp__pi__Edit`, not `mcp__pi__edit`.
fn push_dialect_row(
    catalog: &mut Catalog,
    used: &mut BTreeSet<String>,
    tool: &ToolDef,
    row: &ActiveRow,
) {
    let alias = safe_tool_name(row.name, used);
    let transport = format!("{TRANSPORT_PREFIX}{alias}");
    catalog
        .transport_to_client
        .insert(transport.clone(), tool.function.name.clone());
    catalog.transport_target.insert(
        transport.clone(),
        CatalogTarget::Dialect {
            row: row.name,
            pi_tool: row.pi_tool,
        },
    );
    catalog
        .client_to_transport
        .insert(tool.function.name.clone(), transport.clone());
    catalog.public_map.push(json!({
        "transportName": transport,
        "piName": tool.function.name,
    }));
    catalog.entries.push(json!({
        "name": alias,
        "description": row.description(),
        "inputSchema": row.schema(),
    }));
}

pub(crate) fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

fn short_digest(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    hex::encode(digest)[..16].to_string()
}

fn safe_tool_name(name: &str, used: &mut BTreeSet<String>) -> String {
    let base = if is_safe_name(name) {
        name.to_string()
    } else {
        format!("tool_{}", short_digest(name))
    };
    let mut candidate = base.clone();
    let mut suffix = 1;
    while used.contains(&candidate) {
        let head: String = base.chars().take(43).collect();
        candidate = format!("{head}_{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

pub fn unavailable_historical_name(name: &str) -> String {
    format!("[unavailable Pi tool {}]", short_digest(name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::thin::openai::FunctionDef;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: Some(format!("{name} tool")),
                parameters: Some(
                    json!({"type": "object", "properties": {"p": {"type": "string"}}}),
                ),
                strict: None,
            },
        }
    }

    fn edit_tool() -> ToolDef {
        ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: "edit".into(),
                description: Some("Edit a file.".into()),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "edits": {"type": "array", "items": {"type": "object", "properties": {
                            "oldText": {"type": "string"}, "newText": {"type": "string"}
                        }}}
                    }
                })),
                strict: None,
            },
        }
    }

    #[test]
    fn named_choice_offers_one_dialect_tool() {
        let active = dialect::activate(&[edit_tool(), tool("read")]);
        let catalog = build_offered(
            &[edit_tool(), tool("read")],
            &active,
            &ToolChoice::Named("edit".into()),
        )
        .unwrap();
        assert_eq!(
            catalog.expected_tools().into_iter().collect::<Vec<_>>(),
            vec!["mcp__pi__Edit".to_string()]
        );
        assert_eq!(catalog.entries[0]["name"], "Edit");
    }

    #[test]
    fn none_offers_nothing() {
        let catalog = build_offered(&[tool("read")], &[], &ToolChoice::None).unwrap();
        assert!(catalog.expected_tools().is_empty());
    }

    #[test]
    fn unknown_named_choice_is_a_400() {
        let err =
            build_offered(&[tool("read")], &[], &ToolChoice::Named("missing".into())).unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.param(), Some("tool_choice"));
    }

    #[test]
    fn safe_names_kept_and_sorted() {
        let catalog = build_catalog(&[tool("write"), tool("read"), tool("bash")]).unwrap();
        let names: Vec<&str> = catalog
            .entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["bash", "read", "write"]);
        assert_eq!(catalog.transport_to_client["mcp__pi__read"], "read");
        assert_eq!(catalog.client_to_transport["read"], "mcp__pi__read");
        assert_eq!(
            catalog.expected_tools(),
            ["mcp__pi__bash", "mcp__pi__read", "mcp__pi__write"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn unsafe_names_get_deterministic_alias() {
        let a = build_catalog(&[tool("weird name!")]).unwrap();
        let b = build_catalog(&[tool("weird name!")]).unwrap();
        let alias = a.entries[0]["name"].as_str().unwrap().to_string();
        assert!(alias.starts_with("tool_"));
        assert_eq!(alias.len(), 5 + 16);
        assert_eq!(alias, b.entries[0]["name"].as_str().unwrap());
        assert_eq!(
            a.transport_to_client[&format!("mcp__pi__{alias}")],
            "weird name!"
        );
    }

    #[test]
    fn alias_collision_gets_suffix() {
        let mut used = BTreeSet::new();
        assert_eq!(safe_tool_name("read", &mut used), "read");
        assert_eq!(safe_tool_name("read", &mut used), "read_1");
        assert_eq!(safe_tool_name("read", &mut used), "read_2");
    }

    #[test]
    fn duplicate_tool_rejected() {
        let err = build_catalog(&[tool("read"), tool("read")]).unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("duplicate"));
    }

    #[test]
    fn missing_parameters_default_to_empty_object_schema() {
        let mut t = tool("noop");
        t.function.parameters = None;
        let catalog = build_catalog(&[t]).unwrap();
        assert_eq!(catalog.entries[0]["inputSchema"]["type"], "object");
    }

    #[test]
    fn historical_name_for_unknown_tool_is_bracketed() {
        let catalog = build_catalog(&[tool("read")]).unwrap();
        assert_eq!(catalog.historical_name("read"), "mcp__pi__read");
        assert!(catalog
            .historical_name("gone")
            .starts_with("[unavailable Pi tool "));
    }

    #[test]
    fn pi_tool_catalog_round_trips_sanitize_mcp_and_cursor_value() {
        use crate::thin::oauth::antigravity::sanitize_schema;
        use crate::thin::oauth::cursor::{decode_protobuf_value, encode_protobuf_value};

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/thin/pi-tool-catalog.json");
        let tools: Vec<ToolDef> =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("pi-tool-catalog"))
                .expect("OpenAI tools array from dump-tools");
        assert!(!tools.is_empty(), "catalogue fixture must not be empty");

        let orig_bits = schema_bits(&Value::Array(
            tools
                .iter()
                .filter_map(|t| t.function.parameters.clone())
                .collect(),
        ));

        let sanitized: Vec<Value> = tools
            .iter()
            .map(|t| {
                t.function
                    .parameters
                    .as_ref()
                    .map(sanitize_schema)
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}))
            })
            .collect();
        assert_bits_retained(
            "sanitize_schema",
            &orig_bits,
            &schema_bits(&Value::Array(sanitized)),
        );

        let catalog = build_catalog(&tools).expect("catalog");
        assert_eq!(catalog.entries.len(), tools.len());
        for t in &tools {
            let transport = catalog.client_to_transport.get(&t.function.name).unwrap();
            assert!(
                transport.starts_with(TRANSPORT_PREFIX),
                "{transport} missing mcp__pi__ prefix"
            );
            assert_eq!(catalog.transport_to_client[transport], t.function.name);
        }
        let catalog_schemas: Vec<Value> = catalog
            .entries
            .iter()
            .map(|e| e["inputSchema"].clone())
            .collect();
        assert_bits_retained(
            "mcp catalog aliasing",
            &orig_bits,
            &schema_bits(&Value::Array(catalog_schemas.clone())),
        );

        let decoded: Vec<Value> = catalog_schemas
            .iter()
            .map(|s| decode_protobuf_value(&encode_protobuf_value(s), 0))
            .collect();
        assert_bits_retained(
            "cursor google.protobuf.Value",
            &orig_bits,
            &schema_bits(&Value::Array(decoded)),
        );
    }

    struct SchemaBits {
        properties: BTreeSet<String>,
        enums: BTreeSet<String>,
        required: BTreeSet<String>,
    }

    fn schema_bits(v: &Value) -> SchemaBits {
        let mut properties = BTreeSet::new();
        let mut enums = BTreeSet::new();
        let mut required = BTreeSet::new();
        walk_schema(v, &mut properties, &mut enums, &mut required);
        SchemaBits {
            properties,
            enums,
            required,
        }
    }

    fn walk_schema(
        v: &Value,
        properties: &mut BTreeSet<String>,
        enums: &mut BTreeSet<String>,
        required: &mut BTreeSet<String>,
    ) {
        match v {
            Value::Object(m) => {
                if let Some(p) = m.get("properties").and_then(Value::as_object) {
                    for (k, child) in p {
                        properties.insert(k.clone());
                        walk_schema(child, properties, enums, required);
                    }
                }
                if let Some(e) = m.get("enum").and_then(Value::as_array) {
                    for item in e {
                        enums.insert(item.to_string());
                    }
                }
                if let Some(r) = m.get("required").and_then(Value::as_array) {
                    for item in r {
                        if let Some(s) = item.as_str() {
                            required.insert(s.to_string());
                        }
                    }
                }
                for (k, child) in m {
                    if k != "properties" && k != "enum" && k != "required" {
                        walk_schema(child, properties, enums, required);
                    }
                }
            }
            Value::Array(a) => {
                for child in a {
                    walk_schema(child, properties, enums, required);
                }
            }
            _ => {}
        }
    }

    fn assert_bits_retained(stage: &str, orig: &SchemaBits, after: &SchemaBits) {
        assert!(
            orig.properties.is_subset(&after.properties),
            "{stage} dropped properties {:?} → {:?}",
            orig.properties
                .difference(&after.properties)
                .collect::<Vec<_>>(),
            after.properties
        );
        assert!(
            orig.enums.is_subset(&after.enums),
            "{stage} dropped enum entries {:?} → {:?}",
            orig.enums.difference(&after.enums).collect::<Vec<_>>(),
            after.enums
        );
        assert!(
            orig.required.is_subset(&after.required),
            "{stage} dropped required {:?} → {:?}",
            orig.required
                .difference(&after.required)
                .collect::<Vec<_>>(),
            after.required
        );
    }
}
