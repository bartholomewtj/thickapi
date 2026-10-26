# thickAPI

**thickAPI is a local daemon that turns the coding subscriptions you already pay for into one OpenAI-compatible model API.** Your editor, agent harness or script keeps its own prompts, tools and session. thickAPI is only the model.

```
your client ──POST /v1/chat/completions──▶ thickapi (127.0.0.1:8787) ──▶ Claude Code · Grok · Antigravity · Cursor · Codex
   (Pi, an OpenAI SDK, curl)                  one key, one catalogue        (your logged-in subscription CLIs)
```

- **Contract:** [`SPEC.md`](SPEC.md) — the single source of truth.
- **For coding agents:** [`AGENTS.md`](AGENTS.md).

> **Notice.** thickAPI is an independent project. It is not affiliated with or endorsed by Anthropic, xAI, Google, Anysphere (Cursor) or OpenAI. It uses **your own** logged-in subscriptions on **your own** machine. The Claude path drives the official `claude` CLI. The Grok, Antigravity, Cursor and Codex paths, and the optional direct Claude path, call vendor endpoints with the token each vendor's CLI stored. Those endpoints are not public APIs, and using them this way may breach a vendor's terms. Read each vendor's terms before you turn a path on. You are responsible for how you use it. MIT licence, no warranty.

## The problem

A coding subscription (Claude Code, Grok, Cursor, Antigravity, Codex) is a logged-in CLI, not an HTTP model. Each vendor has its own spawn flags, stream format, tool protocol and thinking knobs. If you want an agent harness, a script or a pipeline of agents to use those seats, you end up maintaining an adapter per vendor. A thin chat proxy still cannot answer tool calls the way your harness expects.

thickAPI puts those adapters in one place, on your machine, behind the contract every OpenAI client already speaks.

## What it does

One Rust binary, `thickapi`, bound to localhost (`127.0.0.1:8787` by default). It has two surfaces:

| | Thin — the model (main use) | Thick — the whole turn |
|---|---|---|
| Route | `POST /v1/chat/completions` | `POST /v1/agent/turn`, or `thickapi turn` in-process |
| Who runs tools | **Your client.** A tool call comes back as `finish_reason: tool_calls`; you run it and send the next request | The vendor CLI, inside a `cwd` you name |
| You send | messages, tools, `reasoning_effort` | prompt, `cwd`, session, optional write policy |
| You get back | OpenAI chunks or a `chat.completion` | SSE events, then a persisted result you can reconnect to |
| Key scope | `chat:write` | `turn:write` (HTTP only) |

The thick lane is frozen: it gets bug fixes, not features. New work goes on the thin surface.

What you get on the thin surface:

- **One client for every seat.** Point anything that speaks `/v1/chat/completions` at the daemon. The model id is the only switch.
- **Thinking as a request field.** `reasoning_effort` (`off` … `max`) maps to each vendor's own knob (SPEC §19.5).
- **Tools stay yours.** Your function schemas go to the vendor. The vendor's tool calls come back as OpenAI `tool_calls`. On the Claude bridge, Claude Code sees your tools under its own names (`Read`, `Edit`, `Bash` …) and the daemon translates back (SPEC §19.7).
- **Honest errors.** One OpenAI error envelope with a stable `code` (`vendor_login_required`, `rate_limit_error`, `insufficient_quota`, `vendor_drift`, …) plus a `hint` that names the `thickapi doctor` command to run.
- **A stable catalogue.** One row per model family, always pointed at that family's newest version. `thickapi models check` tells you when a row has fallen behind.
- **Local ops.** Loopback by default, hashed `ta_` keys, single-instance lock, per-vendor timeouts and cooldowns, a `/ready` that says which path each vendor will take, and an access log without prompt text.

## Requirements

- **Windows 10/11.** That is where it is built and tested, and CI runs `windows-latest`. Linux and macOS should build (there are Unix code paths) but have not been verified.
- **Rust**, version pinned in `rust-toolchain.toml` (rustup picks it up).
- **At least one vendor CLI, installed and logged in**, for the subscriptions you have:

| Vendor | CLI to install and log in | Path | Rows |
|---|---|---|---|
| Anthropic Claude (Pro/Max) | `claude` (Claude Code) | bridge: spawns `claude` per request | `subs/claude/fable`, `opus`, `sonnet` |
| xAI Grok | `grok` | direct, with the CLI's token | `subs/grok/grok-4.7` |
| Google Antigravity | `agy` | direct, with the CLI's token | `subs/agy/gemini-3.8-flash`, `gemini-4-argon`, `opus` |
| Cursor | `cursor-agent` | direct, with the CLI's token | `subs/cursor/fable`, `grok-4.7`, `auto`, `composer-2.5` |
| OpenAI Codex (ChatGPT plan) | `codex` | direct, with the CLI's token | `subs/codex/gpt-sol`, `gpt-astra`, `gpt-luna` |

thickAPI never logs in for you and never refreshes a vendor token. Log in with the vendor's CLI. A row whose CLI is not logged in answers `401 vendor_login_required`. Set `enabled: false` on rows you will not use (below).

- **Optional:** [Pi](https://github.com/earendil-works/pi) as the harness (`thickapi sync-pi`), and [bun](https://bun.sh) for the Pi extension's tests and the scripts in `scripts/`.

## Install and first run

```bash
cargo install --path crates/thickapi-cli    # puts `thickapi` on PATH
thickapi keys create --label dev            # prints a ta_… key once; scopes chat:write + models:read
thickapi serve                              # first run copies configs/*.example into ~/.thickapi/
```

In a second terminal:

```bash
thickapi doctor            # every vendor: CLI found, token present, path, fixture set; one 1-token call per direct vendor
thickapi doctor --offline  # same checks without spending a request
thickapi models list       # the catalogue
```

## Use it

### curl

```bash
export TA=ta_...   # from `keys create`
curl -s http://127.0.0.1:8787/v1/models -H "Authorization: Bearer $TA"
curl -s http://127.0.0.1:8787/v1/chat/completions -H "Authorization: Bearer $TA" \
  -H "content-type: application/json" \
  -d '{"model":"subs/claude/sonnet","reasoning_effort":"low","messages":[{"role":"user","content":"Reply OK"}]}'
```

### Any OpenAI SDK

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8787/v1", api_key="ta_...")
reply = client.chat.completions.create(
    model="subs/grok/grok-4.7",
    messages=[{"role": "user", "content": "Name one prime above 100."}],
)
print(reply.choices[0].message.content)
```

Send `tools` as usual. When the answer is `finish_reason: "tool_calls"`, run the tool yourself, append a `tool` message and call again. That is the whole loop.

### Pi

```bash
thickapi sync-pi              # writes providers.subs into ~/.pi/agent/models.json (one chat:write key)
pi -m subs/claude/fable       # Pi keeps its own prompt, tools, skills and compaction
```

`/thinking` in Pi maps to `reasoning_effort`. The optional extension in [`extensions/pi-thin/`](extensions/pi-thin/README.md) adds a `/ready` probe, daemon auto-start and clearer error hints.

### One-shot turn (thick, no daemon needed)

```bash
thickapi turn --cwd . --model subs/grok/grok-4.7 --stream "summarize the failing test"
```

This runs in-process and spawns the vendor CLI in `--cwd`. Over HTTP, the same turn is `POST /v1/agent/turn` with a `turn:write` key (`keys create --label scripts --scope turn:write --scope models:read`), and `enforce_policy` can roll back writes outside an allow-list (SPEC §1.5, §7).

## Configuration

Everything lives in `~/.thickapi/` (Windows: `%USERPROFILE%\.thickapi`). The first `serve` copies the examples from `configs/`. An unknown key in any of the YAML files is a load error that names it.

| File | Holds |
|---|---|
| `router.yaml` | `listen`, `lan`, timeouts, CORS, `claude_oauth`, per-vendor knobs under `thin.vendors.<id>` (SPEC §10.1) |
| `models.yaml` | the catalogue: row id, backend, wire id, context window, `enabled`, aliases (SPEC §10.3) |
| `accounts.yaml` | one `cli_login` account per pool, `max_concurrent`, cooldown overrides (SPEC §10.2) |
| `combos.yaml` | thick-lane failover chains; a paid OpenRouter tail runs only with `allow_paid` and under `max_paid_usd_per_day` (SPEC §10.4) |
| `keys.yaml` | hashed `ta_` keys; re-read when the file changes, so `keys create` / `keys revoke` need no restart |

- **A row you will not use:** set `enabled: false` in `~/.thickapi/models.yaml`, restart `serve`, and run `sync-pi` again if you use Pi.
- **Port:** set `listen` in `router.yaml`. `lan: false` (the default) refuses any non-loopback host. `lan: true` allows `0.0.0.0`; use one key per remote client.
- **A vendor shipped a new model version:** `thickapi models check` reports `is behind: <vendor> offers <id>`. Bump `cli_model` / `vendor_model` in `models.yaml`, keep the row id, restart `serve`, then run `sync-pi`.

### The Claude path

Claude rows go through the **bridge**: one real `claude` process per request, with your tools exposed to it through a proposal-only MCP server. The CLI proposes tool calls and never executes them. This is the default (`claude_oauth.mode: off`). `prefer` and `only` call Anthropic directly with the Claude Code token. Anthropic bills that as extra usage and treats it as third-party use, so those modes exist only for measurement (SPEC §19.2, §19.15). `/ready` and `doctor` name the path each request will take.

## Run it as a service (Windows)

```bash
thickapi install-service              # serve at logon + a daily `reap` of old artifacts and logs
```

Start it with that task, not with a redirected `Start-Process`: a redirected start dies with its shell and leaves the lock file behind. Only one `serve` runs at a time.

## Troubleshooting

| You see | Do |
|---|---|
| `401 authentication_error` | The `ta_` key is wrong or revoked. With Pi, run `thickapi sync-pi` |
| `401 vendor_login_required` | Log in with that vendor's CLI (`claude login`, `grok login`, run `agy` once and sign in, `cursor-agent login`, `codex login`) |
| `402 insufficient_quota` | The subscription is out of usage. The message is the vendor's own |
| `429 rate_limit_error` | Throttled. Honour `Retry-After` |
| `502 vendor_drift` | The vendor changed its wire or CLI. Run `thickapi doctor`, then see "Vendor wire changed" in `AGENTS.md` |
| `503 vendor_down` | Vendor unreachable, CLI missing, or a turn that stalled past its budget |

`GET /ready` (with a key) shows every vendor's token state, CLI version, path, cooldowns and last rejection. Logs are in `~/.thickapi/logs/`. One access line per request, without prompt text.

## Develop

```bash
just gate                                   # what CI runs: fmt --check, clippy -D warnings, cargo test, bun test
cargo test --workspace                      # no vendor CLIs or network needed
bun scripts/verify-all.ts                   # gate + doctor --offline + HTTP probes on a spare port
THICKAPI_LIVE=1 cargo test -- --ignored     # live tests against your real CLIs (spends requests)
```

`scripts/tool-smoke.ts`, `claude-knobs.ts` and `claude-soak.ts` drive a **running** daemon through Pi and spend real requests. They are never in CI. `scripts/README.md` has the details.

Read `AGENTS.md` before changing code. It has the invariants and the wire-drift workflow.

## Layout

| Path | What |
|---|---|
| `crates/thickapi-core/` | types, config, thin adapters (`src/thin/`), thick engine, enforce, router, store, trust |
| `crates/thickapi-api/` | axum HTTP surface (`/health`, `/ready`, `/v1/models`, `/v1/chat/completions`, `/v1/agent/turn`) |
| `crates/thickapi-cli/` | the `thickapi` binary and the `capture` tooling |
| `configs/` | example YAML copied to `~/.thickapi` on first run |
| `fixtures/` | redacted vendor wire captures and stream replays for the tests |
| `extensions/pi-thin/` | optional Pi extension |
| `scripts/` | verification and live smoke scripts (bun) |

## Licence

MIT. See [`LICENSE`](LICENSE).
