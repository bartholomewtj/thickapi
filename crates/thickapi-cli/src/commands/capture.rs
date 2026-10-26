//! `thickapi capture <vendor>` (SPEC §19.11): run one text turn and one tool turn against the
//! live vendor through the same adapter `serve` uses, with a wire tap, and write the redacted
//! exchange plus the events it produced to `fixtures/<vendor>/<version>/<scenario>/`.
//!
//! The adapter mapper tests replay the newest set (`crates/thickapi-core/tests/wire_fixtures.rs`),
//! so the fix loop for a vendor wire change is: capture, watch tests fail, fix, tests pass.
//! `--check` captures to a temp directory outside `fixtures/`, runs the leak check and the
//! replay there, prints a diff against the newest committed set, and never promotes.
//! Recording, redaction and set writing live in [`crate::capture`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use thickapi_core::config::models::{ModelEntry, Transport};
use thickapi_core::thin::capture::{summarize, CaptureSwitches};
use thickapi_core::thin::fixture_set::META_FILE;
use thickapi_core::thin::oauth::cursor::ReadMissShape;
use thickapi_core::thin::openai::ChatRequest;
use thickapi_core::thin::runtime::ThinRuntime;
use thickapi_core::thin::{ThinError, ThinEvent, ThinRequest};
use thickapi_core::types::BackendId;

use crate::capture::{self, set, Recorder, CREATE_CWD};

pub const VENDORS: &[&str] = &["claude", "claude-bridge", "grok", "agy", "cursor", "codex"];
const SCENARIOS: &[&str] = &["text", "tool"];
/// Opt-in seat-shaped Cursor capture (`--scenario sdlc`). Not part of `--all`.
const SDLC_SCENARIO: &str = "sdlc";
/// Opt-in Cursor scenarios for the built-in and stream-edge faults (F1, #181). Like `sdlc`:
/// Cursor-only, never in `--all`, merged into an existing version dir (as
/// `<scenario>-<row>/`), and an `Error` does not fail the set — the point is to record what
/// happens.
///
/// - `create` / `create-fnf`: the `sdlc` request in a fictional cwd, where every built-in
///   read is of a missing file and is answered on the duplex (`create`: the capture-only
///   `ReadResult.rejected`; `create-fnf`: `ReadResult.file_not_found`, `serve`'s own answer
///   since G1) and the Run continues.
/// - `quiet`: the second request of the 29 Sep named-tool prompt, with
///   `idle_after_text_with_tools_seconds = 0` so the Run is recorded to its real end.
/// - `mcp-repeat`: the fifth request of that prompt, tried up to [`MCP_REPEAT_TRIES`] times
///   for a Run that repeats `mcp_state_exec_args` or sends a `1.27` / `1.3` frame.
const F1_SCENARIOS: &[&str] = &["create", "create-fnf", "quiet", "mcp-repeat"];
const MCP_REPEAT_SCENARIO: &str = "mcp-repeat";
const QUIET_SCENARIO: &str = "quiet";
/// Runs `mcp-repeat` tries for the burst before keeping the last one.
const MCP_REPEAT_TRIES: usize = 5;
/// Wall clock on one F1 Run: `quiet` turns the after-text budget off, so nothing else
/// bounds a Run whose answer never ends on the wire.
const F1_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(600);

/// Fresh Runs an F1 scenario gets when one dies in the handshake (P4).
const HANDSHAKE_RETRIES: usize = 3;

/// A Run that ended in an error before any output: the P4 handshake death.
fn handshake_only(summary: &[Value]) -> bool {
    let kinds: Vec<&str> = summary.iter().filter_map(|v| v["type"].as_str()).collect();
    matches!(kinds.as_slice(), ["started", "error"] | ["error"])
}

/// `sdlc` and the F1 scenarios: Cursor-only, opt-in, merged into an existing set.
fn is_opt_in(scenario: &str) -> bool {
    scenario == SDLC_SCENARIO || F1_SCENARIOS.contains(&scenario)
}

/// The capture-only adapter switches a scenario runs with (every one off but `create*`).
fn capture_switches(scenario: &str) -> CaptureSwitches {
    CaptureSwitches {
        cursor_read_miss_shape: match scenario {
            "create" => Some(ReadMissShape::Rejected),
            "create-fnf" => Some(ReadMissShape::FileNotFound),
            _ => None,
        },
    }
}

/// The directory a scenario lands in: F1 sets carry the row (`create-fable`).
fn scenario_dir_name(scenario: &str, row_id: &str) -> String {
    if F1_SCENARIOS.contains(&scenario) {
        let row = row_id.rsplit('/').next().unwrap_or(row_id);
        format!("{scenario}-{row}")
    } else {
        scenario.to_string()
    }
}

/// What an F1 Run's Connect frames show, read off [`dump_connect_exec`]'s field tree.
#[derive(Debug, Default, PartialEq, Eq)]
struct F1Shape {
    /// `f2.f36 mcp_state_exec_args` asks.
    mcp_state_asks: usize,
    /// `f1.f27` (`tool_requests_listed`) or `f1.f3` (`tool_call_completed`) frames.
    tool_progress_frames: usize,
    /// Of those, the ones before any answer text or reasoning: what feeds the nested drift
    /// counter (P5 tripped it on 29 Sep).
    tool_progress_before_output: usize,
    /// Built-in exec fields seen (`builtin=Some(n)`), in order.
    builtins: Vec<u64>,
}

impl F1Shape {
    fn from_dump(dump: &str) -> Self {
        let mut shape = Self::default();
        let mut output = false;
        for line in dump.lines() {
            if line.starts_with("  f2.f36 ") {
                shape.mcp_state_asks += 1;
            }
            if line.starts_with("  f1.f1 ") || line.starts_with("  f1.f4 ") {
                output = true;
            }
            if line.starts_with("  f1.f27 ") || line.starts_with("  f1.f3 ") {
                shape.tool_progress_frames += 1;
                if !output {
                    shape.tool_progress_before_output += 1;
                }
            }
            if let Some(rest) = line.split(" builtin=Some(").nth(1) {
                if let Some(n) = rest.split(')').next().and_then(|n| n.parse().ok()) {
                    shape.builtins.push(n);
                }
            }
        }
        shape
    }

    /// The P5 burst `mcp-repeat` looks for: a repeated state ask, or a `1.27` / `1.3` frame
    /// before any output. On 2026.09.28 both frames follow every tool step, so one after
    /// output (the drift counter no longer runs) is not the burst.
    fn shows_mcp_repeat(&self) -> bool {
        self.mcp_state_asks > 1 || self.tool_progress_before_output > 0
    }
}

pub struct CaptureArgs {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub out: PathBuf,
    pub scenario: Option<String>,
    pub all: bool,
    /// Leave the temp fixture tree on disk when a scenario fails (field-7 diagnosis).
    pub keep_failed: bool,
    /// Capture to a temp dir, leak-check and replay it, diff against the newest committed
    /// set; never write under `out`.
    pub check: bool,
}

/// Which catalogue backend a capture vendor maps to, and the transport forced on the row.
fn backend_for(vendor: &str) -> Result<(BackendId, Transport)> {
    Ok(match vendor {
        "claude" => (BackendId::ClaudeCode, Transport::OauthDirect),
        "claude-bridge" => (BackendId::ClaudeCode, Transport::CliBridge),
        "grok" => (BackendId::Grok, Transport::OauthDirect),
        "agy" => (BackendId::Agy, Transport::OauthDirect),
        "cursor" => (BackendId::Cursor, Transport::OauthDirect),
        "codex" => (BackendId::Codex, Transport::OauthDirect),
        other => bail!("unknown vendor {other:?}; one of {}", VENDORS.join(", ")),
    })
}

/// The `/ready` vendor key whose `cli_version` names the fixture set.
fn snapshot_key(vendor: &str) -> &str {
    match vendor {
        "claude-bridge" => "claude",
        v => v,
    }
}

fn pick_row(models: &[ModelEntry], backend: BackendId, wanted: Option<&str>) -> Result<ModelEntry> {
    if let Some(id) = wanted {
        let full = if id.starts_with("subs/") {
            id.to_string()
        } else {
            format!("subs/{id}")
        };
        return models
            .iter()
            .find(|m| m.id == full || m.id == id)
            .cloned()
            .with_context(|| format!("model {id:?} is not in models.yaml"));
    }
    models
        .iter()
        .find(|m| m.backend == backend && m.enabled && m.transport != Transport::CliTurn)
        .or_else(|| models.iter().find(|m| m.backend == backend && m.enabled))
        .cloned()
        .with_context(|| format!("no enabled models.yaml row for backend {backend:?}"))
}

fn pi_tool_catalog() -> Result<Value> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/thin/pi-tool-catalog.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read Pi tool catalogue {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn sdlc_prompt() -> &'static str {
    "You are the builder seat of an SDLC pipeline. One agent, one purpose: implement. \
     You do not plan, review, or write the docs.\n\n\
     ## Request\n\
     Add src/smoke-marker.txt containing the single line ok. Do not change other product files. \
     This is a seat smoke; keep the change tiny.\n\n\
     ## Worktree\n\
     The workspace is the current working directory. Implement here.\n\n\
     ## Allowed writes\n\
     src/\n\n\
     Stay inside this list.\n\n\
     ## What to do\n\
     Inspect the workspace with the read and bash tools before editing. Do not guess file contents. \
     Make the smallest change that satisfies the request. Use write or edit to create \
     src/smoke-marker.txt. Git is not yours: do not commit, branch, or push.\n\n\
     ## Output\n\
     Write envelope.json as a JSON object only (no markdown fence):\n\
     {\"status\":\"success\",\"summary\":\"Added smoke marker\",\"changed_files\":[\"src/smoke-marker.txt\"]}"
}

/// The Claude preset's Pi schemas: the smoke catalogue, plus `todo` in the shape the
/// TodoWrite row requires and the `subagent` / `askuserquestion` tools. Declaring these is
/// what turns the dialect rows on for a `dialect` or `todo` capture (SPEC §19.7).
fn claude_preset_catalog() -> Result<Value> {
    let mut tools = smoke_tool_catalog()?;
    let list = tools
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("smoke catalogue is not an array"))?;
    // The smoke fixture's `todo` is the older list/add/toggle shape, which leaves the
    // TodoWrite row inactive. Replace it with the current Pi schema.
    list.retain(|t| {
        t.get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            != Some("todo")
    });
    list.push(todo_tool());
    list.push(subagent_tool());
    list.push(ask_tool());
    Ok(tools)
}

fn todo_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "todo",
            "description": "Replace or inspect the session todo list.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {"type": "string"},
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
            }
        }
    })
}

fn subagent_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "subagent",
            "description": "Launch a Pi subagent.",
            "parameters": {
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
            }
        }
    })
}

fn ask_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "askuserquestion",
            "description": "Ask the user one or more questions.",
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

/// Pi's built-in tools plus the `todo` extension, as the 29 Sep retest enabled them
/// (`--tools ls,find,grep,read,write,edit,bash,todo`).
fn smoke_tool_catalog() -> Result<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/thin/pi-tool-catalog-smoke.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read Pi smoke tool catalogue {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Pi's default system prompt for those tools (pi-coding-agent `buildSystemPrompt`, docs
/// section and cwd dropped).
fn smoke_system_prompt() -> &'static str {
    "You are an expert coding assistant operating inside pi, a coding agent harness. You help \
     users by reading files, executing commands, editing code, and writing new files.\n\n\
     <tools>\n\
     - ls: List directory contents\n\
     - find: Find files by glob pattern (respects .gitignore)\n\
     - grep: Search file contents for patterns (respects .gitignore)\n\
     - read: Read file contents\n\
     - write: Create or overwrite files\n\
     - edit: Make precise file edits with exact text replacement, including multiple disjoint \
     edits in one call\n\
     - bash: Execute bash commands (ls, grep, find, etc.)\n\
     - todo: Manage a todo list\n\n\
     In addition to the tools above, you may have access to other custom tools depending on \
     the project.\n\
     </tools>\n\n\
     <rules>\n\
     - Use read to examine files instead of cat or sed.\n\
     - Use write only for new files or complete rewrites.\n\
     - Use edit for precise changes (edits[].oldText must match exactly)\n\
     - Be concise in your responses\n\
     - Show file paths clearly when working with files\n\
     </rules>"
}

/// The 29 Sep retest's named-tool prompt (`~/.thickapi/toolsmoke/2026-09-29/run.py`).
fn smoke_prompt() -> &'static str {
    "This is a tool-coverage test. Work in the current directory. Do each step in order, one \
     tool call per step, using exactly the tool named. Do not use bash for anything except \
     step 7.\n\n\
     1. ls: list the directory `src`.\n\
     2. find: find every file matching `*.txt` under `src`.\n\
     3. grep: search `src` for the pattern `MARKER-\\d+`.\n\
     4. read: read `src/input.txt`.\n\
     5. write: create the NEW file `src/out.txt` (it does not exist yet) whose content is \
     exactly the marker you found in input.txt, then a newline, then the word `alpha`.\n\
     6. edit: in `src/out.txt` replace `alpha` with `omega`.\n\
     7. bash: run `wc -l src/out.txt`.\n\
     8. todo: add one task titled `tool smoke done`.\n\n\
     When all eight steps are done, reply with one line: DONE <the marker>. Nothing else."
}

/// One finished tool step of the retest transcript: the assistant's call (with any text it
/// streamed first) and Pi's result.
fn smoke_step(n: usize, text: &str, name: &str, args: Value, result: &str) -> [Value; 2] {
    let id = format!("call_f1smokestep{n:010}");
    [
        json!({
            "role": "assistant",
            "content": text,
            "tool_calls": [{
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            }]
        }),
        json!({"role": "tool", "tool_call_id": id, "content": result}),
    ]
}

/// The 29 Sep retest transcript up to request `upto` (2 = after `ls`, 5 = after `read`), as
/// the Cursor rows answered it (`out2/subs-cursor-{grok-4.7,fable}/events.jsonl`).
fn smoke_messages(upto: usize) -> Vec<Value> {
    let mut messages = vec![
        json!({"role": "system", "content": smoke_system_prompt()}),
        json!({"role": "user", "content": smoke_prompt()}),
    ];
    let steps = [
        smoke_step(
            1,
            if upto == 2 {
                "I'll run the eight-step tool smoke test in order, starting by checking the \
                 thickapi tool schemas so each step uses the named tool correctly.Listing \
                 `src` first."
            } else {
                "Running the eight-step tool smoke in order, starting with the schemas for the \
                 `thickapi` tools."
            },
            "ls",
            json!({"path": "src"}),
            "input.txt\nmain.py\nnested/",
        ),
        smoke_step(
            2,
            "",
            "find",
            json!({"path": "src", "pattern": "*.txt"}),
            "input.txt\nnested/notes.txt",
        ),
        smoke_step(
            3,
            "",
            "grep",
            json!({"path": "src", "pattern": "MARKER-\\d+"}),
            "input.txt:2: MARKER-4172",
        ),
        smoke_step(
            4,
            "",
            "read",
            json!({"path": "src/input.txt"}),
            "header\nMARKER-4172\nfooter\n",
        ),
    ];
    for step in steps.into_iter().take(upto.saturating_sub(1)) {
        messages.extend(step);
    }
    messages
}

/// The draftr builder brief of the 29 Sep smoke (`draftr smoke --roles builder`), with its
/// worktree path as `{worktree}`.
fn draftr_builder_brief(worktree: &str) -> Result<String> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/thin/draftr-builder-brief.md");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read draftr builder brief {}", path.display()))?;
    Ok(text.replace("{worktree}", worktree))
}

/// `create`: the 29 Sep draftr Fable builder cell up to its create (#175), in `cwd`
/// ([`CREATE_CWD`]): the headless prompt, the brief read, a bash check that finds no
/// `plan.md` and no `src/`. Asked the bare `sdlc` request instead, Fable's first move is an MCP `bash`
/// inspection (measured 2026-09-29), which ends a one-request capture before any write.
fn create_messages(cwd: &str) -> Result<Vec<Value>> {
    let brief_path = format!("{cwd}\\brief.md");
    let mut messages = vec![
        json!({"role": "system", "content": builder_system_prompt()}),
        json!({"role": "user", "content": format!(
            "Read {brief_path} and do what it says. When finished, reply with ONLY the JSON \
             object the brief's Report section shows, then stop."
        )}),
    ];
    messages.extend(smoke_step(
        1,
        "I'll read the brief first.",
        "read",
        json!({"path": brief_path}),
        &draftr_builder_brief(cwd)?,
    ));
    messages.extend(smoke_step(
        2,
        "I'll check for the plan file and existing marker, then create `src/smoke-marker.txt`.",
        "bash",
        json!({"command": "ls -la plan.md src src/smoke-marker.txt 2>&1; echo \"---\"; git status --short"}),
        "ls: cannot access 'plan.md': No such file or directory\n\
         ls: cannot access 'src': No such file or directory\n\
         ls: cannot access 'src/smoke-marker.txt': No such file or directory\n---\n",
    ));
    Ok(messages)
}

/// Pi's default system prompt for a draftr builder seat's four tools.
fn builder_system_prompt() -> &'static str {
    "You are an expert coding assistant operating inside pi, a coding agent harness. You help \
     users by reading files, executing commands, editing code, and writing new files.\n\n\
     <tools>\n\
     - read: Read file contents\n\
     - bash: Execute bash commands (ls, grep, find, etc.)\n\
     - edit: Make precise file edits with exact text replacement, including multiple disjoint \
     edits in one call\n\
     - write: Create or overwrite files\n\n\
     In addition to the tools above, you may have access to other custom tools depending on \
     the project.\n\
     </tools>\n\n\
     <rules>\n\
     - Use bash for file operations like ls, rg, find\n\
     - Use read to examine files instead of cat or sed.\n\
     - Use edit for precise changes (edits[].oldText must match exactly)\n\
     - Use write only for new files or complete rewrites.\n\
     - Be concise in your responses\n\
     - Show file paths clearly when working with files\n\
     </rules>"
}

/// 16x16 solid blue (`#2266cc`), committed at `fixtures/images/blue-16.png`.
/// Both image scenarios ask the colour so a dropped attachment shows up in the answer.
fn blue_square_data_url() -> String {
    const PNG: &[u8] = include_bytes!("../../../../fixtures/images/blue-16.png");
    let b64 = base64::engine::general_purpose::STANDARD.encode(PNG);
    format!("data:image/png;base64,{b64}")
}

fn image_request(model: &str, scenario: &str) -> Value {
    let url = blue_square_data_url();
    if scenario == "image" {
        json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test. Answer in one short sentence."},
                {"role": "user", "content": [
                    {"type": "text", "text": "what colour is this square?"},
                    {"type": "image_url", "image_url": {"url": url}}
                ]}
            ],
            "max_tokens": 64
        })
    } else {
        json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test. Answer in one short sentence."},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_read_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"fixtures/images/blue-16.png\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_read_1", "name": "read", "content": [
                    {"type": "text", "text": "Read image file fixtures/images/blue-16.png"},
                    {"type": "image_url", "image_url": {"url": url}}
                ]},
                {"role": "user", "content": "what colour was the image you read?"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file. Image files come back as an image attachment.",
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": ["path"]
                    }
                }
            }],
            "max_tokens": 64
        })
    }
}

fn chat_request(model: &str, scenario: &str) -> Result<ChatRequest> {
    let body = match scenario {
        "text" => json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test."},
                {"role": "user", "content": "Reply with exactly: CAPTURE OK"}
            ],
            "max_tokens": 64
        }),
        "tool" => json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test. When a tool fits the task, call it instead of answering in prose."},
                {"role": "user", "content": "Read the file notes.txt with the read_file tool and tell me its first line. Do not guess its contents."}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read a UTF-8 text file and return its contents.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to the file, relative to the workspace."}
                        },
                        "required": ["path"]
                    }
                }
            }],
            "max_tokens": 512
        }),
        "quiet" | "mcp-repeat" => {
            let upto = if scenario == QUIET_SCENARIO { 2 } else { 5 };
            json!({
                "model": model,
                "stream": true,
                "messages": smoke_messages(upto),
                "tools": smoke_tool_catalog()?,
                "max_tokens": 4096
            })
        }
        "create" | "create-fnf" => {
            json!({
                "model": model,
                "stream": true,
                "messages": create_messages(CREATE_CWD)?,
                "tools": pi_tool_catalog()?,
                "max_tokens": 2048
            })
        }
        "sdlc" => {
            let tools = pi_tool_catalog()?;
            json!({
                "model": model,
                "stream": true,
                "messages": [
                    {"role": "system", "content": "You are the builder seat of an SDLC pipeline. Implement. Do not plan, review, or write docs."},
                    {"role": "user", "content": sdlc_prompt()}
                ],
                "tools": tools,
                "max_tokens": 2048
            })
        }
        "image" | "image-tool" => image_request(model, scenario),
        "dialect" => json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test. When a tool fits the task, call it instead of answering in prose."},
                {"role": "user", "content": "In notes.txt, replace the word alpha with omega. Use one edit. Do not read the file first and do not explain."}
            ],
            "tools": claude_preset_catalog()?,
            "max_tokens": 512
        }),
        "todo" => json!({
            "model": model,
            "stream": true,
            "messages": [
                {"role": "system", "content": "You are a terse assistant under test. When a tool fits the task, call it instead of answering in prose."},
                {"role": "user", "content": "Replace your todo list with three items: read the notes, edit the file, confirm the change. Mark the first completed and leave the other two pending. One update of the whole list. Do not explain."}
            ],
            "tools": claude_preset_catalog()?,
            "max_tokens": 512
        }),
        other => bail!(
            "unknown scenario {other:?}; one of {}, or (Cursor, opt-in) {SDLC_SCENARIO}, {}",
            SCENARIOS.join(", "),
            F1_SCENARIOS.join(", ")
        ),
    };
    Ok(serde_json::from_value(body)?)
}

/// True when this scenario's event summary is not a valid capture (SPEC §19.11).
pub fn capture_scenario_failure(scenario: &str, summary: &[Value]) -> Option<String> {
    if is_opt_in(scenario) {
        return None;
    }
    if summary.iter().any(|v| v["type"] == "error") {
        return Some(format!("{scenario} ended in Error"));
    }
    if scenario == "tool"
        && !summary
            .iter()
            .any(|v| v["type"] == "tool_call" && v["name"] == "read_file")
    {
        return Some(format!("{scenario} lacks a read_file call"));
    }
    // Dialect on: the client sees the Pi name. `edit` is what an Edit prompt must produce;
    // `todo` is what a TodoWrite prompt must produce (the vendor name stays on the access line).
    if scenario == "dialect"
        && !summary
            .iter()
            .any(|v| v["type"] == "tool_call" && v["name"] == "edit")
    {
        return Some(format!("{scenario} lacks an edit call"));
    }
    if scenario == "todo"
        && !summary
            .iter()
            .any(|v| v["type"] == "tool_call" && v["name"] == "todo")
    {
        return Some(format!("{scenario} lacks a todo call"));
    }
    None
}

fn sdlc_shape_captured(dump: &str) -> bool {
    dump.contains("builtin=Some(7)") || dump.contains("builtin=Some(5)")
}

/// Append image scenario names to an existing set's `meta.json` without rewriting
/// `captured_at` (that stamp picks `newest_set`).
fn merge_image_scenario_meta(set_dir: &Path, written: &[String]) -> Result<()> {
    let path = set_dir.join(META_FILE);
    let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("read {}", path.display()))?;
    let list = meta
        .get_mut("scenarios")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("{} scenarios is not an array", path.display()))?;
    for name in written {
        if !list.iter().any(|v| v.as_str() == Some(name)) {
            list.push(Value::String(name.clone()));
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

pub async fn run(config_dir: &Path, args: CaptureArgs) -> Result<()> {
    if args.all {
        let mut failed = false;
        for vendor in VENDORS {
            let one = CaptureArgs {
                vendor: Some((*vendor).to_string()),
                model: args.model.clone(),
                out: args.out.clone(),
                scenario: args.scenario.clone(),
                all: false,
                keep_failed: args.keep_failed,
                check: args.check,
            };
            if let Err(err) = run_vendor(config_dir, one).await {
                eprintln!("[fail] capture {vendor}: {err:#}");
                failed = true;
            }
        }
        if failed {
            bail!("capture --all: one or more vendors failed");
        }
        return Ok(());
    }
    let vendor = args
        .vendor
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("vendor required (or pass --all)"))?;
    run_vendor(
        config_dir,
        CaptureArgs {
            vendor: Some(vendor.to_string()),
            model: args.model,
            out: args.out,
            scenario: args.scenario,
            all: false,
            keep_failed: args.keep_failed,
            check: args.check,
        },
    )
    .await
}

async fn run_vendor(config_dir: &Path, args: CaptureArgs) -> Result<()> {
    let vendor = args
        .vendor
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("vendor required (or pass --all)"))?;
    let (backend, transport) = backend_for(vendor)?;
    let mut config = thickapi_core::load_config(config_dir)?;
    let mut entry = pick_row(&config.models.models, backend, args.model.as_deref())?;
    entry.transport = transport;
    entry.enabled = true;

    let scenarios: Vec<&str> = match args.scenario.as_deref() {
        Some(s) => vec![s],
        None => SCENARIOS.to_vec(),
    };
    if let Some(s) = scenarios.iter().find(|s| is_opt_in(s)) {
        if vendor != "cursor" {
            bail!("--scenario {s} is Cursor-only");
        }
    }
    let opt_in_only = matches!(scenarios.as_slice(), [s] if is_opt_in(s));
    // Image scenarios are additive on the bridge set: a default capture still writes
    // {text,tool}, and one image scenario must not promote_capture_set over them.
    let image_only = matches!(scenarios.as_slice(), ["image" | "image-tool"]);
    // `dialect` and `todo` are additive on the bridge set, the same way an image scenario is:
    // a default capture still writes {text,tool}, and one of these must not replace them.
    let additive = image_only || matches!(scenarios.as_slice(), ["dialect" | "todo"]);
    // Capture-only switches ride on the runtime the way the tap does; `serve` never sets them.
    let switches = match scenarios.as_slice() {
        [s] => capture_switches(s),
        _ => CaptureSwitches::default(),
    };
    if scenarios.iter().any(|s| s.starts_with("create")) {
        config.router.thin.vendors.cursor.cwd = Some(CREATE_CWD.to_string());
    }
    if scenarios.contains(&QUIET_SCENARIO) {
        // Record the Run to its real end: no after-text quiet budget (F1 `quiet`).
        config
            .router
            .thin
            .vendors
            .cursor
            .budgets
            .idle_after_text_with_tools_seconds = 0;
    }

    let cwd = std::env::current_dir()?;
    let ctx = capture::RedactContext::for_checkout(&cwd);
    let recorder = Arc::new(Recorder::with_context(ctx));
    let runtime = ThinRuntime::with_capture(&config, Some(recorder.clone()), switches);
    runtime.wait_cli_versions();
    let adapter = runtime
        .adapter_for(&entry)
        .with_context(|| format!("no thin adapter serves {}", entry.id))?;

    let snapshot = runtime.vendor_snapshot();
    let vendor_view = snapshot.get(snapshot_key(vendor)).unwrap_or(&Value::Null);
    let version = vendor_view
        .get("cli_version")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown")
        .to_string();
    if vendor == "claude" && vendor_view.get("path").and_then(Value::as_str) != Some("oauth") {
        bail!(
            "claude oauth path is not live ({}); capture `claude-bridge` instead",
            vendor_view
                .get("path_reason")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
    }
    let set_dir = args.out.join(vendor).join(&version);
    // `--check` keeps its whole tree under the OS temp dir: nothing under `out` changes.
    let check_root = args.check.then(|| {
        std::env::temp_dir().join(format!("thickapi-capture-check-{}", uuid::Uuid::new_v4()))
    });
    let tmp_dir = match &check_root {
        Some(root) => root.join(vendor).join(&version),
        None => args.out.join(vendor).join(format!("{version}.tmp")),
    };
    let cleanup = check_root.clone().unwrap_or_else(|| tmp_dir.clone());
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .with_context(|| format!("clear {}", tmp_dir.display()))?;
    }
    std::fs::create_dir_all(&tmp_dir)?;
    println!(
        "capturing {} as {} ({} {}) → {}",
        entry.id,
        vendor,
        snapshot_key(vendor),
        version,
        if args.check { &tmp_dir } else { &set_dir }.display()
    );

    let mut written = Vec::new();
    let mut failures = Vec::new();
    let mut promote_dir = String::new();
    for &scenario in &scenarios {
        let dir_name = scenario_dir_name(scenario, &entry.id);
        let f1 = F1_SCENARIOS.contains(&scenario);
        let tries = if scenario == MCP_REPEAT_SCENARIO {
            MCP_REPEAT_TRIES
        } else {
            1
        };
        let mut last = None;
        let mut attempt = 0;
        let mut handshake_retries = 0;
        while attempt < tries {
            attempt += 1;
            let request = chat_request(&entry.id, scenario)?;
            let req = ThinRequest {
                request,
                entry: entry.clone(),
                effort: Some(thickapi_core::Effort::Low),
                request_id: format!("capture-{}-{}", scenario, uuid::Uuid::new_v4()),
                path: "oauth",
                fallback: None,
                cursor_cwd: None,
            };
            let (tx, mut rx) = tokio::sync::mpsc::channel::<ThinEvent>(256);
            let run = async {
                if !f1 {
                    return adapter.run(&req, tx).await;
                }
                match tokio::time::timeout(F1_WALL_CLOCK, adapter.run(&req, tx)).await {
                    Ok(result) => result,
                    Err(_) => Err(ThinError::vendor_down(
                        vendor,
                        format!(
                            "capture wall clock: the Run had not ended after {}s",
                            F1_WALL_CLOCK.as_secs()
                        ),
                    )),
                }
            };
            let collect = async {
                let mut events = Vec::new();
                while let Some(ev) = rx.recv().await {
                    events.push(ev);
                }
                events
            };
            let (result, mut events) = tokio::join!(run, collect);
            if let Err(err) = result {
                eprintln!("  {dir_name}: {}", err.message);
                events.push(ThinEvent::Error(err));
            }
            let recording = recorder.take();
            // Redacted as the frames were, so the set passes the leak check and replays.
            let summary = recorder.redact_summary(&summarize(&events), vendor == "cursor");

            let scenario_dir = tmp_dir.join(&dir_name);
            if scenario_dir.exists() {
                std::fs::remove_dir_all(&scenario_dir)
                    .with_context(|| format!("clear {}", scenario_dir.display()))?;
            }
            recording
                .write(&scenario_dir)
                .with_context(|| format!("write {}", scenario_dir.display()))?;
            capture::write_events(&scenario_dir, &summary)?;
            let dump = if vendor == "cursor" {
                let chunks: Vec<Vec<u8>> = recording.frames.iter().map(|f| f.bytes()).collect();
                thickapi_core::thin::oauth::cursor::dump_connect_exec(&chunks)
            } else {
                String::new()
            };
            println!(
                "  {dir_name}: {} frame(s), {} event(s): {}",
                recording.frames.len(),
                events.len(),
                describe(&summary)
            );
            let shape = F1Shape::from_dump(&dump);
            if f1 {
                println!(
                    "  {dir_name}: mcp_state asks {}, 1.27/1.3 frames {} ({} before output), built-in execs {:?}",
                    shape.mcp_state_asks,
                    shape.tool_progress_frames,
                    shape.tool_progress_before_output,
                    shape.builtins
                );
            }
            let died_in_handshake = f1 && handshake_only(&summary);
            last = Some((summary, dump));
            if died_in_handshake && handshake_retries < HANDSHAKE_RETRIES {
                // P4 (`internal: No exec result` before any output) is not what these
                // scenarios measure; a fresh Run does not use up a try.
                handshake_retries += 1;
                attempt -= 1;
                println!("  {dir_name}: the Run died in the handshake; a fresh Run ({handshake_retries}/{HANDSHAKE_RETRIES})");
                continue;
            }
            if scenario != MCP_REPEAT_SCENARIO || shape.shows_mcp_repeat() {
                break;
            }
            if attempt < tries {
                println!("  {dir_name}: try {attempt}/{tries} showed no mcp_state repeat or 1.27/1.3 frame; again");
            } else {
                println!(
                    "  {dir_name}: no Run in {tries} tries showed the burst; keeping the last"
                );
            }
        }
        let Some((summary, dump)) = last else {
            continue;
        };
        promote_dir = dir_name.clone();
        let sdlc_miss = scenario == SDLC_SCENARIO && !sdlc_shape_captured(&dump);
        if sdlc_miss {
            let why =
                "sdlc lacks ExecServerMessage field 7 or 5 (not a seat-shaped builtin capture)"
                    .to_string();
            eprintln!("[fail] {vendor}/{scenario}: {why}");
            failures.push(why);
        } else if let Some(why) = capture_scenario_failure(scenario, &summary) {
            eprintln!("[fail] {vendor}/{scenario}: {why}");
            failures.push(why);
        } else {
            written.push(dir_name.clone());
        }
    }

    if !failures.is_empty() {
        eprintln!("capture temp tree: {}", tmp_dir.display());
        if !args.keep_failed {
            let _ = std::fs::remove_dir_all(&cleanup);
        }
        bail!(
            "capture {} aborted (no {}): {}",
            vendor,
            META_FILE,
            failures.join("; ")
        );
    }

    let meta = set::meta(vendor, &version, &entry.id, entry.wire_model(), &written);
    std::fs::write(
        tmp_dir.join(META_FILE),
        serde_json::to_string_pretty(&meta)?,
    )?;

    let roots = capture::repo_roots(&cwd);
    if args.check {
        return report_check(vendor, &version, &tmp_dir, &cleanup, &args, &roots);
    }
    let promoted = if opt_in_only || additive {
        capture::promote_capture_scenario(&tmp_dir, &set_dir, &promote_dir, &roots)
    } else {
        capture::promote_capture_set(&tmp_dir, &set_dir, &roots)
    };
    match promoted {
        Ok(()) => {
            if additive {
                merge_image_scenario_meta(&set_dir, &written)?;
            }
            println!("wrote {}", set_dir.join(META_FILE).display());
            Ok(())
        }
        Err(err) => {
            for line in err.lines() {
                eprintln!("[fail] {line}");
            }
            bail!("{err}")
        }
    }
}

/// `--check`'s verdict: print the diff summary and any failure, then remove the temp tree
/// (kept on a failure with `--keep-failed`). Exit 1 only on a leak or a replay mismatch.
fn report_check(
    vendor: &str,
    version: &str,
    tmp_dir: &Path,
    cleanup: &Path,
    args: &CaptureArgs,
    roots: &[PathBuf],
) -> Result<()> {
    let report = capture::check_set(vendor, tmp_dir, &args.out, roots);
    for line in &report.diff {
        println!("  diff: {line}");
    }
    for line in &report.failures {
        eprintln!("[fail] {vendor}: {line}");
    }
    if report.failures.is_empty() || !args.keep_failed {
        let _ = std::fs::remove_dir_all(cleanup);
    } else {
        eprintln!("capture temp tree: {}", tmp_dir.display());
    }
    if !report.failures.is_empty() {
        bail!(
            "capture --check {vendor} {version}: {} problem(s); nothing promoted",
            report.failures.len()
        );
    }
    println!("check {vendor} {version}: ok (leak check and replay clean; nothing promoted)");
    Ok(())
}

fn describe(summary: &[Value]) -> String {
    summary
        .iter()
        .map(|v| match v["type"].as_str().unwrap_or("?") {
            "text" => format!("text {:?}", trim(v["text"].as_str().unwrap_or(""), 40)),
            "reasoning" => format!("reasoning[{}]", v["text"].as_str().map_or(0, str::len)),
            "tool_call" => format!("tool_call {}", v["name"]),
            "finish" => format!("finish {}", v["reason"]),
            "error" => format!("error {} {}", v["status"], v["code"]),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn vendors_map_to_backends_and_transports() {
        assert_eq!(
            backend_for("claude-bridge").unwrap(),
            (BackendId::ClaudeCode, Transport::CliBridge)
        );
        assert_eq!(
            backend_for("cursor").unwrap(),
            (BackendId::Cursor, Transport::OauthDirect)
        );
        assert!(backend_for("openai").is_err());
        assert_eq!(snapshot_key("claude-bridge"), "claude");
    }

    #[test]
    fn scenarios_build_valid_chat_requests() {
        let text = chat_request("subs/grok/grok-4.7", "text").unwrap();
        assert!(text.tools.is_empty());
        let tool = chat_request("subs/grok/grok-4.7", "tool").unwrap();
        assert_eq!(tool.tools[0].function.name, "read_file");
        let sdlc = chat_request("subs/cursor/auto", "sdlc").unwrap();
        assert_eq!(sdlc.tools.len(), 4);
        let names: Vec<&str> = sdlc
            .tools
            .iter()
            .map(|t| t.function.name.as_str())
            .collect();
        assert_eq!(names, ["read", "bash", "edit", "write"]);
        assert!(sdlc.messages.iter().any(|m| m
            .content
            .as_ref()
            .is_some_and(|c| c.text().contains("smoke-marker"))));
        let pasted = chat_request("subs/claude/sonnet", "image").unwrap();
        assert!(pasted.messages[1].content.as_ref().unwrap().has_image());
        let read_back = chat_request("subs/claude/sonnet", "image-tool").unwrap();
        assert_eq!(read_back.messages[2].role, "tool");
        assert!(read_back.messages[2].content.as_ref().unwrap().has_image());
        assert!(chat_request("m", "nope").is_err());
    }

    #[test]
    fn sdlc_shape_requires_documented_builtin_dump() {
        assert!(sdlc_shape_captured("chunk 0 frame 1: builtin=Some(7)\n"));
        assert!(sdlc_shape_captured("builtin=Some(5)"));
        assert!(!sdlc_shape_captured("builtin=None"));
        assert!(!sdlc_shape_captured(""));
    }

    #[test]
    fn pick_row_prefers_thin_rows_and_accepts_short_ids() {
        let mut thick = ModelEntry::bridge_probe("opus");
        thick.id = "subs/claude/opus".into();
        thick.transport = Transport::CliTurn;
        let mut thin = ModelEntry::bridge_probe("sonnet");
        thin.id = "subs/claude/sonnet".into();
        let rows = vec![thick, thin];
        assert_eq!(
            pick_row(&rows, BackendId::ClaudeCode, None).unwrap().id,
            "subs/claude/sonnet"
        );
        assert_eq!(
            pick_row(&rows, BackendId::ClaudeCode, Some("claude/opus"))
                .unwrap()
                .id,
            "subs/claude/opus"
        );
        assert!(pick_row(&rows, BackendId::Grok, None).is_err());
    }

    #[test]
    fn dialect_and_todo_scenarios_declare_the_preset_and_fail_without_their_call() {
        for scenario in ["dialect", "todo"] {
            let req = chat_request("subs/claude/opus", scenario).unwrap();
            let names: Vec<&str> = req.tools.iter().map(|t| t.function.name.as_str()).collect();
            for tool in ["read", "edit", "todo", "subagent", "askuserquestion"] {
                assert!(names.contains(&tool), "{scenario} lacks {tool}: {names:?}");
            }
            // The current Pi todo schema, so the TodoWrite row activates.
            let todo = req
                .tools
                .iter()
                .find(|t| t.function.name == "todo")
                .unwrap();
            let params = todo.function.parameters.as_ref().unwrap();
            assert!(params["properties"]["todos"]["items"]["properties"]["content"].is_object());
        }
        let edit = vec![json!({"type": "tool_call", "name": "edit"})];
        assert!(capture_scenario_failure("dialect", &edit).is_none());
        assert!(capture_scenario_failure("dialect", &[])
            .unwrap()
            .contains("edit"));
        let todo = vec![json!({"type": "tool_call", "name": "todo"})];
        assert!(capture_scenario_failure("todo", &todo).is_none());
        assert!(capture_scenario_failure("todo", &edit)
            .unwrap()
            .contains("todo"));
    }

    #[test]
    fn f1_scenarios_are_cursor_opt_in_and_never_fail_on_error() {
        for s in ["sdlc", "create", "create-fnf", "quiet", "mcp-repeat"] {
            assert!(is_opt_in(s), "{s}");
            let err = vec![
                json!({"type":"started"}),
                json!({"type":"error","status":503}),
            ];
            assert!(capture_scenario_failure(s, &err).is_none(), "{s}");
        }
        assert!(!is_opt_in("text") && !is_opt_in("tool"));
        assert!(
            !SCENARIOS.iter().any(|s| is_opt_in(s)),
            "never in the default set"
        );
        assert_eq!(
            scenario_dir_name("create", "subs/cursor/fable"),
            "create-fable"
        );
        assert_eq!(
            scenario_dir_name("mcp-repeat", "subs/cursor/grok-4.7"),
            "mcp-repeat-grok-4.7"
        );
        assert_eq!(scenario_dir_name("sdlc", "subs/cursor/fable"), "sdlc");
        assert_eq!(scenario_dir_name("text", "subs/cursor/fable"), "text");
    }

    #[test]
    fn only_create_scenarios_pick_a_read_miss_shape() {
        assert_eq!(
            capture_switches("create").cursor_read_miss_shape,
            Some(ReadMissShape::Rejected)
        );
        assert_eq!(
            capture_switches("create-fnf").cursor_read_miss_shape,
            Some(ReadMissShape::FileNotFound)
        );
        for s in ["text", "tool", "sdlc", "quiet", "mcp-repeat"] {
            assert_eq!(capture_switches(s), CaptureSwitches::default(), "{s}");
        }
    }

    #[test]
    fn f1_requests_carry_the_retest_and_draftr_histories() {
        let quiet = chat_request("subs/cursor/grok-4.7", "quiet").unwrap();
        let names: Vec<&str> = quiet
            .tools
            .iter()
            .map(|t| t.function.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["ls", "find", "grep", "read", "write", "edit", "bash", "todo"]
        );
        let roles = |r: &ChatRequest| -> Vec<String> {
            r.messages.iter().map(|m| m.role.clone()).collect()
        };
        assert_eq!(roles(&quiet), ["system", "user", "assistant", "tool"]);
        let repeat = chat_request("subs/cursor/fable", "mcp-repeat").unwrap();
        assert_eq!(
            repeat.messages.len(),
            10,
            "four tool steps: ls, find, grep, read"
        );
        assert_eq!(
            repeat
                .messages
                .last()
                .unwrap()
                .content
                .as_ref()
                .unwrap()
                .text(),
            "header\nMARKER-4172\nfooter\n"
        );
        for s in ["create", "create-fnf"] {
            let create = chat_request("subs/cursor/fable", s).unwrap();
            assert_eq!(create.tools.len(), 4);
            let brief = create.messages[3].content.as_ref().unwrap().text();
            assert!(brief.contains("smoke-marker.txt"), "{s}");
            assert!(
                brief.contains(CREATE_CWD) && !brief.contains("{worktree}"),
                "{s}"
            );
            assert!(create
                .messages
                .last()
                .unwrap()
                .content
                .as_ref()
                .unwrap()
                .text()
                .contains("cannot access 'src'"));
        }
    }

    #[test]
    fn f1_shape_reads_the_dump() {
        let dump = "chunk 0 frame 0: 9 bytes builtin=None\n\
                    f1 x\n  f1.f4 <thinking>\n\n\
                    chunk 1 frame 0: 9 bytes builtin=None\n\
                    f2 x\n  f2.f36 thickapi\n\n\
                    chunk 2 frame 0: 9 bytes builtin=None\n\
                    f1 x\n  f1.f27 varint 1\n\n\
                    chunk 3 frame 0: 9 bytes builtin=Some(7)\n\
                    f2 x\n  f2.f7 <read>\n\n\
                    chunk 4 frame 0: 9 bytes builtin=Some(3)\n";
        let shape = F1Shape::from_dump(dump);
        assert_eq!(shape.mcp_state_asks, 1);
        assert_eq!(shape.tool_progress_frames, 1);
        assert_eq!(shape.tool_progress_before_output, 0, "after the reasoning");
        assert_eq!(shape.builtins, [7, 3]);
        assert!(!shape.shows_mcp_repeat());

        let burst = "f2 x\n  f2.f36 a\nf1 x\n  f1.f27 varint 4\nf1 x\n  f1.f3 done\n";
        let shape = F1Shape::from_dump(burst);
        assert_eq!(shape.tool_progress_before_output, 2);
        assert!(shape.shows_mcp_repeat());
        let repeat = "  f2.f36 a\n  f1.f4 t\n  f2.f36 b\n";
        assert!(F1Shape::from_dump(repeat).shows_mcp_repeat());
    }

    #[test]
    fn a_handshake_death_is_started_then_error_only() {
        let died = vec![
            json!({"type":"started"}),
            json!({"type":"error","code":"vendor_down"}),
        ];
        assert!(handshake_only(&died));
        assert!(handshake_only(&[json!({"type":"error"})]));
        let later = vec![
            json!({"type":"started"}),
            json!({"type":"reasoning","text":"x"}),
            json!({"type":"error"}),
        ];
        assert!(!handshake_only(&later));
    }

    #[test]
    fn error_or_missing_read_file_fails_the_capture() {
        let ok_text = vec![
            json!({"type":"started"}),
            json!({"type":"text"}),
            json!({"type":"finish","reason":"stop"}),
        ];
        assert!(capture_scenario_failure("text", &ok_text).is_none());
        let err = vec![json!({"type":"error","status":502,"code":"vendor_drift"})];
        assert!(capture_scenario_failure("text", &err)
            .unwrap()
            .contains("ended in Error"));
        let tool_no_read = vec![
            json!({"type":"started"}),
            json!({"type":"tool_call","name":"bash"}),
            json!({"type":"finish","reason":"tool_calls"}),
        ];
        assert!(capture_scenario_failure("tool", &tool_no_read)
            .unwrap()
            .contains("read_file"));
        let tool_ok = vec![
            json!({"type":"started"}),
            json!({"type":"tool_call","name":"read_file"}),
            json!({"type":"finish","reason":"tool_calls"}),
        ];
        assert!(capture_scenario_failure("tool", &tool_ok).is_none());
    }
}
