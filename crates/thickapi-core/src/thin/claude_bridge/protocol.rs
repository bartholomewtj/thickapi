//! Port of `claude-protocol.ts`: init-record validation, advisory rate-limit notices and
//! terminal-result detail. Every failed assertion is named so a `502 vendor_drift` body says
//! exactly what moved.

use std::collections::BTreeSet;

use serde_json::Value;

/// A named protocol assertion failure (`code` mirrors upstream's `ClaudeCodeError` codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: &'static str,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub struct InitExpectation<'a> {
    pub tools: &'a BTreeSet<String>,
    /// `true` when the Pi proposal server was configured (non-empty catalog).
    pub mcp_pi: bool,
    pub private_paths: &'a [String],
    /// Extra observed tool names to warn-and-continue (SPEC §19.7 `thin.vendors.claude.bridge.tolerate`).
    pub extra_tools: &'a [String],
    /// Inventory keys (`slash_commands` / `skills` / `plugins`) allowed to be nonempty.
    pub nonempty_inventories: &'a [String],
}

/// Exact port of `validateClaudeInitialization`. Returns the resolved model.
pub fn validate_init(value: &Value, expect: &InitExpectation<'_>) -> Result<String, ProtocolError> {
    let record = value.as_object().ok_or_else(|| {
        ProtocolError::new("protocol_shape", "Claude initialization must be an object")
    })?;
    if record.get("type").and_then(Value::as_str) != Some("system")
        || record.get("subtype").and_then(Value::as_str) != Some("init")
    {
        return Err(ProtocolError::new(
            "protocol_init",
            "Claude initialization record was invalid",
        ));
    }
    let tools = record
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProtocolError::new("protocol_init", "Claude initialization omitted tools")
        })?;
    let mut observed = BTreeSet::new();
    for tool in tools {
        let name = tool.as_str().ok_or_else(|| {
            ProtocolError::new(
                "protocol_init",
                "Claude initialization contained an invalid tool name",
            )
        })?;
        observed.insert(name.to_string());
    }
    if observed.len() != tools.len() {
        return Err(ProtocolError::new(
            "isolation_tools",
            format!(
                "Claude Code initialized with an unexpected tool set (expected: {}; observed: {})",
                format_names(expect.tools),
                format_names(&observed)
            ),
        ));
    }
    if observed != *expect.tools {
        let extra: BTreeSet<String> = observed.difference(expect.tools).cloned().collect();
        let missing: BTreeSet<String> = expect.tools.difference(&observed).cloned().collect();
        let tolerated: BTreeSet<String> = expect.extra_tools.iter().cloned().collect();
        if !missing.is_empty() || !extra.iter().all(|t| tolerated.contains(t)) {
            return Err(ProtocolError::new(
                "isolation_tools",
                format!(
                    "Claude Code initialized with an unexpected tool set (expected: {}; observed: {})",
                    format_names(expect.tools),
                    format_names(&observed)
                ),
            ));
        }
        tracing::warn!(
            extra = %format_names(&extra),
            "Claude init included extra tools {}; continuing (thin.bridge.tolerate.extra_tools)",
            format_names(&extra)
        );
    }
    if record.get("permissionMode").and_then(Value::as_str) != Some("dontAsk") {
        return Err(ProtocolError::new(
            "isolation_permissions",
            "Claude Code did not enter dontAsk permission mode (permissionMode != \"dontAsk\")",
        ));
    }
    let inventories = ["slash_commands", "skills", "plugins"];
    for key in inventories {
        let Some(list) = record.get(key).and_then(Value::as_array) else {
            return Err(ProtocolError::new(
                "protocol_init",
                format!("Claude initialization omitted customization inventories ({key})"),
            ));
        };
        // Claude Code 2.1.283 lists plugins that ship inside the CLI (`path: "builtin"`,
        // `source: "<name>@builtin"`). They are part of the binary, not a user customization.
        let loaded = list
            .iter()
            .filter(|entry| !is_builtin_plugin(key, entry))
            .count();
        if loaded > 0 {
            if expect.nonempty_inventories.iter().any(|k| k == key) {
                tracing::warn!(
                    inventory = key,
                    entries = loaded,
                    "Claude init nonempty {key} ({loaded} entries); continuing (thin.bridge.tolerate.nonempty_inventories)"
                );
            } else {
                return Err(ProtocolError::new(
                    "isolation_customizations",
                    format!(
                        "Claude Code loaded unexpected customizations ({key} has {loaded} entries)"
                    ),
                ));
            }
        }
    }
    let api_key_source = record.get("apiKeySource").and_then(Value::as_str);
    if api_key_source != Some("none") {
        return Err(ProtocolError::new(
            "isolation_auth",
            format!(
                "init.apiKeySource={} expected \"none\" (Claude Code did not confirm subscription-backed authentication)",
                api_key_source.map(|s| format!("\"{s}\"")).unwrap_or_else(|| "missing".into())
            ),
        ));
    }
    if let Some(errors) = record.get("mcp_server_errors") {
        let list = errors.as_array().ok_or_else(|| {
            ProtocolError::new(
                "protocol_init",
                "Claude initialization contained invalid MCP server errors",
            )
        })?;
        if !list.is_empty() {
            let mut details = Vec::new();
            for err in list {
                details.push(mcp_error_detail(err, expect.private_paths)?);
            }
            return Err(ProtocolError::new(
                "isolation_mcp",
                format!(
                    "Claude Code reported MCP initialization errors: {}",
                    details.join("; ")
                ),
            ));
        }
    }
    let servers = record
        .get("mcp_servers")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProtocolError::new(
                "protocol_init",
                "Claude initialization omitted MCP inventory",
            )
        })?;
    if !expect.mcp_pi {
        if !servers.is_empty() {
            return Err(ProtocolError::new(
                "isolation_mcp",
                "Claude Code loaded an unexpected MCP server",
            ));
        }
    } else {
        let ok = servers.len() == 1
            && servers
                .first()
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                == Some("pi")
            && servers
                .first()
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                == Some("connected");
        if !ok {
            return Err(ProtocolError::new(
                "isolation_mcp",
                format!(
                    "The Pi proposal MCP server did not initialize correctly (mcp_servers={})",
                    serde_json::to_string(servers).unwrap_or_default()
                ),
            ));
        }
    }
    match record.get("model").and_then(Value::as_str) {
        Some(model) if !model.is_empty() => Ok(model.to_string()),
        _ => Err(ProtocolError::new(
            "protocol_init",
            "Claude initialization omitted the resolved model",
        )),
    }
}

fn mcp_error_detail(value: &Value, private_paths: &[String]) -> Result<String, ProtocolError> {
    let invalid = || {
        ProtocolError::new(
            "protocol_init",
            "Claude initialization contained an invalid MCP server error",
        )
    };
    let obj = value.as_object().ok_or_else(invalid)?;
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(invalid)?;
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(invalid)?;
    Ok(format!(
        "{}: {}",
        safe_diagnostic(kind, private_paths, 64),
        safe_diagnostic(message, private_paths, 512)
    ))
}

/// Longest Claude CLI error text (`result`, assistant text, `errors[]`) that reaches a client
/// or a log (R14).
pub const CLAUDE_ERROR_MAX: usize = 500;

/// CLI error text for the client: private paths removed (as written, with forward slashes,
/// and JSON-escaped), then tokens, bearer headers and emails redacted, then capped (R14).
pub fn client_safe_error(text: &str, private_paths: &[String]) -> String {
    let mut paths = Vec::new();
    for path in private_paths.iter().filter(|p| !p.is_empty()) {
        paths.push(path.clone());
        paths.push(path.replace('\\', "/"));
        paths.push(path.replace('\\', r"\\"));
    }
    let safe = safe_diagnostic(text, &paths, usize::MAX);
    crate::thin::oauth::state::redact(&safe)
        .chars()
        .take(CLAUDE_ERROR_MAX)
        .collect()
}

/// Strip control characters, redact private paths, truncate.
pub fn safe_diagnostic(value: &str, private_paths: &[String], limit: usize) -> String {
    let mut safe: String = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut paths: Vec<&String> = private_paths.iter().filter(|p| !p.is_empty()).collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for path in paths {
        safe = safe.replace(path.as_str(), "<PRIVATE>");
    }
    safe.chars().take(limit).collect()
}

fn format_names(names: &BTreeSet<String>) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitNotice {
    pub rejected: bool,
    pub rate_limit_type: String,
    /// Fraction 0..=1.
    pub utilization: Option<f64>,
    /// Unix seconds.
    pub resets_at: Option<u64>,
    pub overage_disabled_reason: Option<String>,
}

fn alert_status(v: Option<&Value>) -> Option<&str> {
    match v.and_then(Value::as_str) {
        Some(s @ ("allowed_warning" | "rejected")) => Some(s),
        _ => None,
    }
}

fn valid_timestamp(v: Option<&Value>) -> Option<u64> {
    let n = v.and_then(Value::as_f64)?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    // Wire value is Unix seconds; tolerate a future epoch-millisecond value.
    let secs = if n >= 100_000_000_000.0 {
        n / 1000.0
    } else {
        n
    };
    Some(secs as u64)
}

/// Port of `parseRateLimitNotice`. Advisory: malformed details yield `None`, never an error.
pub fn parse_rate_limit_notice(info: &Value) -> Option<RateLimitNotice> {
    let rate = info.as_object()?;
    let primary = alert_status(rate.get("status"));
    let overage = alert_status(rate.get("overageStatus"));
    let using_overage = rate.get("isUsingOverage").and_then(Value::as_bool) == Some(true);
    let overage_blocking =
        overage == Some("rejected") && (primary == Some("rejected") || using_overage);
    if primary.is_none() && !overage_blocking {
        return None;
    }
    let rejected = primary == Some("rejected") || overage_blocking;
    let has_primary = primary.is_some();
    let overage_relevant = overage.is_some() && (primary == Some("rejected") || using_overage);
    let rate_limit_type = if has_primary {
        rate.get("rateLimitType")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string()
    } else {
        "overage".to_string()
    };
    let resets_at = valid_timestamp(if has_primary {
        rate.get("resetsAt")
    } else {
        rate.get("overageResetsAt")
    });
    let utilization = if has_primary {
        rate.get("utilization")
            .and_then(Value::as_f64)
            .filter(|u| u.is_finite() && (0.0..=1.0).contains(u))
    } else {
        None
    };
    let reason = if overage_relevant {
        rate.get("overageDisabledReason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    } else {
        None
    };
    Some(RateLimitNotice {
        rejected,
        rate_limit_type,
        utilization,
        resets_at,
        overage_disabled_reason: reason,
    })
}

/// Port of `terminalResultErrorDetail`.
pub fn terminal_result_detail(
    record: &Value,
    assistant_diagnostic: Option<&str>,
    rate_limit_failure: Option<&str>,
) -> String {
    let result = record
        .get("result")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let errors = record
        .get("errors")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .filter(|s| !s.is_empty());
    let terminal = record
        .get("terminal_reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    result
        .map(str::to_string)
        .or_else(|| assistant_diagnostic.map(str::to_string))
        .or(errors)
        .or(terminal)
        .or_else(|| rate_limit_failure.map(str::to_string))
        .unwrap_or_else(|| "unknown error".into())
}

/// ISO-8601 (UTC, seconds) from Unix seconds, for `x-thickapi-rate-limit` and error text.
pub fn iso8601(unix: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix as i64, 0)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// A plugin that ships inside the Claude Code binary (`path: "builtin"` or a `…@builtin`
/// source). Only the `plugins` inventory has these.
fn is_builtin_plugin(key: &str, entry: &Value) -> bool {
    key == "plugins"
        && (entry.get("path").and_then(Value::as_str) == Some("builtin")
            || entry
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|s| s.ends_with("@builtin")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn good_init() -> Value {
        json!({
            "type": "system", "subtype": "init",
            "tools": ["mcp__pi__read", "mcp__pi__bash"],
            "permissionMode": "dontAsk",
            "slash_commands": [], "skills": [], "plugins": [],
            "apiKeySource": "none",
            "mcp_servers": [{"name": "pi", "status": "connected"}],
            "model": "claude-sonnet-4-6"
        })
    }

    #[test]
    fn valid_init_returns_model() {
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        assert_eq!(
            validate_init(&good_init(), &expect).unwrap(),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn extra_tool_is_isolation_failure() {
        let mut init = good_init();
        init["tools"].as_array_mut().unwrap().push(json!("Bash"));
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let err = validate_init(&init, &expect).unwrap_err();
        assert_eq!(err.code, "isolation_tools");
        assert!(err
            .message
            .contains("observed: Bash, mcp__pi__bash, mcp__pi__read"));
    }

    #[test]
    fn extra_tool_in_tolerate_list_warns_and_continues() {
        let mut init = good_init();
        init["tools"].as_array_mut().unwrap().push(json!("Foo"));
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let extra = vec!["Foo".to_string()];
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &extra,
            nonempty_inventories: &[],
        };
        let warns = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = WarnSink(warns.clone());
        tracing::subscriber::with_default(sink, || {
            assert_eq!(validate_init(&init, &expect).unwrap(), "claude-sonnet-4-6");
        });
        let lines = warns.lock().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("tolerate.extra_tools") && l.contains("Foo")),
            "Foo extra tool must warn-and-continue: {lines:?}"
        );
    }

    struct WarnSink(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tracing::Subscriber for WarnSink {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut msg = String::new();
            event.record(&mut MsgVisit(&mut msg));
            self.0.lock().unwrap().push(msg);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct MsgVisit<'a>(&'a mut String);
    impl tracing::field::Visit for MsgVisit<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{}={value:?}", field.name()));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{}={value}", field.name()));
        }
    }

    #[test]
    fn nonempty_inventory_in_tolerate_list_warns_and_continues() {
        let mut init = good_init();
        init["skills"] = json!(["x"]);
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let inventories = vec!["skills".to_string()];
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &inventories,
        };
        assert_eq!(validate_init(&init, &expect).unwrap(), "claude-sonnet-4-6");
    }

    #[test]
    fn api_key_source_oauth_names_the_assertion() {
        let mut init = good_init();
        init["apiKeySource"] = json!("ANTHROPIC_API_KEY");
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let err = validate_init(&init, &expect).unwrap_err();
        assert_eq!(err.code, "isolation_auth");
        assert!(err
            .message
            .contains("init.apiKeySource=\"ANTHROPIC_API_KEY\" expected \"none\""));
    }

    #[test]
    fn customizations_and_permission_mode_checked() {
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let mut init = good_init();
        init["skills"] = json!(["x"]);
        assert_eq!(
            validate_init(&init, &expect).unwrap_err().code,
            "isolation_customizations"
        );
        // Claude Code 2.1.283 lists plugins built into the CLI; those are not customizations.
        let mut init = good_init();
        init["plugins"] = json!([
            {"name": "agents-md", "path": "builtin", "source": "agents-md@builtin"},
            {"name": "telemetry", "path": "builtin", "source": "telemetry@builtin"}
        ]);
        assert!(validate_init(&init, &expect).is_ok());
        let mut init = good_init();
        init["plugins"] = json!([
            {"name": "agents-md", "path": "builtin", "source": "agents-md@builtin"},
            {"name": "mine", "path": "C:/plugins/mine", "source": "mine@local"}
        ]);
        let err = validate_init(&init, &expect).unwrap_err();
        assert_eq!(err.code, "isolation_customizations");
        assert!(err.message.contains("plugins has 1 entries"));
        let mut init = good_init();
        init["permissionMode"] = json!("bypassPermissions");
        assert_eq!(
            validate_init(&init, &expect).unwrap_err().code,
            "isolation_permissions"
        );
    }

    #[test]
    fn mcp_server_must_be_pi_connected_when_catalog_present() {
        let expected = tools(&["mcp__pi__read", "mcp__pi__bash"]);
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let mut init = good_init();
        init["mcp_servers"] = json!([{"name": "pi", "status": "failed"}]);
        assert_eq!(
            validate_init(&init, &expect).unwrap_err().code,
            "isolation_mcp"
        );
        let mut init = good_init();
        init["mcp_server_errors"] = json!([{"type": "spawn", "message": "ENOENT C:\\priv\\x"}]);
        let paths = vec!["C:\\priv".to_string()];
        let expect = InitExpectation {
            tools: &expected,
            mcp_pi: true,
            private_paths: &paths,
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let err = validate_init(&init, &expect).unwrap_err();
        assert_eq!(err.code, "isolation_mcp");
        assert!(err.message.contains("<PRIVATE>"), "{}", err.message);
    }

    #[test]
    fn no_catalog_means_no_mcp_servers() {
        let empty = BTreeSet::new();
        let expect = InitExpectation {
            tools: &empty,
            mcp_pi: false,
            private_paths: &[],
            extra_tools: &[],
            nonempty_inventories: &[],
        };
        let mut init = good_init();
        init["tools"] = json!([]);
        assert_eq!(
            validate_init(&init, &expect).unwrap_err().code,
            "isolation_mcp"
        );
        init["mcp_servers"] = json!([]);
        assert!(validate_init(&init, &expect).is_ok());
    }

    #[test]
    fn rate_limit_notice_parses_primary_and_overage() {
        let n = parse_rate_limit_notice(&json!({
            "status": "allowed_warning", "rateLimitType": "five_hour",
            "utilization": 0.83, "resetsAt": 1_800_000_000
        }))
        .unwrap();
        assert!(!n.rejected);
        assert_eq!(n.utilization, Some(0.83));
        assert_eq!(n.resets_at, Some(1_800_000_000));
        let rejected = parse_rate_limit_notice(&json!({
            "status": "rejected", "rateLimitType": "seven_day",
            "overageStatus": "rejected", "overageDisabledReason": "org_level_disabled"
        }))
        .unwrap();
        assert!(rejected.rejected);
        assert_eq!(
            rejected.overage_disabled_reason.as_deref(),
            Some("org_level_disabled")
        );
        assert!(parse_rate_limit_notice(&json!({"status": "allowed"})).is_none());
        assert!(parse_rate_limit_notice(&json!({"overageStatus": "rejected"})).is_none());
    }

    #[test]
    fn terminal_detail_prefers_result_text() {
        let record = json!({"result": " boom ", "errors": ["e1"], "terminal_reason": "x"});
        assert_eq!(terminal_result_detail(&record, None, None), "boom");
        let record = json!({"errors": ["e1", " e2 "]});
        assert_eq!(terminal_result_detail(&record, None, None), "e1; e2");
        assert_eq!(terminal_result_detail(&json!({}), None, Some("rl")), "rl");
        assert_eq!(
            terminal_result_detail(&json!({}), None, None),
            "unknown error"
        );
    }
}
