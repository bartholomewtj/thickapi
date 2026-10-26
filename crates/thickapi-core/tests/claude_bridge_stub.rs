#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! Process+protocol test against the `fake-claude` stub (SPEC §19.15, §19.7 capture #145).
//! Does not spawn the live `claude.exe` or bind the live daemon's port.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;
use thickapi_core::config::models::ModelEntry;
use thickapi_core::thin::claude_bridge::{BridgeSettings, ClaudeBridge};
use thickapi_core::thin::openai::FinishReason;
use thickapi_core::thin::{ThinEvent, ThinRequest};
use thickapi_core::thinking::Effort;

fn stub_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-claude"))
}

fn settings(tmp: &tempfile::TempDir) -> BridgeSettings {
    BridgeSettings {
        tmp_root: tmp.path().to_path_buf(),
        claude_path: Some(stub_exe()),
        thickapi_exe: None,
        total_timeout: Duration::from_secs(15),
        idle_timeout: Duration::from_secs(10),
        mcp_ready_timeout: Duration::from_millis(500),
        trace: false,
        bridge_max_concurrent: 2,
        queue_timeout: Duration::from_secs(5),
        handoff_drain: BridgeSettings::default_handoff_drain(),
        extra_tools: vec![],
        nonempty_inventories: vec![],
        dialect: thickapi_core::thin::dialect::MODE_OFF.to_string(),
        catalogue_ids: vec![],
    }
}

fn request(prompt: &str) -> ThinRequest {
    let request: thickapi_core::thin::openai::ChatRequest = serde_json::from_value(json!({
        "model": "m",
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": 32
    }))
    .unwrap();
    ThinRequest {
        request,
        entry: ModelEntry::bridge_probe("claude-sonnet-4-6"),
        effort: Some(Effort::Off),
        request_id: "stub-1".into(),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    }
}

#[tokio::test]
async fn fake_claude_text_turn_starts_and_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = ClaudeBridge::new(settings(&tmp));
    let req = request("Reply OK");
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    bridge.run_bridge(&req, &tx).await.expect("stub run");
    drop(tx);
    let mut started = false;
    let mut text = String::new();
    let mut finished = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            ThinEvent::Started { path, .. } => {
                started = true;
                assert_eq!(path, "bridge");
            }
            ThinEvent::TextDelta(t) => text.push_str(&t),
            ThinEvent::Finish(_) => finished = true,
            ThinEvent::Error(e) => panic!("stub error: {e:?}"),
            _ => {}
        }
    }
    assert!(started, "Started from init");
    assert_eq!(text, "Hello from the bridge.");
    assert!(finished);
    assert_eq!(bridge.state().snapshot()["status"], "ok");
}

// ---- #145: a content block left open at the stop ----

fn request_on(model: &str, tools: bool) -> ThinRequest {
    let mut body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "Reply OK"}],
        "max_tokens": 32
    });
    if tools {
        body["tools"] = json!([{"type": "function", "function": {"name": "read",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]);
    }
    ThinRequest {
        request: serde_json::from_value(body).unwrap(),
        entry: ModelEntry::bridge_probe(model),
        effort: Some(Effort::Off),
        request_id: format!("stub-{model}"),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    }
}

async fn run_stub(bridge: &ClaudeBridge, req: &ThinRequest) -> Vec<ThinEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    bridge.run_bridge(req, &tx).await.expect("stub run");
    drop(tx);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    events
}

#[tokio::test]
async fn open_text_block_at_message_stop_finishes_stop_with_the_text() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = ClaudeBridge::new(settings(&tmp));
    let events = run_stub(&bridge, &request_on("fake-open-text-block", false)).await;
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello from the bridge.");
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::Stop))
    ));
    let ready = bridge.state().snapshot();
    assert_eq!(ready["status"], "ok");
    assert_eq!(ready["skipped"]["blocks"], 1, "{ready}");
}

#[tokio::test]
async fn open_tool_use_block_at_tool_use_stop_is_vendor_drift() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = ClaudeBridge::new(settings(&tmp));
    let events = run_stub(&bridge, &request_on("fake-open-tool-block", true)).await;
    assert!(matches!(events.first(), Some(ThinEvent::Started { .. })));
    let Some(ThinEvent::Error(err)) = events.last() else {
        panic!("expected the error last: {events:?}");
    };
    assert_eq!(err.code, "vendor_drift", "{err:?}");
    assert_eq!(err.status, 502);
    assert!(err.message.contains("protocol_blocks"), "{}", err.message);
    assert!(
        err.message.contains("unclosed content blocks"),
        "{}",
        err.message
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, ThinEvent::ToolCall { .. } | ThinEvent::Finish(_))));
    assert_eq!(bridge.state().snapshot()["skipped"]["blocks"], 0);
}

fn pi_edit() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "edit",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {"type": "string"},
                                "newText": {"type": "string"}
                            }
                        }
                    }
                }
            }
        }
    })
}

fn propose_request(model: &str, tools: serde_json::Value) -> ThinRequest {
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "edit it"}],
        "tools": tools,
        "max_tokens": 32
    });
    ThinRequest {
        request: serde_json::from_value(body).unwrap(),
        entry: ModelEntry::bridge_probe(model),
        effort: Some(Effort::Off),
        request_id: "stub-propose".into(),
        path: "bridge",
        fallback: None,
        cursor_cwd: None,
    }
}

fn dialect_on(tmp: &tempfile::TempDir) -> ClaudeBridge {
    let mut settings = settings(tmp);
    settings.dialect = thickapi_core::thin::dialect::MODE_CLAUDE_CODE.to_string();
    // The stub exits before the handoff kill. A dead child's stdout reaches EOF at once, so a
    // short drain is enough; the loop still reads whatever was buffered.
    settings.handoff_drain = Duration::from_millis(200);
    ClaudeBridge::new(settings)
}

#[tokio::test]
async fn named_choice_writes_one_edit_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let mut settings = settings(&tmp);
    settings.dialect = thickapi_core::thin::dialect::MODE_CLAUDE_CODE.to_string();
    settings.trace = true;
    let bridge = ClaudeBridge::new(settings);
    let mut req = propose_request(
        "claude-sonnet-4-6",
        json!([pi_edit(), {
            "type": "function",
            "function": {"name": "read", "parameters": {"type": "object"}}
        }]),
    );
    req.request.tool_choice = Some(json!({"type": "function", "function": {"name": "edit"}}));
    req.request_id = "stub-named".into();
    let events = run_stub(&bridge, &req).await;
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let text = std::fs::read_to_string(tmp.path().join("stub-named").join("tools.json"))
        .expect("catalog written");
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(rows.len(), 1, "{text}");
    assert_eq!(rows[0]["name"], "Edit");
}

#[tokio::test]
async fn edit_proposal_reaches_the_client_as_a_pi_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let model =
        r#"fake-propose:Edit:{"file_path":"src/main.rs","old_string":"foo","new_string":"bar"}"#;
    let req = propose_request(model, json!([pi_edit()]));
    let names: Vec<&str> = req
        .request
        .tools
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    let active = thickapi_core::thin::dialect::activate(&req.request.tools);
    assert_eq!(names, vec!["edit"], "tools did not parse");
    assert_eq!(active.len(), 1, "row inactive: {active:?}");
    let events = run_stub(&bridge, &req).await;
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let call = events.iter().find_map(|e| match e {
        ThinEvent::ToolCall {
            name, arguments, ..
        } => Some((name, arguments)),
        _ => None,
    });
    let Some((name, arguments)) = call else {
        panic!("no tool call: {events:?}");
    };
    assert_eq!(name, "edit", "events={events:?}");
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap();
    assert_eq!(
        args,
        json!({"path": "src/main.rs", "edits": [{"oldText": "foo", "newText": "bar"}]})
    );
}

#[tokio::test]
async fn private_path_inside_translated_args_is_the_existing_fault() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let private = tmp.path().join("stub-propose").display().to_string();
    let args = serde_json::json!({
        "file_path": format!("{private}/system-prompt.txt"),
        "old_string": "a",
        "new_string": "b"
    });
    let model = format!("fake-propose:Edit:{args}");
    let events = run_stub(&bridge, &propose_request(&model, json!([pi_edit()]))).await;
    let Some(ThinEvent::Error(err)) = events.iter().find(|e| matches!(e, ThinEvent::Error(_)))
    else {
        panic!("expected 502, got {events:?}");
    };
    assert_eq!(err.status, 500, "{err:?}");
    assert_eq!(err.code, "server_error");
    assert!(err.message.contains("provider-private"), "{}", err.message);
    assert!(!events
        .iter()
        .any(|e| matches!(e, ThinEvent::ToolCall { .. })));
}

#[tokio::test]
async fn a_bad_edit_is_retried_once_and_the_corrected_call_is_published() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let model = r#"fake-propose-twice:Edit:{"path":"src/main.rs","oldText":"foo","newText":"bar"}"#;
    let events = run_stub(&bridge, &propose_request(model, json!([pi_edit()]))).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "Trying.",
        "retry must not resend the first text: {text}"
    );
    let calls: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["edit"]);
    assert!(matches!(
        events.last(),
        Some(ThinEvent::Finish(FinishReason::ToolCalls))
    ));
}

#[tokio::test]
async fn a_second_bad_edit_is_dialect_args() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let model = r#"fake-propose:Edit:{"path":"src/main.rs"}"#;
    let events = run_stub(&bridge, &propose_request(model, json!([pi_edit()]))).await;
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::Error(err) => Some(err),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{events:?}");
    assert_eq!(errors[0].status, 502);
    assert_eq!(errors[0].code, "dialect_args");
    assert!(errors[0].message.contains("Edit"), "{}", errors[0].message);
    assert!(
        errors[0].message.contains("file_path"),
        "{}",
        errors[0].message
    );
    assert!(!errors[0].should_retry().unwrap_or(true));
    assert!(!events
        .iter()
        .any(|e| matches!(e, ThinEvent::ToolCall { .. })));
}

fn pi_subagent() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "subagent",
            "parameters": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "description": {"type": "string"},
                    "subagent_type": {"type": "string"},
                    "model": {"type": "string"},
                    "run_in_background": {"type": "boolean"}
                },
                "required": ["prompt", "description", "subagent_type"]
            }
        }
    })
}

fn pi_ask() -> serde_json::Value {
    json!({
        "type": "function",
        "function": {
            "name": "askuserquestion",
            "parameters": {
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
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
            }
        }
    })
}

fn pi_todo() -> serde_json::Value {
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

fn published_calls(events: &[ThinEvent]) -> Vec<(String, String, serde_json::Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            ThinEvent::ToolCall {
                id,
                name,
                arguments,
            } => Some((
                id.clone(),
                name.clone(),
                serde_json::from_str(arguments).unwrap(),
            )),
            _ => None,
        })
        .collect()
}

fn tool_result(id: &str, content: &str) -> serde_json::Value {
    json!({"role": "tool", "tool_call_id": id, "content": content})
}

/// R1 from the stub, R2 from the short-circuit, then the next spawn sees one TodoWrite.
#[tokio::test]
async fn todowrite_rounds_fold_into_one_call_on_the_next_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let todos = json!({"todos": [
        {"content": "Read SPEC", "status": "completed", "activeForm": "reading SPEC"},
        {"content": "Write the row", "status": "in_progress", "activeForm": "writing the row"}
    ]});
    let model = format!("fake-todo-then-text:TodoWrite:{todos}");
    let mut req = propose_request(&model, json!([pi_todo()]));
    req.request_id = "stub-todo".into();
    let first = run_stub(&bridge, &req).await;
    assert!(
        !first.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{first:?}"
    );
    let r1 = published_calls(&first);
    assert_eq!(r1.len(), 2, "{first:?}");
    assert_eq!(r1[0].1, "todo");
    assert_eq!(r1[0].2, json!({"action": "list"}));
    assert_eq!(r1[1].2["action"], "create");
    assert_eq!(r1[1].2["todos"][0]["metadata"]["tw_status"], "completed");

    // Pi's results, including two pre-existing tasks. The sibling is not a todo call.
    req.request.messages.push(json_message("assistant", &r1));
    req.request.messages.push(tool_message(
        &r1[0].0,
        "[pending] #3 Old\n[completed] #4 Done",
    ));
    req.request.messages.push(tool_message(
        &r1[1].0,
        "Created #10: Read SPEC (pending)\nCreated #11: Write the row (pending)",
    ));

    let second = run_stub(&bridge, &req).await;
    assert!(
        !second.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{second:?}"
    );
    let r2 = published_calls(&second);
    assert_eq!(r2.len(), 4, "{second:?}");
    assert_eq!(r2[0].2, json!({"action": "delete", "id": 3}));
    assert_eq!(r2[1].2, json!({"action": "delete", "id": 4}));
    assert_eq!(
        r2[2].2,
        json!({"action": "update", "id": 10, "status": "completed"})
    );
    assert_eq!(
        r2[3].2,
        json!({"action": "update", "id": 11, "status": "in_progress"})
    );
    assert!(matches!(
        second.first(),
        Some(ThinEvent::Started { path: "bridge", .. })
    ));
    // The short-circuit took no slot and wrote no private dir of its own.
    assert!(!tmp.path().join("stub-todo-r1").exists());

    req.request.messages.push(json_message("assistant", &r2));
    for (id, content) in [
        (r2[0].0.as_str(), "Deleted #3: Old"),
        (r2[1].0.as_str(), "Deleted #4: Done"),
        (r2[2].0.as_str(), "Updated #10 (pending → completed)"),
        (r2[3].0.as_str(), "Updated #11 (pending → in_progress)"),
    ] {
        req.request.messages.push(tool_message(id, content));
    }
    req.request
        .messages
        .push(serde_json::from_value(json!({"role": "user", "content": "continue"})).unwrap());
    req.request_id = "stub-todo-r1".into();
    let third = run_stub(&bridge, &req).await;
    let text: String = third
        .iter()
        .filter_map(|e| match e {
            ThinEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Done.", "{third:?}");
    let seen = std::fs::read_to_string(tmp.path().join("fake-claude-seen-prompt.txt"))
        .expect("stub recorded the folded prompt");
    // Header map, the one folded toolCall, and its one toolResult. Round calls are gone.
    let calls = seen.matches("mcp__pi__TodoWrite").count();
    assert_eq!(calls, 3, "header + one toolCall + one toolResult: {seen}");
    assert_eq!(seen.matches("toolResult").count(), 1, "{seen}");
    assert!(!seen.contains("\"action\":\"list\""), "{seen}");
    assert!(
        seen.contains("Todos have been modified successfully"),
        "{seen}"
    );
}

fn json_message(
    role: &str,
    calls: &[(String, String, serde_json::Value)],
) -> thickapi_core::thin::openai::ChatMessage {
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .map(|(id, name, arguments)| {
            json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}})
        })
        .collect();
    serde_json::from_value(json!({"role": role, "tool_calls": tool_calls})).unwrap()
}

fn tool_message(id: &str, content: &str) -> thickapi_core::thin::openai::ChatMessage {
    serde_json::from_value(tool_result(id, content)).unwrap()
}

fn published_call(events: &[ThinEvent]) -> (String, serde_json::Value) {
    let (name, arguments) = events
        .iter()
        .find_map(|e| match e {
            ThinEvent::ToolCall {
                name, arguments, ..
            } => Some((name.clone(), arguments.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool call: {events:?}"));
    (name, serde_json::from_str(&arguments).unwrap())
}

#[tokio::test]
async fn agent_proposal_reaches_the_client_as_a_pi_subagent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut settings = settings(&tmp);
    settings.dialect = thickapi_core::thin::dialect::MODE_CLAUDE_CODE.to_string();
    settings.catalogue_ids = vec!["subs/claude/sonnet".into(), "subs/claude/opus".into()];
    let bridge = ClaudeBridge::new(settings);
    let model = r#"fake-propose:Agent:{"description":"Read the dialect","prompt":"Find the row","subagent_type":"Explore","model":"sonnet","run_in_background":true}"#;
    let events = run_stub(&bridge, &propose_request(model, json!([pi_subagent()]))).await;
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let (name, args) = published_call(&events);
    assert_eq!(name, "subagent");
    assert_eq!(
        args,
        json!({
            "description": "Read the dialect",
            "prompt": "Find the row",
            "subagent_type": "Explore",
            "model": "subs/claude/sonnet",
            "run_in_background": true
        })
    );
}

#[tokio::test]
async fn ask_proposal_reaches_the_client_as_a_pi_ask() {
    let tmp = tempfile::tempdir().unwrap();
    let bridge = dialect_on(&tmp);
    let model = r#"fake-propose:AskUserQuestion:{"questions":[{"question":"Which file?","header":"File","multiSelect":false,"options":[{"label":"dialect.rs","description":"The table"}]}],"annotations":{}}"#;
    let events = run_stub(&bridge, &propose_request(model, json!([pi_ask()]))).await;
    assert!(
        !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
        "{events:?}"
    );
    let (name, args) = published_call(&events);
    assert_eq!(name, "askuserquestion");
    assert_eq!(
        args,
        json!({
            "questions": [{
                "question": "Which file?",
                "header": "File",
                "multiSelect": false,
                "options": [{"label": "dialect.rs", "description": "The table"}]
            }]
        })
    );
}
