# thickAPI thin lane for Pi (`subs`)

`thickapi-subs.ts` is the small Pi extension SPEC §19.10 keeps. `thickapi sync-pi` writes
`providers.subs` into `~/.pi/agent/models.json` as a plain `openai-completions` provider on
`http://<router.yaml listen>/v1` (default `http://127.0.0.1:8787/v1`) with a `chat:write` key.
Pi keeps its own system prompt, tools, skills and compaction, and thickAPI is only the model.

Pi works with `models.json` alone. This extension is optional: it re-registers `subs` with a
thin `streamSimple` wrapper that adds:

- `/ready` probe on `session_start` and `/model`; per-vendor path (`oauth` | `bridge`) in the
  status bar;
- on `ECONNREFUSED` one `thickapi serve` spawn (binary and config dir from the
  `~/.pi/agent/thickapi-subs.json` sidecar `sync-pi` writes, or `THICKAPI_EXE`; an absolute path
  only, never a bare `thickapi` from PATH), then
  "daemon not running (started it, retry)";
- 401 → "stale ta_ key: run `thickapi sync-pi`" (the daemon re-reads `keys.yaml` when its mtime
  changes, so no daemon restart); 403 → "key lacks chat:write";
- `x-thickapi-rate-limit` → "thickAPI quota NN%" in the status bar, warning at 80 %.

## Install

```powershell
thickapi sync-pi                      # daemon must be up; reuses a live pi-thin key, or mints one; writes models.json + sidecar
$lib = "$env:USERPROFILE\.pi\agent\lib\thickapi-subs"
New-Item -ItemType Directory -Force -Path $lib | Out-Null
Copy-Item extensions\pi-thin\thickapi-subs*.ts $lib
# Do not copy the modules into ~/.pi/agent/extensions/ — Pi loads every .ts there.
```

Then list `$lib\thickapi-subs.ts` in `~/.pi/agent/settings.json` → `extensions`. If another
extension of yours unregisters providers, list this one after it. Restart Pi.
`pi --list-models subs` shows the thin rows — the newest of each family (SPEC §10.3):
`claude/fable`, `claude/opus`, `claude/sonnet`, `grok/grok-4.7`, `agy/gemini-3.8-flash`,
`agy/gemini-4-argon`, `agy/opus`, `cursor/fable`, `cursor/grok-4.7`, `cursor/auto`,
`cursor/composer-2.5`, `codex/gpt-sol`, `codex/gpt-astra`, `codex/gpt-luna`. Rows whose vendor
CLI is not logged in still list; their requests answer `401 vendor_login_required`. Re-run
`thickapi sync-pi` after the catalogue is bumped.

Pi ≥ 0.85: `openAICompletionsApi` comes from `@earendil-works/pi-ai/compat`, not the root.

## Tests

```bash
cd extensions/pi-thin && bun test
```

## The thick lane is gone

`extensions/pi` (provider `thick`, `POST /v1/agent/turn`, daemon owns tools) retired at T5.
`sync-pi` removes a leftover `providers.thick` block and revokes its `pi-sync` key.
`/v1/agent/turn` itself stays for `thickapi turn` and scripts.
