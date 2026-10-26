//! `thickapi capture claude-tools` (K2, #220): one Claude CLI start against a loopback
//! listener, the `tools` array of the first `POST /v1/messages`, written to
//! `fixtures/claude-tools/<cli-version>/tools.json`.
//!
//! Default tools (no `--tools ""`). The bridge allowlist plus a dummy Anthropic key, so
//! the CLI talks to the listener instead of the live API. The listener answers 400 and the
//! child is killed on the job object, so the capture spends no model tokens when the CLI
//! posts first.
//!
//! `--check` captures to a temp dir and diffs tool names and `input_schema` property sets
//! against the newest committed set. It never writes under `out`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use thickapi_core::engine::job::JobObject;
use thickapi_core::thin::claude_bridge::spawn::{self, KNOWN_EXIT};
use thickapi_core::thin::claude_tools::{
    schema_diff, tools_from_body, ClaudeTool, ClaudeTools, TOOLS_FILE, VENDOR,
};
use thickapi_core::thin::fixture_set::{compiled_fixtures_root, leaks, newest_set, Scope};

/// How long the CLI gets to POST before the capture gives up and falls back to the bundle.
const POST_PATIENCE: Duration = Duration::from_secs(30);
/// Dummy key. The leak table must not see it in the written fixture.
const DUMMY_KEY: &str = "sk-ant-capture-dummy";
const CAPTURE_ERROR: &str =
    "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"capture\"}}";

pub struct ClaudeToolsArgs {
    pub out: PathBuf,
    /// Capture to a temp dir, leak-check, diff names and property sets; never promote.
    pub check: bool,
}

pub async fn run(args: ClaudeToolsArgs) -> Result<()> {
    let program = spawn::resolve_claude(None)
        .ok_or_else(|| anyhow::anyhow!("claude CLI not found (resolve_cli returned no exe)"))?;
    let version = probe_version(&program).unwrap_or_else(|| "unknown".to_string());
    let captured_at = captured_at_now();
    println!(
        "capturing claude-tools from {} ({})",
        program.display(),
        version
    );

    // 2.1.288 is a Bun executable: the schemas are compiled in, not a JS file beside
    // the exe, so a missed POST has nothing honest to fall back to.
    let mut tools = capture_live(&program)
        .await
        .context("live POST /v1/messages")?;
    println!("  live POST: {} tool(s)", tools.len());
    if tools.is_empty() {
        bail!("capture produced no tools");
    }
    pin_sdk_tools(&program, &mut tools)?;
    let set = ClaudeTools {
        cli_version: version.clone(),
        captured_at,
        source: None,
        tools,
    };
    let doc = set.to_value();
    let text = serde_json::to_string_pretty(&doc)?;
    let hits = leaks(Scope::Text, &text);
    if !hits.is_empty() {
        bail!("tools.json matches the leak table: {}", hits.join(", "));
    }

    if args.check {
        return check_against_committed(&set, &text);
    }
    let dir = args.out.join(VENDOR).join(&version);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(TOOLS_FILE);
    std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
    // newest_set only counts a directory with meta.json (the wire sets' stamp file).
    let meta = json!({"captured_at": set.captured_at, "cli_version": set.cli_version});
    std::fs::write(
        dir.join(thickapi_core::thin::fixture_set::META_FILE),
        serde_json::to_string_pretty(&meta)?,
    )?;
    println!("wrote {} ({} tools)", path.display(), set.tools.len());
    print_names(&set);
    Ok(())
}

fn check_against_committed(fresh: &ClaudeTools, text: &str) -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "thickapi-claude-tools-check-{}",
        uuid::Uuid::new_v4()
    ));
    let dir = root.join(VENDOR).join(&fresh.cli_version);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(TOOLS_FILE), text)?;
    let fixtures = compiled_fixtures_root();
    let Some(committed_dir) = newest_set(&fixtures, VENDOR) else {
        println!("no committed {VENDOR} set to compare");
        let _ = std::fs::remove_dir_all(&root);
        return Ok(());
    };
    let committed =
        ClaudeTools::load(&committed_dir.join(TOOLS_FILE)).map_err(|err| anyhow::anyhow!(err))?;
    let diff = schema_diff(fresh, &committed);
    println!(
        "fresh {} vs committed {}",
        fresh.cli_version, committed.cli_version
    );
    if diff.is_empty() {
        println!("tool names and input_schema property sets match");
    } else {
        for line in &diff {
            println!("  {line}");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    if diff.is_empty() {
        Ok(())
    } else {
        bail!(
            "claude-tools schemas changed ({} difference(s))",
            diff.len()
        );
    }
}

fn print_names(set: &ClaudeTools) {
    let names: Vec<&str> = set.tools.iter().map(|t| t.name.as_str()).collect();
    println!("tools: {}", names.join(", "));
}

fn captured_at_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "unknown".to_string())
}

fn probe_version(program: &Path) -> Option<String> {
    let mut command = std::process::Command::new(program);
    command
        .arg("--version")
        .env_clear()
        .envs(spawn::claude_env(None, false, None))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let started = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut child.stdout.take()?, &mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Tools `claude -p` does not offer, taken from `sdk-tools.d.ts` beside the exe.
/// A name the live POST already sent is left as the CLI sent it.
fn pin_sdk_tools(program: &Path, tools: &mut Vec<ClaudeTool>) -> Result<()> {
    let path = thickapi_core::thin::claude_tools::sdk_tools_path(program)
        .ok_or_else(|| anyhow::anyhow!("sdk-tools.d.ts not beside {}", program.display()))?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let pinned = thickapi_core::thin::claude_tools::sdk_pinned_tools(&text)
        .map_err(|err| anyhow::anyhow!(err))?;
    let have: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    for tool in pinned {
        if have.iter().any(|n| n == &tool.name) {
            continue;
        }
        println!("  pinned {} from {}", tool.name, path.display());
        tools.push(tool);
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

/// Spawn the CLI against a loopback listener and return the tools of the first Messages POST.
async fn capture_live(program: &Path) -> Result<Vec<ClaudeTool>> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind loopback")?;
    listener
        .set_nonblocking(true)
        .context("nonblocking listener")?;
    let port = listener.local_addr().context("listener addr")?.port();
    let cwd = tempfile_cwd()?;
    let env = capture_env(port);
    let args = capture_args();
    let mut child = spawn_on_job(program, &cwd, &args, &env)?;
    let body = match read_first_post(&listener, &mut child, POST_PATIENCE) {
        Ok(body) => body,
        Err(err) => {
            kill_child(&mut child);
            let _ = std::fs::remove_dir_all(&cwd);
            return Err(err);
        }
    };
    kill_child(&mut child);
    let _ = std::fs::remove_dir_all(&cwd);
    let value: Value = serde_json::from_slice(&body).context("parse Messages body")?;
    tools_from_body(&value).context("Messages body has no tools array")
}

/// The brief argv: default tools, empty settings sources, empty MCP, dontAsk, stream-json.
fn capture_args() -> Vec<String> {
    [
        "-p",
        "hi",
        "--output-format",
        "stream-json",
        "--verbose",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        spawn::EMPTY_MCP,
        "--permission-mode",
        "dontAsk",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The bridge allowlist, plus the loopback base URL and the dummy key.
fn capture_env(port: u16) -> BTreeMap<String, String> {
    let mut env = spawn::claude_env(None, false, None);
    env.insert(
        "ANTHROPIC_BASE_URL".into(),
        format!("http://127.0.0.1:{port}"),
    );
    env.insert("ANTHROPIC_API_KEY".into(), DUMMY_KEY.into());
    env
}

fn tempfile_cwd() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("thickapi-claude-tools-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

struct Spawned {
    child: std::process::Child,
    job: JobObject,
}

fn spawn_on_job(
    program: &Path,
    cwd: &Path,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<Spawned> {
    let job = JobObject::new().context("create job object")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn().context("spawn claude")?;
    #[cfg(windows)]
    {
        job.assign(&child).context("assign claude to job")?;
    }
    #[cfg(not(windows))]
    {
        let _ = &job;
    }
    Ok(Spawned { child, job })
}

fn kill_child(spawned: &mut Spawned) {
    if spawned.job.terminate(KNOWN_EXIT).is_err() {
        let _ = spawned.child.kill();
    }
    let _ = spawned.child.wait();
}

/// `accept` on a nonblocking listener, polling until `patience` or the child exits.
fn accept_within(
    listener: &TcpListener,
    spawned: &mut Spawned,
    patience: Duration,
    started: Instant,
) -> Result<std::net::TcpStream> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= patience {
                    bail!("timed out waiting for claude to connect ({patience:?})");
                }
                if spawned.child.try_wait().ok().flatten().is_some() {
                    bail!("claude exited before connecting");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err).context("accept claude connection"),
        }
    }
}

/// Answer the health probe (`HEAD /api/hello`) and keep reading until the Messages POST.
fn read_first_post(
    listener: &TcpListener,
    spawned: &mut Spawned,
    patience: Duration,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let mut stream = accept_within(listener, spawned, patience, started)?;
    let left = patience.saturating_sub(started.elapsed());
    stream
        .set_read_timeout(Some(left.max(Duration::from_secs(1))))
        .ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    while started.elapsed() < patience {
        if spawned.child.try_wait().ok().flatten().is_some() && buf.is_empty() {
            bail!("claude exited before POST /v1/messages");
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(chunk) = tmp.get(..n) {
                    buf.extend_from_slice(chunk);
                }
                if let Some(answered) = answer_probe(&buf) {
                    let _ = stream.write_all(answered.as_bytes());
                    let _ = stream.flush();
                    buf.clear();
                    continue;
                }
                if let Some(body) = split_http_body(&buf) {
                    let _ = stream.write_all(capture_response().as_bytes());
                    let _ = stream.flush();
                    return Ok(body);
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => {
                return Err(err).context(format!("read claude POST; got {}", preview(&buf)));
            }
        }
    }
    bail!(
        "timed out waiting for POST /v1/messages ({patience:?}); first bytes: {}",
        preview(&buf)
    )
}

fn preview(buf: &[u8]) -> String {
    let text = String::from_utf8_lossy(buf.get(..200).unwrap_or(buf));
    text.replace('\r', "").replace('\n', " | ")
}

/// A complete health probe (`HEAD`/`GET /api/hello`) gets a 200 with no body. `None` otherwise.
fn answer_probe(buf: &[u8]) -> Option<String> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(buf.get(..end)?).ok()?;
    let line = head.lines().next().unwrap_or("");
    let probe = line.starts_with("HEAD /api/hello") || line.starts_with("GET /api/hello");
    probe.then(|| {
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n".to_string()
    })
}

fn capture_response() -> String {
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        CAPTURE_ERROR.len(),
        CAPTURE_ERROR
    )
}

/// Headers plus a complete body, when Content-Length has arrived. None while incomplete.
fn split_http_body(buf: &[u8]) -> Option<Vec<u8>> {
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(buf.get(..head_end)?).ok()?;
    if !head.starts_with("POST ") {
        return None;
    }
    let body_at = head_end + 4;
    if chunked(head) {
        return decode_chunks(buf.get(body_at..)?);
    }
    let len = content_length(head)?;
    let end = body_at.checked_add(len)?;
    buf.get(body_at..end).map(<[u8]>::to_vec)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}

fn chunked(head: &str) -> bool {
    head.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    })
}

/// A complete chunked body, or `None` while the terminating chunk has not arrived.
fn decode_chunks(buf: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = buf;
    loop {
        let nl = rest.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(rest.get(..nl)?).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        let data_at = nl + 2;
        if size == 0 {
            return Some(out);
        }
        let data_end = data_at.checked_add(size)?;
        out.extend_from_slice(rest.get(data_at..data_end)?);
        rest = rest.get(data_end + 2..)?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn split_http_body_waits_for_content_length() {
        let head = b"POST /v1/messages HTTP/1.1\r\nContent-Length: 5\r\n\r\n";
        assert!(split_http_body(head).is_none());
        let mut full = head.to_vec();
        full.extend_from_slice(b"hello");
        assert_eq!(split_http_body(&full).as_deref(), Some(&b"hello"[..]));
        assert!(split_http_body(b"GET / HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn capture_args_leave_tools_at_the_default() {
        let args = capture_args();
        assert!(args.iter().any(|a| a == "-p"));
        assert!(!args.iter().any(|a| a == "--tools"));
        assert!(args.windows(2).any(|w| w == ["--setting-sources", ""]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-mode", "dontAsk"]));
    }

    #[test]
    fn capture_env_is_the_allowlist_plus_the_dummy_key() {
        let env = capture_env(9);
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some(DUMMY_KEY)
        );
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:9")
        );
        assert!(!env.keys().any(|k| k.starts_with("OPENAI")));
    }
}
