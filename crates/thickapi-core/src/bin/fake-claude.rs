//! CI stub for the thin Claude bridge (SPEC §19.15). Speaks stream-json on stdout,
//! optionally touches the proposal MCP ready file, then exits 0.
//!
//! Not installed as a user command. Tests point `thin.vendors.claude.path` at this binary.
//!
//! `--model fake-empty-always` answers every run with an empty `end_turn` stop (SPEC §19.16);
//! `--model fake-empty-once` does that on the first run in a tmp root and answers the text turn
//! after (the run count lives next to the private dirs, in the cwd's parent).
//!
//! `--model fake-open-text-block` answers the text turn without its `content_block_stop`
//! (the Claude Code 2.1.283 shape of #145); `--model fake-open-tool-block` proposes the first
//! catalog tool and signals `stop_reason: tool_use` with that `tool_use` block still open.
//!
//! `--model fake-two-tool-turn` prints `fixtures/thin/claude-bridge/two-tool-turn.jsonl` as
//! recorded (C4 publish parity: the live run and `replay` must agree on it).
//!
//! `--model fake-text-then-tool` answers with text and an `end_turn` stop on the first run in
//! a tmp root, and proposes the first catalog tool on a later run (the private dir of a retry
//! ends `-r{n}`). `--model fake-text-always` is that text stop on every run.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const TWO_TOOL_TURN: &str =
    include_str!("../../../../fixtures/thin/claude-bridge/two-tool-turn.jsonl");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(ready) = ready_path(&args) {
        let _ = std::fs::write(ready, b"ready\n");
    }
    // Drain the prompt the bridge writes, then EOF. Kept so a later model can record it.
    let stdin = io::stdin();
    let mut prompt = String::new();
    for line in stdin.lock().lines().map_while(Result::ok) {
        prompt.push_str(&line);
        prompt.push('\n');
    }
    let tools = catalog_tools(&args);
    let model = model_flag(&args).unwrap_or_else(|| "claude-sonnet-4-6".into());
    let text = "Hello from the bridge.";
    let out = io::stdout();
    let mut out = out.lock();
    let tools_json = serde_json::to_string(&tools).unwrap_or_else(|_| "[]".into());
    let mcp = if tools.is_empty() {
        "[]".to_string()
    } else {
        r#"[{"name":"pi","status":"connected"}]"#.to_string()
    };
    if text_then_tool(&model, &args) {
        if retry_dir(&args) {
            write_tool_call(&mut out, &model, &tools_json, &mcp, &tools);
        } else {
            write_text_stop(&mut out, &model, &tools_json, &mcp, "No.");
        }
        return;
    }
    if model == "fake-text-always" {
        write_text_stop(&mut out, &model, &tools_json, &mcp, "No.");
        return;
    }
    if empty_stop(&model) {
        let records = [
            format!(
                r#"{{"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":{tools_json},"mcp_servers":{mcp},"model":"{model}","permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}}"#
            ),
            format!(
                r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_00","type":"message","role":"assistant","model":"{model}","content":[],"stop_reason":null,"usage":{{"input_tokens":99,"output_tokens":1}}}}}},"session_id":"s1","uuid":"u2"}}"#
            ),
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":99,"output_tokens":3}},"session_id":"s1","uuid":"u3"}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u4"}"#.to_string(),
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"duration_api_ms":10,"num_turns":1,"result":"","stop_reason":"end_turn","session_id":"s1","total_cost_usd":0.0,"usage":{"input_tokens":99,"output_tokens":3},"uuid":"u5"}"#.to_string(),
        ];
        for line in records {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        return;
    }
    if let Some(proposal) = propose(&model) {
        write_proposal(&mut out, &model, &tools_json, &mcp, &proposal);
        return;
    }
    // First run proposes a TodoWrite; a later run (the folded history) answers with text and
    // copies the prompt it was given, so a test can see the single folded call.
    if let Some(spec) = model.strip_prefix("fake-todo-then-text:") {
        // Run 0 proposes. Run 1 is the spawn that should already see the folded TodoWrite,
        // so it records that prompt and answers with text. The counter lives next to the
        // private dirs, in the cwd's parent, the same place `fake-propose-twice` uses.
        let runs = bump_counter("fake-claude-todo-runs");
        if runs == 0 {
            if let Some((name, todo_args)) = spec.split_once(':') {
                write_proposal(
                    &mut out,
                    &model,
                    &tools_json,
                    &mcp,
                    &(name.to_string(), todo_args.to_string()),
                );
            }
        } else {
            let _ = std::fs::write(seen_prompt_path(), &prompt);
            write_text_stop(&mut out, &model, &tools_json, &mcp, "Done.");
        }
        return;
    }
    if model == "fake-two-tool-turn" {
        for line in TWO_TOOL_TURN.lines().filter(|l| !l.trim().is_empty()) {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        return;
    }
    if model == "fake-open-tool-block" {
        let tool = tools
            .first()
            .cloned()
            .unwrap_or_else(|| "mcp__pi__read".into());
        let records = [
            format!(
                r#"{{"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":{tools_json},"mcp_servers":{mcp},"model":"{model}","permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}}"#
            ),
            format!(
                r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_02","type":"message","role":"assistant","model":"{model}","content":[],"stop_reason":null,"usage":{{"input_tokens":1,"output_tokens":1}}}}}},"session_id":"s1","uuid":"u2"}}"#
            ),
            format!(
                r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_01","name":"{tool}","input":{{}}}}}},"session_id":"s1","uuid":"u3"}}"#
            ),
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}},"session_id":"s1","uuid":"u4"}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":4}},"session_id":"s1","uuid":"u5"}"#.to_string(),
            r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u6"}"#.to_string(),
        ];
        for line in records {
            let _ = writeln!(out, "{line}");
        }
        let _ = out.flush();
        return;
    }
    let open_text_block = model == "fake-open-text-block";
    let records = [
        format!(
            r#"{{"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":{tools_json},"mcp_servers":{mcp},"model":"{model}","permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}}"#
        ),
        format!(
            r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_01","type":"message","role":"assistant","model":"{model}","content":[],"stop_reason":null,"usage":{{"input_tokens":1,"output_tokens":1}}}}}},"session_id":"s1","uuid":"u2"}}"#
        ),
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"s1","uuid":"u3"}"#.to_string(),
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}},"session_id":"s1","uuid":"u4"}}"#
        ),
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s1","uuid":"u5"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":4}},"session_id":"s1","uuid":"u6"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u7"}"#.to_string(),
        format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"duration_api_ms":10,"num_turns":1,"result":"{text}","stop_reason":"end_turn","session_id":"s1","total_cost_usd":0.0,"usage":{{"input_tokens":1,"output_tokens":4}},"uuid":"u8"}}"#
        ),
    ];
    for line in records {
        if open_text_block && line.contains(r#""type":"content_block_stop""#) {
            continue;
        }
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

/// `fake-propose:<Name>:<json args>`, or the same text in `FAKE_CLAUDE_PROPOSE`.
/// Proposes `mcp__pi__<Name>` with those args and stops on `tool_use`.
/// `fake-propose-twice:` proposes the bad args on the first run in this tmp root and the
/// corrected args on the second, so a stub can drive the one dialect retry.
fn propose(model: &str) -> Option<(String, String)> {
    if let Some(spec) = model.strip_prefix("fake-propose-twice:") {
        return propose_twice(spec);
    }
    let spec = model
        .strip_prefix("fake-propose:")
        .map(str::to_string)
        .or_else(|| std::env::var("FAKE_CLAUDE_PROPOSE").ok())?;
    let (name, args) = spec.split_once(':')?;
    if name.is_empty() || serde_json::from_str::<serde_json::Value>(args).is_err() {
        return None;
    }
    Some((name.to_string(), args.to_string()))
}

/// First run: the spec as given (the bad call). Second run: `file_path`/`old_string`/`new_string`
/// when the spec used Pi's names, else the spec unchanged.
fn propose_twice(spec: &str) -> Option<(String, String)> {
    let (name, args) = spec.split_once(':')?;
    let counter = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.parent().map(|p| p.join("fake-claude-propose-runs")))?;
    let runs: u32 = std::fs::read_to_string(&counter)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&counter, format!("{}", runs + 1));
    let args = if runs == 0 {
        args.to_string()
    } else {
        corrected_edit_args(args)
    };
    Some((name.to_string(), args))
}

fn corrected_edit_args(args: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.to_string();
    };
    let Some(obj) = value.as_object_mut() else {
        return args.to_string();
    };
    if let Some(path) = obj.remove("path") {
        obj.insert("file_path".into(), path);
    }
    if let Some(old) = obj.remove("oldText") {
        obj.insert("old_string".into(), old);
    }
    if let Some(new) = obj.remove("newText") {
        obj.insert("new_string".into(), new);
    }
    value.to_string()
}

fn write_proposal(
    out: &mut impl Write,
    model: &str,
    tools_json: &str,
    mcp: &str,
    proposal: &(String, String),
) {
    let (name, args) = proposal;
    let tool = format!("mcp__pi__{name}");
    let tools: serde_json::Value =
        serde_json::from_str(tools_json).unwrap_or_else(|_| serde_json::json!([]));
    let servers: serde_json::Value =
        serde_json::from_str(mcp).unwrap_or_else(|_| serde_json::json!([]));
    let records = [
        serde_json::json!({"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":tools,"mcp_servers":servers,"model":model,"permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}),
        serde_json::json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_03","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":1}}},"session_id":"s1","uuid":"u2"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"s1","uuid":"u3"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Trying."}},"session_id":"s1","uuid":"u4"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s1","uuid":"u5"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_prop","name":tool,"input":{}}},"session_id":"s1","uuid":"u6"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":args}},"session_id":"s1","uuid":"u7"}),
        serde_json::json!({"type":"stream_event","event":{"type":"content_block_stop","index":1},"session_id":"s1","uuid":"u8"}),
        serde_json::json!({"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":1,"output_tokens":4}},"session_id":"s1","uuid":"u9"}),
        serde_json::json!({"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u10"}),
    ];
    for record in records {
        let _ = writeln!(out, "{record}");
    }
    let _ = out.flush();
}

/// `fake-text-then-tool`: text on the first private dir, a tool call on a `-r{n}` retry.
fn text_then_tool(model: &str, _args: &[String]) -> bool {
    model == "fake-text-then-tool"
}

fn bump_counter(name: &str) -> u32 {
    let path = std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.parent().map(|p| p.join(name)))
        .unwrap_or_else(|| PathBuf::from(name));
    let runs: u32 = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&path, format!("{}", runs + 1));
    runs
}

/// Where the stub leaves the prompt of a `fake-todo-then-text` retry, next to the run counter.
fn seen_prompt_path() -> PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.parent().map(|p| p.join("fake-claude-seen-prompt.txt")))
        .unwrap_or_else(|| PathBuf::from("fake-claude-seen-prompt.txt"))
}

/// A §19.16 retry's private dir is `{id}-r{n}` (the bridge's cwd).
fn retry_dir(args: &[String]) -> bool {
    let cwd = std::env::current_dir().ok();
    let name = cwd
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let _ = args;
    name.rsplit_once("-r")
        .is_some_and(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn write_text_stop(out: &mut impl Write, model: &str, tools_json: &str, mcp: &str, text: &str) {
    let model = serde_json::to_string(model).unwrap_or_else(|_| "\"\".".into());
    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "\"\".".into());
    let delta = serde_json::json!({
        "type": "stream_event",
        "event": {"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": text}},
        "session_id": "s1", "uuid": "u4"
    });
    let records = [
        format!(
            r#"{{"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":{tools_json},"mcp_servers":{mcp},"model":{model},"permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}}"#
        ),
        format!(
            r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_04","type":"message","role":"assistant","model":{model},"content":[],"stop_reason":null,"usage":{{"input_tokens":2,"output_tokens":1}}}}}},"session_id":"s1","uuid":"u2"}}"#
        ),
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"session_id":"s1","uuid":"u3"}"#.to_string(),
        delta.to_string(),
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s1","uuid":"u5"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":2,"output_tokens":2}},"session_id":"s1","uuid":"u6"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u7"}"#.to_string(),
        format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"duration_api_ms":10,"num_turns":1,"result":{text_json},"stop_reason":"end_turn","session_id":"s1","total_cost_usd":0.0,"usage":{{"input_tokens":2,"output_tokens":2}},"uuid":"u8"}}"#
        ),
    ];
    for line in records {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

/// A closed `tool_use` of the first catalog tool, then `stop_reason: tool_use`.
fn write_tool_call(
    out: &mut impl Write,
    model: &str,
    tools_json: &str,
    mcp: &str,
    tools: &[String],
) {
    let tool = tools
        .first()
        .cloned()
        .unwrap_or_else(|| "mcp__pi__read".into());
    let args = if tool.ends_with("Edit") {
        serde_json::json!({"file_path": "a.txt", "old_string": "x", "new_string": "y"})
    } else {
        serde_json::json!({"path": "a.txt"})
    };
    let delta = serde_json::json!({
        "type": "stream_event",
        "event": {"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": args.to_string()}},
        "session_id": "s1", "uuid": "u4"
    });
    let records = [
        format!(
            r#"{{"type":"system","subtype":"init","cwd":".","session_id":"s1","tools":{tools_json},"mcp_servers":{mcp},"model":"{model}","permissionMode":"dontAsk","slash_commands":[],"apiKeySource":"none","skills":[],"plugins":[],"output_style":"default","uuid":"u1"}}"#
        ),
        format!(
            r#"{{"type":"stream_event","event":{{"type":"message_start","message":{{"id":"msg_05","type":"message","role":"assistant","model":"{model}","content":[],"stop_reason":null,"usage":{{"input_tokens":3,"output_tokens":1}}}}}},"session_id":"s1","uuid":"u2"}}"#
        ),
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_retry","name":"{tool}","input":{{}}}}}},"session_id":"s1","uuid":"u3"}}"#
        ),
        delta.to_string(),
        r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0},"session_id":"s1","uuid":"u5"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":3,"output_tokens":4}},"session_id":"s1","uuid":"u6"}"#.to_string(),
        r#"{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s1","uuid":"u7"}"#.to_string(),
    ];
    for line in records {
        let _ = writeln!(out, "{line}");
    }
    let _ = out.flush();
}

/// Whether this run answers with an empty stop (see the module docs).
fn empty_stop(model: &str) -> bool {
    match model {
        "fake-empty-always" => true,
        "fake-empty-once" => {
            let Some(counter) = std::env::current_dir()
                .ok()
                .and_then(|cwd| cwd.parent().map(|p| p.join("fake-claude-runs")))
            else {
                return false;
            };
            let runs: u32 = std::fs::read_to_string(&counter)
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0);
            let _ = std::fs::write(&counter, format!("{}", runs + 1));
            runs == 0
        }
        _ => false,
    }
}

fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2).find_map(|w| match w {
        [flag, value] if flag == name => Some(value.as_str()),
        _ => None,
    })
}

fn model_flag(args: &[String]) -> Option<String> {
    flag_value(args, "--model").map(str::to_string)
}

fn ready_path(args: &[String]) -> Option<PathBuf> {
    let json = flag_value(args, "--mcp-config")?;
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let argv = v
        .pointer("/mcpServers/pi/args")
        .and_then(|a| a.as_array())?;
    argv.windows(2).find_map(|w| match w {
        [flag, value] if flag.as_str() == Some("--ready") => {
            Some(PathBuf::from(value.as_str().unwrap_or_default()))
        }
        _ => None,
    })
}

fn catalog_tools(args: &[String]) -> Vec<String> {
    let Some(json) = flag_value(args, "--mcp-config") else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(argv) = v.pointer("/mcpServers/pi/args").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    let Some(catalog) = argv.windows(2).find_map(|w| match w {
        [flag, value] if flag.as_str() == Some("--catalog") => {
            Some(PathBuf::from(value.as_str().unwrap_or_default()))
        }
        _ => None,
    }) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(catalog) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
        .map(|n| {
            if n.starts_with("mcp__pi__") {
                n.to_string()
            } else {
                format!("mcp__pi__{n}")
            }
        })
        .collect()
}
