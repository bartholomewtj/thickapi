# thickAPI scripts

| Script | Purpose |
|--------|---------|
| `verify-all.ts` | Full verification suite (SPEC §14, §17): fmt, clippy, tests, doctor --offline, HTTP probes, pi-thin `bun test`. Run after build or on demand. |
| `tool-smoke.ts` | All-model tool smoke (#179): one low-effort Pi turn per `subs` row and mode (`named`, `session`, `plain`) against the **live** daemon; each cell must call all eight tools and leave `src/out.txt` right. `bun scripts/tool-smoke.ts [--models a,b] [--modes named,session,plain] [--repeat N] [--cell-seconds 600] [--out DIR]`; writes `matrix.md` + per-cell files under `~/.thickapi/toolsmoke/<stamp>/`; exits 1 on any failed cell. Pure parts tested by `tool-smoke.test.ts` over `fixtures/`. |

Gitignored runtime files: `verification-report.json`, `verify.log`.

`verify-all.ts` HTTP steps (`/health` `/ready`, `GET /v1/models` auth) bind an unused loopback port starting at 19787 — never the live daemon's port, never a `listen` already in use — against a temp config dir, not `~/.thickapi`. Bind failure fails the step. Required `cargo test --workspace` is not repeated per crate (env_strip, enforce, adapter, api, cli were the same coverage). Optional `THICKAPI_LIVE=1 cargo test -- --ignored` stays: `--ignored` is extra. Pi: `bun test` in `extensions/pi-thin`; retired `extensions/pi` is not skip-passed.
