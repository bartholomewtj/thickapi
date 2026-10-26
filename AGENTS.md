# AGENTS.md — working on thickAPI

Read this before you change anything. People start at `README.md`; the contract is `SPEC.md`.

## What thickAPI is

A local Rust daemon (`thickapi`) that serves the user's **subscription-backed coding agents** — Claude Code, Grok, Google Antigravity (`agy`), Cursor and Codex — as **one OpenAI-compatible model API** on `127.0.0.1:8787`.

- **Thin surface (main use):** `POST /v1/chat/completions`. The client (Pi, an OpenAI SDK, curl) owns the system prompt, the tools and the session. thickAPI is only the model: a tool call goes back to the client as `finish_reason: tool_calls`, and the client runs it. SPEC §19.
- **Thick surface (frozen):** `POST /v1/agent/turn` and `thickapi turn`. The vendor CLI runs its own tool loop in a `cwd`, optionally under a write policy with rollback. SPEC §3–§8.

How each vendor is reached (SPEC §19.2):

| Vendor | Transport | Code |
|---|---|---|
| Claude | `cli_bridge`: one `claude` process per request; the client's tools are offered through a proposal-only MCP server (`thickapi mcp-proposal`) and **never executed by `claude`** | `crates/thickapi-core/src/thin/claude_bridge/` |
| Claude (direct, opt-in) | `oauth_direct` with the Claude Code token; `claude_oauth.mode: prefer \| only`, default `off` | `thin/oauth/claude_oauth.rs` |
| Grok, Antigravity, Cursor, Codex | `oauth_direct` with the token the vendor CLI stored | `thin/oauth/{grok,antigravity,codex}.rs`, `thin/oauth/cursor/` |

What it is **not**: not a multi-user server, not an OAuth client (users log in with each vendor's own CLI), not a token refresher, not an account rotator. One `cli_login` account per pool.

## Read order for a task

1. This file: find the task in **Where to go**.
2. The `SPEC.md` section it names. Code comments cite sections as `SPEC §19.8`, and the numbers are stable.
3. The code and its tests.

`#NNN`, `PR #NNN` and slice ids (`K3`, `G1`, `R22 C5`, `D3`) in comments and SPEC are labels from the pre-public tracker. They are not links. Keep them when you edit nearby text.

## Where to go

| Task | Read / run |
|---|---|
| Thin request/response contract, error envelope | SPEC §19.3, §19.4 |
| Thinking levels (`reasoning_effort`) per vendor | SPEC §19.5; `crates/thickapi-core/src/thinking.rs` is the source of truth, not yaml `thinking` |
| Claude bridge; Claude Code tool names (`mcp__pi__Read` …) | SPEC §19.7 (**Dialect**); `thin/claude_bridge/`, `thin/dialect.rs`. Switch: `router.yaml` `thin.vendors.claude.bridge.dialect: claude_code \| off` |
| Bridge drift after a Claude Code update | `thickapi capture claude-bridge`, `thickapi capture claude-tools`, then `cargo test -p thickapi-core --test wire_fixtures`; `/ready` → `thin.vendors.claude.bridge` |
| Claude direct path, block/probe/fallback | SPEC §19.8.1, §19.15. Keep `off` as the default (`DECISIONS.md`) |
| Grok / Antigravity / Codex adapters | SPEC §19.8; `thin/oauth/` |
| Grok goes quiet during a long tool call | xAI sends a tool call whole, so generation is wire silence. Raise `thin.vendors.grok.budgets.tool_call_floor_seconds`, not `idle_seconds` (SPEC §19.4) |
| Cursor (Connect protocol, built-ins, pre-reads, loop guard) | SPEC §19.8 Cursor; `thin/oauth/cursor/` (`mapper.rs`, `request.rs`, `tests.rs`) |
| Vendor wire changed (`502 vendor_drift`, replay tests red) | **Vendor wire changed** below; SPEC §19.11 |
| A vendor shipped a new model version | SPEC §10.3: bump `cli_model` / `vendor_model` in `configs/models.yaml.example` (and your `~/.thickapi/models.yaml`), keep the row id; `thickapi models check` |
| Keys, private files, ACLs | SPEC §10.5, §19.9; `config/keys.rs`, `thin/private_dir.rs` |
| Pi provider (`sync-pi`) and the Pi extension | SPEC §12, §19.10; `crates/thickapi-cli/src/commands/sync_pi.rs`, `extensions/pi-thin/` |
| HTTP route code | `crates/thickapi-api/src/routes/` (`chat_completions/` is the thin route) |
| Thick turn engine, enforce, trust | SPEC §5–§7; bug fixes only |

## Rules

- **`SPEC.md` is the single source of truth.** A behaviour change updates the SPEC section in the same change. There are no separate architecture docs. When you bump the SPEC version, set `Cargo.toml` `workspace.package.version` to match.
- **The thick lane is frozen** (`/v1/agent/turn`, engine, enforce, trust, pools, combos, `thickapi turn`). Fix bugs a caller hits. Do not add features and do not delete it.
- **The daemon never runs a client's tool on the thin surface.** On the bridge, `claude` proposes and the client executes. Do not give `claude` real tools.
- **Claude stays bridge-only by default** (`claude_oauth.mode: off`). Anthropic bills the direct path as extra usage.
- **Vendor credentials are read-only.** Never write, refresh or copy a CLI's token store. Per-turn config overlays (MCP servers, deny rules) go in the CLI's `{cwd}` file where it has one and are restored after the turn. agy has none, so its home `mcp_config.json` is merged and restored under a lock. Never touch `~/.cursor/mcp.json` or `~/.grok/config.toml` (SPEC §6, `DECISIONS.md`). Trust seeding adds the turn's `cwd` and prunes only entries it created (SPEC §5).
- **No secrets in the repo.** Fixtures are redacted by `thickapi capture`. One pattern table, `thin::fixture_set::TOKEN_PATTERNS`, and `cargo test -p thickapi-cli --test capture_redact` guards every committed set. Do not hand-edit a capture to make it pass.
- **Every request field is honoured, ignored with an `x-thickapi-ignored` header, or rejected.** Nothing is silently misread (SPEC §19.3).
- **Every thin error uses the OpenAI envelope, and its `message` ends with ` [code=<code>]`** (SPEC §19.4). Clients classify on that marker.
- **Catalogue: the newest version of each family only.** One `subs/*` row per family, and the row id survives a version bump (SPEC §10.3; `models check` enforces it).
- **No `unwrap` / `expect` / slice indexing in library code** (workspace clippy lints, `-D warnings`). Test files opt out with `#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]`.
- **Tests never touch the user's live daemon or real vendors.** Use an ephemeral port, a temp config dir and the `fake-claude` binary. Live tests are `#[ignore]` and gated on `THICKAPI_LIVE=1`. Never stop processes by image name (`taskkill /IM thickapi.exe`); that kills the user's daemon. Kill by pid.
- **Only one `serve` runs** (lock file in the data dir; `serve` sweeps `~/.thickapi/tmp/`). Do not start a second one to test. Use `scripts/verify-all.ts`, which binds a spare port with a temp config.
- **Windows is the primary platform.** Child processes run in a Job Object with `CREATE_NO_WINDOW`. Where `cursor-agent` is spawned (thick lane, `models check`), it runs as its sibling `node.exe index.js`, because the `.cmd` shim hangs under piped stdio. Keep `#[cfg(windows)]` and Unix paths both compiling.
- **`DECISIONS.md`** records chose-X-not-Y, capped at 15 rows. Do not reverse a row without the maintainer's agreement, and propose new rows in the PR rather than adding them yourself.

## Verify

```bash
just gate                       # = CI: cargo fmt --check, clippy -D warnings, cargo test --workspace, bun test (extensions/pi-thin)
bun scripts/verify-all.ts       # gate + doctor --offline + HTTP probes on a spare port and temp config
```

A change is done when `just gate` is green. Live checks (`THICKAPI_LIVE=1 cargo test -- --ignored`, `scripts/tool-smoke.ts`, `claude-knobs.ts`, `claude-soak.ts`) spend real subscription requests. Run them only when the user asks.

## Vendor wire changed

The fix is mechanical:

1. `thickapi capture --check <vendor>`: capture into a temp dir, leak-check, replay, and diff against the newest committed set. It never writes `fixtures/`.
2. If the wire moved: `thickapi capture <vendor>` writes `fixtures/<vendor>/<cli-version>/{text,tool}/`.
3. `cargo test -p thickapi-core --test wire_fixtures` should now fail the same way the live daemon does.
4. Fix the adapter until it passes. Update SPEC §19.8 / §19.11 if the contract moved.

## Swap a running daemon

Swap only when nothing is in flight: a killed request surfaces to the client as `vendor_down`.

```powershell
cargo build --release -p thickapi-cli
Copy-Item $HOME\.cargo\bin\thickapi.exe $HOME\.cargo\bin\thickapi.prev.exe   # rollback copy
Stop-Process -Id <serve pid>
Copy-Item target\release\thickapi.exe $HOME\.cargo\bin\thickapi.exe
schtasks /Run /TN thickapi-serve                                             # the install-service task
thickapi doctor --offline; thickapi sync-pi
```

If a newer config would not load in the old binary (a new `backend:` value, say), keep a copy of `~/.thickapi/models.yaml` with the rollback exe.
