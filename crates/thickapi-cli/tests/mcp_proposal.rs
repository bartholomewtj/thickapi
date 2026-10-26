#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn fixture_catalog() -> String {
    // MCP proposal catalog shape ({name, description, inputSchema}), not the
    // OpenAI dump-tools fixture in fixtures/thin/pi-tool-catalog.json.
    r#"[{"name":"read","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]"#.to_string()
}

#[test]
fn mcp_proposal_binary_handshakes_and_refuses_calls() {
    let temp = tempfile::tempdir().expect("tempdir");
    let catalog = temp.path().join("tools.json");
    std::fs::write(&catalog, fixture_catalog()).expect("write catalog");
    let ready = temp.path().join("mcp-ready");
    let violation = temp.path().join("mcp-execution-attempt");

    let mut child = Command::new(env!("CARGO_BIN_EXE_thickapi"))
        .args([
            "mcp-proposal",
            "--catalog",
            catalog.to_str().unwrap(),
            "--ready",
            ready.to_str().unwrap(),
            "--violation",
            violation.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn thickapi mcp-proposal");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();

    let mut send = |line: &str| {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };

    send(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
    );
    let init: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert!(!ready.exists(), "ready marker must wait for tools/list");

    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let list: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|t| t["name"] == "read"));
    assert!(ready.exists());

    send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{"path":"x"}}}"#,
    );
    let call: serde_json::Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(call["error"]["code"], -32001);
    assert!(violation.exists());

    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success(), "clean EOF exit, got {status:?}");
}

#[test]
fn mcp_proposal_binary_rejects_markers_outside_catalog_dir() {
    let temp = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("tempdir2");
    let catalog = temp.path().join("tools.json");
    std::fs::write(&catalog, "[]").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_thickapi"))
        .args([
            "mcp-proposal",
            "--catalog",
            catalog.to_str().unwrap(),
            "--ready",
            other.path().join("ready").to_str().unwrap(),
            "--violation",
            temp.path().join("violation").to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "nothing on stdout for a startup failure"
    );
}
