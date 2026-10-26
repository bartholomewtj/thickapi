//! Proposal-only MCP server (SPEC §19.7): exposes Pi's tool schemas over JSON-RPC on stdio,
//! writes a ready marker on `tools/list`, writes a violation marker and returns an error on
//! every `tools/call`. Port of `pi-claude-code-provider/bridge/mcp-proposal-server.js`.
//!
//! Tracing goes to **stderr** only; a log line on stdout corrupts JSON-RPC.

use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-11-25";
pub const SERVER_NAME: &str = "pi-tool-proposals";
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const READY_MARKER: &str = "ready\n";
pub const VIOLATION_MARKER: &str = "tools/call reached proposal-only MCP server\n";

/// Exit code when the catalog cannot be loaded (mirrors upstream `failStartup`).
pub const EXIT_STARTUP: i32 = 2;
/// Exit code when input framing is rejected (mirrors upstream `failInput`).
pub const EXIT_INPUT: i32 = 1;

#[derive(Debug, Clone)]
pub struct ProposalPaths {
    pub catalog: PathBuf,
    pub ready: PathBuf,
    pub violation: PathBuf,
}

/// Canonicalise the three paths and require every one of them to sit directly inside the same
/// private request directory, with no reparse point on the way (SPEC §19.7).
pub fn validate_paths(catalog: &Path, ready: &Path, violation: &Path) -> Result<ProposalPaths> {
    let catalog = canonical_existing(catalog).context("catalog path")?;
    let parent = catalog
        .parent()
        .ok_or_else(|| anyhow!("catalog path has no parent directory"))?
        .to_path_buf();
    let ready = sibling_marker(&parent, ready).context("ready marker path")?;
    let violation = sibling_marker(&parent, violation).context("violation marker path")?;
    if ready == violation {
        bail!("ready and violation markers must differ");
    }
    Ok(ProposalPaths {
        catalog,
        ready,
        violation,
    })
}

fn canonical_existing(path: &Path) -> Result<PathBuf> {
    let canonical =
        dunce::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
    reject_reparse_points(&canonical)?;
    Ok(canonical)
}

/// Markers do not exist yet; canonicalise the parent and require a plain file name.
fn sibling_marker(parent: &Path, marker: &Path) -> Result<PathBuf> {
    let name = marker
        .file_name()
        .ok_or_else(|| anyhow!("marker path {} has no file name", marker.display()))?;
    if marker
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        bail!("marker path {} must not traverse ..", marker.display());
    }
    let marker_parent = marker.parent().filter(|p| !p.as_os_str().is_empty());
    let resolved_parent = match marker_parent {
        Some(p) => {
            dunce::canonicalize(p).with_context(|| format!("canonicalize {}", p.display()))?
        }
        None => parent.to_path_buf(),
    };
    if resolved_parent != parent {
        bail!(
            "marker {} is not inside the catalog directory {}",
            marker.display(),
            parent.display()
        );
    }
    let full = parent.join(name);
    if full.exists() {
        bail!("marker {} already exists", full.display());
    }
    Ok(full)
}

fn reject_reparse_points(path: &Path) -> Result<()> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        cursor.push(component.as_os_str());
        if cursor.as_os_str().is_empty() {
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&cursor) {
            if meta.file_type().is_symlink() {
                bail!("{} traverses a reparse point", cursor.display());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    bail!("{} traverses a reparse point", cursor.display());
                }
            }
        }
    }
    Ok(())
}

pub fn load_catalog(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read catalog {}", path.display()))?;
    let value: Value = serde_json::from_str(&text).context("catalog is not JSON")?;
    match value {
        Value::Array(items) => Ok(items),
        _ => bail!("catalog must be an array"),
    }
}

/// Create the marker with `create_new` so a second writer never clobbers it; `AlreadyExists` is fine.
pub fn write_marker(path: &Path, contents: &str) -> Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .with_context(|| format!("write marker {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).with_context(|| format!("create marker {}", path.display())),
    }
}

/// One JSON-RPC line → zero or one response line. `None` means nothing to send.
pub fn handle_line(line: &str, tools: &[Value], paths: &ProposalPaths) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return Some(error(Value::Null, -32700, "Invalid JSON-RPC request")),
    };
    let Some(object) = message.as_object() else {
        return Some(error(Value::Null, -32600, "Invalid JSON-RPC request"));
    };
    let id = object.get("id").cloned();
    let method = object.get("method").and_then(|m| m.as_str()).unwrap_or("");
    match method {
        "initialize" => id.map(|id| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": "1.0.0" },
                }
            })
        }),
        "tools/list" => {
            if let Err(err) = write_marker(&paths.ready, READY_MARKER) {
                tracing::error!(target: "thickapi::mcp_proposal", "ready marker: {err}");
            }
            id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }))
        }
        "tools/call" => {
            if let Err(err) = write_marker(&paths.violation, VIOLATION_MARKER) {
                tracing::error!(target: "thickapi::mcp_proposal", "violation marker: {err}");
            }
            id.map(|id| {
                error(
                    id,
                    -32001,
                    "Security invariant: this server proposes tools but never executes them",
                )
            })
        }
        _ => id.map(|id| error(id, -32601, &format!("Method not found: {}", method))),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Serve JSON-RPC over the given reader/writer until EOF. Returns the process exit code.
/// CRLF-tolerant; a record over `MAX_REQUEST_BYTES` is fatal so rejected framing state is never reused.
pub fn serve<R: BufRead, W: Write>(paths: &ProposalPaths, mut input: R, mut output: W) -> i32 {
    let tools = match load_catalog(&paths.catalog) {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("invalid tool catalog: {err}");
            return EXIT_STARTUP;
        }
    };
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let limit = MAX_REQUEST_BYTES + 2;
        let mut limited = (&mut input).take(limit as u64);
        let read = match limited.read_until(b'\n', &mut buf) {
            Ok(0) => return 0,
            Ok(n) => n,
            Err(err) => {
                eprintln!("MCP input failure: {err}");
                return EXIT_INPUT;
            }
        };
        let complete = buf.last() == Some(&b'\n');
        if complete {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if buf.len() > MAX_REQUEST_BYTES || (!complete && read >= limit) {
            let fail = error(Value::Null, -32700, "MCP request exceeds the bridge limit");
            let _ = writeln!(output, "{}", fail);
            let _ = output.flush();
            return EXIT_INPUT;
        }
        if buf.is_empty() {
            if !complete {
                return 0;
            }
            continue;
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        if let Some(response) = handle_line(&line, &tools, paths) {
            if writeln!(output, "{}", response).is_err() || output.flush().is_err() {
                return EXIT_INPUT;
            }
        }
        if !complete {
            return 0;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn paths_in(dir: &Path) -> ProposalPaths {
        let catalog = dir.join("tools.json");
        std::fs::write(
            &catalog,
            r#"[{"name":"read","description":"Read a file","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}}]"#,
        )
        .expect("catalog");
        validate_paths(
            &catalog,
            &dir.join("mcp-ready"),
            &dir.join("mcp-execution-attempt"),
        )
        .expect("paths")
    }

    fn run(paths: &ProposalPaths, input: &str) -> (i32, Vec<Value>) {
        let mut out = Vec::new();
        let code = serve(paths, Cursor::new(input.as_bytes().to_vec()), &mut out);
        let lines = String::from_utf8(out)
            .expect("utf8")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("json line"))
            .collect();
        (code, lines)
    }

    #[test]
    fn handshake_list_and_call_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(temp.path());
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\r\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read","arguments":{}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"prompts/list"}"#,
            "\n",
        );
        let (code, lines) = run(&paths, input);
        assert_eq!(code, 0);
        assert_eq!(lines.len(), 4, "id-less notification produces no line");
        assert_eq!(lines[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(lines[0]["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(lines[1]["result"]["tools"][0]["name"], "read");
        assert_eq!(lines[2]["error"]["code"], -32001);
        assert_eq!(lines[3]["error"]["code"], -32601);
        assert_eq!(std::fs::read_to_string(&paths.ready).unwrap(), READY_MARKER);
        assert_eq!(
            std::fs::read_to_string(&paths.violation).unwrap(),
            VIOLATION_MARKER
        );
    }

    #[test]
    fn ready_marker_written_on_list_not_initialize() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(temp.path());
        let (_, _) = run(
            &paths,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n",
        );
        assert!(!paths.ready.exists());
        assert!(!paths.violation.exists());
    }

    #[test]
    fn invalid_json_gets_parse_error_with_null_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(temp.path());
        let (code, lines) = run(&paths, "not json\n");
        assert_eq!(code, 0);
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert!(lines[0]["id"].is_null());
    }

    #[test]
    fn oversized_record_is_fatal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(temp.path());
        let big = format!("{}\n", "x".repeat(MAX_REQUEST_BYTES + 10));
        let (code, lines) = run(&paths, &big);
        assert_eq!(code, EXIT_INPUT);
        assert_eq!(
            lines[0]["error"]["message"],
            "MCP request exceeds the bridge limit"
        );
    }

    #[test]
    fn missing_catalog_is_startup_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(temp.path());
        std::fs::remove_file(&paths.catalog).unwrap();
        let (code, _) = run(&paths, "");
        assert_eq!(code, EXIT_STARTUP);
    }

    #[test]
    fn paths_must_share_the_catalog_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let other = tempfile::tempdir().expect("tempdir2");
        let catalog = temp.path().join("tools.json");
        std::fs::write(&catalog, "[]").unwrap();
        assert!(validate_paths(
            &catalog,
            &other.path().join("ready"),
            &temp.path().join("violation")
        )
        .is_err());
        assert!(validate_paths(
            &catalog,
            &temp.path().join("../ready"),
            &temp.path().join("violation")
        )
        .is_err());
        assert!(validate_paths(
            &catalog,
            &temp.path().join("same"),
            &temp.path().join("same")
        )
        .is_err());
        assert!(validate_paths(
            &catalog,
            &temp.path().join("ready"),
            &temp.path().join("violation")
        )
        .is_ok());
    }

    #[test]
    fn marker_is_create_new_and_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let marker = temp.path().join("m");
        write_marker(&marker, "one\n").unwrap();
        write_marker(&marker, "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "one\n");
    }
}
