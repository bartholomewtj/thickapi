# thickAPI

**Version:** 0.15 — Cargo `workspace.package.version` equals the SPEC version at every SPEC bump (`0.15.0`); `thickapi --version` and `GET /health` report it with the build's git sha.  
**Status:** Shipped through v0.15. Both surfaces are implemented. The thick turn lane is frozen: bug fixes only (`DECISIONS.md`).  
**Single source of truth:** this file is the only contract document; `README.md`, `AGENTS.md` and the folder READMEs point back to it.

> **Public edition.** Section numbers are stable because code comments cite them (`SPEC §19.8`). §15, §16, §18 and §20 (build phases, closed decisions, port sources, the hardening record) and the revision history are not in this edition. Issue and PR numbers (`#137`, `PR #216`), slice ids (`K3`, `G1`, `R22 C5`, `D3`, `H9`) and audit ids (`audit R11`) are labels from the project's pre-public tracker and plans. They are kept so the code and this file still match; they are not links.

thickAPI is a local Rust daemon that exposes subscription-backed coding agents (Claude, Grok, agy, Cursor, Codex) over HTTP. Two surfaces: a **thin** OpenAI `POST /v1/chat/completions` (Pi is the harness; thickAPI is the model) and a **thick turn** — prompt + `cwd` + session + optional write policy — that spawns the right CLI, streams JSONL as SSE, and optionally rolls back filesystem changes.

| | |
|---|---|
| Binary | `thickapi` |
| Config | `~/.thickapi/` (Windows: `%USERPROFILE%\.thickapi`) |
| Listen | `127.0.0.1:8787` (`router.yaml` `listen`) |
| Primary client | Pi (`providers.subs`, `api: "openai-completions"`) |
| API | `POST /v1/chat/completions` (thin: Pi, any OpenAI client) and `POST /v1/agent/turn` (thick, `thickapi turn` / scripts) |
| Keys | `ta_*` bearer tokens in `keys.yaml` (hashed); loaded at serve start and re-read when the file mtime changes (revocation and mint without restart) |

**Quick start:**

```bash
cargo install --path crates/thickapi-cli
thickapi serve
thickapi doctor
thickapi sync-pi            # mints or reuses a chat:write key for Pi
# Pi: select subs/claude/fable, chat in a repo
```

---

## 1. Design

### 1.1 What this is

One HTTP contract for any client (Pi, `thickapi turn`, curl, scripts). Spawns CLIs directly — same standing as running `claude -p` by hand. Thin proxies (OpenProxy, CLIProxyAPI) solve chat completions, not tool loops in a `cwd`.

**Not in the thick v1:** MCP server surface, multi-user auth, OAuth login (CLIs must be pre-authenticated). `/v1/chat/completions` and Pi `openai-completions` for `subs/*` are the **thin surface** in §19 (shipped).

### 1.2 Primary client: Pi

Two surfaces, two roles:

- **Thin (§19, shipped):** Pi is the harness. It calls `POST /v1/chat/completions` as a plain `openai-completions` provider with one `ta_` key (`chat:write`), keeps its own system prompt, tools, skills and compaction; thickAPI is only the model. This is the Pi lane.
- **Thick (this section, shipped):** HTTP client of a whole agent turn. The daemon's CLI runs its own system prompt and tool loop. Used by `thickapi turn` and scripts. The Pi thick lane (`extensions/pi/`, `api: "thickapi"`) retired at §19 T5.

`thickapi sync-pi` writes `providers.subs` into `~/.pi/agent/models.json` (`api: "openai-completions"`, `baseUrl: http://<router.yaml listen>/v1`, §19.10; one `chat:write` key). Catalogue rows come from `GET /v1/models?format=pi`. Pi `reasoning` / `thinkingLevelMap` / `defaultOptions.thinking` come from `thinking.rs` (`reasoning_ui` + `advertise`), not from yaml `thinking`. `sync-pi` does not force `reasoning: true`. Auto advertises `reasoning: false` and no map. Pi `/thinking` maps to `reasoning_effort` (§19.5). See §12 and §19.10.

### 1.3 Other clients

| Client | How |
|--------|-----|
| `thickapi turn` | CLI; `--stream` for SSE. `TurnSpec.thinking` is `Effort` (`off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`; omitted → `medium`) |
| curl / scripts | Thick: `POST /v1/agent/turn`. Thin: `POST /v1/chat/completions` |
| Multi-seat pipelines | Each seat is a Pi `--model subs/<vendor>/<id>:<level>` against `POST /v1/chat/completions`, not `POST /v1/agent/turn` |

Canonical types: `TurnSpec`, `TurnEvent`, `TurnResult`.

### 1.4 Backends (v1)

| Backend | Pool auth | Notes |
|---------|-----------|-------|
| `claude_code` | `cli_login` | One account per pool; policy-sensitive |
| `grok` | `cli_login` | Measured spawn in `agent_grok.py` |
| `agy` | `cli_login` | Quota-based |
| `cursor` | `cli_login` | `cursor-agent -p`; W3 overlays `{cwd}/.cursor/mcp.json` |
| `pi` (child) | `env` | `or/*` OpenRouter combo tail only; isolated `PI_CODING_AGENT_DIR` |

Combos + sticky sessions: shipped (phase 4 / `combos.yaml`). Paid OpenRouter tail: **opt-in** (`allow_paid: false` default), daily `max_paid_usd` cap. Headerless `429` → `suspected_blocked` cooldown (6 h, or clear it in `doctor`; §10.2).

### 1.5 Enforce model

| Layer | Who | What |
|-------|-----|------|
| CLI deny args | Router | `deny_writes` / `deny_globs` → `--disallowedTools` / `--deny Edit()` |
| Post-turn rollback | Router when `enforce_policy` set | Git fingerprint (default in git repos) or tree copy. Patterns match the whole path (`*`/`?` within a segment, `**` across, `**/` also none), case-insensitively on Windows (`cfg!(windows)` only). `writes` is exact; `protected_files` globs also cover anything nested under a match (`*.env` protects `config/prod.env`) and fail closed. Tree walk uses `symlink_metadata` (no junction follow), skips `.git`/`target`, and refuses the turn before spawn (400 `invalid_spec`) past 2 MiB, 10_000 files, or depth 64. The after-walk (enforce) uses the same ignore/no-follow rules with no file/depth cap so new files still roll back. Tree-copy rollback never deletes a file that predates the turn. Git fingerprints file content, not numstat counts; a `git` failure is an error, not an empty snapshot. `deny_globs` emits a CLI `--deny` only for a path `permitted()` rejects (`writes: ["**/*.rs"]` + `protected_files: ["src/*.rs"]` does not deny the write set) |

A thick-turn client sends `enforce_policy` when it wants a write guard. Shell holes mean enforce stays authoritative over CLI deny alone.

### 1.6 Security

| Threat | Mitigation |
|--------|------------|
| LAN exposure | Default `127.0.0.1`; `lan: false` refuses a non-loopback `listen` host (only `127.0.0.1`, `localhost`, `::1`) at config load, naming `listen`/`lan` (D3); `lan: true` + key rotation warning for `0.0.0.0` |
| Key in `models.json` | `ta_*` scoped `chat:write`; `write_private_atomic` (create empty sibling temp, Windows: `icacls /inheritance:r /grant:r *<SID>:F` then write bytes then rename; Unix: create mode 600). Callers: `keys.yaml`, Pi `models.json`, the sidecar, `claude_oauth_state.json`. Yaml stores hashes only |
| Private request dir | `PrivateDir` fails closed if the ACL cannot be applied; `Drop` retries `ERROR_SHARING_VIOLATION` and removes the dir. `doctor` fails on inherited ACEs for those files and every live `tmp/<req>` |
| Child inherits secrets | `child_env()` strips `ANTHROPIC_*`, `OPENROUTER_*`, `CLAUDECODE`, and inherited `MAX_THINKING_TOKENS` unless `allow_api` |
| cwd escape | Refuse list; `dunce::canonicalize`; case-insensitive prefix match |
| Subscription ToS | One `cli_login` account per pool; no quota-scored rotation |
| HTTP crate traces | `EnvFilter` from `THICKAPI_LOG` (else `RUST_LOG`, else `thickapi=info`) with `hyper=warn,h2=warn,reqwest=warn,rustls=warn` appended after, so the env cannot raise them |

### 1.7 Operations

| Concern | Design |
|---------|--------|
| Hung child | Total timeout + idle stall; Job Object terminate then wait (thick cancel and Claude bridge); `kill_process_tree` fallback |
| Orphans | PID registry with identity (owner pid + owner and child start stamps); startup closes the rows of a dead owner and leaves a live owner's rows (a `thickapi turn` on the same database) alone. On Windows no recorded pid is killed — the `KILL_ON_JOB_CLOSE` job object already took the tree with its owner; elsewhere a stale child is killed only when its pid still carries the recorded start stamp (audit R11); `serve` runs reap retention on a 24 h `tokio` interval (tmp, turns, logs); `install-service` also registers a daily `reap` task |
| Disconnect | Thick: turn completes on both the JSON and SSE routes (it runs on its own task, not the request's); `result.json` persisted; reconnect via `GET /v1/agent/turn/{id}`. A finished turn leaves the in-memory registry once it is 1 h old — swept when the next turn registers, not on a timer, so an idle daemon keeps its last turns until then; status, events and a re-`POST` of the same `turn_id` are then answered from `result.json` / `events.jsonl`, never by a second run. Thin: abort-on-drop of the runner `JoinHandle`; adapters `select!` on `tx.closed()` at long awaits |
| Drain | Thin in-flight gauge + `CancellationToken`; thin routes return `503 vendor_down "draining"`; shutdown is `timeout(drain_seconds, …)` then exit |
| Single instance | Lock file in `data_dir`; second `serve` refuses |
| Artifacts | `~/.thickapi/turns/<id>/` (`<id>` as-is when it is a plain file name, else `h-<sha256[..32]>` — a client id never escapes `turns/`; same for `pi-agents/<session_id>`); rolling logs `thin.log_dir/thickapi.<date>.log` (default `~/.thickapi/logs/`, keep `thin.log_keep_days`); `thickapi reap --days 7` |
| Health | `/health` liveness plus build identity (`version`, git `sha`; no auth); `/ready` DB + pool + rows-driven CLI + real `free_bytes` + thin vendor/path |

### 1.8 Rust stack

**Crates:** `thickapi-core` (types, config, adapters, enforce, engine, router, pool, store, trust, thinking, process, thin; plus `bootstrap`, `constants`, `disk`, `env`, `error`, `lock`, and the `fake-claude` test binary in `src/bin/`), `thickapi-api` (routes, auth middleware, `server.rs`; `build.rs` stamps the git sha), `thickapi-cli` (commands, plus the `thickapi capture` tooling in `src/capture/`).

**Size (2026-09-29, `git ls-files 'crates/**/*.rs' | xargs cat | wc -l`, tests included):** `thickapi-core/src/thin/` is ≈ 19.9k of ≈ 59.3k lines (34%); with the thin route `thickapi-api/src/routes/chat_completions/` (≈ 2.1k) the thin surface is ≈ 22.0k (37%). Integration tests under `crates/*/tests/` count in the total only.

**DAG:** `thickapi-cli → thickapi-api → thickapi-core`

**Deps:** `tokio`, `axum`, `serde`, `rusqlite`, `tracing`, `clap`. No embedded Python.

Inside `thickapi-core`: `adapters::<backend>/` owns argv, parser, `MEASURED` table, fixtures.

Inside `thickapi-core/src/thin/` (§19): `runtime.rs` builds one adapter per vendor and holds `VENDORS`, the one table that maps a catalogue backend to its adapter and a `/ready` vendor key to its block (`adapter_for`, `vendor_snapshot`, `wait_cli_versions` and `model_check::vendor_of` all read it); `oauth/` the OAuth-direct adapters (`vendor.rs`: `CliBacked`, `VendorSession`, `VendorRequest`; `claude_oauth.rs`, `grok.rs`, `antigravity.rs`, `cursor/`, `creds.rs`, `wincred.rs`, `sse.rs`, `state.rs`); `claude_bridge/` the Claude CLI bridge (§19.7); `openai.rs` the wire types; `error.rs` `ThinError`; `model_check.rs` family/version matching (§10.3); `capture.rs` the wire tap adapters carry (`FrameTap`, the record types, the scenario readers, `summarize`); `fixture_set.rs` fixture sets on disk and `TOKEN_PATTERNS` (§19.11); `private_dir.rs`, `empty_stop.rs` (§19.16), `anthropic_blocks.rs`, `mcp_proposal.rs`.

Inside `thickapi-api/src/routes/`: `chat_completions/` the thin route (§19.3–§19.4: `mod.rs` entry and `AbortOnDrop`, `validate.rs`, `start.rs` (request id, row resolve, wait for `Started`), `stream.rs`, `collect.rs` (non-stream), `headers.rs` (`x-thickapi-path` / `-fallback`), `access_log.rs`); `models.rs`, `ready.rs`, `health.rs`; the thick `turn.rs`, `turn_status.rs`, `turn_events.rs`, `turn_cancel.rs`. `middleware/auth.rs` holds the scope checks and `KeyLabel`.

---

## 2. Workspace layout

```
thickapi/
  Cargo.toml
  SPEC.md                    # the contract
  README.md                  # what it is, install, use (people)
  AGENTS.md                  # orientation and rules for coding agents (CLAUDE.md points here)
  CONTEXT.md                 # short map of the repo
  DECISIONS.md               # chose-X-not-Y record
  LICENSE                    # MIT
  crates/
    thickapi-core/
      src/thin/              # thin surface (§1.8, §19)
      src/bin/fake-claude.rs # CI stand-in for claude.exe (§14)
    thickapi-api/
      build.rs               # THICKAPI_GIT_SHA for `--version` and /health
      src/routes/chat_completions/   # thin route (§1.8)
    thickapi-cli/
      src/capture/           # `thickapi capture` tooling: Recorder, redact, verify, set (§19.11)
  extensions/pi-thin/        # Pi thin extension (§12, §19.10); extensions/pi/ deleted at T5
    thickapi-subs.ts         # entry Pi loads
    thickapi-subs-{error,path,ready,spawn}.ts   # SDK-free modules it imports
    thickapi-subs.test.ts    # bun test
  configs/                   # examples copied to ~/.thickapi on first serve (embedded via include_str!)
    router.yaml.example
    accounts.yaml.example
    combos.yaml.example
    models.yaml.example
  fixtures/
    {claude,grok,agy,cursor,pi}_{init,stream}.jsonl   # thick adapter replays
    fake-*.{cmd,sh}          # thick-lane fixture children (§14)
    pi-models-{before,after}.json, sse-turn-*.jsonl
    <vendor>/<cli-version>/{text,tool}/   # `thickapi capture` wire sets (§19.11)
    thin/                    # bridge stream-json turns (claude-bridge/), pi-tool-catalog.json, router yaml fixtures
    trust/                   # trust-file fixtures (claude/, grok/)
  scripts/
    verify-all.ts            # full local verification (scripts/README.md)
    tool-smoke.ts, claude-knobs.ts, claude-soak.ts   # live-daemon checks through Pi; not in CI
  .github/workflows/ci.yml
  rust-toolchain.toml
  justfile
```

---

## 3. HTTP API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat/completions` | Thin OpenAI-compatible model call (§19). Scope `chat:write` |
| `POST` | `/v1/agent/turn` | Start turn. SSE if `Accept: text/event-stream`, else JSON body when done |
| `GET` | `/v1/agent/turn/{id}` | `result.json` / state; reconnect after disconnect |
| `GET` | `/v1/agent/turn/{id}/events?since=` | Resume SSE from offset |
| `POST` | `/v1/agent/turn/{id}/cancel` | Cancel in-flight turn |
| `GET` | `/v1/models` | Model catalogue. Default OpenAI list shape; `?format=pi` for `sync-pi` (§3.2, §19.6) |
| `GET` | `/health` | `{ "status": "ok", "uptime_seconds": N, "version": "0.15.0", "sha": "<7-hex or unknown>" }`; no auth |
| `GET` | `/ready` | DB probe, per-pool `in_use/max`, CLI resolvable for configured rows, real disk `free_bytes` |

**Auth:** `Authorization: Bearer ta_...` — scopes `chat:write` (thin), `turn:write` (thick), `models:read`. `chat:write` satisfies `models:read`. `turn:write` does not imply `chat:write`. `/ready`, `/v1/models` and `/v1/chat/completions` answer an auth failure in the same OpenAI envelope (§19.4): missing or unknown key is `401 authentication_error`; a key present but lacking the route's scope is `403 permission_error` (D3). `/v1/agent/turn*` keeps its own thick `{code,message}` shape.

### 3.2 `GET /v1/models`

Default shape is the OpenAI list `{object:"list", data:[{id, object:"model", created, owned_by}]}`. `?format=pi` returns Pi model entries for thin-capable rows (`transport` ≠ `cli_turn`, `enabled`, an adapter wired). Each Pi entry:

| Field | Source |
|-------|--------|
| `id`, `name` | unprefixed id (`claude/fable`), `models.yaml` name |
| `contextWindow`, `maxTokens`, `input` | `context_window`, `max_tokens`, `input` |
| `reasoning` | `reasoning_ui(&ModelEntry)` in `thinking.rs` — **not** yaml `thinking`. Claude/Grok/Agy always true; Cursor iff the wire id has an effort suffix; Pi-child (`or/*`) follows yaml `thinking` |
| `cost` | explicit zeros for subs models |
| `thinkingLevelMap` | `profile(backend).advertise(entry)` when `reasoning_ui`; omitted when not |
| `defaultOptions.thinking` | `"medium"` only when `reasoning_ui`; omitted otherwise |

`thinkingLevelMap` keys are Pi `/thinking` levels (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`). Values are the string Pi sends as `reasoning_effort`, or `null` (do not send / hide the rung). Tables: §19.5.

`thickapi sync-pi` copies this payload into `providers.subs.models`. It does not force `reasoning: true`. Yaml `thinking` is a human hint on the catalogue row.

Ids resolve through one `ModelsConfig::find` (alias + `subs/` prefix or strip). Thick `router::resolve_model` is `find.ok_or(UnknownModel)` and returns `&ModelEntry`, so `claude/fable` and `subs/claude/fable` both work. There is no `ResolvedModel`.

**Idempotency:** `turn_id` in body or `Idempotency-Key` header. The first persist of a `turn_id` wins. A re-`POST` of a spent id with the **same** prompt never runs anything and replays the first answer with its original status (a turn that ended `429`/`502`/`504`/`500` replays as that error, not as `200`; one the child ended replays as `200` with the result). A re-`POST` with a **different** prompt answers `409 turn_conflict` and does not overwrite the store. Replay reads `TurnResult.error_code` when present; it does not re-parse `text`. A panicked or dropped turn persists `internal_error` and answers 500 the first time and on replay. The daily paid cap persists `rate_limited` and answers 429 on the first POST and on replay. `GET /v1/agent/turn/{id}` is always `200` with the stored result. Unknown `TurnSpec` / `EnforcePolicy` fields are rejected as `422 invalid_spec` with the thick `{code,message}` envelope (not axum's bare 422) — a misspelt `enforce_policy` must not run the turn unenforced; client extensions go in `metadata`. A crash between `result.json` and `TurnEnd` still replays from the store on both JSON and SSE — SSE synthesises `TurnEnd` from `result.json` and never starts a second run.

**Rate limits:** Router never sleeps. Emits `TurnEnd{state: rate_limited, retry_after_seconds}` on SSE. HTTP `429` only for pre-spawn pool refusal. Client owns retry.

**CORS:** off by default; `cors_origins: []` in `router.yaml` lists the allowed origins (`*` = any) for the thin routes `/v1/models` and `/v1/chat/completions`, independent of `lan` (§19.4).

### 3.1 Error codes

| `code` | HTTP | Meaning |
|--------|------|---------|
| `refused_workspace` | 400 | cwd on refuse list |
| `unknown_model` | 400 | model id not in catalogue |
| `invalid_spec` | 400 / 422 | bad `TurnSpec`: unknown combo, cwd that does not resolve, `or/*` without `allow_paid`, snapshot/enforce that cannot run (tree past 2 MiB / walk caps, `git` snapshot failure) are 400. An unknown JSON field (`deny_unknown_fields`) is 422 with the same `{code,message}` envelope, not axum's bare 422. Daemon-side snapshot IO (backup dir) is `internal_error` 500 |
| `queue_timeout` | 504 | pool slot wait exceeded |
| `turn_timeout` | 504 | child exceeded `timeout_seconds` |
| `rate_limited` | 429 | pre-spawn pool refusal, the daily paid cap, or the account is in a `rate_limited` / `suspected_blocked` / `spawn_failed` cooldown (§10.2) |
| `subscription_blocked` | 402 | vendor policy: the account is in a `subscription_blocked` cooldown (§10.2) |
| `permission_breach` | 422 | enforce rollback |
| `spawn_failed` | 502 | CLI not found or immediate exit |
| `turn_not_found` | 404 | status, events or cancel for an id the daemon has never seen |
| `turn_in_progress` | 409 | `POST` of a `turn_id` that is still running |
| `turn_already_finished` | 409 | cancel of a turn that has ended |
| `turn_conflict` | 409 | `POST` of a spent `turn_id` with a different prompt; first persist wins |
| `draining` | 503 | shutdown has begun; no new turns |
| `internal_error` | 500 | store failure, a panicked turn task, anything untyped |

---

## 4. Types

```rust
pub type TurnId = String;
pub type SessionId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct TurnSpec {
    pub model: String,              // subs/claude/opus | or/openrouter/...
    pub prompt: String,
    #[serde(default)] pub system_prompt: Option<String>,
    pub system_prompt_file: Option<PathBuf>,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    #[serde(default = "default_thinking")] pub thinking: Effort,

    #[serde(default)] pub tools: Vec<String>,
    #[serde(default)] pub deny_writes: Vec<String>,
    #[serde(default)] pub enforce_policy: Option<EnforcePolicy>,
    #[serde(default)] pub mcp: McpMode,
    #[serde(default)] pub mcp_config: Option<PathBuf>,
    pub setting_sources: Option<String>,
    #[serde(default)] pub allow_api: bool,
    pub combo: Option<String>,
    #[serde(default)] pub allow_paid: bool,
    #[serde(default = "default_timeout")] pub timeout_seconds: u64,
    pub client: Option<String>,     // "pi" | "cli" | "http" | freeform
    pub turn_id: Option<TurnId>,
    #[serde(default)] pub metadata: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct EnforcePolicy {
    pub writes: Option<Vec<String>>,       // None=unrestricted; []=read-only; [...]=allow-list
    pub protected_files: Vec<String>,
    pub out_of_scope: Option<Vec<String>>,
    pub always_writable: Vec<String>,
    #[serde(default = "default_enforce_mode")] pub mode: EnforceMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforceMode { GitFingerprint, TreeCopy }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    TurnStart { turn_id: TurnId, session_id: SessionId, backend: BackendId,
                account_id: String, model: String },
    Queued { position: u32 },
    AssistantDelta { text: String },
    ToolCall { tool: String, tool_call_id: String, args: serde_json::Value,
               label: Option<String>, ok: Option<bool>, result_snippet: Option<String>,
               duration_ms: Option<u64>, started_at: String, ended_at: Option<String> },
    Usage { input: u64, output: u64, cache_read: u64, cache_write: u64,
            reasoning: u64, cost: f64, cost_components: serde_json::Value },
    TurnFallback { from_model: String, to_model: String, reason: String },
    TurnEnd { state: TurnState, returncode: i32, text: String,
              retry_after_seconds: Option<u64>, touched_paths: Vec<String>,
              breaches: Vec<PermissionBreach> },
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnResult {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub state: TurnState,
    pub returncode: i32,
    /// Every assistant message of the turn in order, joined by a blank line (not only the
    /// last one). The `assistant_delta` events of the turn concatenate to exactly this.
    pub text: String,
    pub tokens: u64,
    pub cost: f64,
    pub context_tokens: u64,
    pub usage: UsageBreakdown,
    pub context_window: u64,
    pub backend: BackendId,
    pub account_id: String,
    pub resolved_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>, // §3.1 code when the engine ended the turn; replay uses this
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    Success, Failed, Cancelled, Timeout, RateLimited, SubscriptionBlocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendId { ClaudeCode, Grok, Agy, Cursor, Pi }

/// Shared effort ladder (`crates/thickapi-core/src/thinking.rs`; types re-export `Effort`).
/// Thick omitted field → Medium. Thin `None` = Absent (never coerced to Medium).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effort { Off, Minimal, Low, #[default] Medium, High, XHigh, Max }
```

**Session ID:** opaque, client-owned. Router maps to CLI session via `session_bindings` + `uuid_v5(THICKAPI_CLAUDE_NS, session_id)` for Claude. There is no `ThinkingLevel`.

### 4.1 Model ID grammar

```
model_id := "subs" "/" backend "/" model_rest
          | "or" "/" provider "/" model_rest

backend  := "claude" | "grok" | "agy" | "cursor"
provider := "openrouter" | ...
```

Pi shows `subs/claude/opus` (provider name `subs` + path). Aliases in `models.yaml` (`opus` → `subs/claude/opus`).

---

## 5. Turn engine

```
validate_spec → router.resolve → [per combo member]
  → session_binding
  → deny_writes (spec or deny_globs(enforce_policy))
  → cwd mutex (when enforce_policy set)
  → cancel check → Enforcer.snapshot (client failure → 400 `invalid_spec`; tree past cap does not spawn) → cancel check
  → spawn (Job Object, piped stdout/stderr, tee to artifacts)
  → parse JSONL → TurnEvent stream
  → wait (timeout + idle stall)
  → Enforcer.enforce (if enforce_policy; all terminal states; client failure → 400 `invalid_spec`)
  → result.json + session_binding update + TurnEnd
```

A cancel that arrives during snapshot is honoured before spawn. Enforce still runs after a child so writes roll back.

**JSONL.** `JsonlAccumulator` joins assistant messages with `\n\n`. An identical consecutive message — the line being written, the whole text, or the messages concatenated the way a CLI `result` line recaps them — is dropped. A Cursor `result` that recaps the turn and then adds trailing text keeps only the tail. Adapter tests replay one thick fixture (`fixtures/{claude,grok,cursor,agy,pi}_stream.jsonl`) through the accumulator.

**Windows spawn:**

```rust
#[cfg(windows)]
fn spawn_child(plan: &SpawnPlan, job: &JobObject) -> Result<Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let child = Command::new(&plan.program)
        .args(&plan.args)
        .envs(&plan.env)
        .current_dir(&plan.cwd)   // dunce-canonicalised
        .stdin(plan.stdin.to_stdio())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    job.assign(&child)?;
    Ok(child)
}
```

Reader threads tee to `stdout.jsonl` / `stderr.log`; stderr tail kept as bytes (last 16 KiB) and decoded once when the reader ends, so a multibyte char split across reads never panics the reader (audit R12), for `classify_failure`. Trust seeding behind process-wide mutex; every trust file (`.claude.json` nested + legacy, `~/.grok/trusted_folders.toml`, agy) is written via a sibling temp file and a rename; the Claude files are re-read just before the write and re-merged if another session changed them. Claude `projects` entries thickAPI created are listed in `~/.thickapi/claude-trust-seeded.json`, and only those are pruned, once their cwd no longer exists. `seed_backend_trust` returns its `SeedResult` and the engine logs a `warn` when seeding did not complete (audit R13); sharing-violation retries on reap/enforce.

Readers start before the prompt is written and the prompt goes to stdin on its own thread, so a prompt larger than the pipe buffer cannot deadlock a child that writes before it reads. The child exits, then the reader is joined and the line channel drained to disconnect — never the other way round.

**Idle stall:** output on stdout *or* stderr resets it. The window is per backend — 120 s, except agy at 14400 s, which prints nothing between its init line and its final result and is given the same figure in `--print-timeout` — and `router.yaml idle_stall_seconds.<pool>` overrides it (§10.1).

**Terminal states:** total timeout, idle stall, spawn failure, a CLI missing from PATH, a refused cwd and an unknown model all leave by the same exit as a clean run — a `TurnEnd` with the state (`timeout`, `failed`, `cancelled`), a persisted `result.json`, and the registry entry finished. The typed error is returned after that, so JSON callers still get the §3.1 code while an SSE client sees the stream end properly.

**SSE handler:**

```rust
async fn turn_sse(spec: TurnSpec, state: AppState) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move { state.engine.run_turn(spec, tx).await });
    Sse::new(ReceiverStream::new(rx).map(|e| {
        Event::default()
            .json_data(e)
            .unwrap_or_else(|_| Event::default())
    }))
}
```

---

## 6. Backend adapters

```rust
pub trait BackendAdapter: Send + Sync {
    fn id(&self) -> BackendId;
    fn build_spawn(&self, spec: &TurnSpec, resolved: &ModelEntry,
                   session: &SessionBinding, data_dir: &Path) -> Result<SpawnPlan>;
    fn parse_line(&self, line: &str) -> Vec<ParsedLine>;
    fn assistant_text(&self, event: &serde_json::Value) -> Option<String>;
    fn classify_failure(&self, returncode: i32, stderr: &str,
                        last_events: &[serde_json::Value]) -> TurnFailureClass;
    fn observe_init(&self, event: &serde_json::Value) -> Option<LearnedTools>;
    fn session_followed(&self, init_event: &serde_json::Value) -> Option<String>;
}
```

`build_spawn` / `build_argv` take `&ModelEntry`. Engine reads `entry.cli_model`, `entry.context_window`, `entry.backend`, `entry.id`. There is no `BackendAdapter::resolve_model` and no struct `ResolvedModel`.

MCP and write-deny placement is a `match` on `BackendId` (`McpPlacement` / `DenyPlacement` in `adapters/mcp.rs`) that calls the overlay helpers in place:

| Backend | MCP | Deny |
|---|---|---|
| Claude | argv `--mcp-config` | `--disallowedTools` |
| Grok | `{cwd}/.grok/config.toml` | `--deny Edit(…)` |
| Agy | home `mcp_config.json` + lock | none |
| Cursor | `{cwd}/.cursor/mcp.json` | `{cwd}/.cursor/cli.json` |
| Pi | none | none |

### 6.1 Claude — `crates/thickapi-core/src/adapters/claude.rs`

- Binary: `resolve_cli()` in `adapters/cli_resolve.rs` (PATHEXT; `.cmd` shim → `claude.exe`).
- Args: `-p`, `--output-format stream-json`, `--verbose`, `--model`, `--effort`, `--permission-mode bypassPermissions`, MCP, `--settings`, `--system-prompt-file`, `--disallowedTools` (**last**), `--resume` | `--session-id`. Prompt on stdin; close stdin.
- `path_deny_rule`: directory prefix → `/**`; globs pass through.
- Trust: `trust/` (`CLAUDE_CONFIG_DIR`, nested + legacy `.claude.json`, forward-slash keys).
- `MEASURED: &[(flag, cli_version, date)]`; `THICKAPI_LIVE=1` asserts flags in `--help`.

### 6.2 Grok — `grok`

Not `seats.grok.ts` (`--always-approve` is interactive seat mode).

- `--prompt-file`, `--verbatim`, `--output-format streaming-messages-json`, `--permission-mode bypassPermissions`, `--deny Edit(path)` per rule (not `--disallowed-tools`).
- Stream: grok 1.0.34+ emits Claude-shaped JSONL (`type:system`/`subtype:init`, `type:assistant` with `message.content` text blocks, closing `type:result`). The adapter also still parses the older `type:assistant_message` string body (committed `grok_stream.jsonl`). Thinking / `tool_use` blocks are not `TurnResult.text`. A `result` line that only recaps already-emitted text is dropped (SPEC §5).
- Session: the first turn of a `session_id` passes no session flag; grok mints the id, the adapter reads `session_id` off the stream and the engine stores it as `cli_session` on success. Later turns pass `--resume <cli_session>`. No `--session-id` on the first turn — grok refuses an id that already exists, so a first turn that failed after the CLI had created the session would lock the session out. Flag read from `grok --help` (1.0.34, 2026-09-20). When the stream has no `session_id`, turns stay independent. **Gap (live):** a resumed turn has not been run against the real CLI (#64). A `cli_session` the CLI no longer has is cleared on the unknown-session resume failure and the turn retries once without `--resume`; `thickapi doctor clear-session <session_id>` does the same by hand (Claude too).
- Trust: `~/.grok/trusted_folders.toml` + git common-dir for worktrees.
- MCP (W3): grok 1.0.30 has no `--mcp-config`. When `mcp != off` and `mcp_config` is set, merge servers into `{cwd}/.grok/config.toml` `[mcp_servers.*]` for the turn (same-name overrides user `~/.grok/config.toml`) and restore after. Copy each server's `env` into the child environment (stdio MCP inherits it).

### 6.3 Agy — `agy` (Google Antigravity)

- Trust seed, `--add-dir` root-trap, `--print-timeout`, session follow (#37).
- MCP (W3): agy 1.2.4 has no `--mcp-config` and no project MCP file. When `mcp != off` and `mcp_config` is set, merge `mcpServers` into `~/.gemini/config/mcp_config.json` (`url` → `serverUrl`) for the turn and restore after. Child `env` inherit as grok.

### 6.4 Cursor — `cursor-agent`

Not pane `seats.cursor.ts` (interactive `--trust` seat). Print-mode spawn:

- Binary: `cursor-agent` (`resolve_cli`; Windows `.cmd` shim).
- Args: `-p`, `--output-format stream-json`, `--workspace <cwd>`, `--trust`, `--force`, `--approve-mcps`, `--model <cli_model>`, optional `--resume`, prompt as the last argument. Stdin closed.
- MCP (W3): no `--mcp-config`. When `mcp != off` and `mcp_config` is set, merge `mcpServers` into `{cwd}/.cursor/mcp.json` for the turn (Claude-shaped `command`/`args`/`env`/`url`) and restore after. `--approve-mcps` so the child does not prompt. Do not mutate `~/.cursor/mcp.json`.
- Deny writes: merge scoped `Write(...)` rules into `{cwd}/.cursor/cli.json` `permissions.deny` for the turn and restore.

### 6.5 Pi child (`or/*` only)

- `pi -p` with isolated `PI_CODING_AGENT_DIR`. Combo tail when `allow_paid: true`.

### 6.6 Drift defences

- `learned_tools` ledger from `observe_init`; version-stamped fixtures under `fixtures/<vendor>/<ver>/`; adapter argv goldens; `serve --dump-tools`; capture fail-closed; `thin.vendors.claude.bridge.tolerate`; CLI vs fixture-set warn in `doctor` (§19.11). There is no `measured_versions` in `router.yaml`.

---

## 7. Enforcer

`crates/thickapi-core/src/enforce/` (git snapshot + non-git walk):

```rust
impl Enforcer {
    pub fn deny_globs(policy: &EnforcePolicy, data_dir: &Path) -> Vec<String>;
    pub fn path_deny_rule(pattern: &str) -> String;
    pub fn snapshot(&self, cwd: &Path, mode: EnforceMode) -> Result<Snapshot>;
    pub fn enforce(&self, cwd: &Path, policy: &EnforcePolicy,
                   before: &Snapshot) -> Result<EnforceOutcome>;
}
```

Git mode: content hashes of dirty / untracked / selected ignored paths (`h:`/`u:`/`i:` + sha256), plus `HEAD_KEY`. Paths come from `git -z` (NUL-split, never C-quoted). `git()` errors propagate — an empty map is not a successful snapshot. Tree mode: `symlink_metadata` (no junction follow), skip `.git`/`target`, refuse past 2 MiB or walk caps; the after-walk has no file/depth cap. `_uncommit`, `changed_paths`, outcomes (`REVERTED-BY-AGENT`, `left as-is`, `deleted`, `rolled back`). `deny_globs` exports a CLI `--deny` only when `permitted()` rejects a probe of that pattern.

---

## 8. Router

- `ModelsConfig::find` (alias + `subs/` prefix or strip) is the one catalogue resolver.
- One account per `cli_login` pool; round-robin only for `auth: env`.
- Combos: `resolve_combo`, sticky `session_bindings`.

---

## 9. Store (SQLite WAL)

```sql
CREATE TABLE session_bindings (
  session_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  backend TEXT NOT NULL,
  cli_session TEXT,
  cwd TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE processes (
  id INTEGER PRIMARY KEY,
  turn_id TEXT,
  pid INTEGER,
  backend TEXT,
  started_at TEXT,
  ended_at TEXT,
  owner_pid INTEGER,       -- identity for the orphan reap (§1.7); added by migrate()
  owner_started INTEGER,
  pid_started INTEGER
);

CREATE TABLE account_health (
  account_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE paid_daily (
  day TEXT PRIMARY KEY,
  usd REAL NOT NULL
);
```

The source is `crates/thickapi-core/src/store/schema.sql`. Trace writes best-effort. `busy_timeout=5000`.

---

## 10. Configuration

All files live in `~/.thickapi/`. Examples ship in `configs/*.example`.

### 10.1 `router.yaml`

```yaml
listen: "127.0.0.1:8787"
data_dir: "~/.thickapi"
lan: false
default_timeout_seconds: 5400
queue_timeout_seconds: 300
max_queue_depth: 16
max_paid_usd_per_day: 5.00
drain_seconds: 120
refuse_paths: []
cors_origins: []           # thin-route CORS origins; independent of lan (§3, §19.4)
idle_stall_seconds:        # optional, per pool; unset pools take the per-backend default
  agy: 14400
claude_oauth: { ... }      # §19.15
thin:                      # every key optional; defaults shown
  start_deadline_seconds: 300
  reply_timeout_seconds: 30
  keepalive_seconds: 15
  max_body_mib: 32
  log_dir: ~/.thickapi/logs
  log_keep_days: 14
  trace: false
  trace_keep: 20
  oauth_in_flight: 4
  vendors:
    claude:
      path: null           # bridge claude.exe; default resolved from PATH
      bridge: { max_concurrent: 2, timeout_seconds: 1800, idle_seconds: 300, mcp_ready_timeout_ms: 5000,
                handoff_drain_seconds: 10, tolerate: { extra_tools: [], nonempty_inventories: [] } }
      budgets: { first_event_seconds: 120, idle_seconds: 180 }
    grok:   { base_url: null, credentials_path: null, client_version: null,
              budgets: { first_event_seconds: 120, idle_seconds: 180, tool_call_floor_seconds: 900 } }
    agy:    { base_url: null, credentials_path: null, client_version: null, project: null,
              budgets: { first_event_seconds: 120, idle_seconds: 180 } }
    cursor: { base_url: null, credentials_path: null, client_version: null, cwd: null,
              shell: null, builtin_loop_limit: null,
              budgets: { first_output_seconds: 90, idle_seconds: 4, idle_after_text_with_tools_seconds: 300,
                         list_models_seconds: 60 } }
```

`thin.vendors.cursor.shell` (G4, default `bash`) is the `RequestContext.env.shell` string advertised to the agent service: the client's shell tool, which is Pi's `bash` (Git bash on Windows), not the host OS default; set it only for a client whose shell tool is something else (§19.8).

`thin.vendors.cursor.builtin_loop_limit` (G3, default 3, `0` = off; 1 is a load error, it must be 0 or at least 2) is the loop guard's N: a Cursor request whose recent history holds that many identical tool calls with identical results is refused `502 builtin_loop` before any Run opens (§19.8). They need not be consecutive: a different call between them does not reset the count.

A vendor's knobs live only in `thin.vendors.<id>` (R22 C5); `configs/router.yaml.example` names the flat key each one replaced. **Legacy keys:** the flat pre-C5 keys (`thin.cursor_cwd`, `thin.grok_tool_call_seconds`, `thin.oauth_idle_seconds`, `thin.bridge_max_concurrent`, `thin.bridge.tolerate`, … — the table is `config/legacy.rs`) still load, each rewritten to its new key with one `warn` per load (`router.yaml: legacy key; move it to thin.vendors…`); `oauth_first_event_seconds` / `oauth_idle_seconds` fill claude, grok and agy; where the new key is also set it wins and the warn says the legacy one was ignored. `measured_versions` (deleted by H9 — §6.6) loads with a `warn` "ignored (removed at H9)". Any other unknown key in `router.yaml`, `accounts.yaml`, `models.yaml` or `combos.yaml` is a load error naming it (`deny_unknown_fields`, audit D8).

### 10.2 `accounts.yaml`

```yaml
pools:
  claude:
    max_concurrent: 2
    accounts:
      - id: claude-main
        backend: claude_code
        auth: cli_login
        policy:
          tos_url: "https://www.anthropic.com/legal/aup"
          reviewed_on: "2026-09-15"
  grok:
    max_concurrent: 1
    accounts:
      - id: grok-main
        backend: grok
        auth: cli_login
  agy:
    max_concurrent: 1
    accounts:
      - id: agy-main
        backend: agy
        auth: cli_login
  cursor:
    max_concurrent: 1
    accounts:
      - id: cursor-main
        backend: cursor
        auth: cli_login
  openrouter:
    max_concurrent: 4
    accounts:
      - id: or-main
        backend: pi
        auth: env
        env_key: OPENROUTER_API_KEY
```

**Account health.** A turn that ends `rate_limited`, `subscription_blocked`, `suspected_blocked` or `spawn_failed` records that status against its account, and the router refuses the account (`429 rate_limited`, message names the seconds left) while the status is fresh. The block lapses on its own: `rate_limited` after 15 min, `spawn_failed` after 60 s, anything else after 6 h, counted from the row's `updated_at`. The first turn after that is the probe — success records `ok`, another failure restarts the clock. Override per pool with `health_ttl_seconds: {<status>: <seconds>}`; `0` keeps the block until `thickapi doctor clear-health <account_id>`. A pool with more than one account rotates (mixed-auth included) and steps over blocked members, including a sticky session whose bound account is blocked. `/ready` and `doctor` fail only on blocks still in force. `/ready` `accounts` rows are `{status, blocked, retry_in_seconds}`: a lapsed row keeps its status until the probe but `blocked` is false.

### 10.3 `models.yaml`

**Policy: newest of each family only.** The catalogue carries one `subs/*` row per vendor model
family and that row names the vendor's **newest** version: Claude Fable, Opus and Sonnet, Grok, Gemini
Flash (Antigravity), and Cursor's Claude Fable, Grok, Auto and Composer. Superseded versions
(Sonnet 5, Opus 4.6, Gemini 3.1 Pro, ...) are not rows; families not wanted (Haiku,
GPT, ...) are not rows either. Row ids are versionless where the family has one newest member
(`subs/claude/fable`, `subs/cursor/auto`) so a Pi pick survives a version roll: bump
`cli_model` / `vendor_model`, keep the id, restart `serve`, run `sync-pi`. `thickapi models
check` and the `models:` section of `doctor` (§19.11) enforce the policy against each vendor's
live list; the example below is what `check` reported current on 2026-09-22 for the Grok
rows (the Opus row moved to Opus 5.5 on 2026-09-28, #146; Sonnet returned as `subs/claude/sonnet` / `claude-sonnet-5-5` on 2026-10-03, K1, #219; Composer returned as `subs/cursor/composer-2.5` on 2026-10-11, #262; other families last confirmed 2026-09-17). Cursor rows name the id
`cursor-agent --list-models` prints; the adapter decomposes it into the wire's base id
plus `ModelParameter` pairs (§19.8). Grok 4.7's printed slug `grok-4.7-high` decomposes to
`grok-4.7` plus `reasoning_effort` and `fast`. The same legacy slug names a `context=256k`
variant and a `context=500k` max-mode variant (`increasesModelCost`). `subs/cursor/grok-4.7`
sends `context=500k` because its `context_window` is 500000, and sets
`RequestedModel.max_mode` (field 2) because that variant is `is_max_mode`. Connect
answers `not_found` when `context=500k` arrives without the flag. `wire_model` with no
context argument still defaults to 256k and leaves `max_mode` unset.
The parameter id is `reasoning_effort`, not the `effort` earlier Grok versions used.

```yaml
aliases:
  fable: subs/claude/fable
  opus: subs/claude/opus
  sonnet: subs/claude/sonnet
  grok-4.7: subs/grok/grok-4.7
  flash: subs/agy/gemini-3.8-flash
  cursor-fable: subs/cursor/fable
  grok-4.7-high: subs/cursor/grok-4.7
  cursor-grok-4.7: subs/cursor/grok-4.7
  cursor-grok-4.7-high: subs/cursor/grok-4.7
  cursor-auto: subs/cursor/auto
  composer: subs/cursor/composer-2.5
  composer-2.5: subs/cursor/composer-2.5

models:
  - id: subs/claude/fable
    backend: claude_code
    cli_model: claude-fable-5-1
    context_window: 200000
    max_tokens: 32000
    thinking: true
    input: [text, image]
    cost: 0
    name: "Claude Fable 5.1 (subscription)"
    transport: oauth_direct
  - id: subs/claude/opus
    backend: claude_code
    cli_model: claude-opus-5-5
    context_window: 200000
    max_tokens: 32000
    thinking: true
    input: [text, image]
    cost: 0
    name: "Claude Opus 5.5 (subscription)"
    transport: oauth_direct
  - id: subs/claude/sonnet
    backend: claude_code
    cli_model: claude-sonnet-5-5
    context_window: 1000000
    max_tokens: 128000
    thinking: true
    input: [text, image]
    cost: 0
    name: "Claude Sonnet 5.5 (subscription)"
    transport: oauth_direct
  - id: subs/grok/grok-4.7
    backend: grok
    cli_model: grok-4.7
    context_window: 500000
    max_tokens: 32000
    thinking: true
    cost: 0
    name: "Grok 4.7 (subscription)"
    transport: oauth_direct
  - id: subs/agy/gemini-3.8-flash
    backend: agy
    cli_model: gemini-3.8-flash
    vendor_model: gemini-3.8-flash-tiered
    context_window: 1000000
    max_tokens: 65536
    thinking: true
    input: [text, image]
    cost: 0
    name: "Gemini 3.8 Flash (agy subscription)"
    transport: oauth_direct
  - id: subs/agy/gemini-4-argon
    backend: agy
    cli_model: gemini-4-argon-medium
    vendor_model: gemini-4-argon-medium   # effort-suffixed: the adapter swaps -medium per request
    context_window: 1048576
    max_tokens: 65536
    thinking: true
    input: [text, image]
    cost: 0
    name: "Gemini 4 Argon (agy subscription)"
    transport: oauth_direct
  - id: subs/agy/opus
    backend: agy
    cli_model: claude-opus-5-5-medium
    vendor_model: claude-opus-5-5-medium
    context_window: 1000000
    max_tokens: 128000
    thinking: true
    input: [text, image]
    cost: 0
    name: "Claude Opus 5.5 (agy subscription)"
    transport: oauth_direct
  - id: subs/cursor/fable
    backend: cursor
    cli_model: claude-fable-5-1-thinking-high
    vendor_model: claude-fable-5-1-thinking-high
    context_window: 300000
    max_tokens: 32000
    thinking: true
    input: [text, image]
    cost: 0
    name: "Claude Fable 5.1 (Cursor subscription)"
    transport: oauth_direct
  - id: subs/cursor/grok-4.7
    backend: cursor
    cli_model: grok-4.7-high
    vendor_model: grok-4.7-high
    context_window: 500000
    max_tokens: 32000
    thinking: true
    input: [text]
    cost: 0
    name: "Grok 4.7 (Cursor subscription)"
    transport: oauth_direct
  - id: subs/cursor/auto
    backend: cursor
    cli_model: auto
    vendor_model: auto
    context_window: 128000
    max_tokens: 32000
    thinking: false
    input: [text, image]
    cost: 0
    name: "Auto (Cursor subscription)"
    transport: oauth_direct
  - id: subs/cursor/composer-2.5
    backend: cursor
    cli_model: composer-2.5
    vendor_model: composer-2.5
    context_window: 200000
    max_tokens: 32000
    thinking: false
    input: [text, image]
    cost: 0
    name: "Composer 2.5 (Cursor subscription)"
    transport: oauth_direct
  - id: or/anthropic/claude-opus-5
    backend: pi
    provider: openrouter
    cli_model: anthropic/claude-opus-5
```

Pi field mapping: `context_window` → `contextWindow`, `cost: 0` explicit. Yaml `thinking` is a **human hint**, not the Pi source of truth. `GET /v1/models?format=pi` and `sync-pi` call `reasoning_ui` / `advertise` in `thinking.rs` and never force `reasoning: true`. Auto: yaml `thinking: false`, and `reasoning_ui` is false because that id has no effort suffix. Grok `reasoning_ui` is true even when yaml `thinking` is false. When `reasoning_ui`, sync emits `thinkingLevelMap` (§19.5) and `defaultOptions: { thinking: "medium" }`.

### 10.4 `combos.yaml`

```yaml
combos:
  claude-then-or:
    - subs/claude/fable
    - or/anthropic/claude-opus-5
```

Default combo ends in `rate_limited` failure unless `allow_paid: true`.

### 10.5 `keys.yaml` (`write_private_atomic`)

```yaml
keys:
  - id: pi-thin                        # sync-pi reuses or mints this one for Pi's models.json
    hash: "sha256:..."
    scopes: [chat:write, models:read]
    created: "2026-09-28T12:00:00+00:00"
  - id: scripts                        # keys create --label scripts --scope turn:write --scope models:read
    hash: "sha256:..."
    scopes: [turn:write, models:read]
    created: "2026-09-28T12:05:00+00:00"
```

`thickapi keys create --label <name> [--scope <scope>]...` prints plaintext once; `--scope` is repeatable (`chat:write` | `turn:write` | `models:read`, an unknown scope is a clap error) and defaults to `[chat:write, models:read]` when absent (D3) — the thin shape, not a thick-turn credential; pass `--scope turn:write --scope models:read` for `thickapi turn` / scripts. `thickapi keys list` prints `id`, `scopes` and `created` — never the hash; `created` is blank for a row written before this field existed. `thickapi keys revoke <id>` removes every row with that id via `save_keys` (D3). Keys are hashed in `keys.yaml`; `serve` loads them at start and `LiveKeys` re-reads when the file mtime changes (a revocation — by `keys revoke` or a hand edit — takes effect without restart; `sync-pi` mint needs no restart either). A token that matches no key does not re-read the file while its mtime is unchanged. `sync-pi` reuses the `pi-thin` token already in `models.json` when that row would still be accepted, and mints only when it is missing or would be rejected: Pi loads `models.json` at startup and does not reload it, so replacing a live key makes the running process's next turn `401 authentication_error`. `save_keys`, `sync-pi` (`models.json` + sidecar) and `state.rs::store` write through `write_private_atomic`: a sibling temp is created empty (Unix mode 600), ACL-tightened (`secure_private_file`), then the bytes are written, the temp is verified to have no inherited ACEs, renamed, and the final path is verified again. `doctor` fails on inherited ACEs for `keys.yaml`, Pi `models.json`, the sidecar, `thin/claude_oauth_state.json`, and every live `tmp/<req>`.

Pi's `pi-thin` key is `[chat:write, models:read]`. `turn:write` is for `thickapi turn` / scripts, not for the key in `models.json`.

### 10.6 Constants

- `THICKAPI_CLAUDE_NS` — fixed UUID namespace for Claude session mapping; never change.
- `THICKAPI_LOG=debug` — verbose logging; HTTP crates stay pinned to `warn` (§1.6).
- `THICKAPI_LIVE=1` — run live CLI integration tests.
- `THICKAPI_TEST_CHILD` — debug/test only (`cfg!(debug_assertions)`). A release `thickapi.exe` ignores it.

---

## 11. CLI

```bash
thickapi --version          # `thickapi 0.15.0 (<git sha>)`; `unknown` when built without .git (§1.7 Health)
thickapi serve [--config dir] [--dump-tools PATH]   # dump-tools: first thin `tools` array, then clear
thickapi doctor [--offline] [--config dir]     # spends one 1-token request per OAuth vendor; --offline skips it (and the models live check)
thickapi doctor clear-health <account_id>      # drop an account-health block (§10.2)
thickapi doctor clear-session <session_id>     # forget the session's CLI resume id; the account binding stays
thickapi turn [--config dir] [--cwd .] [--model subs/claude/fable] [--combo <name>] [--allow-paid] [--stream] "prompt"
thickapi sync-pi [--config dir] [--key <plaintext>]
thickapi models list        # every models.yaml row: id, backend, wire id, transport, aliases
thickapi models check       # each subs/* row vs the vendor's live list; exit 1 when behind or gone (§10.3)
thickapi keys create --label <name> [--scope chat:write|turn:write|models:read]...   # repeatable; default [chat:write, models:read] (D3)
thickapi keys list          # id, scopes, created — never hashes (D3)
thickapi keys revoke <id>   # removes the row; no restart needed (§10.5)
thickapi capture <vendor>   # also: --all, --model, --out (default fixtures), --scenario, --keep-failed. Cursor `--scenario sdlc` is opt-in (not in `--all`). Exit 1 and no meta.json when a scenario ends in Error or the tool scenario lacks read_file
thickapi capture --check <vendor>   # or --check --all: capture to a temp dir, leak-check, replay, diff against the newest set; never promotes (§19.11)
thickapi install-service [--weekly-capture]    # Windows: serve at logon + daily reap; weekly capture --all is opt-in, default off, and passes an absolute --out <data_dir>/captures (D3)
thickapi reap --days 7 [--dry-run]
thickapi mcp-proposal --catalog <p> --ready <p> --violation <p>   # hidden from --help; the bridge's proposal server, spawned by the daemon (§19.7)
```

---

## 12. Pi client (`extensions/pi-thin/`)

**Rewritten at §19 T5.** Pi is an OpenAI client of the thin surface (§19); it never calls `POST /v1/agent/turn`. The Phase 2 thick provider (`extensions/pi/`, provider `thick`, `api: thickapi`, `[sonnet](thickapi)` lane aliases) shipped, ran alongside the thin lane through T1–T4, and was deleted at T5; `/v1/agent/turn` itself stays for `thickapi turn` and scripts.

`thickapi sync-pi` (§19.9–§19.10):

1. `--key`, else reuse the `pi-thin` token already in `models.json` when `keys.yaml` still has it with `[chat:write, models:read]` and without `turn:write`. Otherwise mint a `pi-thin` key beside the current one. Reuse is the default because Pi keeps the key it loaded at startup: minting on a healthy file revokes that key and the next turn is `401 authentication_error`.
2. `GET /ready` with that key — refuses to write when the daemon does not answer, and the sidecar is written before `models.json` so handing Pi the key is the last step that can fail. When `sync-pi` minted the key, what `models.json` ends up holding decides which key is retired: the minted one if Pi never got it, its predecessors if it did (one live key per label). A reused key is not rotated. A retire failure warns and never masks the sync result.
3. `GET /v1/models?format=pi` → thin rows as Pi model entries (unprefixed ids `claude/fable`, `grok/grok-4.7`, …; `thinkingLevelMap`, `defaultOptions.thinking`, zero cost).
4. Atomic merge into `~/.pi/agent/models.json` via `write_private_atomic`: an unparseable file is refused, never rewritten; the previous file is kept as `models.json.bak` (private); replace only `providers.subs` (`api: openai-completions`, `baseUrl` from `router.yaml` `listen`, §19.10); remove a leftover `providers.thick` (or a pre-T1 `subs` still `api: thickapi`) and revoke its `pi-sync` `turn:write` key; write the `~/.pi/agent/thickapi-subs.json` sidecar (daemon exe, config dir, base URL; `allow_debug: true` when the minted exe sits under `\target\`).
5. Warn when `install-service` is not registered.

`extensions/pi-thin/thickapi-subs.ts` is the one Pi extension that stays. It is copied as the whole `thickapi-subs*.ts` module to `~/.pi/agent/lib/thickapi-subs/`; `~/.pi/agent/settings.json` `extensions` lists `~/.pi/agent/lib/thickapi-subs/thickapi-subs.ts`. Never under `~/.pi/agent/extensions/` — Pi loads every `.ts` there (`extensions/pi-thin/README.md`). It re-registers `subs` with a thin `streamSimple` wrapper: `/ready` probe on `session_start` and `/model`; on `ECONNREFUSED` it spawns only an absolute sidecar `exe` or `THICKAPI_EXE` (never a bare name; refuses a `\target\` exe unless the sidecar has `allow_debug: true`); child stdio → `~/.thickapi/logs/autostart.log`; polls `/ready` for 10 s. Classifies an error on the envelope `code` or the trailing `[code=…]` marker (§19.4), never on status digits in free text, and puts the marker back on the end of the text it rewrites (Pi records that text; a pipeline can read the code from it). Rewrites to "stale ta_ key: run `thickapi sync-pi`" only when the code (or envelope `type`) is `authentication_error` and there is no vendor (no `vendor` field, no `vendor ` prefix on a mid-stream message); `permission_error` without a vendor is the missing `chat:write` hint; otherwise renders `message` and `hint` verbatim; `vendor_drift` becomes `thickAPI: <vendor> wire changed — <message>. Run <exe> doctor` with no status digits. Against an older daemon without the marker it falls back to the old text match, with folded `retry_after=Ns` removed first. Status bar is two segments — `path` (selected row's vendor: `claude direct` / `claude via bridge (probe 19:43)` / `grok` …, from `x-thickapi-path`) and `quota` (`x-thickapi-rate-limit`) — neither overwrites the other; one `notify` per path flip. Pi keeps its own system prompt, tools, skills and compaction; Pi `/thinking` maps to `reasoning_effort` (§19.5) and reasoning deltas stream as `reasoning_content` (plus `reasoning_details` on Claude direct, §19.8.1). Hints name the exe path when `thickapi` is not on PATH.

After a daemon rebuild: restart `serve`, re-run `sync-pi`. A sync that reuses the current key does not require a Pi restart. A sync that minted (no reusable key, or `--key`) does: restart Pi so it loads the new `models.json` key. The daemon itself needs no restart after `keys create` or `sync-pi` (keys re-read when `keys.yaml` mtime changes).

---

## 13. Optional integrations

None ship in this tree. A pipeline of agent seats drives Pi `subs/…:<level>` against `POST /v1/chat/completions` (§1.3); there is no pipeline-specific route.

---

## 14. Testing

| Layer | Tests |
|-------|-------|
| `adapters` | argv goldens; `test_the_deny_flag_stays_last_in_argv`; env scrubbing |
| `enforce` | git fingerprint; tree copy; deny_globs; uncommit; breach rollback |
| `engine` | fixture JSONL; fake child; cumulative cost |
| `trust` | port `trust.test.ts` fixtures |
| `api` | SSE parse; reconnect; idempotency; `/health` `version` + `sha` on an ephemeral port |
| `sync-pi` | merge keeps other providers, drops retired `thick`, revokes `pi-sync` (`tests/sync_pi.rs`, unit tests) |
| wire fixtures | `tests/wire_fixtures.rs`: newest `fixtures/<vendor>/<version>/` set replays to the recorded events (§19.11) |
| Live | `THICKAPI_LIVE=1 cargo test -- --ignored` |
| CI | `.github/workflows/ci.yml` on `push` to `main`, `pull_request` and `workflow_dispatch`, on `windows-latest`: fmt check, clippy `-D warnings`, `cargo test --workspace`, `bun test` in `extensions/pi-thin`. Toolchain pinned by `rust-toolchain.toml`. Live tests never run in CI |

**Enumerated enforce tests:** `out_of_scope_suffix_match`, `writes_not_suffix_matching`, `none_fail_closed`, `empty_writes_noop`, `unlock_by_naming`, `deny_globs_never_forbids_what_permitted_allows`, `deny_globs_does_not_deny_the_write_set`, `uncommit_committed_breach`, `rewritten_history_reported`, `head_not_changed_path`, `globs_match_the_whole_path`, `matching_ignores_case_on_windows`, `tree_rollback_never_deletes_a_snapshotted_file`, `tree_snapshot_refuses_past_the_cap`, `tree_walk_skips_symlinks_git_and_target`, `tree_enforce_rolls_back_files_past_the_snapshot_depth_cap`, `git_snapshot_propagates_errors`, `git_fingerprint_is_content_not_numstat`, `git_snapshot_fingerprints_quoted_names`, `protected_globs_cover_nested_paths`.

**Fixture child:** `fixtures/fake-claude.sh` / `.cmd` emit recorded stream-json lines for the thick lane. Thin bridge CI uses the `fake-claude` binary (`crates/thickapi-core/src/bin/fake-claude.rs`).

CI runs with no vendor CLIs on `PATH` and no ACL inheritance on the runner's temp files, so `resolve_cli_skips_extensionless_npm_shims`, `unwrap_cursor_node_finds_sibling_node`, and `ready_a_lapsed_block_is_visible_but_not_blocking`'s `cli`-dependent assertion are live-gated (`#[ignore]` + `THICKAPI_LIVE`, like `live_measured_flags_in_help`) or scoped to the account-block fields under test; `secure_private_file_removes_inheritance` and `inherited_models_json_fails_acl_check` check the `has_inherited_access` precondition first and self-skip with an `eprintln!` when it does not hold.

---

## 17. Success metrics

1. Pi completes multi-turn coding session via `subs/*` through the extension.
2. `thickapi doctor` green for claude + grok + agy + cursor-agent; `/ready` before Pi connects.
3. `thickapi turn` works without Pi installed.
4. Enforce rollback passes when `enforce_policy` set.
5. `allow_api: false` proves env strip in test hook.
6. Release binary < 15 MB stripped.

---

## 19. Thin API — OpenAI-compatible model surface

**Status:** shipped (phases T0–T5, §19.12).

### 19.1 Goal

One `ta_` key, one `POST /v1/chat/completions`, subscription-backed models. Pi — or any OpenAI client — keeps its own system prompt, tools, skills and compaction; thickAPI is only the model, exactly as OpenRouter would be. The thick `/v1/agent/turn` (§3–§8) stays for `thickapi turn` and scripts.

```mermaid
sequenceDiagram
  participant Pi as Pi TUI
  participant TA as thickAPI /v1/chat/completions
  participant CB as claude bridge
  participant OA as oauth adapter
  Pi->>TA: messages + tools + system, stream=true
  TA->>TA: resolve model -> transport
  alt oauth_direct (Grok, Antigravity, Cursor; Claude only under prefer or only, when the direct path is open)
    TA->>OA: vendor request with CLI-stored token
    OA-->>TA: native stream incl. tool calls
  else cli_bridge (Claude default, mode off; also the prefer fallback when Anthropic rejects the direct path)
    TA->>CB: serialize transcript, write catalog
    CB->>CB: spawn claude.exe --tools "" --mcp-config {pi: thickapi mcp-proposal}
    CB-->>TA: text/thinking deltas; stop_reason tool_use -> all tool_use blocks -> kill child
  end
  TA-->>Pi: OpenAI chunks, finish_reason stop|tool_calls
  Pi->>Pi: runs its own tool, sends next request
```

### 19.2 Decisions

- **Wire:** OpenAI chat completions only. No Anthropic `/v1/messages` shim.
- **Per-vendor transport**, a new `transport` field on `ModelEntry` (§10.3):
  - `cli_turn` — default; every existing thick row, unchanged.
  - `oauth_direct` — **Claude (off by default since 2026-09-28, see §19.15), Grok, Antigravity (`agy`), Cursor.** Rust adapters that read the CLIs' stored access tokens (§19.8). No daemon-side refresh in v1.
  - `cli_bridge` — **Claude's path.** A real `claude.exe` per request with Pi's tools exposed through a proposal-only MCP server (§19.7). This is the default (`claude_oauth.mode: off`, §19.15). Under `prefer` it is also the fallback when the direct path is blocked (§19.8.1).
- **Claude rows are bridge-only by default (2026-09-28, #137).** Anthropic bills a direct `sk-ant-oat` request as a third-party app (extra usage). The Feb 2026 policy names use outside Claude Code a Consumer ToS violation. `off` sends every Claude row through the bridge, runs no probe, and leaves a bridge fault on the bridge. `prefer` and `only` stay for measuring. Under `prefer` the direct path is best-effort and falls back to the bridge when Anthropic rejects it (§19.8.1, §19.15). `/ready` names the path.
- **Rust only.** No Node at runtime. The MCP proposal server is the `thickapi mcp-proposal` subcommand.
- **Pi collapses** to one provider `subs` (`api: openai-completions`). The `(thickapi)` thick lane and `extensions/pi/` retired at T5 (§12).
- **Enforce/deny do not apply** to thin requests: Pi owns tools. Thick turns use `PoolManager` (`cli_login`, `max_concurrent`). Thin Claude **does not** take a pool slot: the bridge is capped by `thin.vendors.claude.bridge.max_concurrent` (default 2); OAuth-direct vendors are capped by `thin.oauth_in_flight` (default 4). Waiting on the bridge semaphore longer than `queue_timeout` → `429 rate_limit_error`, `Retry-After: 5`, `x-should-retry: true`.

### 19.3 Request contract

Every field is honoured, ignored-with-header, or rejected. Nothing is silently misread.

| Field | Behaviour |
|-------|-----------|
| `model` | Required. `subs/<vendor>/<m>`, `<vendor>/<m>` or a `models.yaml` alias. Unknown → `404 model_not_found`. Row `enabled: false` → 404 |
| `messages[].role` | `system`, `developer` (→ system), `user`, `assistant`, `tool` |
| `messages[].content` | String or parts array. `text` always; `image_url` only when the row has `input: image`, else `400 invalid_request_error` with `param: messages[i].content`. On the Claude bridge a remote `http(s)` image URL is `400` (no fetch); only a `data:` URL is accepted. Assistant `content: null` + `tool_calls` accepted |
| `messages[]` (`tool`) | Keyed by client `tool_call_id`; tool name resolved from the preceding assistant `tool_calls` |
| `tools` | Passthrough. Function schemas → vendor tools (`oauth_direct`) or MCP catalog (`cli_bridge`) |
| `tool_choice` | Absent/`auto` honoured. `none`, `required`, and named `{type:"function", function:{name}}` are native on `oauth_direct` where supported. On `cli_bridge`: `none` spawns with an empty MCP catalog (init `tools: []`; history still renders); `required` and a named choice apply the §19.16 hold-all verdict; a named choice restricts the catalog to that one client tool, presented through the dialect when a row maps it. An unknown name, an unknown string, or a wrong type is `400` with `param: tool_choice` |
| `parallel_tool_calls` | Default `true`. `false` returns only the first `tool_use` (§19.7 capture), adds "Call at most one tool per message." to the prompt note, and counts the dropped calls in `/ready` `thin.vendors.claude.bridge.skipped.parallel` |
| `stream` | Boolean. `stream_options.include_usage: true` → usage chunk with `choices: []` |
| `reasoning_effort` / `reasoning.effort` | Both accepted; mapped per §19.5 |
| `max_tokens` / `max_completion_tokens` | Aliases. `cli_bridge` → `CLAUDE_CODE_MAX_OUTPUT_TOKENS` |
| `temperature`, `top_p`, `stop` | Native on `oauth_direct`. Ignored on `cli_bridge` with `x-thickapi-ignored`: `claude.exe` has no flag for them |
| `response_format` ≠ `text`, `n > 1`, `logprobs` | `400 invalid_request_error`, `param` set |
| `user`, `metadata`, `store`, `seed` | Ignored, no header |

### 19.4 Response contract

- **Streaming:** `object: chat.completion.chunk`; `delta.content`; `delta.reasoning_content` (Claude thinking, so Pi stores and replays it); `delta.tool_calls[]` with sequential `index`, `id` = `call_<24 alnum>`, `function.name`, `function.arguments` (single chunk per call); `finish_reason` `stop` | `tool_calls` | `length`; optional usage chunk; `data: [DONE]`. A stream that has been silent for `thin.keepalive_seconds` (15; `0` = off) carries an SSE comment (`: keepalive`) so a client or proxy read timeout does not kill a turn that is only thinking. Because the keep-alive holds those timeouts off, a started stream that has stalled is ended by the adapter's idle budget, or — when that budget is off (the serving vendor's `thin.vendors.<id>.budgets.idle_seconds: 0`; for Cursor, either after-output budget) — by the route using the default idle (180 s). Keep-alive comments do not reset the route idle. Cursor, on a request that offered tools, does not end that silence as `stop`: the turn ends on `f1.14 turn_ended` or the tool call, and `idle_after_text_with_tools_seconds` (300) is a `vendor_down` safety net (§19.8). **A failed stream ends with the error frame and no `[DONE]`:** `[DONE]` means the completion finished, and no `finish_reason` was sent. The frame is the same envelope an HTTP error carries, including `retry_after` and `should_retry`, which the headers can no longer say; Pi's client surfaces only the frame's `message`, so those two fields are also folded into that `message` (`retry_after=7s, should_retry=true`) and a vendor 401/403 is prefixed `vendor ` so Pi's stale-key rewrite does not fire. The frame's `message` ends with the code marker, as every thin error does (below). (Pi's `openai-completions` client raises on an `error` chunk and on a stream that ends without a `finish_reason`, so a truncated answer is never recorded as a finished turn.) An empty Claude stop while `tools` were offered is not this success; it is §19.16.
- **Tool-call ids** are `call_` + 24 alphanumerics. `call_tw` + 19 alphanumerics + a round digit + a 2-digit index is reserved for a TodoWrite round (§19.7): ordinary id generation never produces the `tw` prefix, and the route keeps a reserved id as it keeps any other route-shaped id.
- **Non-stream:** full `chat.completion` object. An adapter that closes without a `Finish` is `503 vendor_down` (`stream ended without finish`, as on the stream path), never a `stop` completion with zero usage (#121).
- **Usage:** `prompt_tokens`, `completion_tokens`, `total_tokens`, `prompt_tokens_details.cached_tokens`. On `cli_bridge` usage is built from `message_start` + `message_delta` — **never** from `result`, which does not arrive on tool turns. Without this Pi's context meter stays at 0 and compaction never fires.
- **Errors:** OpenAI envelope `{"error":{"message","type","param","code"}}` plus `vendor` and `hint`. `message` names the vendor, installed CLI version and the failed assertion (e.g. `claude 2.1.290: init.apiKeySource="oauth" expected "none"`); `hint` is the `thickapi doctor` command to run. **`x-request-id` (A2):** every thin response — success or error, HTTP or the mid-stream frame — carries `x-request-id: <request_id>` on the response headers; an error's body repeats it as `request_id` next to `vendor` / `hint`. The Pi extension ignores unknown fields. Context-overflow errors pass the vendor wording through verbatim (Pi's overflow regexes depend on it). Client-facing `message` redacts tokens, bearer headers, refresh tokens and emails only — not model ids, quota metric names, or overflow wording. `retry_after` (seconds) and `should_retry` mirror the `Retry-After` and `x-should-retry` headers in the body. **Code marker (#136):** Pi records an error's `message` and nothing else, so every thin error `message` — HTTP body and mid-stream frame, including the key auth errors — ends with ` [code=<code>]`, the envelope `code` (the table below: `vendor_down`, `rate_limit_error`, `insufficient_quota`, `vendor_drift`, `model_not_found`, `vendor_login_required`, `permission_error`, `authentication_error`, …). It comes after the folded retry hints and appears once. a pipeline can classify a failed seat from it; the Pi extension (§19.10) classifies on the envelope `code` or this marker, never on status digits in free text (a folded `retry_after=401s` is not a stale key), and keeps the marker at the end of the text it rewrites. The `code` field and `ThinError.message` inside the daemon stay bare. Every error from the thin surface — `/v1/chat/completions`, `/v1/models`, `/ready` (D3) and any unknown path — uses this envelope, including the unknown path itself (`404 route_not_found` — not the `401` it used to be, when auth wrapped every fallback), a wrong method on a thin route (`405 method_not_allowed`, `Allow` kept, behind the key) and a body over `thin.max_body_mib` (32; `0` = off; `413 request_too_large`). `/v1/agent/turn` keeps the thick error shape (§3.1) and axum's 2 MiB body limit. **No request waits for its turn to start for longer than `thin.start_deadline_seconds`** (300; `0` = off): past it the route answers `503 vendor_down` and drops the adapter, whatever the adapter's own budgets are. The deadline covers only the wait for `Started`. Config load refuses a `start_deadline_seconds` below any vendor's `thin.vendors.<id>.budgets.first_event_seconds`, Cursor's `budgets.first_output_seconds` or the bridge's `mcp_ready_timeout_ms` (`0` on either side is off and not compared): a first-event budget the route would pre-empt is a contradiction, not a setting (#129). After `Started`, stream or not, silence is bounded by the adapter's own idle budget (`thin.vendors.<id>.budgets.idle_seconds`, 180, on `oauth_direct` — on Grok a request that offers tools raises the **idle** budget, not the first-event one, to at least `thin.vendors.grok.budgets.tool_call_floor_seconds` (900), because xAI delivers a tool call whole, §19.8 `grok.rs`; `thin.vendors.claude.bridge.idle_seconds` and `.timeout_seconds` on `cli_bridge`). Before C5 these were the flat `thin.oauth_idle_seconds`, `thin.grok_tool_call_seconds` and `thin.bridge_*`, still accepted with a warning (§10.1). Every non-streaming vendor exchange — the Claude probe, `list_models`, agy's `loadCodeAssist`, and the body read of a rejected response — is bounded by `thin.reply_timeout_seconds` (30; `0` = off): past it the call is `503 vendor_down … timed out`, and a stalled error body reads as empty, so the status alone classifies it (#125). `budgets.idle_seconds: 0` turns the adapter budget off; the route still ends a started turn that has been silent for the default idle (180 s), because keep-alive would otherwise hold the client forever.

| HTTP | `type` / `code` | When |
|------|-----------------|------|
| 400 | `invalid_request_error` | §19.3 rejections, malformed body |
| 401 | `authentication_error` | Missing or unknown `ta_` key |
| 401 | `vendor_auth` / `vendor_login_required` | Vendor token absent or past `expires_at`; `hint: run grok login` |
| 403 | `permission_error` | Key present but lacking the route's scope (`chat:write` on `/v1/chat/completions`; `models:read` on `/ready` / `/v1/models`, satisfied by `chat:write`, D3); or a vendor 403 on an expired token |
| 404 | `not_found` / `model_not_found` | Unknown or disabled model |
| 404 | `invalid_request_error` / `route_not_found` | No such path |
| 405 | `invalid_request_error` / `method_not_allowed` | Wrong method on a thin route |
| 413 | `invalid_request_error` / `request_too_large` | Body over `thin.max_body_mib` |
| 429 | `rate_limit_error` | Vendor quota or pool refusal (see below) |
| 402 | `insufficient_quota` | `subscription_blocked`; vendor 402. **Never a 403** |
| 502 | `vendor_drift` | Init-record or protocol assertion failed; body names it. A vendor 403 on an unexpired token (the vendor refuses our request shape; message names the headers sent) |
| 502 | `server_error` / `empty_completion` | Claude request offered `tools` and the assistant stop had no text, reasoning, or tool call (§19.16). Not wire drift |
| 503 | `vendor_down` | Vendor unreachable, `mcp-proposal` missing, spawn failed |
| 500 | `server_error` | Everything else |

Vendor HTTP statuses map through one table, `vendor::status_error` (audit R8): 400 → `invalid_request_error`, 401 → `vendor_login_required`, 402 → `insufficient_quota`, 403 → `vendor_drift` on an unexpired token else `permission_error`, 404 → `model_not_found`, 429 → `limit_error` (below), 5xx → `vendor_down`, other → `server_error`. A vendor matches its own statuses first and falls through for the rest: Claude's 400/401 block classification (§19.8.1), agy's Google `UNSUPPORTED_CLIENT` (and its bare 403 stays permission — agy passes no token state), Cursor's 401 wording, 464 and unknown-status drift.

- **429 semantics:** `Retry-After` in seconds always (the header's integer or HTTP-date form — a date already past names no wait — else a body hint such as `google.rpc.RetryInfo`, else 60). "Slow down" and "you are out" share a status and often a vocabulary, so the wait decides first (`vendor::limit_error`, one policy for every OAuth vendor): wording about a rate (per minute/second, rate limit, too many requests) is a throttle; wording about the plan (billing, payment, credit, upgrade, plan/usage limit, monthly) is exhaustion even beside a short wait or a "try again at 4pm"; a bare "try again" is a throttle; otherwise a vendor-named wait of an hour or less is a throttle and a longer one is exhaustion, and with no wait named `quota`/`exhausted` tip it to exhaustion (Google uses both words for per-minute throttles, so they decide nothing beside a wait). A throttle is `429`, `x-should-retry: true`, the vendor's own `Retry-After`, no `quota exceeded` prefix. Exhaustion the text does not name as quota/usage/payment stays a `429` with `x-should-retry: false` and the `quota exceeded` prefix. Exhaustion whose vendor text names quota/usage/payment becomes `402 insufficient_quota` with message beginning `quota exceeded` (Pi's non-retryable pattern) and `x-should-retry: false`. Pool / bridge-semaphore refusal uses a short `Retry-After` and `x-should-retry: true`. Advisory utilisation from Claude's `rate_limit_event` is exposed as `x-thickapi-rate-limit: <0..1>;reset=<iso8601>` on successful responses.
- **CORS:** `cors_origins: []` in `router.yaml`, independent of `lan`. `OPTIONS` answered on thin routes.

### 19.5 Reasoning map

Source of truth: `crates/thickapi-core/src/thinking.rs` (`Effort`, `reasoning_ui`, `advertise`, `thick_apply`, `thin_apply`). Yaml `thinking` is a human hint. Pi maps come from `advertise`, never from `ModelEntry.thinking`, and `sync-pi` does not force `reasoning: true`.

`normalise_effort`: `none`|`off` → `Off`; `minimal`|`low`|`medium`|`high`|`xhigh`|`max` kept; unknown → 400. Accept `none` as an alias; Pi maps write `"off"`, never `"none"`. Thick `TurnSpec.thinking: Effort` (omitted → `Medium`; no Absent on the thick wire). Thin `ThinRequest.effort: Option<Effort>` (`None` = Absent = vendor default; never coerced to Medium).

`reasoning_ui`: Claude/Grok/Agy true; Cursor iff `vendor_model` (else `cli_model`) has suffix `-low|-medium|-high|-xhigh|-max|-minimal|-none` (same list as Cursor `wire_model`); Pi-child (`or/*`) yaml `thinking` only. `defaultOptions.thinking` is `"medium"` only when `reasoning_ui`. Auto: `reasoning: false`, no map, no `defaultOptions.thinking`.

**Advertise** (Pi `thinkingLevelMap`; keys always when `reasoning_ui`; value = string Pi sends as `reasoning_effort`, or `null` = do not send / hide the rung):

| Pi key | Claude (and Pi-child if yaml thinking) | Grok | Agy | Cursor suffixed | Codex |
|---|---|---|---|---|---|
| off | `"off"` | `"off"` | `"off"`; `null` on a `claude-*` wire id | `null` | `"off"` on Luna (`*-luna`), else `null` |
| minimal | `"minimal"` | `"minimal"` | `"minimal"`; `null` on a `claude-*` wire id | `null` | `null` |
| low / medium / high | same | same | same | same | same |
| xhigh | `"xhigh"` | `"xhigh"` | `"high"` | `"xhigh"` | `"xhigh"` |
| max | `"max"` | `null` | `"high"` | `null` | `"max"` |

Grok Off is **not** vendor-off: Pi may send `"off"`; thin/thick still floor at `minimal`. Do not hide Grok Off; do not claim it disables thinking.

**Thick apply** (omitted field → Medium). `child_env` strips inherited `MAX_THINKING_TOKENS`; Claude Off then pins it. Agy `--model` is `cli_model` after stripping a trailing `-low|-medium|-high`. Cursor / Pi-child ignore `spec.thinking`.

| Effort | Claude | Grok `--reasoning-effort` | Agy `--effort` | Cursor / Pi-child |
|---|---|---|---|---|
| Off, Minimal | no `--effort`; `MAX_THINKING_TOKENS=0` | `minimal` (never `none`) | omit `--effort` (CLI has no measured disable; thin `thinkingBudget: 0` is the real off) | ignore |
| Low, Medium, High | `--effort <level>` | same name | `--effort <level>` | ignore |
| XHigh, Max | `--effort high` | `xhigh` | `--effort high` | ignore |

**Thin apply** (`None` = Absent = vendor default). Claude OAuth Off/Minimal → `thinking: disabled`; Low/Medium/High/XHigh/Max → `budget_tokens` from `claude_oauth.budgets` (4k / 16k / 32k / 64k). Grok Off/Minimal → body `"minimal"` (`none` → 400). Agy Off/Minimal → `{thinkingBudget: 0}`; Low/Medium/High native; XHigh/Max → `high`. Claude behind Antigravity (`claude-*` wire id) cannot turn thinking off (`thinkingBudget: 0`, `includeThoughts: false` and `thinkingLevel: minimal` all still return thought parts; `none` is `400`; measured 2026-10-25), so Off/Minimal there → its lowest level, `low`. Cursor Off/Minimal do not override a suffix; no-suffix ids `Ignore` all levels. Claude bridge Off/Minimal: `CLAUDE_CODE_DISABLE_THINKING=1`, no `--effort` (`MAX_THINKING_TOKENS=0` no longer turns thinking off on Claude Code 2.1.308: Opus 5.5 still thinks under it); Low/Medium/High/XHigh/Max → `--effort <same name>` (Claude Code 2.1.308 takes `xhigh` and `max`, and Fable 5.1, Opus 5.5 and Sonnet 5.5 answer at both, measured 2026-10-25; the frozen thick column still folds them to `high`); the access line's `wire_params` is `effort=<level>` or `thinking=disabled`. With thinking on, the bridge passes `--thinking-display summarized` (hidden from `--help`): Claude Code 2.1.308 otherwise streams thinking blocks with empty text in `-p` mode, so Pi saw no reasoning at any level, and the `showThinkingSummaries` setting is not honoured under `--setting-sources ""` (measured 2026-10-25). Codex Absent → no `reasoning` (the server's `medium`); Off → `reasoning.effort: none` where the model takes it (`gpt-6-luna`), else `low`; Minimal → `low`; Low/Medium/High/XHigh/Max native (`ultra`, listed by `/codex/models`, is `400` on the wire; `none` is `400` on Sol and Astra).

Guard (Claude direct): last assistant message has `tool_calls` and no recoverable signature while `thin_apply` would enable thinking → send `thinking: disabled` and header `x-thickapi-ignored: thinking`. Same for a bridge→direct flip mid-conversation.

Claude OAuth streams each thinking block as `delta.reasoning_content` and, on `signature_delta`, `delta.reasoning_details: [{type: "reasoning.text", text, signature}]`. `redacted_thinking` → `{type: "reasoning.encrypted", data}`. Replay `reasoning_details[]` (and legacy `reasoning_signature`) as Anthropic `thinking` / `redacted_thinking` blocks in order before `text` / `tool_use`.

### 19.6 Catalogue and Pi ids

- `ModelEntry` gains `transport`, `vendor_model` (wire id), `enabled: bool` (default true), `input: [text|image]`. `enabled: false` removes the row from `/v1/models` and `sync-pi` — the per-vendor kill switch for a moved Cursor version floor or rotated Grok issuer.
- Pi provider id is `subs`. `sync-pi` writes ids **without** the `subs/` prefix (`claude/fable`, `grok/grok-4.7`) so `pi --model subs/claude/fable` resolves and the picker does not show `subs/subs/...`. The daemon accepts both forms.
- `GET /v1/models` returns `{object:"list", data:[{id, object:"model", created, owned_by}]}` by default (SDK auto-discovery); `?format=pi` returns the Pi-shaped payload. `chat:write` satisfies `models:read`.
- `input: ["text"]` unless the adapter implements images; Pi then refuses pastes up front instead of a vendor 400. The three Claude rows (`subs/claude/fable`, `opus`, `sonnet`) declare `input: [text, image]` (K7, #225): a user `data:` image and a Pi `read` image returned as a tool-result part both reach the bridge as `image_attachment` files. A remote `http(s)` URL is still `400`.

### 19.7 Claude bridge (`crates/thickapi-core/src/thin/claude_bridge/`)

The Claude **fallback** path (§19.8.1 decides when). A port of `pi-claude-code-provider`. `mod.rs` is stamped with the exact upstream version and commit ported, so a breakage morning starts with `git diff` of upstream `claude-args.ts` / `claude-protocol.ts`, not with reverse-engineering `claude.exe`.

**Argv** — all thirteen reference flags, verbatim, in `spawn.rs`:
`-p --setting-sources "" --settings {"disableAllHooks":true,"autoMemoryEnabled":false,"totalTokensReminder":"off"} --disable-slash-commands --strict-mcp-config --permission-mode dontAsk --no-chrome --no-session-persistence --prompt-suggestions false --mcp-config <json> --tools "" --model <m> --effort <e> --input-format stream-json --output-format stream-json --include-partial-messages --verbose --system-prompt-file <path>`.
Binary via `resolve_cli` (`claude.exe`, never the `.cmd` shim). Child `cwd` = the request's private directory. `--mcp-config` uses the absolute `current_exe()` path.

**Environment** — an **allowlist**, not `child_env`'s denylist: `USERPROFILE HOMEDRIVE HOMEPATH PATH PATHEXT SystemRoot ComSpec TEMP TMP LANG LC_* HTTP_PROXY HTTPS_PROXY NO_PROXY CLAUDE_CONFIG_DIR`, plus pins `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`, `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, and `MAX_THINKING_TOKENS=0` when effort is off. Anything inherited (`ANTHROPIC_*`, other API keys on this host) flips `apiKeySource` — init fails — or silently bills the API.

**Transcript** (`transcript.rs`) — append-stable records: system prompt, then messages, then tool results keyed by client `tool_call_id`, then the growing image-attachment list. A `data:` image in a user message or inside a `tool` message is written once as `image-<sha>.<ext>` and referenced as an `image_attachment` block (the tool-result image is inside the `toolResult` record, the same way a user image is); a remote `http(s)` URL is `400` (no fetch). Literal `@` JSON-escaped (Claude expands `@path`). Tool-result `isError` is always `false` on this wire; document it.

**Catalog** (`catalog.rs`) — OpenAI `tools` → MCP catalog file; safe names kept, others get deterministic request-local aliases.

**Init validation** (`protocol.rs`) — port of `validateClaudeInitialization`: `type system/init`; `tools` equals the expected set; `permissionMode == dontAsk`; `slash_commands`, `skills`, `plugins` all empty; `apiKeySource == "none"`; `mcp_servers == [{name:"pi", status:"connected"}]`; no `mcp_server_errors`; `model` non-empty. Any miss → `502 vendor_drift` naming the assertion. `router.yaml` `thin.vendors.claude.bridge.tolerate: { extra_tools: [], nonempty_inventories: [] }` warn-and-continues for the tool-set and customisation assertions only; `apiKeySource`, `permissionMode` and `mcp_servers` stay hard.

**Lifecycle.** `thin.vendors.claude.bridge.max_concurrent` (default 2) is a semaphore around the bridge spawn. An abort-on-drop guard owns the runner `JoinHandle`; adapters `select!` on `tx.closed()` at MCP-ready, `http.execute()`, and Cursor first-byte/heartbeat. Every terminate path awaits `timeout(drain, child.wait())` (`thin.vendors.claude.bridge.handoff_drain_seconds`, 10 s; the `HANDOFF_DRAIN` const before C5) before `PrivateDir` drops. A bridge run is four phases composed by `run_inner`: `spawn_phase` (argv, env, transcript, catalog, the child in its job object), `wait_ready` (the proposal server's ready marker, bounded by `mcp_ready_timeout`), `protocol_loop` (stream-json lines to events; the tool handoff kill and drain) and `publish` (tool calls or finish, after the violation marker and the private-path guard). The child lives in a `ChildGuard` from spawn to reap: every early exit calls `ChildGuard::finish` (terminate, then the bounded wait) before returning, and the private dir is declared before the guard, so it cannot drop under a live child; the guard's `Drop` (a cancelled future) terminates through the job object without blocking. Failures are typed (`BridgeFault`); the client message is the variant's `Display` after the `claude <version>: ` label, so the text is what it was. The route writes `x-thickapi-path` / `x-thickapi-fallback` once the path is final — including on pre-`Started` errors.

**Capture** — stream text and thinking deltas live. Terminate **only** on `message_delta.stop_reason == "tool_use"` with every content block closed; then emit **every** `tool_use` block as `tool_calls[i]` and `finish_reason: tool_calls`. A block of any kind still open at that handoff fails the request (`502 vendor_drift`, `protocol_blocks`). At `message_stop` or `result` the rule splits (#145): a `text` or `thinking` block the CLI never closed is closed there — its deltas were already streamed, and a thinking block's signature is published as a real `content_block_stop` would — counted in `/ready` `thin.vendors.claude.bridge.skipped.blocks`, logged at `warn` with `request_id`, the CLI version and the closed indices and kinds, and the turn finishes as a clean stop does; an open `tool_use` block, whose arguments may be incomplete, is still `502 vendor_drift` (`protocol_blocks`). `replay` applies the same rule: it feeds the same `Mapper` (`Mapper::accept_line`) and calls the same `publish`, skipping only the checks that need a live child. `parallel_tool_calls: false` → first block only. Kill with `TerminateJobObject(job, KNOWN_EXIT)` (or `taskkill /PID /T /F` as `process.rs` does); accept only that exit status plus at most one `result{is_error, subtype: error_during_execution, terminal_reason: aborted_streaming}` record as a handoff. Verify the violation marker is absent and no tool argument contains a private path. Only then publish.

**`thickapi mcp-proposal --catalog --ready --violation`** — tracing to **stderr** (a log line on stdout corrupts JSON-RPC and fails every request); no config load. `protocolVersion: "2025-11-25"`, `capabilities: {tools: {}}`; id-less notifications (`notifications/initialized`) ignored; ready marker written on `tools/list`, not `initialize`; `tools/call` writes the violation marker and returns an error; CRLF-tolerant, 1 MiB line cap. The three paths are canonicalised, must sit inside the server's own parent directory, must not traverse a reparse point, and markers open with `create_new`.

**Cache** — spawn-per-request replays history, so caching depends on the append-stable prefix plus the `totalTokensReminder` pin; measure with upstream's `DEVELOPING.md` procedure before changing either. Claude Code prepends its own identity line and injects a `<system-reminder>` that no flag removes; document, do not fight.

**Dialect** (`thin/dialect.rs`, v0.15). `claude.exe` is trained on Claude Code's tool names, so the bridge shows Pi's tools under those names and translates calls back. `claude.exe` never executes a tool: the proposal server only lists them, and a `tools/call` is still the violation marker. `router.yaml` `thin.vendors.claude.bridge.dialect` is `claude_code` (the default) or `off`. `off` is today's pass-through, byte for byte.

One declarative table. A row is active for a request only when its Pi tool is in `tools`, every `requires` path is in that tool's schema, and no Pi tool already uses the Claude name. An inactive row is pass-through, with one `info` line per process per reason (`dialect_skip=<tool>:<reason>`). An active row replaces its Pi tool in the catalog: Claude sees `mcp__pi__Edit`, not `mcp__pi__edit` as well. The prompt note names the active mappings; `parallel_tool_calls: false` adds one line to it, "Call at most one tool per message." (§19.3).

| Claude tool | Pi tool | Exposed args | Translation |
|---|---|---|---|
| `Read` | `read` | `file_path`, `offset`, `limit` | `file_path` → `path`. Both offsets are 1-based. No `pages`. The description says output has no `cat -n` prefixes. |
| `Write` | `write` | `file_path`, `content` | `file_path` → `path`. Pi creates parent directories. |
| `Edit` | `edit` | `file_path`, `old_string`, `new_string` | One `edits` block. No `replace_all`. A history `edit` with two blocks has no Claude form. |
| `Bash` | `bash` | `command`, `timeout` | `timeout` milliseconds → seconds, rounded up. `description` is dropped. No `run_in_background`. |
| `Glob` | `find` | `pattern`, `path` | A pattern with no `/` matches the basename; one with `/` is auto-prefixed with `**/`. Results are not mtime-sorted. |
| `Grep` | `grep` | `pattern`, `path`, `glob`, `-i`, `-C`, `head_limit` | `-i` → `ignoreCase`, `-C` → `context`, `head_limit` → `limit`. Output is `path:line: text`. |
| `Agent` | `subagent` | `description`, `prompt`, `subagent_type`, `model`, `run_in_background` | Pass-through, except `model`: `sonnet` / `opus` / `fable` / `haiku` → `subs/claude/<x>` when that id is in the catalogue the daemon loaded, else the key is dropped. Any other value is dropped. No `isolation`. The description says the agent runs as a Pi subagent. Reverse is exact for a call this row produced. `get_subagent_result` and `steer_subagent` stay pass-through. |
| `AskUserQuestion` | `askuserquestion` | `questions` (max 4: `question`, `header`, `multiSelect`, `options[{label, description}]`) | Identity on `questions`, capped at 4. No `annotations`, `answers` or `metadata`. `multiSelect` is accepted; the description says the UI is single-select. The result passes through. |
| `TodoWrite` | `todo` | `todos[{content, status, activeForm}]` | Replaces the whole list (§19.7 **TodoWrite rounds**). Active only when Pi's `todo` schema has `action`, `todos[]` (`subject`, `content`, `activeForm`, `metadata`), `id` and `status`. A raw Pi `todo` call has no Claude form. |

Three hooks. **Catalog:** `transport_to_client` resolves a dialect transport name to `{Client(name), Dialect(row)}`, and `expected_tools()` follows the transport names. **Publish:** a dialect `tool_use` runs `to_pi`, and Pi receives its own name and args. `contains_private_path` runs on the translated args too; a hit is the same private-path fault as an untranslated one (`500`, `server_error`, the same message) and is not retried. A `to_pi` failure publishes nothing from that assistant message and re-spawns once inside the same request (`run_bridge_attempt`, private dir `<id>-r<n>`). The retry's transcript appends that message — its text and every tool call, in Claude form — and one `isError` tool result per call: the bad call names the missing or wrong args, and any other call in the message says it was not run. Text the client already received stays sent; the retry does not resend it. A second `to_pi` failure in the same request is `502`, `type: server_error`, `code: dialect_args`, `should_retry: false`, the message naming the tool and the reason. Both the retried and the fatal case log `dialect_error=<Tool>:<reason>` on the access line. **Transcript:** a Pi-shaped history call renders through `to_claude`. `None` (a two-block `edit`, an unmapped tool) renders as a historical `toolCall` under `mcp__pi__<pi name>`, which `claude.exe` accepts even when that name is not in the catalog. The opaque `[unavailable Pi tool <digest>]` stays only for an unsafe name. `toolResult.toolName` matches the rendered call. Rendering is deterministic.

The bridge appends one generated paragraph to `system-prompt.txt`, naming only the active mappings (`edit` is `mcp__pi__Edit`, …). It is append-only, so the cached prefix is unchanged.

**TodoWrite rounds.** Pi runs one assistant message's tool calls concurrently, and a `create` id is known only from its result, so one `TodoWrite` is up to two rounds. Each round's calls are independent of each other. The daemon keeps no state: round 1's arguments carry the list and the target statuses, and round 1's results carry the ids.

- **Round 1**, published with Claude's turn: `todo {action: list}`, and `todo {action: create, todos:[{subject, activeForm, metadata:{tw, tw_i, tw_status}}]}` when `todos` is not empty. An empty `todos` is the list call only. The ids are `call_tw` + a 19-char base taken from Claude's tool-use id + the round digit + a 2-digit index (§19.4), so the same proposal always publishes the same ids.
- **Round 2**, answered by the daemon, not by `claude.exe`: `delete` for every id the list result names that round 1 did not create, then `update {id, status}` for every created item whose `tw_status` is not `pending`. List ids are read from `formatListLine` (`[status] #<id> <subject>`), anchored at the start of the line, so a subject that itself contains `#12` is not an id. When round 2 would be empty, there is no round 2 and the fold happens at once.
- **Short-circuit.** When the trailing tool results contain a complete round 1 whose round 2 is not empty, `run_bridge_attempt` does not spawn `claude.exe`. It emits `Started` (`path: bridge`), then round 2's `todo` calls, then `finish_reason: tool_calls`, with zero usage. It takes no bridge semaphore slot and skips the empty-stop retry. The same history yields the same calls and the same ids. A round 2 goes out only when `todo` is in the request's `tools`. A mixed batch counts: the other results wait in the history and fold into the next spawn.
- **Fold.** The transcript renders round 1 and round 2 as one `TodoWrite` `toolCall`, arguments rebuilt from the create metadata in `tw_i` order, and one `toolResult`. The success text is Claude Code's own: `Todos have been modified successfully. Ensure that you continue to use the todo list to track your progress. Please proceed with the current tasks if applicable`. `isError` stays false on this wire. An error result in either round becomes that text, prefixed `Error:` when it does not already say so. A raw Pi `todo` call (a Grok turn, an id that is not a round id) is not folded.

### 19.8 OAuth-direct adapters (`crates/thickapi-core/src/thin/oauth/`)

- **Credentials are read-only.** `creds.rs` reads the access token and expiry from `~/.claude/.credentials.json` (`claudeAiOauth.accessToken` / `expiresAt`), `~/.grok/auth.json`, the `gemini:antigravity` Windows Credential Manager entry (Antigravity; `~/.gemini/oauth_creds.json` is gemini-cli's and goes stale — measured T3), and cursor-agent's `auth.json` (`%APPDATA%\Cursor\auth.json` on Windows, `~/.cursor/auth.json` on macOS, `$XDG_CONFIG_HOME/cursor/auth.json` on Linux, `CURSOR_CONFIG_DIR` overrides; `{accessToken, refreshToken}` session JWTs whose `exp` is the expiry — measured T4). Past expiry → `401 vendor_login_required` — except Claude, where an expired token routes to the bridge, whose `claude.exe` run refreshes `.credentials.json` as a side effect. **No `~/.thickapi/tokens/` cache and no daemon-side refresh:** Grok and Cursor rotate refresh tokens and the CLI serialises writes with its own lock; a daemon refresh forks the chain and logs the CLI out. If refresh is ever added, it takes the CLI's lock and writes back in the CLI's schema.

#### 19.8.1 `claude_oauth.rs` — direct Anthropic Messages API, best-effort

- **Request:** `POST https://api.anthropic.com/v1/messages`, `Authorization: Bearer <sk-ant-oat…>`, `anthropic-version: 2023-06-01`, `anthropic-beta: claude-code-20250219,oauth-2025-04-20` (plus `interleaved-thinking` when thinking is on), `user-agent: claude-cli/<installed claude --version> (external, cli)`, `x-app: cli`. The `system` array **starts with** the exact Claude Code identity block Anthropic validates for non-Haiku models; Pi's system prompt follows as the next block with `cache_control`. Tool names are passed as-is; if a request is rejected for tool naming, retry once with the `mcp__pi__` prefix the CLI uses.
- **Model ids:** `vendor_model` from `models.yaml` (`claude-fable-5-1`, …). Thinking per §19.5; thinking blocks stream as `delta.reasoning_content` **and** `delta.reasoning_details` (§19.5).
- **Native everything:** real streaming, native `tool_use` → `tool_calls[]` (parallel calls preserved), exact `usage` incl. `cache_read_input_tokens` → `cached_tokens`, `stop_reason` → `finish_reason`. No process spawn.
- **Block detection → fallback.** Ordered rule table (first match; corpus in `claude_oauth.rs`). **Haiku never `Blocked` / `SuspectedBlock`**: a Haiku row may stay on the direct path while non-Haiku is bridged, and a Haiku 200 does not clear the non-Haiku block (only the probe or a non-Haiku 200 reopens the path; #122). Named ToS wordings (`only authorized for use with Claude Code`; `Third-party apps` + `extra usage`, Sep 2026) → `Blocked` at any HTTP status. Request-validation patterns (`messages.`, `max_tokens`, `tools.`, `thinking.`, `system.`, `tool_use_id`, `model:`, `invalid model`, `unknown model`, `prompt is too long`) stay `400 invalid_request_error` — a bare `model` substring is not enough, so a reworded block that mentions "model" still falls through. Other non-Haiku 400 (including bare `Error` after the identity block is present) is a *suspected block*: replay through the bridge once; two consecutive suspected blocks → `mark_blocked`. 401 `authentication_error` on a token that is not expired → `Blocked`. A 429 is a rate-limit (`limit_error`) unless a named ToS wording corroborates it; a first-request 429 with `successes == 0` does **not** persist a 6 h block on its own. On a confirmed block the daemon (a) marks `claude_oauth.state = blocked` with the reason and timestamp, (b) **replays the same request through the bridge** so the client sees one answer, (c) keeps routing non-Haiku Claude rows to the bridge until the cooldown (`claude_oauth.retry_after_hours`, default 6) passes, then the R2 probe runs. The rule table is `ClaudeDirectRequest`'s `classify` on the shared runner; a verdict rides on the `ThinError` it returns as a `DirectBlock` tag (`Blocked` / `Suspected`), which `OauthFailure::from` reads back so `ClaudeAdapter` still decides the bridge replay, the `mark_blocked` cooldown and the suspected-block count. The probe sends through the same runner as a request (same `in_flight` permit, `reply_timeout_seconds` bound, `classify` and rejection record) into a sink that stops at `Started`.
- **Not attempted:** the signed `x-anthropic-billing-header` some proxies replay from captured CLI traffic. Its signing scheme is undocumented; if Anthropic requires it, this adapter cannot produce it and that is the moment we fall back. Do not add capture-and-replay here.
- **Config (`router.yaml`):** `claude_oauth.mode: off | prefer | only` (default `off` since 2026-09-28, #137: bridge only, no probe, no cross to the direct path. `prefer` tries direct and falls back. `only` fails instead of falling back. Both are for measuring), `claude_oauth.retry_after_hours`, `claude_oauth.budgets`. The `prefer` behaviour is §19.15.
- **`/ready` and `doctor`:** `claude_oauth: ok | blocked(<reason>, since, next probe) | off`, the token's expiry, persisted file, last probe, and which path non-Haiku rows take today (`thin.vendors.claude.path` / `path_reason`) plus the Haiku row (`haiku_path` / `haiku_path_reason`: Haiku may stay oauth while non-Haiku is bridged). `doctor` prints both; a bridge path on one row is not a doctor failure. `/ready` `free_bytes` is the real free space on the `data_dir` volume. CLI presence follows enabled model rows and account pools (`ClaudeCode`→`claude`, `Grok`→`grok`, `Agy`→`agy`, `Cursor`→`cursor-agent`); a Cursor-only config does not fail because `agy` is missing. A thin vendor that is rate-limited does not by itself fail `/ready` (`/ready.thin` stays). **Availability summary (#110):** every `thin.vendors.<vendor>` also carries `available` (bool), `retry_in_seconds` (int or null), `reason` (string or null) and `rows_unavailable` ([row id]) — one yes/no a client reads before it spawns a seat. `available: false` when any of these holds: the vendor's last rejection is newer than its last success and still inside its window (the vendor's own `Retry-After`, else 60 s for `rate_limit_error`, 15 min for `insufficient_quota` / `subscription_blocked` / `vendor_login_required`, 120 s for `vendor_down` / `vendor_drift` / `overloaded_error`; other codes are the client's, not the vendor's); an account-health block in force on an account of that backend (the subscription is shared by both lanes); an OAuth-direct vendor with no stored credential; for Claude, bridge drift while the direct path is not the planned one (the double fault, or `mode: off`). Claude ignores its direct-path `blocked` / `suspected_block` / 401 (the bridge serves). `retry_in_seconds` is the longest known wait, null when a cause has no known end (a login, drift, a missing credential). `rows_unavailable` lists rows the vendor answered `not_found` for in the last hour (Cursor), so a client skips the row and keeps the vendor. `doctor` prints the same four fields per vendor. **CLI versions:** `cli_version` in `/ready` is the last known `--version` and never waits on the CLI; each `<cli> --version` is probed in the background at start (10 s bound, stdin closed), and again when the exe's mtime changes (an auto-update) or a minute after the CLI was missing. Request headers that need the version (`x-grok-client-version`, Cursor `cli-<ver>`, the Claude direct `user-agent`) follow the same cache (#124). `doctor` performs one 1-token live request per OAuth vendor (grok, agy, cursor) plus the Claude handshake; `--offline` skips the live pings and the models live check. `doctor` CLI resolution uses the same rows-driven set.
- **Risk stated once:** using this token outside Claude Code is a Consumer ToS violation per Anthropic's Feb 2026 policy and carries account-action risk; `doctor` prints that line whenever `claude_oauth` is not `off`.
- **Shared driver (`oauth/vendor.rs`).** A vendor file holds `VendorRequest`: its request builder, its wire→`ThinEvent` decoder, its budgets, its status overrides and its rate-limit advisory. All four vendors run through `VendorSession`; Cursor's duplex reply and per-row rejection, and Claude's block classification and direct-path state, are trait hooks (`duplex`, `classify`, `on_outcome`, each a no-op or the shared table by default; `on_outcome` sees the vendor's own decoder, not only its core). `VendorSession::run` owns the request lifecycle once: fresh token (or one the caller already read and routed on: Claude's) → `thin.oauth_in_flight` permit (or one the caller holds across runs: Claude's §19.16 retry and probe, Cursor's held reasoning-only retry) → build (agy's project discovery runs here, under the permit) → capture tap → send, abandoned when the client leaves → on a non-2xx, the body read under `thin.reply_timeout_seconds`, `classify` (default: the shared status table and `limit_error`) and the rejection recorded with its wait → otherwise the advisory, the decoder and `drive` → the outcome recorded once, after `drive`. The permit is held to the end of the stream. `VendorSession::fetch_json` is the one non-streaming exchange (grok `list_models`, agy `loadCodeAssist`, Claude `GET /v1/models`): bounded end to end, body included, by `reply_timeout_seconds`; a body that fails to read is `503 vendor_down` (`… body read failed`), never an empty list. `usage_from` and `rate_limit_advisory` exist once, keyed per vendor. The response loop (`drive`), SSE framing with an 8 MiB frame cap (`Sse`), `Started`/`pending` bookkeeping (`MapperCore`), CLI token refresh (`CliToken`: single-flight, waiters re-read, an expiry-less token is never refreshed, an attempted refresh is not repeated for 60 s, a token inside the window but unexpired is still used) and the 429 policy exist once. **Stream budgets** are quiet-*event* time, not quiet-byte time: `thin.vendors.<id>.budgets.first_event_seconds` (120) before the first event, `budgets.idle_seconds` (180) between events — SSE comments, pings and Connect heartbeats do not reset them, a tool call's arguments or a thinking signature streaming in pieces do (the answer is growing, though no event is ready), a wire that delivers a tool call whole gets a per-vendor floor instead (Grok: `thin.vendors.grok.budgets.tool_call_floor_seconds`), Cursor ends a text answer on `f1.14 turn_ended` and treats `thin.vendors.cursor.budgets.idle_after_text_with_tools_seconds` (300) as a safety net that fails `vendor_down` rather than `stop` (§19.8), time spent waiting on a slow client does not count against them, and `0` turns one off (the route still bounds a started turn with the default idle, §19.4). Before `Started` a spent budget is `503 vendor_down`; after it, a `ThinEvent::Error`. A broken transport is an error, never a `Finish`, unless the wire had already marked the answer complete. **A body that closes cleanly is the same (#121):** a turn finishes only on its wire's end-of-answer marker — Grok `finish_reason` (or `[DONE]`), agy `finishReason`, Claude `stop_reason` (or `message_stop`); a body that closes after content without one is a truncated answer, `vendor_down` (`<vendor> closed the stream before the answer ended`), never `finish_reason: stop`. The rule is one function in `vendor.rs`, shared by `drive` and fixture replay. **Outcome (#126):** `/ready`'s `successes` / `last_status` / `last_rejection` record how the stream ended — `mark_ok` on a `Finish`, `mark_rejection` on a failure (an in-stream 429, overload or drift included) — never the response headers; a turn the client abandoned records nothing. On Claude direct a finish counts only once the §19.16 guard keeps the attempt: an attempt it drops as an empty tool stop, or a final `empty_completion`, is not a success. All four vendors agree (Cursor records at its own finish). A client that disconnects ends the turn and closes the upstream. Ported: all four — grok, agy, Cursor (duplex: its `request_context` result is written back through the driver, once per ask — Cursor may repeat `f2.f10` and every ask gets a result, because silence is `internal: No exec result`; a result Cursor does not read within 15 s ends the turn (`cursor stopped reading the request stream`) rather than parking it; its budget is its own three-way rule from `thin.vendors.cursor.budgets` `first_output_seconds` (90), `idle_seconds` (4) and `idle_after_text_with_tools_seconds` (300) (`0` = off), which the decoder applies (`VendorRequest::budgets` carries only two fields): `f1.14 turn_ended` finishes a text answer (`stop`, or `tool_calls` if a call arrived) and a spent after-text budget with tools offered is `vendor_down` (`cursor went quiet mid-answer`), not `stop`) and Claude direct (whose `Started` names the model `message_start` served and carries the R4 fallback tag; a body that ends before `message_start` is `502 vendor_drift`). `CliBacked` (R22 C1) composes `CliToken` and `CliVersion` into the one struct that owns the CLI token and version state for grok, agy and Cursor; an adapter holds one and never reads `creds.rs` or spawns `--version` itself.
- `grok.rs` — **measured 2026-09-17 (`grok 1.0.30`):** xAI's `POST https://api.x.ai/v1/chat/completions` accepts the CLI's OIDC bearer from `~/.grok/auth.json` (scope `api:access grok-cli:access`) and is already OpenAI-shaped (`reasoning_content` deltas, whole `tool_calls` chunks with `id: call-<uuid>-<n>`, usage chunk with `cached_tokens`), so the adapter is a pass-through plus the CLI's `x-grok-client-identifier: grok-cli` / `x-grok-client-version` headers. `reasoning_effort` accepts `minimal|low|medium|high|xhigh` (`none` → 400), hence §19.5's `off` → `minimal`. Errors come as `{"code","error"}` (HTTP) or an `error` chunk (stream); both use the HTTP status table — a mid-stream chunk is not a blanket `vendor_down`. 403 on an unexpired token → `502 vendor_drift`; 402 stays `insufficient_quota`. Rate limit from `x-ratelimit-{limit,remaining}-tokens` → `x-thickapi-rate-limit`. **Measured 2026-09-22 (grok-4.7):** the whole `tool_calls` chunk means a long tool call is silence on the wire from the last reasoning delta until the call lands — Pi saw `grok went quiet mid-answer for 180s` twice on a rebuild-spec `write` (the idle budget spent while the call was still generating; measured direct against `api.x.ai` the same day: a forced 3500-word `write` streamed 16 s of reasoning, then 185 s of nothing but an xAI `: keepalive` comment every 15 s, then the whole call). A request that offers tools therefore runs on `StreamBudgets::with_idle_floor(thin.vendors.grok.budgets.tool_call_floor_seconds)` (900; `0` = off; `thin.grok_tool_call_seconds` before C5): the idle budget raised to at least that, the first-event budget unchanged (the floor is for silence mid-answer; #129), a request without tools unchanged, and a vendor that never delivers still ended at the floor. Token life ≈ 6 h; when < 5 min remain the adapter runs `grok models` so the **CLI** refreshes under its own lock — the daemon still never writes `auth.json`. `/ready` → `thin.vendors.grok`; `doctor` prints token/CLI status. CLIProxyAPI was not needed.
- `antigravity.rs` — **measured 2026-09-17 (`agy 1.2.4`):** the CLI is a Google Code Assist client. `POST https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse` (the host `agy` itself calls; `router.yaml thin.vendors.agy.base_url` overrides) with envelope `{project, model, request: <Gemini GenerateContentRequest>, requestType: "agent", userAgent: "antigravity", requestId}`. Client identity is `User-Agent: antigravity/cli/<agy --version> (aidev_client; os_type=<os>; arch=<arch>; auth_method=consumer)` (the `agy 1.4.7` shape, measured 2026-10-24; the backend gates models on it — under the older `antigravity/<version> <os>/<arch>` Gemini 4 Argon is `404` and absent from `fetchAvailableModels`), `X-Goog-Api-Client: google-cloud-sdk vscode_cloudshelleditor/0.1` and `Client-Metadata: {"ideType":"ANTIGRAVITY","platform":…,"pluginType":"GEMINI"}` — without them `loadCodeAssist` answers `UNSUPPORTED_CLIENT` for the free tier. `project` = `loadCodeAssist.cloudaicompanionProject` (`aicode-consumers` on the consumer tier), discovered once per process (`thin.vendors.agy.project` overrides). **Token:** Windows Credential Manager `gemini:antigravity` (blob = JSON `{token:{access_token, refresh_token, expiry <RFC3339>}, auth_method, id_token}`), read with `CredReadW`; 1 h life; when < 5 min remain the adapter runs `agy models` so the CLI's silent auth refreshes it — the daemon never writes. **Models:** `vendor_model` is the Antigravity catalogue id (`fetchAvailableModels`). Two shapes (measured 2026-10-24, `agy 1.4.7`): a **tiered** id (`gemini-3.8-flash-tiered`) is one model, and effort travels only as `generationConfig.thinkingConfig` per §19.5; an **effort-suffixed** id (`gemini-4-argon-low|medium|high`, `claude-opus-5-5-low|medium|high`) is one backend model per level and the bare stem is `404`, so the row names its default level and the adapter swaps the suffix for the requested level and sends the same level as `thinkingLevel` (what the CLI sends); with no level asked the row's suffix is the level, and off keeps the row's id with `thinkingBudget: 0`. `xhigh`/`max` are `high` (`claude-opus-5-5-max` is `404`). The access line's `wire_params=` adds `model=<id>` when the sent id differs from the row's. Claude behind Antigravity answers on the same Gemini wire (its thoughts stream as `thought: true` parts with the signature on a thought part). Its tools go as `parametersJsonSchema` with the client's schema untouched, not the sanitized `parameters`: Google's conversion of a proto schema holding any `anyOf` is rejected by Anthropic (`input_schema: JSON schema is invalid`), while `parametersJsonSchema` reaches Anthropic as sent (measured 2026-10-24; Gemini rows accept it too but keep the measured `parameters` path). **Translation:** system → `systemInstruction`; assistant `tool_calls` → `functionCall` parts; tool messages → one `user` content of `functionResponse {name, id, response: {result}}` (parallel results merged); tools → `functionDeclarations` with schemas cut to the proto subset by `sanitize_schema`, an **accept-list**: the wire rejects an unknown *name* outright, so a key outside the measured set is dropped rather than forwarded (measured 2026-10-22 on `agy 1.4.4` / `gemini-3.8-flash`: accepted `type`, `format`, `title`, `description`, `nullable`, `default`, `enum`, `example`, `items`, `properties`, `required`, `anyOf`, `allOf`, `not`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`, `minItems`, `maxItems`, `minProperties`, `maxProperties`, `propertyOrdering`; rejected `$ref`, `deprecated`, `readOnly`, `writeOnly`, `multipleOf`, `uniqueItems`, `if`/`then`/`else`, `exclusiveMinimum`, `exclusiveMaximum`, `$schema`, `$id`, `$comment`, `examples`, `additionalProperties`, `$defs`, `definitions`, `strict`. `const` → `enum`, `type: [T, "null"]` → `type: T, nullable: true`, another type array → `anyOf`, `oneOf` → `anyOf`, `allOf`/`not` subschemas sanitized too, and an integer `exclusiveMinimum`/`exclusiveMaximum` → the exact inclusive `minimum`/`maximum`; a non-integer exclusive bound has no inclusive equivalent and is dropped rather than widened. **Structure** (measured the same day, one probe each, because Google reports only the first it hits). A `required` name must be one of the `properties` beside it (`required[1]: property is not defined`, top level and nested), so the list is cut to the properties that survived and dropped when the cut is empty. `type: "array"` must carry a *schema* `items` (`items: missing field`), so an array whose `items` is absent, `null` or the tuple form gets `items: {}`. An `enum` entry must be a **string** — the proto field is `repeated string` (`Invalid value … (TYPE_STRING)` on a number or a boolean whatever the declared `type`, while `"1"` is accepted for an integer, and a `null` entry is ignored), so a scalar entry is carried over as its text and an entry the proto cannot carry at all is dropped; a list cut to nothing becomes no `enum`. A `type` name must be one the enum parses (a typo is `Invalid value … (TYPE_ENUM)`, and a bare number is read as the enum's index — `5` is `ARRAY`), so an unparseable name is dropped and the rest of the schema stands. `{}` and `{"type":"object"}` are accepted as a property or as `items`, so an empty object schema is kept there and left off only as a declaration's own `parameters` — dropping a nested one is what left `required` and an array's `items` naming something absent. Not repaired, because the wire does not check them: `propertyOrdering` names, a `required` with no `properties` beside it, `required: []`, dotted property names, `minLength > maxLength`, duplicate `required` names. A malformed *value* on a typed field (`description: 5`) is still the client's own `400`). **Thought signatures:** every `functionCall` part arrives with a `thoughtSignature` that **must be replayed** (400 otherwise). The adapter mints the client-facing `call_…` id itself, remembers `id → {gemini id, signature}` (bounded FIFO, 4096) and restores both on replay; an unknown id (daemon restarted) replays the documented `skip_thought_signature_validator` sentinel, which the API accepts. The route keeps adapter-minted route-shaped ids for this reason. **Stream:** `data: {"response": {candidates, usageMetadata}}`; `thought: true` parts → `reasoning_content`; `finishReason STOP|MAX_TOKENS`; usage = `promptTokenCount` / `candidatesTokenCount + thoughtsTokenCount`; `cachedContentTokenCount` → `cached_tokens`. Errors are Google's `{"error":{code,message,status,details}}`; `RESOURCE_EXHAUSTED` → 429 with `RetryInfo.retryDelay`. `UNSUPPORTED_CLIENT` / `FAILED_PRECONDITION` bodies → `502 vendor_drift` naming the client headers. `finishReason` `MALFORMED_FUNCTION_CALL` / `UNEXPECTED_TOOL_CALL` / `OTHER` → `503 vendor_down`, `Retry-After: 2`; `SAFETY` / `RECITATION` → `finish_reason: content_filter`. Never `stop` for those. Rows: `subs/agy/gemini-3.8-flash`, `subs/agy/gemini-4-argon` (alias `argon`), `subs/agy/opus` (alias `agy-opus`). Sonnet 5.5 and GPT-OSS 120B answer on the same wire and are not catalogued (§10.3 policy). The thick lane strips the suffix for `agy --model` and is untested on the suffixed rows. `/ready` → `thin.vendors.agy`; `doctor` prints token/CLI status and the credential source.
- `codex.rs` — **measured 2026-10-24 (`codex-cli 0.164.0`, ChatGPT Plus):** the codex CLI's ChatGPT login is a bearer for the Responses API, `POST https://chatgpt.com/backend-api/codex/responses` (`router.yaml thin.vendors.codex.base_url` overrides the `…/backend-api` base). Headers: `authorization`, `chatgpt-account-id` (the access token's `https://api.openai.com/auth.chatgpt_account_id` claim; redacted in captures), `OpenAI-Beta: responses=experimental`, `originator: codex_cli_rs`, `user-agent: codex_cli_rs/<codex --version> (<os>; <arch>)`, `session_id`. **Token:** `$CODEX_HOME/auth.json` else `~/.codex/auth.json`, `auth_mode: chatgpt` only (an API-key login is refused), `tokens.access_token` a JWT whose `exp` is the expiry (ten days); `codex login status` runs inside the last day so the CLI refreshes it; the daemon never writes. **Body:** `store: false`, `stream: true`, `instructions` = the system/developer text (else a one-line default), `input` items (user `input_text`/`input_image`; assistant text as an `output_text` message; each tool call a `function_call {call_id, name, arguments}`; each tool result a `function_call_output {call_id, output}`), `tools` as flat `{type: function, name, description, parameters}` with the client's schema untouched and no `strict`, chat `tool_choice` → Responses (`{type: function, name}`), `parallel_tool_calls`, `include: [reasoning.encrypted_content]`, `prompt_cache_key` = a hash of the model, the instructions and the first user record (stable across a conversation's turns), `reasoning: {effort, summary: auto}` per §19.5, `max_output_tokens` (raised to the backend's floor of 16). `temperature`, `top_p` and `stop` are `400 Unsupported parameter` and are dropped. **Stream:** `response.output_text.delta` → text; `response.reasoning_summary_text.delta` → `reasoning_content` (a new summary part starts on a blank line); a `function_call` item in `response.output_item.done` → one tool call whose id is its `call_id`; `response.completed` carries usage (`input_tokens`, `output_tokens` with reasoning inside, `input_tokens_details.cached_tokens`) and ends the turn (`tool_calls` when a call was issued); `response.incomplete` `max_output_tokens` → `length`, `content_filter` → `content_filter`; `response.failed` / `error` → the shared table (a usage limit is `402 insufficient_quota` with the body's `resets_in_seconds`, context length `400`). No `response.completed` is a truncated turn. **Reasoning replay:** each response's `reasoning` items (`id`, `summary`, `encrypted_content`) are kept per `call_id` (bounded FIFO, 2048) and replayed before those calls on the next turn; replay is optional on this wire, so a daemon restart only loses quality. Budgets `thin.vendors.codex.budgets` (first event 120 s, idle 300 s, the CLI's own stream idle timeout). `models check` reads `GET /codex/models?client_version=<v>` without `visibility: hide` rows. Rows `subs/codex/gpt-sol` (`gpt-6.1-sol`), `subs/codex/gpt-astra` (`gpt-6-astra`), `subs/codex/gpt-luna` (`gpt-6-luna`); `context_window` is the model's window at the CLI's 95 % effective share. No thick lane: a `thickapi turn` on a Codex row is refused. `/ready` → `thin.vendors.codex`.
- `cursor.rs` — **measured 2026-09-17 (`cursor-agent 2026.09.15-d2fe57e`):** Connect-over-HTTP/2 protobuf duplex stream, `POST https://agentn.global.api5.cursor.sh/agent.v1.AgentService/Run` (`thin.vendors.cursor.base_url` overrides; https + `*.cursor.sh` or loopback only), `content-type: application/connect+proto`, `connect-protocol-version: 1`, `x-cursor-client-type: cli`, `x-cursor-client-version: cli-<cursor-agent --version>` (`thin.vendors.cursor.client_version` overrides), `x-ghost-mode: true`, `x-request-id`. Wire shape from `pleaseai/shunt` / `1jehuang/jcode` (MIT), hand-rolled protobuf (no `prost`). **Framing:** `[flags u8][len u32 BE][payload]`; `0x01` gzip payload, `0x02` end-of-stream JSON trailer (`{}` or `{"error":{code,message,details}}`); the server gzips the trailer too (`0x03`), and the adapter opens it before reading the error (#249: read raw, a usage-limit trailer parsed as no error and became `503 vendor_down` "closed the stream before sending any output"). **Request:** frame 0 `AgentClientMessage.run_request` = `AgentRunRequest { conversation_state="" (1), action.user_message {text, id, selected_context, mode=AGENT} (2), mcp_tools (4), conversation_id (5, 16), requested_model {name, fast kv} (9), f12=0, model catalogue (14), client_supports_inline_images (19) }`; frame 1 proactive `AgentClientMessage.exec_client_message` (`f2`) with `request_context_result` (`f10`) carrying `RequestContext.env` (os and cwd from the host, tz `UTC`, and `shell` the client's shell tool — `bash` on every platform, overridable with `thin.vendors.cursor.shell` — not the host OS default; G4, #184: on Windows it used to say `powershell` while Pi's `bash` ran the commands, so Cursor Fable's first shell call was `Test-Path …` → `command not found`); then `f3{f3:''}` and eight `f3{f1:N,f3:''}` markers (`kv_client_message {id: N, set_blob_result {}}`) **paced** (2.3 s, then every 0.4 s) and a `f7` heartbeat every 5 s while the answer streams. There is no paced `f5{f1:''}` any more: that frame is `exec_client_control_message.stream_close {id: 0}`, and sent at 1.5 s it crossed the server's exec-0 `request_context_args` in flight (H2, #186). **Exec handshake (measured 2026-09-20, `cursor-agent 2026.09.18-9a7762b`):** the server sends `AgentServerMessage.exec_server_message` (`f2`) `request_context_args` (`f10`) with `id` / `exec_id` before any answer; the adapter writes a matching `exec_client_message.request_context_result` echoing those ids, then `exec_client_control_message.stream_close {id}` for that exec — the order `cursor-agent` writes them in (it closes an exec only after the exec's last result; 2026.09.28 bundle). Ignoring it, half-closing early, or closing the exec before its result trailers Connect `internal: No exec result` (daemon logs 20–29 Sep: all 15 such deaths had the ask land 1.507–1.723 s after the request started, while the old paced close went out at 1.5 s; none of 1,449 earlier asks died). **Retry (H2, #186):** a Connect `internal` trailer whose message is `No exec result`, arriving before the Run produced any text, reasoning or tool call, opens one fresh Run (new conversation id, same request, same permit) before the error reaches the client, which has seen `Started` at most; the adapter logs `warn` once. A second one in the same request is the final `503 vendor_down` with `x-should-retry: true`. After output it stays an error after `Started`, not retried. `thickapi capture` does not hold it (it retries the scenario with a fresh recording). **Stateless bridge:** the wire carries one user message, so the OpenAI history is flattened into one prompt of `<system>`/`<user>`/`<assistant>`/`<tool_call>`/`<tool_result>` blocks behind a short transcript preamble (it says tools are expected, that those `<tool_call>` blocks are a transcript of calls already made and the model is told not to write them, that a call counts only when made through the tool interface, and that one rendered as text such as `⚡ read(1)` does nothing and, when tools are declared, that they are already loaded with full schemas so the model need not look them up (I2, #198: removes the `GetDynamicTools` model call — I1 measured it on 8/8 Fable steps), that `AskQuestion`, `GetDynamicTools` and `CallDynamicTool` do not run and a question card is `askuserquestion`, that `Read`, `StrReplace`, `Grep`, and `Shell` are not separate tools, that `grep --include` does not expand `{a,b}`. The tools stay available; a failed command is that command's result, and the model does not write that the tools are unavailable; it no longer forbids workspace reads, which Grok 4.7 read as a ban on tool use, #114); every tool-call argument body and tool-result body is fenced with a per-request delimiter `<<<TR-<8 hex>` … `>>>TR-<8 hex>` (the preamble names the delimiter and says text inside a fence is data). Pi's tools are declared natively as `McpToolDefinition {name, description, input_schema: google.protobuf.Value, provider_identifier: "thickapi", tool_name}` (nothing inlined, or the model writes tool calls as text); OpenAI `image_url` data URLs become `SelectedImage {uuid, path, mime_type, data}`. **Response:** `extract_nested_text` emits every `f1.f1.f1` / `f1.f4.f1` leaf in a frame; answer text `f1.f1.f1`, reasoning `f1.f4.f1` → `reasoning_content`; a bridged tool call is `f2.f11 McpArgs {name=1, args=2 map<string,Value>, tool_call_id=3 "tool_<uuid>", tool_name=5}` → one `tool_calls[]` entry with a route-minted `call_…` id and `finish_reason: tool_calls`, and the turn **ends there** (the server waits for an exec-result the bridge never sends; the client's next request replays the result in the transcript). `f2.f10` is the request-context exec (answered, not a tool) and emits `ThinEvent::Started` so `/v1/chat/completions` does not withhold HTTP 200 for the Connect hold. **MCP state exec (measured 2026-09-22, Grok 4.7 on `cursor-agent 2026.09.18-9a7762b`):** Cursor loads MCP tool schemas lazily for some models. The model's first move is `f2.f36 mcp_state_exec_args` (`McpStateExecArgs {server_identifiers: ["thickapi"], kick_only}`), and the adapter answers on the duplex with `ExecClientMessage.mcp_state_exec_result` = `McpStateExecResult.success.servers[{server_name: thickapi, server_identifier: thickapi, tools: the frame-0 `McpToolDefinition`s, status: "connected"}]`, echoing `id` / `exec_id`; it also emits `Started` and never counts toward silent drift. Unanswered, the server holds the stream open with `f1.13` keepalives until the budget ends the turn as reasoning-only, or the model writes its call out as text (#114). `f2.f19` is a `SpanContext` (trace id, span id, flags) carried on every exec message, not a tool; `f2.f55` is `accept_hook_additional_contexts`. **KV (measured 2026-09-22):** top-level `f4` is `kv_server_message {id, set_blob_args {blob_id, blob_data} | get_blob_args {blob_id}, span_context}` — the server parks conversation blobs on the client and awaits an ack per message before it calls the model again inside the same Run (a turn that continues past an exec result, as after `mcp_state`, hangs on keepalives otherwise). The adapter answers each on the duplex with `AgentClientMessage.kv_client_message {id, set_blob_result {}}` (or `{error}` past a 256 MiB per-Run budget) and `get_blob_result {blob_data}` (or `{error}` for a blob it never held); blobs live for the Run. These frames are not ThinEvents and never count toward drift. Read, grep, ls, write and shell (`read_args=7`, `grep_args=5`, `ls_args=8`, `write_args=3`, `shell_args=2` and `shell_stream_args=14`) are not executed here. **A read of a missing file is answered in protocol (G1, #175; measured 2026-09-29 on `cursor-agent 2026.09.28-64d2043`, F1 #181):** an `f7 read_args` whose path does not exist — resolved as Pi's `read` resolves it (under the advertised cwd; absolute `C:\x` / `C:/x` stand alone; a leading `@` dropped; `~` the home directory; on Windows `/c/x`, `/mnt/c/x`, `/cygdrive/c/x` → `C:\x`) — is answered on the duplex with `ReadResult.file_not_found { path }` (`f4`, the CLI's own ENOENT answer), echoing `id` / `exec_id`; no tool call is emitted and the Run goes on, whatever the request declared. Cursor's edit built-in pre-reads its target this way, and on `file_not_found` sends `f3 write_args {path, file_text, tool_call_id}` for the same absolute path and tool-call id about 175 ms later (Fable 2/2, Grok 4.7 3/3), which becomes Pi `write` (below). Answered `ReadResult.rejected` (`f3`) instead, the server closes the edit with an empty `EditResult` and the model falls back to Cursor's shell (5/5); handed to Pi as `read`, Pi's ENOENT came back as tool-result text and Cursor restarted the edit (29 Sep smoke, `subs/cursor/fable:high`: 55 identical `read`s, `turn_timeout`). The existence check is a stat on the daemon host, not an execution; a read of an existing file goes by the table below, except the edit's own pre-read. **The edit pre-read of an existing file is answered in protocol too (#248; measured 2026-10-05 on `cursor-agent 2026.10.01-14929f9`, Grok 4.7):** Cursor's edit built-in first announces itself as `interaction_update` `partial_tool_call` (`f1.7`) / `tool_call_started` (`f1.2`) `{call_id, tool_call: ToolCall.edit_tool_call (f12)}`, and its `f7` pre-read carries that `call_id` in `ReadArgs.tool_call_id` (`f2`). A read under an announced edit id, of a UTF-8 file of at most 1 MiB (resolved as above), is answered on the duplex with `ReadResult.success { path, content, total_lines, file_size }` (`f1`, the CLI's own text-read answer), echoing `id` / `exec_id`; no tool call is emitted, and Cursor sends `f3 write_args` with the whole edited text for the same path and call id about 190 ms later, which becomes Pi `write`. A read the model asks for itself (`read_tool_call`, `f8`), one under any other id, and a binary or larger file still go to Pi `read`. Handed to Pi, the pre-read ended the turn; Cursor started the edit over and pre-read again until G3 refused it (`502 builtin_loop`, 3 Oct smoke, `cursor/grok-4.7` plain ×2). The daemon reads that file only to answer Cursor's own pre-read; it runs nothing. **Declared tools only (G2, #182):** each maps to the first option in the built-in table (below this list item) that the request declared — the tool names frame 0 carries — becomes that one Pi tool call, and the turn ends, the same as MCP `f2.f11`; when no option is declared, or the args name no path, pattern or command, it is denied on the duplex like any other documented builtin and the Run goes on, so the model can fall back to a declared MCP tool. The denial names the tools frame 0 declared (32 at most) and says `AskQuestion`, `GetDynamicTools` and `CallDynamicTool` do not run. A brace glob `*.{rs,json}` on a grep built-in becomes one `--include` per suffix; any other glob stays one `--include`. **thickAPI never emits an undeclared tool name:** an MCP `f2.f11` call names a tool frame 0 declared, a built-in maps only onto a declared tool, and nothing runs in the daemon. One function (`route_builtin`) decides for both the event side (the tool call) and the duplex side (hand to Pi, answer a missing read, or deny); the duplex side calls it once per exec and the event side acts on that decision. Before G2 a glob-only grep became Pi `find` and `ls` Pi `ls` whatever the request declared; a draftr seat runs Pi's defaults (`read, bash, edit, write`), so Pi answered `Tool find not found` and Cursor repeated it (29 Sep smoke, `subs/cursor/grok-4.7:high`: `find` ×30, `turn_timeout`). A built-in is recognised by its documented oneof field plus a tool-call id of any shape seen on the wire — Cursor's `tool_<uuid>` (MCP), Claude's `toolu_<id>` (Fable built-in read) or Grok's `call-<uuid>-<n>` (measured 2026-09-22; only `tool_` used to count, so a built-in exec from either model went unanswered and the turn stalled into #114). No exec result is written for those. A builder brief opens with native Read and does not fall back to MCP after a duplex denial (draftr `3ae4656e`, `4f2a1494`). Any other documented builtin, and one no declared tool runs, is answered on the duplex (`ReadResult.rejected`, `GrepResult.error`, or `exec_client_control_message` throw then stream_close) and the turn continues so a later MCP `f2.f11` can still become a tool_call. A `tool_<…>` id on an unknown `f2` field (not MCP `f11`, not a documented built-in) is `502 vendor_drift` + `x-should-retry: false`. Connect `not_found` is `404 model_not_found` naming the `requested_model` name and params sent. That 404 — Connect trailer or HTTP — is a per-row fact: `/ready` records it as `oauth.last_row_rejection` `{wire, code, status}` and does not overwrite vendor `last_rejection`; a Connect `not_found` marks the row unavailable the same as an HTTP 404 (`rows_unavailable`, #147). A usage limit on one model (`ERROR_RATE_LIMITED_CHANGEABLE`, #249, under **Errors** below) is per-row the same way. Other Connect trailers (`unauthenticated`, account-wide `resource_exhausted`, `internal`, unknown codes) set `last_rejection`. `successes` increment when the stream finishes, not on HTTP 200. After the answer the server holds the stream open. A text answer ends on `AgentServerMessage.interaction_update` field 14 (`turn_ended`; `stop` when the turn is text only, `tool_calls` when a call already arrived), which on measured text runs arrives before the Connect trailer and before `idle_seconds` (4) would. `idle_seconds` (4) remains the quiet end for a text-only request that has not yet sent `turn_ended`. When the request offered tools, `idle_after_text_with_tools_seconds` (300) is only a safety net above the longest gap measured on the F1 `quiet` set: spending it after answer text is `503 vendor_down` (`cursor went quiet mid-answer`), not `finish_reason: stop`, and the daemon logs `warn` with the gap and the request id. `first_output_seconds` (90) before any output is `vendor_down` (`0` turns one off). An `interaction_update` heartbeat (`f1.13`) before any answer text widens that wait to 600s, still measured from the last ThinEvent and not reset by later heartbeats: cursor-agent treats `f1.13` as the server still being in the turn, and xhigh Grok often sends only those for longer than 90s before the first thinking delta (`cursor sent nothing for 90s`). Empty `f7` keepalives do not widen it. A turn that ends with reasoning and no answer text is `502 vendor_drift`, not `finish_reason: stop`. The adapter opens one fresh Run (new conversation id) before that error is sent to the client; reasoning already streamed on the first Run stays. A second reasoning-only end is the final `502`. It is never `finish_reason: stop`. A turn whose answer text carries a rendered built-in marker (`⚡ <tool>(n)`, #114: the model wrote its call out instead of making it) and no tool call is `502 vendor_drift` naming the marker, never `stop`, on the first Run: it is **not** held for a fresh Run, because its answer text has already streamed and a second Run's answer would be appended to it as one message (#127). A `<tool_call>` block written as answer text, when it parses to one declared tool and a JSON object, is that tool call and the markup is not answer text; an unparseable block is `502 vendor_drift` `code: text_tool_call` and is not streamed; a `<tool_result>` the model writes is dropped. An unparseable `<tool_call` that streamed no prose is held for one fresh Run the same way a reasoning-only end is; once prose has streamed, that drift is final (#127). A stop that claims a tool is not available (`tool not available`, `is not available in this session`, `isn't available`, `aren't available`, `unavailable`, `still down`, `tools failed`, or `dropped out`) while tools were offered and no call was recovered is the same `text_tool_call` drift. A leading claim is not streamed. Three such claims in one answer end the turn there; the rest is not streamed. Only a Run that streamed no answer text (reasoning only, or an unparseable `<tool_call` with no prose) is held. That `<tool_call` drift is not recorded as `/ready` `rendered_builtin`. `/ready` records a rendered built-in marker as `last_row_rejection` `{wire, code: rendered_builtin, status: 502}` on every occurrence, held or final, without touching vendor `last_rejection`. **Loop guard (G3, #183):** before a token, permit or Run, the adapter scans the OpenAI history back to the latest user turn, collecting up to `3N` assistant turns that each made exactly one tool call; when one (name, arguments, result) triple occurs N times in that window (`thin.vendors.cursor.builtin_loop_limit`, default 3, `0` = off) — same name, byte-identical arguments, byte-identical result — the request is refused `502` `type: vendor_drift`, `code: builtin_loop` with `x-should-retry: false`, the message naming the tool and its first argument (path, pattern or command; 120 characters at most). A different call between them does not reset the count. A different result for the same arguments does not count. `builtin_loop` and `text_tool_call` are not rewritten as a wire change: the Pi client returns the daemon message and the envelope hint, and does not say the wire changed or name `thickapi doctor`. No Run is opened. `/ready` records it as `last_row_rejection` `{wire, code: builtin_loop, status: 502}`, not vendor `last_rejection`; the access log error line carries the code. It exists because a looping seat burnt the whole cell before failover could act (400 s in the 29 Sep smoke, 720 s in draftr: 55 identical `read`s, 30 identical `find`s); nothing is executed here and no tool name the request did not declare is ever emitted. A `{}` trailer with no output at all is `503 vendor_down`, not `stop`. Those timers count quiet ThinEvent time — Connect frames that map to no event (`f7` heartbeat, a repeat `f2.f10`) do not reset them (measured 2026-09-20 wrap-up Completions held ~308s with no SSE because keepalive frames reset `body.next()`). Unknown top-level fields (not `f1`/`f2`, and not conversation-state `f4`) trip after five frames with no ThinEvent (`502 vendor_drift` naming only that counter's paths); a nested renumber — `f1`/`f2` present but no mapped event — has its own five-frame counter and names only nested paths (e.g. `1.9.1`). Empty `f7` keepalives, conversation-state `f4`, and `f2.f10` handshake frames do not count, and the two counters do not add. Measured-healthy nested preamble frames (`f1.13`, `f1.7` tool-id echo, `f1.8`, `f1.25`, `f1.5`, `f1.2`, `f1.27` `tool_requests_listed` (a call count), `f1.3` `tool_call_completed` (`call_id`, `tool_call`, `model_call_id`; the step finished, and the call the client runs is still the exec), and timing `f8`) do not count toward the nested five-frame budget (a lone `f8` metrics frame also does not count toward the top-level budget); they are not ThinEvents; MCP stays `f2.f11`. No usage on the wire: the adapter emits `ThinEvent::Usage` as rendered-prompt bytes / 4 + 750 per image (prompt) and streamed text + reasoning bytes / 4 (completion); the route adds `x-thickapi-usage: estimated`. **Model id (measured 2026-09-17):** `vendor_model` is the id `cursor-agent --list-models` prints; the wire wants the **base** id plus `ModelParameter {id, value}` pairs (the CLI's own `modelParameters` config: `thinking`, `effort`, `fast`). `wire_model` decomposes the alias: trailing `-fast` → `fast=true`; effort suffix `-low|-medium|-high|-xhigh|-max|-minimal|-none` → `effort=<level>`, replaced by the requested §19.5 level (`off`/`minimal` keep the row's default); trailing `-thinking` → `thinking=true`; a leading `cursor-` after those suffixes is stripped so the wire `requested_model` name is the parameterized base (`cursor-grok-4.7-high` → `grok-4.7` + `effort=high` + `fast=false`; measured 2026-09-21 on `cursor-agent 2026.09.18-9a7762b` Connect frame 0); keeping `cursor-grok-4.7` is Connect `not_found`; the `--list-models` slug stays the catalogue `vendor_model`; `auto` → `default`. Sending the alias itself (`claude-fable-5-1-thinking-high`, `auto`) answers Connect `not_found`. Ids without an effort suffix ignore the level. **Errors:** HTTP 464 with empty body (`server: awselb/2.0`) = the ALB refused HTTP/1.1 or the client version is below Cursor's floor → `502 vendor_drift` naming the advertised version; Connect `unauthenticated` → 401 `cursor-agent login`, `resource_exhausted` → 429 (or 402 when the message says usage/quota/payment); the message is Cursor's own `title: detail` from `details[].debug.details` when it sends one (its top-level `message` is just `Error`). **Usage limit on one model (#249, measured 2026-10-05 on `cursor-agent 2026.10.01-14929f9`):** Cursor out of plan usage for one model family (Fable/Opus on an Ultra plan) trailers `resource_exhausted` with `debug.error: ERROR_RATE_LIMITED_CHANGEABLE`, `title` `You've hit your usage limit` and a `detail` naming the reset date → `402 insufficient_quota`, `x-should-retry: false`, message `quota exceeded: cursor: You've hit your usage limit: … reset … 10/21/2026.`. That one is a per-row fact like `not_found`: `/ready` records `last_row_rejection` `{wire, code: insufficient_quota, status: 402}` and lists the row in `rows_unavailable`, and does not set vendor `last_rejection`, so the account's other rows (Grok, Auto) stay available. A `resource_exhausted` without `_CHANGEABLE` stays a vendor rejection, `context_length_exceeded`/`invalid_argument` → 400 (`cursor {code}: {message}`). A 403 on an unexpired token → `502 vendor_drift` naming the header set sent. **Token:** `auth.json` session JWT (~60 days); when < 24 h remain the adapter runs `cursor-agent status` so the CLI refreshes under its own lock. `/ready` → `thin.vendors.cursor` (CLI version, advertised client version, token, `last_rejection`, `last_row_rejection`); `doctor` prints CLI version, advertised client version and token. Verified from Pi: a Cursor read+write tool turn (3 round-trips, 18 s).
  Cursor built-ins → declared Pi tools (G2, #182; first declared option wins):

  | Cursor exec | 1st | fallback | else |
  |---|---|---|---|
  | `f7` read (target exists; a missing one is `ReadResult.file_not_found`, an edit's pre-read is `ReadResult.success`, above) | `read` | `bash` `cat -- '<path>'` | deny (`ReadResult.rejected`) |
  | `f5` grep with a pattern | `grep` | `bash` `grep -rnE [-i] [--include='<glob>'] -- '<pattern>' '<path>'` | deny (`GrepResult.error`) |
  | `f5` glob only | `find` | `bash` `find '<path>' -name '<glob>'` | deny (`GrepResult.error`) |
  | `f8` ls | `ls` | `bash` `ls -la -- '<path>'` | deny (throw) |
  | `f3` write | `write` | — | deny (throw) |
  | `f2` / `f14` shell | `bash` | — | deny (throw) |

  The shell fallbacks single-quote every argument (an embedded `'` is closed, escaped and reopened); Pi's `bash` is Git bash on Windows, so the syntax is POSIX. Paths pass through as Cursor sent them (`.` when a grep or ls names none). `find -name` and `grep --include` match the file name, so a leading `**/` is dropped from the glob, and a glob that still has a `/` is `find '<path>' -path '*/<glob>'`. The grep fallback is `-E` because Cursor's pattern is a ripgrep regex, nearer ERE than BRE. Fixture replay declares what the captured frame 0 declared (`declared_tools_in_body`); the offline default declares nothing, so every built-in is denied. Replay judges a read's target as the capture host saw it (`ReadMiss::for_fixtures`: `<REPO>` / `<HOME>` redactions resolved under the checkout and this machine's home).
- **Per-request cwd (#205).** `POST /v1/chat/completions` may send `x-thickapi-cwd` with one absolute path. For that request only, Cursor `RequestContext.env.cwd` (frame 1 and every repeat `request_context_result`) is that path; the client's configured cwd is unchanged for the next request. No header, or an empty one, keeps today's cwd (`thin.vendors.cursor.cwd`, else the daemon's cwd). A relative path, or a path that does not exist, is `400` `invalid_spec` in the thin `{error: {code, message}}` envelope and does not fall back to the config cwd. There is no deny-outside-cwd rule and no scratch directory.
- Tokens travel only in `Authorization` headers, never URLs. Tracing: `EnvFilter` from `THICKAPI_LOG` (else `RUST_LOG`, else `thickapi=info`) with `hyper=warn,h2=warn,reqwest=warn,rustls=warn` appended after so the env cannot raise them (`log_filter.rs`). Daily rolling files `thin.log_dir/thickapi.<date>.log` (default `~/.thickapi/logs/`); `reap` deletes those and `autostart.log` older than `thin.log_keep_days` (14). The Pi extension reads only a tail of `autostart.log` (last ~8KiB) while polling spawn.
- Each adapter is isolated behind `ModelAdapter`; `/ready` reports per vendor. `last_rejection` stores the classification plus a ≤120-char redacted excerpt; `/ready` emits the classification only; the excerpt goes to the log. The agy `account=` note drops the e-mail regardless of `lan`.
- Deps: `reqwest` (`http2`, `rustls-tls`, `stream`, `json`), `flate2` (Cursor gzip frames), `tokio-util`. No `prost`: the Cursor wire is a dozen hand-rolled fields.

### 19.9 Keys, security, storage

- **Scopes:** thin routes require `chat:write`; `turn:write` does **not** imply it. `sync-pi` mints `[chat:write, models:read]` only — the key in Pi's `models.json` is not a thick-turn credential. `turn:write` is for `thickapi turn` / scripts. Under `lan: true`, one key per remote client; `/ready` omits account emails and expiry.
- **Private files:** `write_private_atomic(path, bytes)` creates a sibling temp empty (Unix mode 600), runs `secure_private_file` (Windows: resolve the caller's SID once via `whoami /user` / `GetTokenInformation`, then `icacls /inheritance:r /grant:r *<SID>:F`; Unix: mode 600/700), writes the bytes, verifies no inherited ACEs, renames, verifies the final path. Callers: `save_keys`, `sync-pi` `models.json` + sidecar, `claude_oauth_state.json`. `PrivateDir::create` fails closed (`500 server_error "private dir ACL failed"`) when icacls fails. `doctor` fails on inherited ACEs for those files and every live `tmp/<req>`.
- **Artifacts:** thin requests write **no** turn artifacts by default (`trace: true` opt-in; `thin.trace_keep` bounds how many traced dirs survive `sweep_dead`). The private directory is `~/.thickapi/tmp/<req>/` with a pid-marker JSON (daemon pid, child pid, and both start stamps). A marker whose daemon pid now carries another start stamp counts as a dead daemon. A dead daemon's child is not killed on Windows (its job object closed with the daemon); elsewhere only when its pid still carries the recorded start stamp (audit R11). `PrivateDir::Drop` retries removal on `ERROR_SHARING_VIOLATION`. `serve` start and each request sweep dead-pid directories; `reap` also sweeps `tmp/` and rolling logs; `serve` itself runs that retention on a 24 h interval; `install-service` also registers the daily `reap` task.
- Turn events stay in `events.jsonl` artifacts. SQLite does not store prompt text or tokens.
- **Access log (A2, restated):** the `thickapi_api::access` line (§19.15 R5) never carries prompt text, tool arguments, tool results, system prompts, or raw tokens — only the request id, key label, model / wire model / wire params / path, status, timings, `finish_reason`, the usage totals already returned to the client, and an error `code`.

### 19.10 Pi side (`~/.pi/agent`, not a repo)

- `models.json` `providers.subs` = `{ api: "openai-completions", baseUrl: "http://127.0.0.1:8787/v1", apiKey: "<ta_>", models: [...] }` written by `sync-pi`. `baseUrl`'s host is `router.yaml`'s `listen` host, rewritten to `127.0.0.1` whenever `listen` is an unspecified-address bind (`0.0.0.0`, `::`, only reachable when `lan: true` — §1.6): Pi on Windows cannot connect to `0.0.0.0` (D3). Pi `reasoning` / maps come from `thinking.rs`, not a force-true helper.
- A **small `subs` extension is kept** (`extensions/pi-thin/thickapi-subs.ts`): wraps `openAICompletionsApi().streamSimple`; pings `/ready` on `session_start` and `/model`, each `/ready` bounded at 3 s (`AbortSignal.timeout`, `thickapi-subs-ready.ts`) and a timed-out `/ready` checked against `/health` so a slow daemon is not taken for a dead one (#124); on `ECONNREFUSED` spawns only an absolute sidecar `exe` or `THICKAPI_EXE` (never a bare name; refuses `\target\` unless `allow_debug`); stdio → `autostart.log` (polled as a last-~8KiB tail); `spawnedOnce` resets after a successful `/ready`; polls `/ready` for 10 s and distinguishes lock-held from spawn-failed. Error rewrite: §12. Status bar: path always; quota only when used is above 50%, with one warning notify on the crossing; `x-thickapi-path` / `x-thickapi-fallback`; one notify per flip.
- `sync-pi` removes a leftover `providers.thick` (or a pre-T1 `subs` still `api: thickapi`) and revokes the `pi-sync` `turn:write` key, so Pi's `models.json` holds no turn credential after T5.
- `sync-pi` refuses to write the new shape unless `/ready` answers, and warns if `install-service` is not installed.
- Each `subs` stream sends `x-thickapi-cwd` set to the session cwd Pi already has (`ExtensionContext.cwd`, via `SimpleStreamOptions.headers`). At a daemon swap, `thickapi sync-pi` is when that pi-thin change is installed: the `thickapi-subs*.ts` module copied to `~/.pi/agent/lib/thickapi-subs/` (§12).

### 19.11 Doctor, drift, fixtures

- **`doctor`:** spawn `claude.exe` with the production argv and a one-token prompt against `mcp-proposal`, validate the init record (§19.7). Compare each CLI's `--version` with the newest `fixtures/<vendor>/` directory → `[warn] agy 1.2.5: no fixture set (run thickapi capture agy)`. There is no `measured_versions` key in `router.yaml`; the fixture set names are the one source. `UPSTREAM` in `claude_bridge/mod.rs` records the upstream package version and tarball sha. One 1-token live request per OAuth vendor (grok, agy, cursor); `--offline` skips live pings and the models live check. **No `--help` grep** — `--system-prompt-file` is documented but absent from help.
- **`serve --dump-tools <path>`:** writes the `tools` array of the first thin request received, then clears the flag.
- **`thickapi capture <vendor>`** (`claude | claude-bridge | grok | agy | cursor`, or `--all`) runs one text turn and one tool turn (`read_file`) through the same adapter `serve` uses, with a `FrameTap` (`thin::capture`; the recording tap is `thickapi-cli/src/capture/`'s `Recorder`, so none of the recorder, redactor or set writer is in the daemon) on the adapter, and writes `fixtures/<vendor>/<cli-version>/{text,tool}/` (bridge `--scenario image` and `--scenario image-tool` are additive image turns, merged into an existing set the way Cursor's opt-in scenarios are, and never replace `{text,tool}`) (Cursor `--scenario sdlc` and the F1 scenarios (#181) `create`, `create-fnf`, `quiet` and `mcp-repeat` are opt-in, not in `--all`, and copy their directory into an existing version dir — they never `promote_capture_set` over `{text,tool}`, and an `Error` does not fail them (they record what happens). F1 sets land as `<scenario>-<row>/` (`create-fable`): `create` / `create-fnf` replay the 29 Sep draftr builder cell up to its create, in a fictional cwd, where every built-in `f7` read is of a missing file and is answered on the duplex (`create`: the capture-only `ReadResult.rejected` shape, `CaptureSwitches.cursor_read_miss_shape`; `create-fnf`: `ReadResult.file_not_found`, what `serve` answers since G1) and the Run goes on; `quiet` is the tool retest's second request with `idle_after_text_with_tools_seconds: 0` under a 600 s wall clock; `mcp-repeat` is its fifth, up to five Runs for a repeated `mcp_state` ask or a `1.27` / `1.3` frame before any output; a Run that dies in the handshake gets up to three fresh Runs. The switch is set beside the tap (`ThinRuntime::with_capture`) and has no `router.yaml` key: `request.json` (method, URL, headers with `authorization`/`cookie`/`x-api-key` redacted, JSON body; for Cursor the initial Connect frames as `body_b64`; for the bridge argv, own env, declared tools, prompt blocks and system prompt), `response.json` (status, headers — denylist: `anthropic-organization-id`, `anthropic-workspace-id`, `anthropic-ratelimit-*`, `request-id`, `x-request-id`, `cf-ray`, `traceparent`, `server-timing`, `set-cookie`, names ending in `-id`), `frames.jsonl` (each body chunk as delivered — UTF-8 text or base64, binary Connect frames gunzipped, redacted, re-encoded — with `seq`/`ms`), `events.jsonl` (the id-free event summary the adapter produced: coalesced text/reasoning, tool calls by name+args, usage, finish; its strings redacted as the frames were — length-preserving for Connect — so the redacted frames replay to it), plus `meta.json` (vendor, version, model, path — the path the set was captured on, `bridge` for `claude-bridge`, `oauth` otherwise — `captured_at`) at the set level. **One pattern table**, `thin::fixture_set::TOKEN_PATTERNS`: `ta_`, `sk-ant-`, `sk-`, `ghp_`, long Bearer values, JWTs (`eyJ…` three-part), Google `ya29.` and `1//` refresh tokens, `C:\Users`, `C:%5CUsers`, e-mails (`@<domain>`), an `anthropic-organization-id` response header, a UUID in a response header value; the redactor, `identity_violations` and the `wire_fixtures` leak assertion all read it. `redact_text` covers `%USERPROFILE%` / repo root → `<HOME>` / `<REPO>` (both slash forms and `%5C`), every text entry of the table → `<redacted>`, and UUIDs as stable `<UUID-n>`. Every bearer seen in a request header is scrubbed from bodies and frames. Exit 1 and no `meta.json` when any scenario ends in `Error` or the tool scenario lacks the `read_file` call. The set is written to a sibling temp directory `<version>.tmp` (gitignored: `fixtures/**/*.tmp/`), `identity_violations` runs there (the table over every file and every decoded body, plus the repo roots), then the temp is renamed to the final path; a leaking set is deleted and does not replace an existing clean set. A directory counts as a set only with a `meta.json` and a name not ending `.tmp`: `newest_set`, `/ready` and `doctor` skip the rest. `serve` never sets a tap or a capture switch.
- **`thickapi capture --check <vendor>`** (or `--check --all`): the same capture, written under the OS temp directory instead of `<out>`; runs `identity_violations` and replays each fresh scenario through the adapter's offline replay against its own recorded events, then prints a diff summary against the newest committed set (event shape, request header names and body keys, frame count per scenario). Exit 1 on a leak, a replay mismatch or a failed scenario; the diff is information. It never promotes and never writes under `<out>` (`--keep-failed` keeps the temp tree of a failed check). Weekly capture is opt-in (`install-service --weekly-capture`, default off); the action is `{exe} capture --all` (plus `--config` when needed), not `cargo test`.
- **Replay tests** (`crates/thickapi-core/tests/wire_fixtures.rs`): per vendor, pick the newest set (`meta.captured_at`, else name), feed `frames.jsonl` through the adapter's replay — for the four OAuth-direct vendors the `VendorRequest::replay` trait default (Claude direct and Cursor through `ClaudeDirectRequest::offline()` / `CursorRequest::offline()`), which drives the vendor's own `wire` decoder through the end-of-body rule `run` uses; `claude_bridge::replay` for the bridge — the same framing and mapper the live `stream` uses and require the recorded `events.jsonl` back (rate-limit advisories excepted: they come from headers), a `Started` first, text + `finish stop` for `text`, a `read_file` tool call + `finish tool_calls` for `tool` (Cursor F1 `create*` sets replay with the reads their capture answered in protocol answered again; the `#[ignore]`d F1 tests beside them state what G1, H1 and H3 must deliver), and no `TOKEN_PATTERNS` hit in any committed set's files or response headers (every committed set, not the newest only). The stronger check — decoded Connect / base64 bodies plus the repo roots — is the CLI's `identity_violations` over every committed set (`crates/thickapi-cli/tests/capture_redact.rs`, which also gives each table entry a positive and a negative sample; `THICKAPI_FIXTURE_REWRITE=1` there re-redacts the committed sets in place). Vendors without a set skip with a note. Live: `THICKAPI_LIVE=1 cargo test -p thickapi-cli --test thin_vendors_live -- --ignored` (grok, agy, cursor; Claude has `claude_oauth_live` and `claude_bridge_live`). A fix is "capture, watch tests fail, fix, tests pass".
- **`thickapi models check`** / `doctor` `models:` section (§10.3 policy): fetch each vendor's live list — Anthropic `GET /v1/models` with the Claude Code token, the grok CLI proxy `cli-chat-proxy.grok.com/v1/models` with the grok token (the subscription's list, not `api.x.ai`'s paid catalogue), `agy models`, `cursor-agent --list-models` — and compare every enabled `subs/*` row's wire id by **family + version** (`claude-fable-5-1-thinking-high` → `claude-fable` 5.1; effort/mode suffixes and date stamps ignored; `thin::model_check`). A row is `[ok]` when its version is the family's newest, `[fail] … is behind: <vendor> offers <id>` when the vendor has a newer one, `[fail] … not in the <vendor> list` when the family/version is gone, `[warn] unchecked` when the vendor list could not be fetched (token expired, CLI missing — never a failure). A Cursor row whose `wire_model_with_context` (§10.3) would send a `context` parameter other than the default is `[warn] … context tier not verifiable from cursor-agent --list-models` instead of `[ok]`: the CLI prints one id per family and never a context tier, so family/version matching a live id cannot confirm which tier this row asks for (#147). Each vendor line also names families offered but not catalogued (information, not a finding). `check` exits 1 and `doctor` fails on any behind/gone row. Fix: bump `cli_model` / `vendor_model`, keep the id, restart `serve`, `sync-pi`. A unit test pins the example catalogue against the vendor lists recorded on 2026-09-17 (Claude ids re-recorded 2026-10-03, when `GET /v1/models` added `claude-sonnet-5-5`).
- `/ready` per vendor: token present/expired, CLI version, last rejection **classification**, `fixture_set` (newest set) and `fixture_warn`, `enabled`. Claude's `bridge` block names the bridge's own set the same way (`claude.bridge.fixture_set` / `fixture_warn` for `fixtures/claude-bridge/`).

### 19.12 Phases

| Phase | Deliverable | Exit |
|-------|-------------|------|
| T0 | §19 in SPEC; `transport`/`enabled`/`input` fields; `chat:write`; deps; `chat_completions` route as 501 stub; `mcp-proposal` subcommand with fixture test | `cargo test` green; route reachable |
| T1 | Claude OAuth adapter (§19.8.1) **and** Claude bridge fallback end to end; block detection + replay; `doctor` handshake reporting which path is live; `sync-pi` new shape and scopes; thin `subs` extension; Pi collapse for Claude rows | In Pi, `/model subs/claude/sonnet` runs a multi-tool edit with Pi's own `read`/`edit`/`bash` visible on whichever path `doctor` reports; Haiku completes a tool turn over OAuth; with `claude_oauth: only` a blocked model returns `502 vendor_drift` naming the block; a two-`tool_use` message yields two `tool_calls` on both paths; `curl` non-stream works |
| T2 | Grok adapter; Grok rows leave `(subscription)` | Tool-calling turn in Pi on `subs/grok/grok-4.7` |
| T3 | Antigravity adapter; Gemini rows leave `(subscription)` | Same on `subs/agy/gemini-3.8-flash` |
| T4 | Cursor: Windows token spike first, then adapter; may end as `enabled: false` if the wire keeps moving | Same on `subs/cursor/auto`, or a documented deferral |
| T5 | **Done 2026-09-17.** `(thickapi)` lane and bridge-library loading removed from `subscription.ts`; `extensions/pi/` deleted and its package dropped from Pi `settings.json`; `sync-pi` drops `providers.thick` and revokes `pi-sync`; `?format=pi-thick` removed; §12 rewritten; `capture` per vendor with first sets committed (claude 2.1.274, claude-bridge 2.1.274, grok 1.0.30, agy 1.2.4, cursor 2026.09.15) and replay tests | Met: `doctor` green (all four vendors `path=oauth`, bridge handshake OK); `pi --list-models` shows the six `subs/*` rows and the `(api)` lane only; `pi -p --model subs/grok/grok-4.7` answers |

### 19.13 Verification (T1 minimum)

```bash
thickapi doctor                                   # handshake against claude.exe + mcp-proposal
curl -s http://127.0.0.1:8787/v1/models -H "Authorization: Bearer $TA"            # OpenAI list shape
curl -s http://127.0.0.1:8787/v1/chat/completions -H "Authorization: Bearer $TA" \
  -H "content-type: application/json" \
  -d '{"model":"subs/claude/fable","messages":[{"role":"user","content":"Reply OK"}]}'
# tool loop: send `tools` + a prompt that needs two reads; expect finish_reason tool_calls with 2 entries
```

Pass: Pi session shows its own tool transcript; usage non-zero on tool turns; `~/.thickapi/tmp/` empty after the turn; `keys.yaml` has no inherited ACEs; a key with `turn:write` only gets `403` on the thin route.

### 19.14 Risks kept visible

- **Claude OAuth-direct is a ToS violation** under Anthropic's Feb 2026 policy and bills extra usage (seen live 2026-09-17, and the reason for the 2026-09-28 default). The default is `claude_oauth.mode: off` (bridge only; DECISIONS, amended 2026-09-28). `prefer` and `only` remain for measuring. Under `prefer` a block falls back to the bridge and §19.15 probes it in the background. `doctor` prints the ToS line when the planned path is `oauth`.
- Claude CLI auto-updates; the bridge is a frozen port. Mitigations: upstream stamp, `doctor` handshake in T1, `capture`; §19.15 R3 falls back to the direct path on drift when it is not blocked. The double fault (blocked + drift) is the one case with no automatic recovery.
- Other OAuth-direct vendors are reverse-engineered and ToS-grey; Cursor is version-pinned and moves. Mitigations: `enabled` kill switch, per-vendor `/ready`, raw rejection logging, adapters isolated.
- Latency: Claude over OAuth is near-native; over the bridge 2–4 s spawn per model request.
- `DECISIONS.md` Thin API row (2026-09-17, amended 2026-09-28): Claude default is `claude_oauth.mode: off`.

### 19.15 Claude path reliability (R1–R6) — shipped

**Measured 2026-09-17 from Pi.** Daemon started 13:36 with `path=oauth`. The first Claude request (13:43) drew Anthropic's 400 "Third-party apps now draw from your extra usage"; the daemon marked the direct path blocked for 6 h and replayed through the bridge. Every Claude turn after that — Sonnet and Opus, a five-step `read → write → edit → bash → bash` chain, a Pi-only `scout` extension tool — completed on the bridge with correct answers and non-zero `cache_read`. Pi saw nothing. The failure is not correctness; it is **predictability and visibility**: a user request pays the failed round-trip that discovers the block, a daemon restart forgets the block and pays again, the flip is invisible in Pi, the reason lives only in a console window, and the bridge has no way back to the direct path if `claude.exe` drifts on a blocked day.

**Decisions.**

- **Default `claude_oauth.mode: off` (2026-09-28, #137).** Anthropic now answers the direct OAuth request with 400 `invalid_request_error` "classed as third-party app ... draw from your extra usage": the direct path bills extra usage, not the plan. Decision (maintainer): Claude rows draw on the plan through the bridge only, never extra usage. `off` routes every Claude row to the bridge, runs no probe, and never crosses to the direct path on a bridge fault. `prefer` and `only` stay for measuring; the rest of this section describes `prefer`. No row is pinned to `cli_bridge`. The bridge's 2–4 s spawn cost is accepted.
- Reliability means: (a) the daemon never spends a user request learning what a background probe could have learned; (b) a single-path failure that is not account-wide falls to the other path, in either direction; (c) the path in use is visible in Pi within one turn and in a log that outlives the console.
- Nothing here changes the wire to Anthropic. The signed billing header stays un-forged (§19.8.1); if a day comes when the direct path is always blocked, R1–R6 make that a quiet permanent bridge, not a failure.

**R1 — Persisted block state** (`thin/oauth/state.rs`, `serve`). **Shipped.**
`VendorState` for Claude is mirrored to `~/.thickapi/thin/claude_oauth_state.json`: `{ "blocked": { "reason", "since", "next_probe" } | null, "last_path", "cli_version", "updated_at" }`. Written by `write_private_atomic` on every `mark_blocked` / `mark_ok`; read once on `serve` start. A missing, malformed, or `next_probe`-in-the-past file means "not blocked" and R2 probes on start. `reason` is the classification string only — `last_rejection` bodies stay in memory, and the file holds no tokens or prompt text. `/ready` → `thin.vendors.claude.oauth.persisted: true|false`.

**R2 — Background probe replaces request-as-probe** (`thin/oauth/mod.rs`, `serve`). **Shipped (shape via H4).**
A `serve` task owns the direct-path state transitions. The probe sends the Claude Code shape: identity block, one `read` tool (doctor handshake schema), thinking enabled at `Effort::Low` (`claude_oauth.budgets.low`), `max_tokens >= Low + THINKING_HEADROOM`, prompt `OK`, against `claude_oauth.probe.model` (**default `claude-fable-5-1`**: Haiku has never been blocked, so it cannot detect the block that matters; `probe_model` stays the bridge-handshake model for `doctor`). Schedule: on `serve` start when the persisted state is not blocked (`probe.on_start`), and at `next_probe` while blocked. Outcome: HTTP 200 → `mark_ok` (direct path returns); a §19.8.1 block classification → `mark_blocked` with a fresh cooldown; any other error → state unchanged, retry after `probe.retry_minutes` (15). One probe at a time, under the OAuth `in_flight` semaphore, skipped when a real request is already in flight on the direct path (that request is the probe). Under `prefer`, the direct path is open to user traffic only when the state is not blocked — the cooldown alone does not reopen it. `probe.enabled: false` restores the T1 behaviour (next request after `next_probe` is the probe). `mode: only` is unchanged. `/ready` → `oauth.probe: { "last_at", "last_result": ok | blocked | error(<code>), "next_at" }`.
**Measured 2026-09-17 16:34 (historical):** the original probe (`max_tokens 1`, no tools, thinking disabled) returned 200 while a real tools+thinking turn drew the third-party 400. H4 closed that: `probe_sends_claude_code_shape_and_classifies_like_a_request` asserts the tool, thinking enabled at Low, and headroom.

**R3 — Cross-fallback, bridge → OAuth** (`thin/oauth/mod.rs`, `thin/claude_bridge/mod.rs`). **Built.**
`run_inner` used to fall OAuth → bridge only. The reverse: when the bridge is the chosen path and fails **before `Started`** with a *bridge fault* — `502 vendor_drift` (init validation miss, invalid protocol JSON) or `503 vendor_down` **only** when the cause is `claude.exe` unresolved, spawn failure, or MCP ready timeout (`is_bridge_fault` is a match on `BridgeFault`, not a message match: `CliUnresolved`, `Spawn`, `McpNotReady`, `Init`, `Protocol`, `InvalidJson` (and `HandoffExit`); the bridge marks the `ThinError` it renders from that variant, and `claude_bridge::is_bridge_fault` reads the mark. A Claude error result or a stderr excerpt that reads `not found` or `spawn` is not a fault, nor is a logged-out, quota, idle, or other `vendor_down`; those must not stick `drift` or trigger a second attempt) — and mode is `prefer`, `cross_fallback` is on, the token is present and unexpired, and the direct state is **not blocked at that moment** (re-read after the bridge fails, so a probe that reopened the path mid-request counts), run the direct path once. Never on 429 (plan-wide, same account), `401 vendor_login_required`, `400`, `500`, or any error after `Started` (the route has committed). The bridge is the chosen path for `mode: off`, a `cli_bridge` row, a missing or expired token, and a blocked direct state; only `prefer` crosses, and a `cli_bridge` row under `prefer` therefore may touch the token on a bridge fault. A request is served by at most two attempts, one per path; on a double failure the **bridge's** error is returned and both are logged with `request_id`. The bridge owns `bridge_state` (`ClaudeBridge::state()`, in memory; `reason` = `<code>: <message>`, `since` = first fault of the run), set by any bridge fault before `Started`, cleared by the next successful `run_bridge`, which includes the `doctor` handshake; `/ready` → `thin.vendors.claude.bridge`: `{status: "ok", skipped: {records, events, blocks}}` or `{drift: {reason, since}, skipped}`; skip totals are process-lifetime and survive `mark_ok`; `doctor` prints `bridge_state=`. The documented double fault — direct blocked **and** bridge fault — surfaces as the bridge's error (`502 vendor_drift` naming the assertion, or `503 vendor_down` naming the spawn/resolve failure) with the message `direct blocked: <reason>; bridge drift: <message>`, both when the block was already known and when the request itself discovered it and the bridge replay then faulted; `/ready` → `path_reason: "direct blocked; bridge drift: <reason>"`. `claude_oauth.cross_fallback: true` (default) switches it; `thin.vendors.claude.path` pins the bridge binary (tests point it at a missing file to stub a fault, or at the `fake-claude` stub for a process+protocol turn).

**R3 follow-up — stream vocabulary, handoff, stub (C7 C8 C15 C16).** Isolation checks (`isolation_tools` / `permissions` / `customizations` / `auth` / `mcp`, unknown Pi tool names) stay hard `502 vendor_drift`. Unknown stream-json **record**, **event**, and **content-block** types are skipped and counted (`skipped.{records,events,blocks}` on `/ready`); they must not 502 the turn. A text or thinking block left open at `message_stop` (or `result`) is closed and counted in `skipped.blocks`, not drift (#145); an open `tool_use` block still is (§19.7 **Capture**). A non-init `system` record before `system/init` (Claude Code 2.1.283 sends `system/commands_changed` first, #137) is skipped and counted the same way; any other record before init is `protocol_order` drift, and that message names the record `type` and `subtype`. The same release lists plugins built into the CLI in `init.plugins` (`path: "builtin"`, `source: "<name>@builtin"`); those are not customizations and do not trip `isolation_customizations`. After `stop_reason=tool_use` the mapper drain-and-taps only: an unexpected abort record must not drop captured `tool_calls`, and neither may a line the handoff kill tore in half — after the handoff a line that is not JSON is skipped and counted, and a protocol error after the handoff is skipped (`Mapper::accept_line`, the one path the live loop and `replay` share; audit R3). The private-path guard on proposed calls parses the arguments and walks every string value, with slashes normalised and case ignored, so a JSON-escaped Windows path (`C:\\Users\\…`) is caught (audit R10). A `claude_error` (the CLI's `result`, assistant text or `errors[]`) reaches the client and the log with the private directory removed (as written, with forward slashes, and JSON-escaped), redacted like stderr, and capped at 500 characters (audit R14). A tool handoff accepts a clean (0) child exit when `tool_use_stop` is set, not only the job-object kill code. CI: `fake-claude` (`crates/thickapi-core/src/bin/fake-claude.rs`) speaks stream-json; `tests/claude_bridge_stub.rs` runs one text turn through `run_bridge`. `--model fake-empty-once` / `fake-empty-always` make it answer with an empty `end_turn` stop (the first run in a tmp root, or every run); `tests/claude_empty_stop_bridge.rs` drives §19.16 through `ClaudeAdapter` with them. `--model fake-open-text-block` answers the text turn without its `content_block_stop`, and `fake-open-tool-block` signals `stop_reason: tool_use` with a `tool_use` block still open; `claude_bridge_stub.rs` drives both (#145), and `chat_completions_e2e.rs` streams the first through the route. Thick-lane fixture children stay `fixtures/fake-claude.sh` / `.cmd`. Claude `content_block_start|delta|stop` (text, thinking, tool_use, unknown-skip) is one machine (`thin/anthropic_blocks.rs`); the OAuth SSE parser and the bridge stream-json parser stay separate.

**R4 — Path visibility** (`thin/mod.rs`, route, `extensions/pi-thin/thickapi-subs.ts`). **Shipped (H4).**
`ThinEvent::Started` carries `path: oauth | bridge` and an optional `fallback: oauth->bridge | bridge->oauth`. The route writes `x-thickapi-path: <path>[;reason=<blocked|token_expired|mode|row>][;probe=<iso8601>]` and, when set, `x-thickapi-fallback` on every thin response — including replayed ones and pre-`Started` errors, since nothing is streamed before the path is final. `thickapi-subs.ts` reads the header in `after_provider_response`. Status bar is two segments — `path` (selected row's vendor only: `claude direct` / `claude via bridge (probe 19:43)` / `grok` …) and `quota` (only when used is above 50%, with one warning notify when that line appears; a later reading at or below 50% clears it) — neither overwrites the other; one path `notify` per flip, not per turn. The `session_start` / `model_select` `/ready` report stays as the first-turn baseline. `/ready` `thin.vendors.claude.path` / `path_reason` is the non-Haiku/default planned path; `haiku_path` / `haiku_path_reason` is the Haiku row (Haiku may stay oauth while non-Haiku is bridged; no Haiku row is catalogued). Do not drop `path`.

**R5 — Service with a log that survives** (`serve.rs`, `install-service`, `reap`). **Shipped (H6).**
`serve` adds a second `tracing` writer: daily rolling files `thin.log_dir/thickapi.<date>.log` (`tracing-appender`; default `~/.thickapi/logs/`), console output unchanged when run from a terminal. `reap` deletes files older than `thin.log_keep_days` (14). `serve` also runs reap retention on a 24 h interval. Logged at `info`/`warn` with `request_id`: path chosen and why, block detected (redacted excerpt), probe scheduled/result, cross-fallback taken, bridge drift, token-expiry hand-off (R6). No prompt text, no tool arguments, no tokens (§19.9 holds for logs). `doctor` prints the log path, the persisted state (R1) and the last probe (R2). **Access log (A2):** one line per thin request end — `thickapi_api::access` (`info`): request id, key label, model, wire model, `wire_params` (the vendor parameters the adapter actually sent, `k=v` comma-joined in send order, e.g. `context=256k,reasoning_effort=low,fast=false`; `-` when none — Cursor always fills it, Grok `reasoning_effort`, agy `thinking_level`/`thinking_budget`, Claude direct `thinking_budget`/`thinking=disabled`; the Claude bridge logs `-`; names and values only, never prompt text or tool arguments), path, status (the HTTP status sent, or `200` plus `stream_error` for a mid-stream frame), time to first event (`ttfe_ms`), duration, `finish_reason`, usage (`prompt_tokens`/`completion_tokens`/`cached_tokens`), error `code`, `disconnect`, `tools_out` (the vendor-side tool names the model emitted, comma-joined in order, `Edit,Read`; `-` when it emitted none; a dialect turn names `Edit` not the Pi name `edit`, and a TodoWrite round names `TodoWrite` not `todo`; a non-dialect vendor logs the client names) and `spawn_ms` (bridge child spawn to the first stream-json event, milliseconds; `null` when this path did not spawn). Exactly one line per request, including a client that drops the body mid-stream and the route's own `start_deadline_seconds` 503 (§19.4) — a test fails on zero or two. Every `ThinEvent::Error` arriving after `Started` additionally gets a `warn` (`target: thickapi_api`) with `request_id`, `model`, `wire_model`, `path`, `code` and the (already redacted, client-safe) `message`; a pre-`Started` error keeps only its adapter's own `warn` line above, not this one too.

**R6 — Token-expiry hand-off is named, not silent** (`thin/oauth/mod.rs`). **Shipped (H4).**
When `prefer` routes to the bridge because the OAuth token expired (§19.8: the bridge run refreshes `.credentials.json`), log it with `expires_at`, tag the response `x-thickapi-path: bridge;reason=token_expired` (R4), and re-read the token after the bridge run to log the new expiry. `/ready` `path_reason` already says "token expired; the bridge run refreshes it" — unchanged.

**Not catalogued: `subs/claude/haiku`** — dropped 2026-09-28 (A4). The Haiku wire exception above (`haiku_path` / `model_is_haiku`) is a classification rule, not a row: it applies whenever a Claude request names a Haiku model, catalogued or not.

**Out of scope.** A resident bridge child (`--input-format stream-json` session reuse instead of kill-per-`tool_use`) stays out. Forging `x-anthropic-billing-header`: never (§19.8.1). An empty assistant stop while tools were offered is §19.16, not a path fallback.

**Config (`router.yaml`), all defaults shown:**

```yaml
claude_oauth:
  mode: off                            # default since 2026-09-28 (#137); prefer | only for measuring
  retry_after_hours: 6
  probe_model: claude-haiku-4-5        # doctor bridge handshake (existing)
  probe:
    enabled: true
    model: claude-fable-5-1            # non-Haiku, or it cannot see the block
    on_start: true
    retry_minutes: 15
  cross_fallback: true
thin:
  log_dir: ~/.thickapi/logs
  log_keep_days: 14
  reply_timeout_seconds: 30            # probe, list_models, loadCodeAssist, a rejected body (0 = off)
  vendors:
    claude:
      # path: C:/path/to/claude.exe    # bridge binary; default resolved from PATH (was thin.claude_path)
      budgets: { first_event_seconds: 120, idle_seconds: 180 }   # the direct path
```

**Verification.**

- Unit: state file round-trip (write, read, malformed, `next_probe` past → not blocked); probe state machine against a stand-in `base_url` (200 → ok; third-party 400 → blocked with new cooldown; 529 → unchanged, retry scheduled); cross-fallback with a bridge stub returning `vendor_drift` and a stand-in 200 → one answer, `x-thickapi-fallback: bridge->oauth`, at most two attempts; `Started.path` on both paths. Classification corpus: named blocks, validation paths, bare-`model` suspected block, first 429 not a block, ToS-worded 429 is a block, Haiku never `Blocked`. Mapper: unknown record/event/block skipped; abort after tool handoff keeps `tool_calls`; `is_bridge_fault` a table over every `BridgeFault` variant (a Claude error reading `not found` is not a fault); clean handoff exit 0 accepted when `tool_use_stop`. `/ready` shows both Claude planned paths when they differ.
- e2e (`chat_completions_e2e.rs`): `x-thickapi-path` present on success and on pre-`Started` errors; a blocked-then-replayed request carries `oauth->bridge`. Process: `claude_bridge_stub.rs` against `fake-claude`.
- Live (`THICKAPI_LIVE=1`): block the direct path (`mode: only` against a rejecting stand-in, or a real blocked day), restart `serve`, `/ready` shows the persisted block and a scheduled probe; `doctor` prints state, probe and log path; the log file has the block line.
- Pi: `/model subs/claude/fable`, one turn; status bar names the path; force a flip (edit the state file's `next_probe` to now, wait for the probe) and the bar changes within one turn with a single notify.

**Phases.**

| Phase | Deliverable | Exit |
|---|---|---|
| R1+R2 **(shipped)** | state file; probe task; prefer-mode direct-path gate; `/ready` fields; `doctor` names the file and probe | restart while blocked → still bridge, probe scheduled |
| R2 follow-up **(shipped, H4)** | probe sends identity + `read` tool + thinking at Low + headroom | `probe_sends_claude_code_shape_and_classifies_like_a_request` |
| R3 **(shipped)** | reverse fallback; `bridge_state`; double-fault reason; `cross_fallback` switch; `thin.claude_path` | Met in `chat_completions_e2e.rs` 2026-09-17 |
| R3 follow-up **(#60)** | skip-and-count vocabulary; drain after handoff; `is_bridge_fault` by cause; clean handoff exit; `fake-claude` stub; rule table + 429 corroboration + Haiku-aware block; one Anthropic block machine; dual `/ready` Claude paths | mapper + classify corpus + `claude_bridge_stub` |
| R4+R6 **(shipped, H4)** | `Started.path`; headers; extension status/notify; expiry hand-off log | Pi status bar flips within one turn, one notify |
| R5 **(shipped, H6)** | rolling log; `reap` retention; `doctor` lines; daily reap interval | block/probe lines in `thickapi.<date>.log` |

### 19.16 Empty Claude tool stop

**Measured 2026-09-23 on a live daemon**, `subs/claude/fable`, direct path, `reasoning_effort: low`. A prompt that told the model to choose between the thin names (`read_file`, `glob`, `grep`, `write_file`, `edit_file`, `bash`) and the Claude CLI names (`Read`, `Glob`, `Grep`, `Write`, `Edit`, `Bash`), while the request offered only the thin six, completed in 1.1 s as `finish_reason: stop` with no text and no tool call (`chatcmpl-30ad31f4734c497f8844d3434f538478`). The same body again completed in 2.7 s the same way (`chatcmpl-ef23a7868cdb47fabc044aa605589891`). The SSE body was the route's opening assistant chunk (`delta.content: ""`), then a finish chunk, then `data: [DONE]`. No `reasoning_content`, no `tool_calls`, no `error`, no `oauth rejected` log line. The same model and the same six tools, with the user message `Call read_file with path seed.txt.`, returned `finish_reason: tool_calls`. The daemon had treated the empty stop as a finished completion, so the client ended the tool loop.

**Failure.** A client that offered tools cannot tell "the model chose to stop" from "the model returned nothing". Pi records a finished turn and does not call a tool. The fix is to make that outcome an error after one same-path retry, not to change the prompt the client sent.

**Definition.** A Claude completion is an *empty tool stop* when all of these hold:

- The request's `tools` array is non-empty.
- No forwarded text delta contains a non-whitespace character.
- No forwarded reasoning delta contains a non-whitespace character.
- No tool call was forwarded.
- The vendor stop is not `tool_use` and not `max_tokens`. Anthropic `end_turn`, `stop_sequence`, `refusal`, and any other reason the mapper already collapses to `FinishReason::Stop` (§19.8.1 `message_delta`) all qualify. `max_tokens` stays `finish_reason: length`. A `tool_use` stop stays `tool_calls`.

Whitespace-only text is empty. A reasoning-only reply is not empty. A stop that includes any non-whitespace text and no tool call stays a normal stop: the model declined to call a tool in words. A request with no `tools` is unchanged, including an empty stop.

**Hold-all (`tool_choice: required`, and a named choice).** The verdict above commits on the first non-whitespace text. These two choices must not: the client asked for a tool call, so text and reasoning are held with the usage until a tool call arrives or the attempt stops. A tool call flushes everything held, in order, and the attempt is kept. A `stop` with no tool call drops the attempt (its text included; nothing from it was streamed) and retries once on the same path. The retry's request has one user record appended: `You must call one of the available tools now.` A second miss is the same `502 empty_completion` envelope, with message `tool_choice required: no tool call after retry`. A named choice is this verdict on a catalog of one tool (§19.3).

**Where (built, audit R23).** Both Claude paths, because both can end a Messages turn with `stop` and no blocks. One guard, `thin/empty_stop.rs` (`run_guarded`), sits on the channel between a path's run and the route; `ClaudeAdapter` wraps each path in it (`run_direct` for OAuth, `run_bridge_guarded` for the bridge), so neither mapper learns the policy. The guard forwards `Started` at once (the route commits its 200 headers; no chunk is written for it) and holds back what could still be an empty answer: whitespace-only text and reasoning deltas, `Usage`, and the finish. The first non-whitespace text or reasoning delta, or a tool call, flushes what it held; after that every event passes. A `Finish(stop)` with nothing forwarded while the request has `tools` is the empty tool stop. The check looks at the request that was sent (`ThinRequest` has `tools`). `chat_completions/stream.rs` already turns `ThinEvent::Error` into the error frame and withholds `[DONE]`; it does not learn a second policy. Non-stream uses the same events (`collect.rs`).

**Retry.** When the attempt is an empty tool stop and nothing from it has been sent on the route channel (no text, no reasoning, no tool call, no finish chunk, no role chunk), drop that attempt — including its usage — and run the same path once more with the same body. One retry, not a loop. OAuth retries OAuth. Bridge retries the bridge. Do not cross paths. §19.15 R3 already refuses a path change after `Started`, and `message_start` has happened by the time the stop is known. A second path would also hide a prompt whose tool names are not the ones on the request. `tool_choice: required` is a separate verdict (hold-all, above), not a substitute for this retry.

If the retry is not an empty tool stop, the client sees only the retry: one `Started` (the retry's own is dropped), and the first attempt's usage is gone. No `x-thickapi-fallback` (the path did not change). The dropped attempt is logged at `warn` with `request_id`, model, wire model, path, `finish_reason` (the mapped `stop`: the guard sees events, not the raw Anthropic `stop_reason`), `output_tokens` and the attempt number. The retry keeps its concurrency slot: OAuth reruns under the same `in_flight` permit, and the bridge holds its spawn slot across both runs (`ClaudeBridge::acquire_slot`, then `run_bridge_attempt(.., Some(n))`). The bridge retry's private directory is `<request_id>-r1`: `PrivateDir::create` refuses an existing name, and `trace` keeps the first. A retry that fails after the first attempt's `Started` reached the route reports that failure on the channel (an error frame); it never crosses paths — an OAuth block on the retry is recorded (`mark_blocked`) but not replayed on the bridge, and a bridge fault on the retry does not fall to the direct path.

If the retry is also an empty tool stop, or any delta from the first attempt was already sent (so a retry would splice two answers), do not retry. Emit `ThinEvent::Error` and do not emit `Finish`.

**Error.** HTTP 502, OpenAI `type: server_error`, `code: empty_completion`. Not `vendor_drift` — `doctor` and a fixture refresh do not fix it. `should_retry: false` (`no_retry`, and the folded `should_retry=false` in `message`, §19.4). `message` names the vendor model and the installed CLI version when the bridge knows it, for example `claude-fable-5-1: empty stop while tools were offered`. `hint`: `name only tools present on this request`. No prompt text and no tool arguments in the message, the hint, or the log (§19.9, §19.15 R5). **The 502 status is a non-stream answer only:** a streamed request has already had its 200 headers when the first attempt's `Started` arrived, so a stream carries the same envelope (`status` 502 inside the frame) as its error frame and then closes. No assistant role chunk, no `finish_reason`, no `[DONE]`.

**Not this error.** A block classification (§19.8.1) still falls OAuth → bridge. A bridge fault before `Started` still falls bridge → OAuth under R3. Idle, 429, 401, and `max_tokens` are unchanged. Thick turns (`POST /v1/agent/turn`) are unchanged: the CLI owns that loop.

**Tests.**

- OAuth mapper, stand-in SSE: `message_start`, `message_delta` `stop_reason=end_turn` with `output_tokens`, `message_stop`, request has one tool. First read is that stream; second read is a `tool_use` block. The events the route would send are the tool call and `finish_reason: tool_calls` only. The warn line carries the first stop reason.
- Same fixture twice: the route sees one `empty_completion` error, `should_retry` false, and no `[DONE]`.
- A `text_delta` of `No.` plus `end_turn`, tools present: `finish_reason: stop`, no error.
- A whitespace `text_delta` plus `end_turn`, tools present: empty tool stop.
- A thinking delta plus `end_turn`, no text, tools present: not empty; `reasoning_content` is forwarded and the finish is `stop`.
- `stop_reason=max_tokens` and no text, tools present: `finish_reason: length`, no error, no retry.
- No `tools` on the request, `end_turn`, no text: `finish_reason: stop`, no error.
- Bridge capture: a stream-json turn that reaches a non-tool, non-max stop with no text blocks is the same error after one stub retry. `stop_reason=tool_use` is unchanged.
- Hold-all: `tool_choice: required` with a text stop, then a tool call on the retry, shows the client only the retry (the text is not streamed). Two text stops are `502 empty_completion` with message `tool_choice required: no tool call after retry`.

**Out of scope.** Rewriting client prompts. Forcing `tool_choice`. A third attempt. Treating "text and no tool call" as a failure. Grok, agy, and Cursor. The thick CLI turn.
