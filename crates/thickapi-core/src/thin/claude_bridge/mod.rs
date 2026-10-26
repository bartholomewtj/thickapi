//! Claude bridge: the `cli_bridge` transport for Claude rows (SPEC §19.7).
//!
//! A real `claude.exe` per request with Pi's tools exposed through the proposal-only MCP server
//! (`thickapi mcp-proposal`). Used automatically when the OAuth-direct path is blocked
//! (§19.8.1) and outright with `claude_oauth: off`.
//!
//! **Port stamp.** This is a port of `pi-claude-code-provider` **0.2.0** as installed at
//! `~/.pi/agent/npm/node_modules/pi-claude-code-provider` (files dated 2026-09-10). Files ported: `claude-args.ts` → [`spawn`],
//! `auth.ts#buildClaudeEnvironment` → [`spawn::claude_env`] (Windows allowlist added),
//! `context-serializer.ts` → [`transcript`] + [`catalog`], `claude-protocol.ts` → [`protocol`],
//! `stream-events.ts` → [`capture`], `provider.ts` lifecycle → this file,
//! `bridge/mcp-proposal-server.js` → `thin::mcp_proposal`. A breakage morning starts with
//! `git diff` of those upstream files, not with `claude.exe`.
//!
//! **Lifecycle** (§19.7). One run is four phases — `spawn_phase`,
//! `wait_ready`, `protocol_loop`, `publish` — composed by `run_inner`. The child lives in a
//! [`ChildGuard`] from spawn to reap; failures are typed ([`BridgeFault`]), and whether one is
//! a bridge fault (R3) is a match on the variant, never on the message.

pub mod capture;
pub mod catalog;
pub mod protocol;
pub mod spawn;
pub mod transcript;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::task::JoinHandle;

use crate::config::ConfigBundle;
use crate::thin::capture as wire;
use crate::thin::oauth::state::BridgeState;
use crate::thin::openai::FinishReason;
use crate::thin::private_dir::PrivateDir;
use crate::thin::{AdapterFuture, EventSender, ModelAdapter, ThinError, ThinEvent, ThinRequest};
use crate::thinking::Effort;
use capture::{CapturedToolCall, LineError, Mapper, TerminationCause};
use protocol::ProtocolError;
use transcript::Prepared;

pub const UPSTREAM: &str = "pi-claude-code-provider 0.2.0 sha512-QSJLSATGn6g74NwoYXgSWDV2ddodyTumU6LRWzEKb1ogZAm+AJ/dP4VVo2FQ1mZlEA6wUz7cqX7YUv6qi3odxQ==";
pub const VENDOR: &str = "claude";
const MAX_STDERR_BYTES: usize = 64 * 1024;
/// Well under `state::redact`'s 600-char cap, which keeps the head (and redaction can lengthen
/// short matches): a longer excerpt would lose the end of stderr, where the cause is.
const STDERR_EXCERPT: usize = 450;

/// The last [`MAX_STDERR_BYTES`] of the child's stderr. Bytes, not a `String`: the cut lands
/// wherever the cap puts it, which is rarely a char boundary.
#[derive(Default)]
struct StderrTail(Vec<u8>);

impl StderrTail {
    fn push(&mut self, chunk: &[u8]) {
        self.0.extend_from_slice(chunk);
        let excess = self.0.len().saturating_sub(MAX_STDERR_BYTES);
        self.0.drain(..excess);
    }

    /// Suffix for an error message: the tail's last [`STDERR_EXCERPT`] chars on one line,
    /// secrets and the private dir removed. Empty when the child said nothing.
    fn excerpt(&self, private_path: &str) -> String {
        let text = String::from_utf8_lossy(&self.0);
        let chars: Vec<char> = text.trim().chars().collect();
        if chars.is_empty() {
            return String::new();
        }
        let start = chars.len().saturating_sub(STDERR_EXCERPT);
        let tail: String = chars.iter().skip(start).collect();
        let safe = protocol::safe_diagnostic(
            &crate::thin::oauth::state::redact(&tail),
            // As written, with forward slashes, and JSON-escaped (node tooling prints that).
            &[
                private_path.to_string(),
                private_path.replace('\\', "/"),
                private_path.replace('\\', r"\\"),
            ],
            STDERR_EXCERPT,
        );
        format!("; Claude Code stderr: {safe}")
    }
}

/// The child's stderr tail, shared with its drain task, and the private dir an excerpt must
/// not name.
struct StderrView {
    tail: Arc<Mutex<StderrTail>>,
    private_path: String,
}

impl StderrView {
    fn excerpt(&self) -> String {
        self.tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .excerpt(&self.private_path)
    }
}

/// Keep reading stderr into `tail` until EOF: a full stderr pipe blocks the child.
fn drain_stderr(mut reader: ChildStderr, tail: Arc<Mutex<StderrTail>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let chunk = buf.get(..n).unwrap_or_default();
            tail.lock().unwrap_or_else(|p| p.into_inner()).push(chunk);
        }
    })
}

/// Owns the bridge child from spawn to reap (SPEC §19.7 **Lifecycle**). Every exit path either
/// reaps it or calls [`ChildGuard::finish`], which terminates the tree and awaits
/// `timeout(drain, child.wait())` (`thin.vendors.claude.bridge.handoff_drain_seconds`, the
/// [`BridgeSettings::handoff_drain`]); the request's `PrivateDir` is declared before the
/// guard, so it never drops under a live child. `Drop` is the backstop for a cancelled
/// future: it terminates through the job object and never blocks the runtime.
pub struct ChildGuard {
    spawned: spawn::Spawned,
    /// How long [`reap`](Self::reap) and [`finish`](Self::finish) wait for the child to exit.
    drain: Duration,
    /// The child was reaped, or the drain budget was spent on it: nothing left for `Drop`.
    drained: bool,
}

impl ChildGuard {
    pub fn new(spawned: spawn::Spawned, drain: Duration) -> Self {
        Self {
            spawned,
            drain,
            drained: false,
        }
    }

    pub fn pid(&self) -> u32 {
        self.spawned.pid
    }

    /// Terminate the tree now, without waiting. `true` when the job object did it.
    fn terminate(&mut self) -> bool {
        spawn::terminate(&mut self.spawned)
    }

    /// The exit status if the child has already exited on its own.
    fn try_wait(&mut self) -> Option<ExitStatus> {
        let status = self.spawned.child.try_wait().ok().flatten();
        if status.is_some() {
            self.drained = true;
        }
        status
    }

    /// Wait for the child to exit, bounded by the drain budget; a child that outlives it is
    /// killed by pid tree and reported.
    async fn reap(&mut self) -> Result<ExitStatus, BridgeFault> {
        let waited = tokio::time::timeout(self.drain, self.spawned.child.wait()).await;
        self.drained = true;
        match waited {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(e)) => Err(BridgeFault::Wait(e)),
            Err(_) => {
                crate::process::kill_process_tree(self.spawned.pid);
                Err(BridgeFault::NoExit)
            }
        }
    }

    /// Terminate and await `timeout(drain, child.wait())`. `None` when the child was already
    /// reaped or did not exit within the budget.
    pub async fn finish(mut self) -> Option<ExitStatus> {
        if self.drained {
            return None;
        }
        self.terminate();
        let waited = tokio::time::timeout(self.drain, self.spawned.child.wait()).await;
        self.drained = true;
        waited.ok().and_then(Result::ok)
    }

    fn take_pipes(&mut self) -> Result<(ChildStdin, ChildStdout, ChildStderr), BridgeFault> {
        let child = &mut self.spawned.child;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ThinError::server("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ThinError::server("no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ThinError::server("no stderr"))?;
        Ok((stdin, stdout, stderr))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.drained {
            // No waiting here: the job object kills the tree, `start_kill` covers a child the
            // job could not take. Both return at once.
            let _ = self.spawned.job.terminate(spawn::KNOWN_EXIT);
            let _ = self.spawned.child.start_kill();
        }
    }
}

/// Where an early exit status was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStage {
    /// Before the proposal MCP server wrote its ready marker.
    BeforeMcpReady,
    /// After a successful `result`, with a failing status.
    AfterResult,
    /// Before any terminal event.
    BeforeTerminal,
}

/// Why a bridge run failed, by cause (audit R25). [`BridgeFault::is_bridge_fault`] — the R3
/// cross-fallback and `bridge_state = drift` trigger — is a match on the variant; `Display`
/// renders the client-facing message (after the `claude <version>: ` label for 502/503).
#[derive(Debug)]
pub enum BridgeFault {
    /// `claude.exe` could not be resolved (the `.cmd` shim is refused).
    CliUnresolved,
    Spawn {
        cli: PathBuf,
        error: io::Error,
    },
    McpNotReady {
        waited: Duration,
        thickapi: PathBuf,
        stderr: String,
    },
    /// An init-record assertion (`isolation_*`, `protocol_init`).
    Init(ProtocolError),
    /// Any other stream-json protocol assertion (`protocol_blocks`, `protocol_order`, …).
    Protocol(ProtocolError),
    /// A line that is not JSON, before the tool handoff.
    InvalidJson(serde_json::Error),
    /// Claude Code's own error result. Its text is the vendor's, and it names no origin.
    ClaudeError {
        text: String,
        rate_limit: Option<String>,
    },
    /// The turn ended on a rejected rate-limit notice.
    RateLimited {
        text: String,
    },
    Timeout {
        budget: Duration,
        stderr: String,
    },
    Idle {
        budget: Duration,
        stderr: String,
    },
    StdinWrite {
        error: io::Error,
        stderr: String,
    },
    Stdout(io::Error),
    Wait(io::Error),
    NoExit,
    Exit {
        stage: ExitStage,
        /// `None` offline ([`replay`]): there is no process.
        status: Option<ExitStatus>,
        stderr: String,
    },
    HandoffExit {
        status: ExitStatus,
        killed_by_job: bool,
    },
    /// The violation marker exists: Claude Code executed a proposal tool itself.
    ProposalExecuted,
    /// A proposed tool call names the provider-private directory.
    PrivatePathCall,
    /// Not the bridge's cause: request validation, the slot queue, private-dir setup.
    Other(ThinError),
}

impl BridgeFault {
    /// SPEC §19.15 R3: a failure that is the bridge's own — unresolved `claude.exe`, spawn,
    /// MCP ready timeout, an init assertion, a protocol assertion or invalid JSON. Logged-out,
    /// quota, idle and other exits are not; they must not stick `drift` or cross paths.
    pub fn is_bridge_fault(&self) -> bool {
        matches!(
            self,
            Self::CliUnresolved
                | Self::Spawn { .. }
                | Self::McpNotReady { .. }
                | Self::Init(_)
                | Self::Protocol(_)
                | Self::InvalidJson(_)
                | Self::HandoffExit { .. }
        )
    }

    /// The OpenAI `code` the client sees for this fault.
    pub fn code(&self) -> &'static str {
        self.to_thin(None).code
    }

    fn is_drift(&self) -> bool {
        matches!(
            self,
            Self::Init(_) | Self::Protocol(_) | Self::InvalidJson(_) | Self::HandoffExit { .. }
        )
    }

    /// Rendered after the `claude <version>: ` label (the 502/503 family).
    fn labelled(&self) -> bool {
        !matches!(
            self,
            Self::Other(_)
                | Self::RateLimited { .. }
                | Self::ProposalExecuted
                | Self::PrivatePathCall
        )
    }

    /// Carry the rate-limit notice seen before a Claude error result.
    fn with_rate_limit(mut self, notice: Option<&str>) -> Self {
        if let Self::ClaudeError { rate_limit, .. } = &mut self {
            *rate_limit = notice.map(str::to_string);
        }
        self
    }

    /// Fill in the stderr excerpt, taken once the child is dead.
    fn with_stderr(mut self, excerpt: String) -> Self {
        match &mut self {
            Self::McpNotReady { stderr, .. }
            | Self::Timeout { stderr, .. }
            | Self::Idle { stderr, .. }
            | Self::StdinWrite { stderr, .. }
            | Self::Exit { stderr, .. }
                if stderr.is_empty() =>
            {
                *stderr = excerpt;
            }
            _ => {}
        }
        self
    }

    /// The client error. `label` (`claude <version>`) prefixes 502/503 messages.
    pub fn to_thin(&self, label: Option<&str>) -> ThinError {
        let body = || match label {
            Some(label) => format!("{label}: {self}"),
            None => self.to_string(),
        };
        let err = match self {
            Self::Other(err) => return err.clone(),
            Self::RateLimited { text } => rate_limit_error(text),
            Self::ClaudeError { text, rate_limit } => claude_error(text, rate_limit.as_deref())
                .unwrap_or_else(|| ThinError::vendor_down(VENDOR, body())),
            Self::ProposalExecuted | Self::PrivatePathCall => {
                ThinError::server(self.to_string()).with_vendor(VENDOR)
            }
            _ if self.is_drift() => ThinError::vendor_drift(VENDOR, body()),
            _ => ThinError::vendor_down(VENDOR, body()),
        };
        if self.is_bridge_fault() {
            err.with_bridge_fault()
        } else {
            err
        }
    }
}

impl fmt::Display for BridgeFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CliUnresolved => {
                f.write_str("claude.exe not found on PATH (the .cmd shim is not accepted)")
            }
            Self::Spawn { cli, error } => write!(f, "spawn {} failed: {error}", cli.display()),
            Self::McpNotReady {
                waited,
                thickapi,
                stderr,
            } => write!(
                f,
                "proposal MCP server did not become ready within {}ms; Claude Code was told to launch {} mcp-proposal{stderr}",
                waited.as_millis(),
                thickapi.display()
            ),
            Self::Init(err) | Self::Protocol(err) => write!(f, "{} ({})", err.message, err.code),
            Self::InvalidJson(err) => write!(f, "Claude Code emitted invalid JSON: {err}"),
            Self::ClaudeError { text, .. } | Self::RateLimited { text } => f.write_str(text),
            Self::Timeout { budget, stderr } => {
                write!(f, "Claude Code request exceeded {}s{stderr}", budget.as_secs())
            }
            Self::Idle { budget, stderr } => write!(
                f,
                "Claude Code no protocol activity for {}s{stderr}",
                budget.as_secs()
            ),
            Self::StdinWrite { error, stderr } => {
                write!(f, "write prompt to Claude Code stdin failed: {error}{stderr}")
            }
            Self::Stdout(err) => write!(f, "Claude Code stdout failed: {err}"),
            Self::Wait(err) => write!(f, "wait on Claude Code failed: {err}"),
            Self::NoExit => f.write_str("Claude Code did not exit after termination"),
            Self::Exit {
                stage,
                status,
                stderr,
            } => {
                let (what, status_word) = match stage {
                    ExitStage::BeforeMcpReady => (
                        "exited before the proposal MCP server became ready",
                        "status ",
                    ),
                    ExitStage::AfterResult => ("exited after a successful result", ""),
                    ExitStage::BeforeTerminal => ("exited before a terminal event", ""),
                };
                match status {
                    Some(status) => {
                        write!(f, "Claude Code {what} ({status_word}{status}){stderr}")
                    }
                    None => write!(f, "Claude Code {what}{stderr}"),
                }
            }
            Self::HandoffExit {
                status,
                killed_by_job,
            } => {
                let expected = if *killed_by_job {
                    format!("exit {}", spawn::KNOWN_EXIT)
                } else {
                    "taskkill exit 1".into()
                };
                write!(
                    f,
                    "tool handoff exited unexpectedly ({status}; expected {expected})"
                )
            }
            Self::ProposalExecuted => f.write_str(
                "Security invariant violated: Claude Code attempted to execute a Pi proposal tool internally",
            ),
            Self::PrivatePathCall => f.write_str(
                "Claude Code proposed a Pi tool call against provider-private transport state",
            ),
            Self::Other(err) => f.write_str(&err.message),
        }
    }
}

impl std::error::Error for BridgeFault {}

impl From<ProtocolError> for BridgeFault {
    fn from(err: ProtocolError) -> Self {
        match err.code {
            "claude_error" => Self::ClaudeError {
                text: err.message,
                rate_limit: None,
            },
            code if code == "protocol_init" || code.starts_with("isolation_") => Self::Init(err),
            _ => Self::Protocol(err),
        }
    }
}

impl From<LineError> for BridgeFault {
    fn from(err: LineError) -> Self {
        match err {
            LineError::InvalidJson(e) => Self::InvalidJson(e),
            LineError::Protocol(e) => e.into(),
        }
    }
}

impl From<ThinError> for BridgeFault {
    fn from(err: ThinError) -> Self {
        Self::Other(err)
    }
}

/// A bridge error before `Started` that is the bridge's own fault (SPEC §19.15 R3): the mark
/// [`BridgeFault::to_thin`] set from the typed cause, never a match on the message.
pub fn is_bridge_fault(err: &ThinError) -> bool {
    err.is_bridge_fault()
}

/// Claude Code's error result → 429 / 402 / 401 by the vendor's own wording; `None` → 503.
fn claude_error(text: &str, rate_limit: Option<&str>) -> Option<ThinError> {
    if let Some(rl) = rate_limit {
        return Some(rate_limit_error(rl));
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("(429)") || lower.contains("rate limit") {
        let exhausted = lower.contains("quota")
            || lower.contains("usage")
            || lower.contains("payment")
            || lower.contains("exceeded");
        return Some(ThinError::vendor_limit(VENDOR, text, 60, text, exhausted));
    }
    if lower.contains("(401)") || lower.contains("(403)") || lower.contains("not logged in") {
        return Some(ThinError::vendor_login_required(
            VENDOR,
            text,
            "claude login",
        ));
    }
    // Context-overflow wording passes through verbatim (Pi's regexes depend on it).
    None
}

/// A spawned child and its pipes, from `spawn_phase` to the reap.
struct Spawned {
    guard: ChildGuard,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: StderrView,
    stderr_task: JoinHandle<()>,
    /// The daemon Claude was told to launch as `mcp-proposal` (named in the ready timeout).
    thickapi: PathBuf,
    /// `trace.jsonl` in the private dir when `thin.trace` is on.
    trace: Option<PathBuf>,
    /// When the child existed. The first stdout line is the first event; the difference is
    /// the access line's `spawn_ms`.
    spawned_at: std::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    ClientGone,
}

/// How the protocol loop ended.
#[derive(Debug, Clone, Copy)]
struct LoopEnd {
    cause: TerminationCause,
    /// The handoff kill landed through the job object (exit `KNOWN_EXIT` expected).
    killed_by_job: bool,
}

/// What only a live run can check before publishing: the reaped exit, the violation marker,
/// the private dir in proposed arguments.
struct Live<'a> {
    status: ExitStatus,
    killed_by_job: bool,
    stderr: &'a StderrView,
    violation: Option<&'a Path>,
}

impl Live<'_> {
    fn check_handoff(
        &self,
        calls: &[CapturedToolCall],
        tool_use_stop: bool,
    ) -> Result<(), BridgeFault> {
        if self.violation.is_some_and(Path::exists) {
            return Err(BridgeFault::ProposalExecuted);
        }
        if capture::contains_private_path(calls, &self.stderr.private_path) {
            return Err(BridgeFault::PrivatePathCall);
        }
        if !handoff_exit_ok(&self.status, self.killed_by_job, tool_use_stop) {
            return Err(BridgeFault::HandoffExit {
                status: self.status,
                killed_by_job: self.killed_by_job,
            });
        }
        Ok(())
    }
}

/// A dialect `tool_use` whose arguments `to_pi` refused. The whole assistant message is
/// unpublished and the bridge re-spawns once with an `isError` result for every call in it.
#[derive(Debug, Clone)]
pub(crate) struct DialectArgError {
    tool: String,
    reason: String,
    /// Claude-form calls in the message, in order, with the bad one marked.
    calls: Vec<DialectRetryCall>,
    text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DialectRetryCall {
    id: String,
    /// Transport name Claude proposed (`mcp__pi__Edit`).
    name: String,
    /// Claude's arguments, as proposed.
    arguments: String,
    bad: bool,
}

/// Pi-shaped calls. A dialect row runs `to_pi`; a pass-through keeps the captured call.
/// One refusal fails the whole message: a call that did translate is not published either.
fn translate_dialect_calls(
    mapper: &Mapper<'_>,
    calls: &[CapturedToolCall],
) -> Result<Vec<CapturedToolCall>, DialectArgError> {
    let mut out = Vec::with_capacity(calls.len());
    let mut failure: Option<(String, String)> = None;
    for call in calls {
        let Some(row) = crate::thin::dialect::row_by_name(mapper.dialect(), &call.name) else {
            out.push(call.clone());
            continue;
        };
        let parsed = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or(serde_json::Value::Null);
        match row.to_pi_calls(&parsed) {
            Ok(calls) => {
                let stamped = if row.name == "TodoWrite" {
                    match crate::thin::dialect::todo_round1_calls(&call.id, &parsed) {
                        Ok(rounds) => rounds,
                        Err(reason) => {
                            failure.get_or_insert((row.name.to_string(), reason));
                            continue;
                        }
                    }
                } else {
                    calls
                        .into_iter()
                        .map(|arguments| crate::thin::dialect::TodoPiCall {
                            id: call.id.clone(),
                            arguments,
                        })
                        .collect()
                };
                for pi in stamped {
                    out.push(CapturedToolCall {
                        id: pi.id,
                        name: row.pi_tool.to_string(),
                        wire_name: call.wire_name.clone(),
                        arguments: pi.arguments.to_string(),
                    });
                }
            }
            Err(reason) => {
                failure.get_or_insert((row.name.to_string(), reason));
            }
        }
    }
    if let Some((tool, reason)) = failure {
        return Err(DialectArgError {
            tool,
            reason,
            calls: calls
                .iter()
                .map(|call| DialectRetryCall {
                    id: call.id.clone(),
                    name: if call.wire_name.is_empty() {
                        format!("{}{}", catalog::TRANSPORT_PREFIX, call.name)
                    } else {
                        call.wire_name.clone()
                    },
                    arguments: call.arguments.clone(),
                    bad: crate::thin::dialect::row_by_name(mapper.dialect(), &call.name)
                        .is_some_and(|row| {
                            let parsed = serde_json::from_str::<serde_json::Value>(&call.arguments)
                                .unwrap_or(serde_json::Value::Null);
                            row.to_pi(&parsed).is_err()
                        }),
                })
                .collect(),
            text: mapper.forwarded_text(),
        });
    }
    Ok(out)
}

/// Phase 4, the one publish path for the live run and [`replay`] alike: the captured
/// `tool_calls` + `finish tool_calls` after a handoff, or the finish after a successful
/// `result`. `live` is `None` offline, where there is no exit, marker or private dir to check.
fn dialect_args_error(err: &DialectArgError) -> ThinError {
    ThinError::dialect_args(VENDOR, format!("{}: {}", err.tool, err.reason))
}

const NOT_RUN: &str = "Not run: another call in this message had invalid arguments. Call it again.";

/// The request the one retry sees: the original history, then the failed assistant message
/// (its text and every tool call, in Claude form) and one `isError` result per call.
pub(crate) fn with_dialect_error(req: &ThinRequest, err: &DialectArgError) -> ThinRequest {
    use crate::thin::openai::{ChatMessage, Content, FunctionCall, ToolCallOut};
    let mut retry = req.clone();
    let tool_calls = err
        .calls
        .iter()
        .map(|call| ToolCallOut {
            index: None,
            id: call.id.clone(),
            kind: "function".into(),
            function: FunctionCall {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        })
        .collect();
    retry.request.messages.push(ChatMessage {
        role: "assistant".into(),
        content: if err.text.is_empty() {
            None
        } else {
            Some(Content::Text(err.text.clone()))
        },
        name: None,
        tool_calls: Some(tool_calls),
        tool_call_id: None,
        reasoning_content: None,
        reasoning_signature: None,
        reasoning_details: None,
        tool_is_error: false,
    });
    for call in &err.calls {
        let text = if call.bad {
            format!("Error: invalid arguments for {}: {}.", err.tool, err.reason)
        } else {
            NOT_RUN.to_string()
        };
        retry.request.messages.push(ChatMessage {
            role: "tool".into(),
            content: Some(Content::Text(text)),
            name: Some(call.name.clone()),
            tool_calls: None,
            tool_call_id: Some(call.id.clone()),
            reasoning_content: None,
            reasoning_signature: None,
            reasoning_details: None,
            tool_is_error: true,
        });
    }
    retry
}

/// The retry's events, minus text the client already has. A `Started` from the retry is
/// dropped: the route committed on the first one.
async fn forward_retry(events: Vec<ThinEvent>, tx: &EventSender, already: &str) {
    let mut skip = already.to_string();
    for event in events {
        let event = match event {
            ThinEvent::Started { .. } => continue,
            ThinEvent::TextDelta(text) => match text.strip_prefix(&skip) {
                Some("") => continue,
                Some(rest) => {
                    skip.clear();
                    ThinEvent::TextDelta(rest.to_string())
                }
                None if skip.starts_with(&text) => {
                    skip = skip[text.len()..].to_string();
                    continue;
                }
                None => {
                    skip.clear();
                    ThinEvent::TextDelta(text)
                }
            },
            other => other,
        };
        if tx.send(event).await.is_err() {
            break;
        }
    }
}

fn log_dialect_error(request_id: &str, err: &DialectArgError, fatal: bool) {
    crate::thin::dialect::note_error(request_id, &err.tool, &err.reason);
    tracing::info!(
        request_id,
        dialect_error = %format!("{}:{}", err.tool, err.reason),
        fatal,
        "dialect argument error"
    );
}

enum PublishError {
    Fault(BridgeFault),
    /// `to_pi` refused a call. Not published; the caller re-spawns once.
    Dialect(DialectArgError),
}

impl From<BridgeFault> for PublishError {
    fn from(fault: BridgeFault) -> Self {
        Self::Fault(fault)
    }
}

fn publish(
    cause: TerminationCause,
    mapper: &mut Mapper<'_>,
    live: Option<&Live<'_>>,
) -> Result<Vec<ThinEvent>, PublishError> {
    // Already counted at the end of the live loop. Replay has no loop, so count here.
    mapper.note_parallel_drops();
    let mut out = Vec::new();
    match cause {
        TerminationCause::ToolHandoff => {
            let calls = mapper.tool_calls();
            // Vendor-side names, before `to_pi` renames them. A pass-through call keeps
            // the client name, which is what a non-dialect vendor emitted.
            if let Some(id) = mapper.request_id() {
                let names: Vec<String> = calls.iter().map(|call| call.name.clone()).collect();
                crate::thin::dialect::replace_tools(id, &names);
            }
            let calls = translate_dialect_calls(mapper, &calls).map_err(PublishError::Dialect)?;
            if let Some(live) = live {
                live.check_handoff(&calls, mapper.tool_use_stop())?;
            }
            out.extend(calls.into_iter().map(|call| ThinEvent::ToolCall {
                id: call.id,
                name: call.name,
                arguments: call.arguments,
            }));
            out.extend(mapper.usage().map(ThinEvent::Usage));
            out.push(ThinEvent::Finish(FinishReason::ToolCalls));
        }
        TerminationCause::CallerAbort => {}
        TerminationCause::None => {
            let Some(finish) = mapper.successful_finish() else {
                if let Some(rl) = mapper.rate_limit_failure() {
                    return Err(PublishError::Fault(BridgeFault::RateLimited {
                        text: rl.to_string(),
                    }));
                }
                return Err(PublishError::Fault(BridgeFault::Exit {
                    stage: ExitStage::BeforeTerminal,
                    status: live.map(|l| l.status),
                    stderr: live.map(|l| l.stderr.excerpt()).unwrap_or_default(),
                }));
            };
            if let Some(live) = live.filter(|l| !l.status.success()) {
                return Err(PublishError::Fault(BridgeFault::Exit {
                    stage: ExitStage::AfterResult,
                    status: Some(live.status),
                    stderr: live.stderr.excerpt(),
                }));
            }
            out.extend(mapper.usage().map(ThinEvent::Usage));
            out.push(ThinEvent::Finish(finish));
        }
    }
    Ok(out)
}

/// Forward mapped events to the route; `false` once the client is gone.
async fn forward(events: Vec<ThinEvent>, tx: &EventSender, started: &mut bool) -> bool {
    for event in events {
        if matches!(event, ThinEvent::Started { .. }) {
            *started = true;
        }
        if tx.send(event).await.is_err() {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone)]
pub struct BridgeSettings {
    pub tmp_root: PathBuf,
    /// Explicit `claude.exe` path; `None` resolves from `PATH`.
    pub claude_path: Option<PathBuf>,
    /// Binary spawned as the MCP proposal server; `None` uses `current_exe()` (the daemon).
    pub thickapi_exe: Option<PathBuf>,
    pub total_timeout: Duration,
    pub idle_timeout: Duration,
    pub mcp_ready_timeout: Duration,
    pub trace: bool,
    /// Semaphore size around `spawn` (SPEC §19.7 Lifecycle). Default 2.
    pub bridge_max_concurrent: u32,
    /// How long to wait for a bridge slot (`router.queue_timeout_seconds`).
    pub queue_timeout: Duration,
    /// How long to keep draining stdout after a handoff kill, and how long any terminate path
    /// waits for the child to exit, before giving up on it
    /// (`thin.vendors.claude.bridge.handoff_drain_seconds`).
    pub handoff_drain: Duration,
    pub extra_tools: Vec<String>,
    pub nonempty_inventories: Vec<String>,
    /// `thin.vendors.claude.bridge.dialect`. `off` leaves catalog, transcript and publish as
    /// they were; `claude_code` (the default) shows Pi tools under Claude Code names.
    pub dialect: String,
    /// Catalogue ids the daemon loaded. `Agent`'s `model` alias resolves through these.
    /// Empty when the bridge was not built from a catalogue (tests, `doctor`).
    pub catalogue_ids: Vec<String>,
}

impl BridgeSettings {
    /// `thin.vendors.claude.bridge.handoff_drain_seconds`' default.
    pub fn default_handoff_drain() -> Duration {
        Duration::from_secs(crate::config::router::ClaudeBridgeCfg::default().handoff_drain_seconds)
    }

    pub fn from_config(cfg: &ConfigBundle) -> Self {
        let thin = &cfg.router.thin;
        let claude = &thin.vendors.claude;
        let bridge = &claude.bridge;
        Self {
            tmp_root: cfg.config_dir.join("tmp"),
            claude_path: claude.path.clone(),
            thickapi_exe: None,
            total_timeout: Duration::from_secs(bridge.timeout_seconds.max(1)),
            idle_timeout: Duration::from_secs(bridge.idle_seconds.max(1)),
            mcp_ready_timeout: Duration::from_millis(bridge.mcp_ready_timeout_ms.max(100)),
            trace: thin.trace,
            bridge_max_concurrent: bridge.max_concurrent.max(1),
            queue_timeout: Duration::from_secs(cfg.router.queue_timeout_seconds),
            handoff_drain: Duration::from_secs(bridge.handoff_drain_seconds.max(1)),
            extra_tools: bridge.tolerate.extra_tools.clone(),
            nonempty_inventories: bridge.tolerate.nonempty_inventories.clone(),
            dialect: bridge.dialect.clone(),
            catalogue_ids: crate::thin::dialect::catalogue_ids(&cfg.models),
        }
    }
}

pub struct ClaudeBridge {
    settings: BridgeSettings,
    version: Arc<crate::thin::oauth::vendor::CliVersion>,
    tap: wire::Tap,
    /// `ok | drift` (SPEC §19.15 R3), updated by every `run_bridge` and `handshake`.
    state: BridgeState,
    /// Around spawn only (OAuth adapters take `oauth_in_flight` separately).
    slots: Arc<tokio::sync::Semaphore>,
}

/// Outcome summary for `doctor` / `/ready` probes.
#[derive(Debug, Clone)]
pub struct Handshake {
    pub claude_path: PathBuf,
    pub claude_version: Option<String>,
    pub resolved_model: String,
    pub text: String,
}

impl ClaudeBridge {
    /// Active dialect rows for this request. `off`, and any unknown value, is none.
    /// The loaded catalogue ids travel with the rows so `Agent` can map `model`.
    fn dialect_rows(
        &self,
        req: &crate::thin::openai::ChatRequest,
    ) -> Vec<crate::thin::dialect::ActiveRow> {
        if self.settings.dialect == crate::thin::dialect::MODE_CLAUDE_CODE {
            let ids: Vec<&str> = self
                .settings
                .catalogue_ids
                .iter()
                .map(String::as_str)
                .collect();
            crate::thin::dialect::activate_with(&req.tools, &ids)
        } else {
            Vec::new()
        }
    }

    pub fn new(settings: BridgeSettings) -> Self {
        let slots = Arc::new(tokio::sync::Semaphore::new(
            settings.bridge_max_concurrent.max(1) as usize,
        ));
        let claude_path = settings.claude_path.clone();
        // Same resolution and scrubbed environment as a bridge spawn: an inherited
        // `ANTHROPIC_*` must not change what `--version` reports.
        let version = crate::thin::oauth::vendor::CliVersion::with(
            "claude",
            Box::new(move || spawn::resolve_claude(claude_path.as_deref())),
            Box::new(|command| {
                command
                    .env_clear()
                    .envs(spawn::claude_env(None, false, None));
            }),
        );
        Self {
            settings,
            version,
            tap: None,
            state: BridgeState::new(),
            slots,
        }
    }

    /// `ok | drift(reason, since)` for `/ready`, `doctor` and the adapter's path choice (R3).
    pub fn state(&self) -> &BridgeState {
        &self.state
    }

    /// Record the exchange (`thickapi capture claude-bridge`): argv as the request, each
    /// stream-json line as a frame. `serve` never sets one.
    pub fn set_tap(&mut self, tap: wire::Tap) {
        self.tap = tap;
    }

    pub fn settings(&self) -> &BridgeSettings {
        &self.settings
    }

    /// Wait for a bridge spawn slot. Longer than `queue_timeout` → 429, retry in 5 s.
    pub async fn acquire_slot(
        &self,
        tx: &EventSender,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ThinError> {
        match self.slots.clone().try_acquire_owned() {
            Ok(permit) => Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => {
                Err(ThinError::server("bridge semaphore closed"))
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                tokio::select! {
                    biased;
                    _ = tx.closed() => Err(ThinError::vendor_down(VENDOR, "client disconnected")),
                    permit = self.slots.clone().acquire_owned() => {
                        permit.map_err(|_| ThinError::server("bridge semaphore closed"))
                    }
                    _ = tokio::time::sleep(self.settings.queue_timeout) => {
                        Err(ThinError::rate_limited(
                            VENDOR,
                            "bridge concurrency limit reached",
                            5,
                            false,
                        ))
                    }
                }
            }
        }
    }

    pub fn resolve_claude(&self) -> Option<PathBuf> {
        spawn::resolve_claude(self.settings.claude_path.as_deref())
    }

    /// `claude --version`: probed once (bounded) if nothing is known yet, then kept fresh
    /// in the background when the exe changes (#124). `None` when the CLI is missing or
    /// does not answer.
    pub fn claude_version(&self) -> Option<String> {
        self.version.wait()
    }

    /// The last known `claude --version`, never waiting on the CLI (`/ready`, #124).
    pub fn claude_version_now(&self) -> Option<String> {
        self.version.current()
    }

    /// The shared version cache (the direct path's `user-agent` follows it).
    pub fn version_source(&self) -> Arc<crate::thin::oauth::vendor::CliVersion> {
        self.version.clone()
    }

    fn vendor_label(&self) -> String {
        match self.claude_version() {
            Some(v) => format!("claude {v}"),
            None => "claude".to_string(),
        }
    }

    /// The client error for `fault`; the version label is looked up only when it is shown.
    fn thin_error(&self, fault: &BridgeFault) -> ThinError {
        let label = fault.labelled().then(|| self.vendor_label());
        fault.to_thin(label.as_deref())
    }

    /// One handshake turn for `doctor`: production argv, a one-token prompt, the fixture catalog.
    pub async fn handshake(&self, model: &str) -> Result<Handshake, ThinError> {
        let request: crate::thin::openai::ChatRequest = serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "Reply with the single word OK."}],
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file.",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}],
            "max_tokens": 8
        }))
        .map_err(|e| ThinError::server(e.to_string()))?;
        let req = ThinRequest {
            request,
            entry: crate::config::models::ModelEntry::bridge_probe(model),
            effort: Some(Effort::Off),
            request_id: format!("doctor-{}", uuid::Uuid::new_v4()),
            path: "bridge",
            fallback: None,
            cursor_cwd: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let runner = async {
            let result = self.run_bridge(&req, &tx).await;
            drop(tx); // close the channel so the collector finishes
            result
        };
        let collector = async {
            let mut text = String::new();
            let mut model = String::new();
            let mut error = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    ThinEvent::Started { model: m, .. } => model = m,
                    ThinEvent::TextDelta(t) => text.push_str(&t),
                    ThinEvent::Error(e) => error = Some(e),
                    _ => {}
                }
            }
            (text, model, error)
        };
        let (run, (text, resolved_model, error)) = tokio::join!(runner, collector);
        run?;
        if let Some(err) = error {
            return Err(err);
        }
        Ok(Handshake {
            claude_path: self.resolve_claude().unwrap_or_default(),
            claude_version: self.claude_version(),
            resolved_model,
            text,
        })
    }

    /// Run one request. Errors before `Started` are returned; after it they are sent as
    /// `ThinEvent::Error` (the route has already committed to a 200). A successful run clears
    /// `bridge_state`; a bridge fault before `Started` sets it to `drift` (R3).
    pub async fn run_bridge(&self, req: &ThinRequest, tx: &EventSender) -> Result<(), ThinError> {
        self.run_bridge_attempt(req, tx, None).await
    }

    /// One bridge run. `held` is `Some(n)` when the caller already holds a spawn slot
    /// ([`ClaudeBridge::acquire_slot`]) and this is attempt `n` of a SPEC §19.16 same-path
    /// retry: no second slot is taken, and attempt `n > 0` gets its own private dir name
    /// (`PrivateDir::create` refuses one that exists, and `trace` keeps the first).
    pub async fn run_bridge_attempt(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        held: Option<u32>,
    ) -> Result<(), ThinError> {
        // SPEC §19.7: a TodoWrite round 1 whose round 2 is still pending is answered here.
        // No spawn, no semaphore slot, no empty-stop retry (the caller sees tool calls).
        if let Some(events) = self.todo_round2(req) {
            for event in events {
                if tx.send(event).await.is_err() {
                    break;
                }
            }
            self.state.mark_ok();
            return Ok(());
        }
        let mut started = false;
        let fault = match self.run_inner(req, tx, &mut started, held).await {
            Ok(()) => {
                self.state.mark_ok();
                return Ok(());
            }
            Err(fault) => fault,
        };
        let err = self.thin_error(&fault);
        if started {
            let _ = tx.send(ThinEvent::Error(err)).await;
            return Ok(());
        }
        if fault.is_bridge_fault() {
            tracing::warn!(request_id = %req.request_id, code = err.code, message = %crate::thin::oauth::state::redact_log(&err.message), "claude bridge fault before Started; bridge_state = drift");
            self.state
                .mark_drift(format!("{}: {}", err.code, err.message));
        }
        Err(err)
    }

    /// The composition (SPEC §19.7 **Lifecycle**): spawn, wait for the proposal server, run
    /// the protocol, reap, publish. The private dir is declared before the child's guard, and
    /// every early exit after the spawn goes through [`ChildGuard::finish`] first.
    async fn run_inner(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        started: &mut bool,
        held: Option<u32>,
    ) -> Result<(), BridgeFault> {
        self.run_inner_counted(req, tx, started, held, 0).await
    }

    /// `dialect_tries` is how many `to_pi` failures this request has already re-spawned for.
    /// The §19.16 `held` counter is a different retry and must not be read as one of these.
    async fn run_inner_counted(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        started: &mut bool,
        held: Option<u32>,
        dialect_tries: u32,
    ) -> Result<(), BridgeFault> {
        let (claude, thickapi) = self.preflight(req)?;
        let _slot = match held {
            Some(_) => None,
            None => Some(self.acquire_slot(tx).await?),
        };
        let dir = self.private_dir(req, held)?;
        let active = self.dialect_rows(&req.request);
        let prepared = transcript::prepare(&req.request, &dir, &active)?;
        let mut sp = self
            .spawn_phase(req, &dir, &prepared, &claude, thickapi)
            .await?;
        let expected = prepared.offered.expected_tools();
        let mut mapper = self
            .mapper(req, &expected, &prepared, &sp.stderr.private_path)
            .with_dialect(prepared.offered.active.clone());
        let end = match self
            .drive(req, &mut sp, &prepared, tx, &mut mapper, started)
            .await
        {
            Ok(Some(end)) => end,
            Ok(None) => {
                sp.guard.finish().await;
                return Ok(());
            }
            Err(fault) => {
                sp.guard.finish().await;
                return Err(fault.with_stderr(sp.stderr.excerpt()));
            }
        };
        // The child must be dead before anything is published.
        let status = sp.guard.reap().await?;
        let _ = tokio::time::timeout(Duration::from_secs(2), sp.stderr_task).await;
        if end.cause == TerminationCause::CallerAbort {
            tracing::debug!(request = %req.request_id, "client disconnected; bridge child killed");
            return Ok(());
        }
        let live = Live {
            status,
            killed_by_job: end.killed_by_job,
            stderr: &sp.stderr,
            violation: prepared.violation_path.as_deref(),
        };
        match publish(end.cause, &mut mapper, Some(&live)) {
            Ok(events) => {
                for event in events {
                    let _ = tx.send(event).await;
                }
                Ok(())
            }
            Err(PublishError::Fault(fault)) => Err(fault),
            Err(PublishError::Dialect(err)) => {
                self.retry_dialect(req, tx, held, dialect_tries, &err).await
            }
        }
    }

    /// Spike 2, option (a): one re-spawn inside this request. A second `to_pi` failure is
    /// `502 dialect_args`. A private-path miss is not this path; `publish` reports it as a
    /// fault and it is not retried.
    async fn retry_dialect(
        &self,
        req: &ThinRequest,
        tx: &EventSender,
        held: Option<u32>,
        dialect_tries: u32,
        err: &DialectArgError,
    ) -> Result<(), BridgeFault> {
        log_dialect_error(&req.request_id, err, false);
        if dialect_tries >= 1 {
            log_dialect_error(&req.request_id, err, true);
            return Err(BridgeFault::Other(dialect_args_error(err)));
        }
        let retry = with_dialect_error(req, err);
        let next = held.map(|n| n.saturating_add(1)).unwrap_or(1);
        let (inner_tx, mut inner_rx) = tokio::sync::mpsc::channel(64);
        let mut started = true;
        let run = Box::pin(async move {
            let result = self
                .run_inner_counted(
                    &retry,
                    &inner_tx,
                    &mut started,
                    Some(next),
                    dialect_tries + 1,
                )
                .await;
            drop(inner_tx);
            result
        });
        // Drain while the attempt runs. Forwarding to `tx` here would wait on the route,
        // which is not reading until this attempt returns. The sender is dropped with the
        // attempt, so the drain ends when the attempt does.
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = inner_rx.recv().await {
                events.push(event);
            }
            events
        };
        let (result, events) = tokio::join!(run, collect);
        forward_retry(events, tx, &err.text).await;
        result
    }

    /// Round 2 of a TodoWrite, when the trailing tool results complete a round 1 whose
    /// round 2 is not empty. `None` when this request should spawn `claude.exe` as usual.
    /// Deterministic: the same history yields the same calls and the same ids.
    fn todo_round2(&self, req: &ThinRequest) -> Option<Vec<ThinEvent>> {
        if self.settings.dialect != crate::thin::dialect::MODE_CLAUDE_CODE {
            return None;
        }
        let active = self.dialect_rows(&req.request);
        if !active.iter().any(|row| row.name == "TodoWrite") {
            return None;
        }
        let calls = crate::thin::dialect::pending_todo_round2(&req.request.messages)?;
        if calls.is_empty() {
            return None;
        }
        let model = req.entry.wire_model().to_string();
        let mut events = vec![ThinEvent::Started {
            model,
            path: "bridge",
            fallback: req.fallback,
            wire_params: None,
        }];
        let n = calls.len();
        events.extend(calls.into_iter().map(|call| ThinEvent::ToolCall {
            id: call.id,
            name: "todo".to_string(),
            arguments: call.arguments.to_string(),
        }));
        // The route will see `todo`. The access line must still name the vendor tool.
        crate::thin::dialect::replace_tools(&req.request_id, &vec!["TodoWrite".to_string(); n]);
        events.push(ThinEvent::Usage(crate::thin::openai::Usage::default()));
        events.push(ThinEvent::Finish(FinishReason::ToolCalls));
        Some(events)
    }

    /// Before any slot or process: the `claude.exe` and daemon paths. `tool_choice` is
    /// honoured in [`transcript::prepare`], not rejected here (SPEC §19.3).
    fn preflight(&self, _req: &ThinRequest) -> Result<(PathBuf, PathBuf), BridgeFault> {
        let claude = self.resolve_claude().ok_or(BridgeFault::CliUnresolved)?;
        let thickapi = match &self.settings.thickapi_exe {
            Some(path) => path.clone(),
            None => {
                spawn::thickapi_exe().map_err(|e| ThinError::server(format!("current_exe: {e}")))?
            }
        };
        Ok((claude, thickapi))
    }

    fn private_dir(&self, req: &ThinRequest, held: Option<u32>) -> Result<PrivateDir, BridgeFault> {
        let dir_name = match held {
            Some(n) if n > 0 => format!("{}-r{n}", req.request_id),
            _ => req.request_id.clone(),
        };
        let mut dir = PrivateDir::create(&self.settings.tmp_root, &dir_name)
            .map_err(|e| ThinError::server(format!("create private request dir: {e}")))?;
        if self.settings.trace {
            dir.keep();
        }
        Ok(dir)
    }

    fn mapper<'a>(
        &self,
        req: &ThinRequest,
        expected: &'a std::collections::BTreeSet<String>,
        prepared: &'a Prepared,
        private_path: &str,
    ) -> Mapper<'a> {
        let mut mapper = Mapper::new(
            expected,
            &prepared.offered.transport_to_client,
            vec![private_path.to_string()],
            req.request.parallel_tool_calls(),
        )
        .with_targets(&prepared.offered.transport_target)
        .with_tolerate(
            self.settings.extra_tools.clone(),
            self.settings.nonempty_inventories.clone(),
        )
        .with_log_context(&req.request_id, self.claude_version_now());
        mapper.path = req.path;
        mapper.fallback = req.fallback;
        mapper.wire_params = spawn::wire_params(req.effort);
        mapper
    }

    /// Phase 1: argv, env, the MCP catalog paths, the child in its job object, the stderr
    /// drain. A failure after the spawn finishes the child before returning.
    async fn spawn_phase(
        &self,
        req: &ThinRequest,
        dir: &PrivateDir,
        prepared: &Prepared,
        claude: &Path,
        thickapi: PathBuf,
    ) -> Result<Spawned, BridgeFault> {
        let mcp_paths = match prepared.catalog_path.as_deref() {
            None => None,
            Some(catalog) => Some(spawn::McpPaths {
                catalog,
                ready: prepared
                    .ready_path
                    .as_deref()
                    .ok_or_else(|| ThinError::server("ready path with catalog"))?,
                violation: prepared
                    .violation_path
                    .as_deref()
                    .ok_or_else(|| ThinError::server("violation path with catalog"))?,
            }),
        };
        let mcp_json = spawn::mcp_config_json(&thickapi, mcp_paths.as_ref());
        let model = req.entry.wire_model().to_string();
        let effort = req.effort;
        let args = spawn::claude_args(&mcp_json, &model, effort, &prepared.system_prompt_path);
        let env = spawn::claude_env(
            req.request.max_output_tokens(),
            spawn::thinking_off(effort),
            prepared.catalog_path.as_deref(),
        );
        self.tap_spawn(req, claude, &args, &env, prepared);
        let spawned =
            spawn::spawn(claude, dir.path(), &args, &env).map_err(|error| BridgeFault::Spawn {
                cli: claude.to_path_buf(),
                error,
            })?;
        if let Err(e) = dir.record_child(spawned.pid) {
            tracing::warn!(request_id = %req.request_id, error = %e, "bridge: could not record child pid; sweeper will not see it");
        }
        tracing::debug!(pid = spawned.pid, request = %req.request_id, "claude bridge spawned");
        let mut guard = ChildGuard::new(spawned, self.settings.handoff_drain);
        let (stdin, stdout, stderr) = match guard.take_pipes() {
            Ok(pipes) => pipes,
            Err(fault) => {
                guard.finish().await;
                return Err(fault);
            }
        };
        let view = StderrView {
            tail: Arc::default(),
            private_path: dir.path().to_string_lossy().into_owned(),
        };
        let stderr_task = drain_stderr(stderr, Arc::clone(&view.tail));
        Ok(Spawned {
            guard,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            stderr: view,
            stderr_task,
            thickapi,
            trace: self.settings.trace.then(|| dir.path().join("trace.jsonl")),
            spawned_at: std::time::Instant::now(),
        })
    }

    /// `thickapi capture claude-bridge`: argv and the bridge's own env as the request.
    fn tap_spawn(
        &self,
        req: &ThinRequest,
        claude: &Path,
        args: &[String],
        env: &std::collections::BTreeMap<String, String>,
        prepared: &Prepared,
    ) {
        let Some(tap) = &self.tap else {
            return;
        };
        // "headers" = the env the bridge sets itself; the OS passthrough allowlist is noise.
        let own_env = env
            .iter()
            .filter(|(k, _)| !spawn::ENV_ALLOWLIST.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let tools: Vec<&String> = prepared.catalog.transport_to_client.values().collect();
        // The declared schemas, so a replay can activate the same dialect rows this spawn did.
        let declared: Vec<serde_json::Value> = req
            .request
            .tools
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null))
            .collect();
        tap.request(wire::RequestRecord {
            method: "spawn".into(),
            url: claude.to_string_lossy().into_owned(),
            headers: own_env,
            body: Some(json!({
                "args": args,
                "tools": tools,
                "declared": declared,
                "prompt": prepared.prompt_blocks(),
                "system_prompt": std::fs::read_to_string(&prepared.system_prompt_path).unwrap_or_default(),
            })),
            body_b64: None,
        });
    }

    /// Phases 2–3 between spawn and reap. `Ok(None)`: the client left during the ready wait.
    /// `sp.spawned_at` is the moment the child existed; the first stdout line is the first event.
    async fn drive(
        &self,
        req: &ThinRequest,
        sp: &mut Spawned,
        prepared: &Prepared,
        tx: &EventSender,
        mapper: &mut Mapper<'_>,
        started: &mut bool,
    ) -> Result<Option<LoopEnd>, BridgeFault> {
        if self
            .wait_ready(sp, prepared.ready_path.as_deref(), tx)
            .await?
            == Readiness::ClientGone
        {
            return Ok(None);
        }
        write_prompt(sp, prepared, &req.request_id).await?;
        self.protocol_loop(sp, tx, mapper, started).await.map(Some)
    }

    /// Phase 2: the proposal server (Claude spawns it) must be ready before the prompt is
    /// written. Bounded by `mcp_ready_timeout`.
    async fn wait_ready(
        &self,
        sp: &mut Spawned,
        ready: Option<&Path>,
        tx: &EventSender,
    ) -> Result<Readiness, BridgeFault> {
        let Some(ready) = ready else {
            return Ok(Readiness::Ready);
        };
        let deadline = Instant::now() + self.settings.mcp_ready_timeout;
        loop {
            if ready.exists() {
                return Ok(Readiness::Ready);
            }
            if let Some(status) = sp.guard.try_wait() {
                return Err(BridgeFault::Exit {
                    stage: ExitStage::BeforeMcpReady,
                    status: Some(status),
                    stderr: String::new(),
                });
            }
            if Instant::now() >= deadline {
                return Err(BridgeFault::McpNotReady {
                    waited: self.settings.mcp_ready_timeout,
                    thickapi: sp.thickapi.clone(),
                    stderr: String::new(),
                });
            }
            tokio::select! {
                _ = tx.closed() => return Ok(Readiness::ClientGone),
                _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
        }
    }

    /// Phase 3: the ordered JSONL protocol → live events; the tool handoff kill and its drain.
    async fn protocol_loop(
        &self,
        sp: &mut Spawned,
        tx: &EventSender,
        mapper: &mut Mapper<'_>,
        started: &mut bool,
    ) -> Result<LoopEnd, BridgeFault> {
        let mut trace = sp
            .trace
            .as_ref()
            .and_then(|path| std::fs::File::create(path).ok());
        let total_deadline = Instant::now() + self.settings.total_timeout;
        let mut noted_spawn = false;
        let mut end = LoopEnd {
            cause: TerminationCause::None,
            killed_by_job: false,
        };
        let mut handoff_deadline: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let mut wait = self
                .settings
                .idle_timeout
                .min(total_deadline.saturating_duration_since(now));
            if let Some(h) = handoff_deadline {
                wait = wait.min(h.saturating_duration_since(now));
            }
            let next =
                tokio::time::timeout(wait.max(Duration::from_millis(1)), sp.stdout.next_line())
                    .await;
            let line = match next {
                Err(_) if end.cause == TerminationCause::ToolHandoff => break,
                Err(_) if Instant::now() >= total_deadline => {
                    return Err(BridgeFault::Timeout {
                        budget: self.settings.total_timeout,
                        stderr: String::new(),
                    });
                }
                Err(_) => {
                    return Err(BridgeFault::Idle {
                        budget: self.settings.idle_timeout,
                        stderr: String::new(),
                    });
                }
                Ok(Err(e)) => return Err(BridgeFault::Stdout(e)),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(line))) => line,
            };
            if !noted_spawn {
                noted_spawn = true;
                if let Some(id) = mapper.request_id() {
                    crate::thin::dialect::note_spawn_ms(
                        id,
                        sp.spawned_at.elapsed().as_millis() as u64,
                    );
                }
            }
            if let Some(file) = trace.as_mut() {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
            }
            wire::tap_frame(&self.tap, format!("{line}\n").as_bytes());
            let signals = match mapper.accept_line(&line, end.cause) {
                Ok(signals) => signals,
                Err(LineError::InvalidJson(e)) => return Err(BridgeFault::InvalidJson(e)),
                Err(LineError::Protocol(err)) => {
                    self.add_skipped(mapper);
                    return Err(BridgeFault::from(err).with_rate_limit(mapper.rate_limit_failure()));
                }
            };
            if !forward(mapper.drain(), tx, started).await {
                end.cause = TerminationCause::CallerAbort;
                sp.guard.terminate();
                break;
            }
            if signals.terminate_for_tool_use {
                end.cause = TerminationCause::ToolHandoff;
                end.killed_by_job = sp.guard.terminate();
                handoff_deadline = Some(Instant::now() + self.settings.handoff_drain);
            }
        }
        mapper.note_parallel_drops();
        self.add_skipped(mapper);
        Ok(end)
    }

    fn add_skipped(&self, mapper: &Mapper<'_>) {
        self.state.add_skipped(
            mapper.skipped_records(),
            mapper.skipped_events(),
            mapper.skipped_blocks(),
            mapper.skipped_parallel(),
        );
    }
}

/// Write the prompt record and close stdin (Claude reads to EOF).
async fn write_prompt(
    sp: &mut Spawned,
    prepared: &Prepared,
    request_id: &str,
) -> Result<(), BridgeFault> {
    let prompt = json!({
        "type": "user",
        "message": {"role": "user", "content": prepared.prompt_blocks()},
    });
    let mut line = serde_json::to_vec(&prompt).map_err(|e| ThinError::server(e.to_string()))?;
    line.push(b'\n');
    let mut stdin = sp
        .stdin
        .take()
        .ok_or_else(|| ThinError::server("no stdin"))?;
    if let Err(error) = stdin.write_all(&line).await {
        return Err(BridgeFault::StdinWrite {
            error,
            stderr: String::new(),
        });
    }
    // A child that already closed its end is normal; any other failure is worth a line.
    if let Err(e) = stdin.shutdown().await {
        tracing::debug!(request_id, error = %e, "bridge: closing Claude Code stdin failed");
    }
    Ok(())
}

/// Offline replay of captured stream-json lines (one per frame) through the same [`Mapper`]
/// and `publish` the live run uses (fixture tests, SPEC §19.11). `tool_names` are the client
/// tool names the request declared. Process-exit checks, the private-path guard and the
/// violation marker need a live child and are skipped.
///
/// `dialect` turns the rows on for the names declared, the way a live capture with the
/// dialect set does. Off leaves the catalog a pass-through, which is what every set captured
/// before the dialect scenarios replays as.
pub fn replay(frames: &[Vec<u8>], tool_names: &[&str]) -> Vec<ThinEvent> {
    replay_with(frames, tool_names, false)
}

/// Names only: replay has no schemas, so a dialect row would not activate. The fixture
/// test that wants the dialect on passes the schemas through [`replay_with_tools`].
fn replay_tools(tool_names: &[&str]) -> Vec<crate::thin::openai::ToolDef> {
    tool_names
        .iter()
        .map(|name| crate::thin::openai::ToolDef {
            kind: "function".into(),
            function: crate::thin::openai::FunctionDef {
                name: (*name).to_string(),
                description: None,
                parameters: None,
                strict: None,
            },
        })
        .collect()
}

/// [`replay`] with the dialect on or off. `tool_names` carry no schema, so dialect rows stay
/// inactive unless the caller uses [`replay_with_tools`].
pub fn replay_with(frames: &[Vec<u8>], tool_names: &[&str], dialect: bool) -> Vec<ThinEvent> {
    replay_with_tools(frames, &replay_tools(tool_names), dialect)
}

/// [`replay_with`], but the declared tools carry their schemas, so a dialect row activates
/// when its `requires` paths are present. `wire_fixtures` uses this for the `dialect` and
/// `todo` scenarios.
pub fn replay_with_tools(
    frames: &[Vec<u8>],
    tools: &[crate::thin::openai::ToolDef],
    dialect: bool,
) -> Vec<ThinEvent> {
    let active = if dialect {
        crate::thin::dialect::activate(tools)
    } else {
        Vec::new()
    };
    let built = catalog::build_catalog_with(tools, &active).unwrap_or_default();
    let expected = built.expected_tools();
    let mut mapper = Mapper::new(&expected, &built.transport_to_client, vec![], true)
        .with_targets(&built.transport_target)
        .with_dialect(built.active)
        .with_log_context("replay", None);
    let mapper = &mut mapper;
    let mut out = Vec::new();
    let mut cause = TerminationCause::None;
    for frame in frames {
        for line in String::from_utf8_lossy(frame).lines() {
            match mapper.accept_line(line, cause) {
                Ok(signals) => {
                    out.extend(mapper.drain());
                    if signals.terminate_for_tool_use {
                        cause = TerminationCause::ToolHandoff;
                    }
                }
                Err(err) => {
                    let fault = BridgeFault::from(err).with_rate_limit(mapper.rate_limit_failure());
                    out.push(ThinEvent::Error(fault.to_thin(None)));
                    return out;
                }
            }
        }
    }
    match publish(cause, mapper, None) {
        Ok(events) => out.extend(events),
        Err(PublishError::Fault(fault)) => out.push(ThinEvent::Error(fault.to_thin(None))),
        Err(PublishError::Dialect(err)) => {
            out.push(ThinEvent::Error(dialect_args_error(&err)));
        }
    }
    out
}

fn rate_limit_error(text: &str) -> ThinError {
    // "resets at 2026-..Z" → seconds until then, floor 60.
    let retry_after = text
        .split("resets at ")
        .nth(1)
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(
                s.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()),
            )
            .ok()
        })
        .map(|t| (t.timestamp() - chrono::Utc::now().timestamp()).max(60) as u64)
        .unwrap_or(300);
    let lower = text.to_ascii_lowercase();
    let exhausted = lower.contains("quota")
        || lower.contains("usage")
        || lower.contains("payment")
        || lower.contains("rate limit")
        || lower.contains("(429)");
    ThinError::vendor_limit(VENDOR, text, retry_after, text, exhausted)
}

fn handoff_exit_ok(status: &ExitStatus, killed_by_job: bool, tool_use_stop: bool) -> bool {
    // C16: the child may have exited cleanly before the kill landed.
    if tool_use_stop && status.success() {
        return true;
    }
    if killed_by_job {
        return status.code() == Some(spawn::KNOWN_EXIT as i32);
    }
    #[cfg(windows)]
    {
        status.code() == Some(spawn::TASKKILL_EXIT)
            || status.code() == Some(spawn::KNOWN_EXIT as i32)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().is_some()
    }
}

impl ModelAdapter for ClaudeBridge {
    fn vendor(&self) -> &'static str {
        VENDOR
    }

    fn run<'a>(&'a self, req: &'a ThinRequest, tx: EventSender) -> AdapterFuture<'a> {
        Box::pin(async move { self.run_bridge(req, &tx).await })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// 4 KiB reads of multibyte text put the cap's cut point inside a char.
    #[test]
    fn stderr_tail_survives_a_cut_inside_a_multibyte_char() {
        let mut tail = StderrTail::default();
        let noise = "warn: 🙂 héllo — ".repeat(8000);
        for chunk in noise.as_bytes().chunks(4096) {
            tail.push(chunk);
        }
        tail.push(b"\nfatal: not logged in\n");
        assert_eq!(tail.0.len(), MAX_STDERR_BYTES);
        let excerpt = tail.excerpt("C:\\nowhere");
        assert!(excerpt.ends_with("fatal: not logged in"), "{excerpt}");
        assert!(excerpt.chars().count() <= STDERR_EXCERPT + 30);
    }

    #[test]
    fn stderr_excerpt_hides_tokens_and_the_private_dir() {
        let mut tail = StderrTail::default();
        tail.push(b"auth failed for sk-ant-oat01-abcdefghijklmnopqrstuvwxyz0123456789\r\n");
        tail.push(b"while reading C:\\Users\\x\\.thickapi\\tmp\\req-1\\mcp.json");
        let excerpt = tail.excerpt("C:\\Users\\x\\.thickapi\\tmp\\req-1");
        assert!(!excerpt.contains("sk-ant-oat01-abc"), "{excerpt}");
        assert!(!excerpt.contains("req-1"), "{excerpt}");
        assert!(!excerpt.contains('\n'));

        // Node tooling prints paths JSON-escaped; the private dir must not survive that either.
        let mut escaped = StderrTail::default();
        escaped.push(br#"{"path":"C:\\Users\\x\\.thickapi\\tmp\\req-1\\mcp.json"}"#);
        let shown = escaped.excerpt(r"C:\Users\x\.thickapi\tmp\req-1");
        assert!(!shown.contains("req-1"), "{shown}");
        assert_eq!(StderrTail::default().excerpt(""), "");
    }

    #[test]
    fn rate_limit_error_is_429_no_retry_with_retry_after() {
        let err = rate_limit_error(
            "Claude rate limit rejected (five_hour); resets at 2099-01-01T00:00:00Z",
        );
        assert_eq!(err.status, 429);
        assert!(err.no_retry());
        assert!(err.message.starts_with("quota exceeded"), "{}", err.message);
        assert!(err.retry_after_seconds().unwrap() > 60);
        let err = rate_limit_error("Claude rate limit rejected (seven_day)");
        assert_eq!(err.retry_after_seconds(), Some(300));
        assert!(err.message.starts_with("quota exceeded"), "{}", err.message);
    }

    #[test]
    fn protocol_errors_map_to_drift_and_claude_errors_to_down() {
        let label = Some("claude 2.1.283");
        let drift = BridgeFault::from(ProtocolError::new("isolation_auth", "x")).to_thin(label);
        assert_eq!(drift.status, 502);
        assert_eq!(drift.code, "vendor_drift");
        assert_eq!(drift.message, "claude 2.1.283: x (isolation_auth)");
        let down = BridgeFault::from(ProtocolError::new(
            "claude_error",
            "Claude Code request failed (529): Overloaded",
        ))
        .to_thin(label);
        assert_eq!(down.status, 503);
        assert_eq!(
            down.message,
            "claude 2.1.283: Claude Code request failed (529): Overloaded"
        );
        let rl = BridgeFault::from(ProtocolError::new(
            "claude_error",
            "Claude Code request failed (429): slow down",
        ))
        .to_thin(label);
        assert_eq!(rl.status, 429);
        assert!(!rl.no_retry());
        let login = BridgeFault::from(ProtocolError::new(
            "claude_error",
            "Claude Code request failed: not logged in",
        ))
        .to_thin(label);
        assert_eq!(login.code, "vendor_login_required");
        let notice = BridgeFault::from(ProtocolError::new("claude_error", "boom"))
            .with_rate_limit(Some("Claude rate limit rejected (five_hour)"));
        assert_eq!(notice.to_thin(label).status, 429);
    }

    fn exit_status(code: u32) -> ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(code)
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code as i32)
        }
    }

    /// (a) The R3 trigger is the variant, every variant: a vendor error or stderr that reads
    /// "not found" / "spawn" is not a bridge fault (audit C15).
    #[test]
    fn is_bridge_fault_is_a_match_on_the_variant() {
        let io = || io::Error::other("x");
        let noisy = || "; Claude Code stderr: spawn node ENOENT: claude.exe not found".to_string();
        let claude = |text: &str| BridgeFault::ClaudeError {
            text: text.into(),
            rate_limit: None,
        };
        let exit = |stage| BridgeFault::Exit {
            stage,
            status: Some(exit_status(1)),
            stderr: noisy(),
        };
        let faults = vec![
            BridgeFault::CliUnresolved,
            BridgeFault::Spawn {
                cli: "claude.exe".into(),
                error: io(),
            },
            BridgeFault::McpNotReady {
                waited: Duration::from_millis(100),
                thickapi: "thickapi.exe".into(),
                stderr: String::new(),
            },
            ProtocolError::new("isolation_tools", "tools").into(),
            ProtocolError::new("protocol_blocks", "open").into(),
            BridgeFault::InvalidJson(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
            BridgeFault::HandoffExit {
                status: exit_status(1),
                killed_by_job: true,
            },
        ];
        let not_faults = vec![
            claude("Claude Code request failed: model not found"),
            claude("spawn: claude.exe not found; MCP did not become ready in time"),
            BridgeFault::RateLimited {
                text: "rate limit".into(),
            },
            BridgeFault::Timeout {
                budget: Duration::from_secs(1),
                stderr: noisy(),
            },
            BridgeFault::Idle {
                budget: Duration::from_secs(30),
                stderr: noisy(),
            },
            BridgeFault::StdinWrite {
                error: io(),
                stderr: noisy(),
            },
            BridgeFault::Stdout(io()),
            BridgeFault::Wait(io()),
            BridgeFault::NoExit,
            exit(ExitStage::BeforeMcpReady),
            exit(ExitStage::AfterResult),
            exit(ExitStage::BeforeTerminal),
            BridgeFault::ProposalExecuted,
            BridgeFault::PrivatePathCall,
            ThinError::vendor_down(VENDOR, "spawn claude.exe not found").into(),
            ThinError::vendor_drift(VENDOR, "not ours").into(),
        ];
        for (expected, list) in [(true, faults), (false, not_faults)] {
            for fault in list {
                assert_eq!(fault.is_bridge_fault(), expected, "{fault:?}");
                let thin = fault.to_thin(Some("claude"));
                assert_eq!(is_bridge_fault(&thin), expected, "{fault:?} -> {thin:?}");
            }
        }
    }

    #[test]
    fn fault_codes_and_messages_are_the_old_text() {
        assert_eq!(BridgeFault::CliUnresolved.code(), "vendor_down");
        assert_eq!(
            BridgeFault::CliUnresolved.to_thin(Some("claude")).message,
            "claude: claude.exe not found on PATH (the .cmd shim is not accepted)"
        );
        let mcp = BridgeFault::McpNotReady {
            waited: Duration::from_millis(250),
            thickapi: PathBuf::from("thickapi.exe"),
            stderr: String::new(),
        }
        .with_stderr("; Claude Code stderr: boom".into());
        assert_eq!(
            mcp.to_string(),
            "proposal MCP server did not become ready within 250ms; Claude Code was told to launch thickapi.exe mcp-proposal; Claude Code stderr: boom"
        );
        assert_eq!(BridgeFault::ProposalExecuted.code(), "server_error");
        assert_eq!(
            BridgeFault::ProposalExecuted
                .to_thin(Some("claude"))
                .vendor(),
            Some(VENDOR)
        );
        assert_eq!(
            BridgeFault::Exit {
                stage: ExitStage::BeforeTerminal,
                status: None,
                stderr: String::new(),
            }
            .to_thin(None)
            .message,
            "Claude Code exited before a terminal event"
        );
    }

    fn todo_tool() -> serde_json::Value {
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

    fn bridge_off_claude(tmp: &tempfile::TempDir, dialect: &str) -> ClaudeBridge {
        ClaudeBridge::new(BridgeSettings {
            tmp_root: tmp.path().to_path_buf(),
            claude_path: Some(PathBuf::from("Z:/definitely/missing/claude.exe")),
            thickapi_exe: None,
            total_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            mcp_ready_timeout: Duration::from_millis(100),
            trace: false,
            bridge_max_concurrent: 1,
            handoff_drain: BridgeSettings::default_handoff_drain(),
            queue_timeout: Duration::from_secs(300),
            extra_tools: vec![],
            nonempty_inventories: vec![],
            dialect: dialect.to_string(),
            catalogue_ids: vec![],
        })
    }

    #[tokio::test]
    async fn a_pending_round2_is_answered_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge_off_claude(&tmp, crate::thin::dialect::MODE_CLAUDE_CODE);
        let calls = crate::thin::dialect::todo_round1_calls(
            "toolu_sc",
            &json!({"todos": [
                {"content": "Read SPEC", "status": "completed", "activeForm": "reading SPEC"},
                {"content": "Write the row", "status": "pending", "activeForm": "writing the row"}
            ]}),
        )
        .unwrap();
        let body = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "track it"},
                {"role": "assistant", "content": "On it.", "tool_calls": [
                    {"id": calls[0].id, "type": "function", "function": {"name": "todo", "arguments": calls[0].arguments.to_string()}},
                    {"id": calls[1].id, "type": "function", "function": {"name": "todo", "arguments": calls[1].arguments.to_string()}},
                    {"id": "call_read1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"SPEC.md\"}"}}
                ]},
                {"role": "tool", "tool_call_id": calls[0].id, "content": "[pending] #3 Old\n[completed] #4 Done"},
                {"role": "tool", "tool_call_id": calls[1].id, "content": "Created #10: Read SPEC (pending)\nCreated #11: Write the row (pending)"},
                {"role": "tool", "tool_call_id": "call_read1", "content": "spec"}
            ],
            "tools": [todo_tool()]
        });
        let req = ThinRequest {
            request: serde_json::from_value(body).unwrap(),
            entry: crate::config::models::ModelEntry::bridge_probe("claude-sonnet-4-6"),
            effort: None,
            request_id: "round2".into(),
            path: "bridge",
            fallback: None,
            cursor_cwd: None,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        bridge.run_bridge(&req, &tx).await.expect("short-circuit");
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(matches!(
            &events[0],
            ThinEvent::Started { path: "bridge", .. }
        ));
        let calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThinEvent::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 3, "{events:?}");
        assert!(calls.iter().all(|(_, name, _)| name == "todo"));
        let args: Vec<serde_json::Value> = calls
            .iter()
            .map(|(_, _, arguments)| serde_json::from_str(arguments).unwrap())
            .collect();
        assert_eq!(args[0], json!({"action": "delete", "id": 3}));
        assert_eq!(args[1], json!({"action": "delete", "id": 4}));
        assert_eq!(
            args[2],
            json!({"action": "update", "id": 10, "status": "completed"})
        );
        assert!(matches!(
            events.last(),
            Some(ThinEvent::Finish(FinishReason::ToolCalls))
        ));
        let usage = events.iter().find_map(|e| match e {
            ThinEvent::Usage(u) => Some(u),
            _ => None,
        });
        assert_eq!(usage.unwrap().total_tokens, 0);
        // No spawn: the missing claude.exe was never consulted, and no private dir exists.
        assert!(!tmp.path().join("round2").exists());

        let again = bridge.todo_round2(&req).unwrap();
        let first = bridge.todo_round2(&req).unwrap();
        assert_eq!(
            again.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>(),
            first.iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn missing_claude_is_vendor_down_before_started() {
        let bridge = ClaudeBridge::new(BridgeSettings {
            tmp_root: tempfile::tempdir().unwrap().keep(),
            claude_path: Some(PathBuf::from("Z:/definitely/missing/claude.exe")),
            thickapi_exe: None,
            total_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            mcp_ready_timeout: Duration::from_millis(100),
            trace: false,
            bridge_max_concurrent: 2,
            handoff_drain: BridgeSettings::default_handoff_drain(),
            queue_timeout: Duration::from_secs(300),
            extra_tools: vec![],
            nonempty_inventories: vec![],
            dialect: crate::thin::dialect::MODE_OFF.to_string(),
            catalogue_ids: vec![],
        });
        let request: crate::thin::openai::ChatRequest = serde_json::from_value(
            json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .unwrap();
        let req = ThinRequest {
            request,
            entry: crate::config::models::ModelEntry::bridge_probe("claude-haiku-4-5"),
            effort: None,
            request_id: "t".into(),
            path: "bridge",
            fallback: None,
            cursor_cwd: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        assert!(!bridge.state().is_drifted());
        let err = bridge.run_bridge(&req, &tx).await.unwrap_err();
        assert_eq!(err.status, 503);
        assert_eq!(err.code, "vendor_down");
        assert!(is_bridge_fault(&err));
        let drift = bridge.state().drift().expect("pre-Started fault → drift");
        assert!(
            drift.reason.starts_with("vendor_down: "),
            "{}",
            drift.reason
        );
        assert!(drift.reason.contains("claude.exe not found"));
        assert_eq!(bridge.state().snapshot()["drift"]["reason"], drift.reason);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn mcp_ready_timeout_leaves_tmp_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let stub = PathBuf::from(r"C:\Windows\System32\choice.exe");
        assert!(stub.is_file(), "choice.exe hang stub");
        let bridge = ClaudeBridge::new(BridgeSettings {
            tmp_root: tmp.path().to_path_buf(),
            claude_path: Some(stub.clone()),
            thickapi_exe: Some(stub),
            total_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            mcp_ready_timeout: Duration::from_millis(250),
            trace: false,
            bridge_max_concurrent: 2,
            handoff_drain: BridgeSettings::default_handoff_drain(),
            queue_timeout: Duration::from_secs(300),
            extra_tools: vec![],
            nonempty_inventories: vec![],
            dialect: crate::thin::dialect::MODE_OFF.to_string(),
            catalogue_ids: vec![],
        });
        let request: crate::thin::openai::ChatRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {"name": "read", "parameters": {"type": "object", "properties": {}}}
            }]
        }))
        .unwrap();
        let req = ThinRequest {
            request,
            entry: crate::config::models::ModelEntry::bridge_probe("claude-haiku-4-5"),
            effort: None,
            request_id: "hang".into(),
            path: "bridge",
            fallback: None,
            cursor_cwd: None,
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let err = bridge.run_bridge(&req, &tx).await.unwrap_err();
        assert_eq!(err.status, 503, "{err}");
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            leftover.is_empty(),
            "private dir survived MCP-ready timeout: {:?}",
            leftover.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn torn_record_after_tool_handoff_keeps_tool_calls() {
        // R3: the handoff kill cut the CLI's last `assistant` record in half.
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/thin/claude-bridge");
        let text = std::fs::read_to_string(root.join("two-tool-turn.jsonl")).expect("fixture");
        let mut frames: Vec<Vec<u8>> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| format!("{l}\n").into_bytes())
            .collect();
        frames.push(br#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{"path":"C:\\big"#.to_vec());
        let events = replay(&frames, &["read", "bash"]);
        assert!(
            !events.iter().any(|e| matches!(e, ThinEvent::Error(_))),
            "torn line after handoff must not be drift: {events:?}"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, ThinEvent::ToolCall { .. })));
        assert!(matches!(
            events.last(),
            Some(ThinEvent::Finish(FinishReason::ToolCalls))
        ));
    }

    #[test]
    fn handoff_accepts_clean_exit_when_tool_use_stop() {
        assert!(handoff_exit_ok(&exit_status(0), false, true));
        assert!(!handoff_exit_ok(&exit_status(0), false, false));
        assert!(handoff_exit_ok(&exit_status(spawn::KNOWN_EXIT), true, true));
    }
}
