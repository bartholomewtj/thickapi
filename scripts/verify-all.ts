#!/usr/bin/env bun
/**
 * Post-implementation verification (SPEC §14, §17).
 * Run: bun scripts/verify-all.ts
 *
 * HTTP steps bind an unused loopback port (starting at 19787), never the live daemon's port
 * (live daemon), never a listen already in use, against a temp config dir —
 * not ~/.thickapi. Bind failure fails the step.
 *
 * Serve is the debug `target/debug/thickapi` binary (never `cargo run`, never
 * `--release`). Cleanup kills that pid (Windows: that pid tree), not cargo.
 * No `--ignored` / live vendor tests.
 *
 * Local gate. CI (.github/workflows/ci.yml, B2) runs fmt + clippy -D warnings + cargo test + bun test on every PR;
 * `just gate` is the same subset.
 */
import { $ } from "bun";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const REPO = dirname(dirname(fileURLToPath(import.meta.url)));
const REPORT = join(REPO, "scripts", "verification-report.json");
const LOG = join(REPO, "scripts", "verify.log");
/** First candidate listen; skip any port already in use. Never the live daemon. */
const VERIFY_LISTEN_BASE = 19787;

type Step = { name: string; ok: boolean; detail: string; required: boolean };
const steps: Step[] = [];

function log(msg: string) {
  const line = `${new Date().toISOString()} ${msg}`;
  console.log(msg);
  try {
    appendFileSync(LOG, line + "\n");
  } catch {
    /* */
  }
}

async function runStep(
  name: string,
  required: boolean,
  fn: () => Promise<{ ok: boolean; detail: string }>,
) {
  log(`VERIFY: ${name} ...`);
  try {
    const { ok, detail } = await fn();
    steps.push({ name, ok, detail, required });
    log(`VERIFY: ${name} ${ok ? "PASS" : "FAIL"} — ${detail.slice(0, 200)}`);
    return ok;
  } catch (e) {
    const detail = String(e);
    steps.push({ name, ok: false, detail, required });
    log(`VERIFY: ${name} FAIL — ${detail}`);
    return false;
  }
}

function testEnv() {
  const env = { ...process.env };
  delete env.THICKAPI_LIVE;
  return env;
}

function debugBin(): string {
  const name = process.platform === "win32" ? "thickapi.exe" : "thickapi";
  return join(REPO, "target", "debug", name);
}

let debugBuilt = false;
async function ensureDebugBuild() {
  if (debugBuilt) return;
  const r = await $`cargo build -p thickapi-cli`.cwd(REPO).env(testEnv()).nothrow();
  if (r.exitCode !== 0) {
    throw new Error(`cargo build -p thickapi-cli failed: ${r.text().slice(-500)}`);
  }
  debugBuilt = true;
}

/** Kill this pid (and its tree on Windows). Never `/IM thickapi.exe` — that would hit the live daemon. */
function killServePid(pid: number | undefined) {
  if (pid == null || pid <= 0) return;
  if (process.platform === "win32") {
    Bun.spawnSync(["taskkill", "/PID", String(pid), "/T", "/F"], {
      stdout: "ignore",
      stderr: "ignore",
    });
  } else {
    try {
      process.kill(pid, "SIGTERM");
    } catch {
      /* already gone */
    }
  }
}

function canBind(port: number): Promise<boolean> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", (err: NodeJS.ErrnoException) => {
      if (err.code === "EADDRINUSE") {
        resolve(false);
        return;
      }
      reject(new Error(`cannot bind 127.0.0.1:${port}: ${err.message}`));
    });
    server.once("listening", () => {
      server.close((closeErr) => {
        if (closeErr) {
          reject(new Error(`cannot release 127.0.0.1:${port}: ${closeErr.message}`));
          return;
        }
        resolve(true);
      });
    });
    server.listen(port, "127.0.0.1");
  });
}

/** Unused loopback port. Fail closed if none in the verify range can bind. */
async function pickUnusedPort(): Promise<number> {
  for (let port = VERIFY_LISTEN_BASE; port < VERIFY_LISTEN_BASE + 100; port++) {
    if (await canBind(port)) return port;
  }
  throw new Error(
    `no unused loopback port in ${VERIFY_LISTEN_BASE}–${VERIFY_LISTEN_BASE + 99}; fail closed`,
  );
}

function writeTestConfig(dir: string, port: number) {
  mkdirSync(dir, { recursive: true });
  const router = join(REPO, "configs", "router.yaml.example");
  if (existsSync(router)) {
    let text = readFileSync(router, "utf8");
    text = text.replace(/127\.0\.0\.1:8787/, `127.0.0.1:${port}`);
    const dataDir = join(dir, "data").replace(/\\/g, "/");
    text = text.replace('data_dir: "~/.thickapi"', `data_dir: "${dataDir}"`);
    writeFileSync(join(dir, "router.yaml"), text);
  }
  for (const f of ["accounts.yaml.example", "models.yaml.example", "combos.yaml.example"]) {
    const src = join(REPO, "configs", f);
    if (existsSync(src)) {
      writeFileSync(join(dir, f.replace(".example", "")), readFileSync(src, "utf8"));
    }
  }
}

async function mintKey(configDir: string): Promise<string> {
  await ensureDebugBuild();
  const bin = debugBin();
  const r = await $`${bin} keys create --label verify --config ${configDir}`
    .cwd(REPO)
    .env(testEnv())
    .nothrow();
  const out = r.text();
  const m = out.match(/(ta_[a-zA-Z0-9_]+)/);
  if (!m) throw new Error(`keys create failed: ${out.slice(-300)}`);
  return m[1];
}

async function prepareConfigDir(): Promise<{ dir: string; key: string }> {
  const dir = join(tmpdir(), `thickapi-verify-${Date.now()}`);
  // Listen is rewritten to an unused port immediately before serve (not here).
  writeTestConfig(dir, VERIFY_LISTEN_BASE);
  const key = await mintKey(dir);
  return { dir, key };
}

async function withServe(
  fn: (ctx: { key: string; base: string }) => Promise<{ ok: boolean; detail: string }>,
): Promise<{ ok: boolean; detail: string }> {
  const { dir, key } = await prepareConfigDir();
  const port = await pickUnusedPort();
  writeTestConfig(dir, port);
  const base = `http://127.0.0.1:${port}`;
  await ensureDebugBuild();
  const proc = Bun.spawn([debugBin(), "serve", "--config", dir], {
    cwd: REPO,
    stdout: "pipe",
    stderr: "pipe",
    env: testEnv(),
  });
  try {
    const deadline = Date.now() + 30_000;
    let up = false;
    while (Date.now() < deadline) {
      if (proc.exitCode !== null) {
        return { ok: false, detail: `serve exited before bind (code ${proc.exitCode})` };
      }
      try {
        const health = await fetch(`${base}/health`);
        if (health.ok) {
          up = true;
          break;
        }
      } catch {
        /* not listening yet */
      }
      await Bun.sleep(250);
    }
    if (!up) {
      return { ok: false, detail: `serve did not bind ${base} within 30s` };
    }
    return await fn({ key, base });
  } catch (e) {
    return { ok: false, detail: String(e) };
  } finally {
    killServePid(proc.pid);
  }
}

async function main() {
  log("VERIFY: start full suite");

  // Workspace run covers env_strip, enforce, adapter, api, and cli.
  await runStep("cargo test --workspace", true, async () => {
    const r = await $`cargo test --workspace`.cwd(REPO).env(testEnv()).nothrow();
    return { ok: r.exitCode === 0, detail: r.text().split("\n").slice(-8).join("\n") };
  });

  await runStep("cargo fmt --check", true, async () => {
    const r = await $`cargo fmt --all -- --check`.cwd(REPO).nothrow();
    return { ok: r.exitCode === 0, detail: r.text().split("\n").slice(-8).join("\n") };
  });

  await runStep("cargo clippy", true, async () => {
    const r = await $`cargo clippy --workspace --all-targets -- -D warnings`.cwd(REPO).nothrow();
    return { ok: r.exitCode === 0, detail: r.text().split("\n").slice(-5).join("\n") };
  });

  await runStep("debug build", true, async () => {
    try {
      await ensureDebugBuild();
    } catch (e) {
      return { ok: false, detail: String(e) };
    }
    const bin = debugBin();
    if (!existsSync(bin)) return { ok: false, detail: `binary missing: ${bin}` };
    return { ok: true, detail: bin };
  });

  await runStep("thickapi doctor --offline", true, async () => {
    const { dir } = await prepareConfigDir();
    const r = await $`${debugBin()} doctor --offline --config ${dir}`.cwd(REPO).env(testEnv()).nothrow();
    const out = r.text();
    const ok = r.exitCode === 0 && !out.includes("[fail]");
    return { ok, detail: out.split("\n").filter((l) => l.includes("[ok]") || l.includes("[fail]") || l.includes("[skip]")).slice(-10).join("\n") };
  });

  await runStep("HTTP /health /ready", true, async () => {
    return withServe(async ({ key, base }) => {
      const health = await fetch(`${base}/health`);
      const ready = await fetch(`${base}/ready`, {
        headers: { Authorization: `Bearer ${key}` },
      });
      const readyJson = await ready.json();
      const ok = health.ok && ready.ok && readyJson.ready === true;
      return {
        ok,
        detail: `health=${health.status} ready=${ready.status} ready_flag=${readyJson.ready}`,
      };
    });
  });

  await runStep("GET /v1/models auth", true, async () => {
    return withServe(async ({ key, base }) => {
      const noAuth = await fetch(`${base}/v1/models`);
      const withAuth = await fetch(`${base}/v1/models`, {
        headers: { Authorization: `Bearer ${key}` },
      });
      return {
        ok: noAuth.status === 401 && withAuth.ok,
        detail: `no_auth=${noAuth.status} with_auth=${withAuth.status}`,
      };
    });
  });

  await runStep("thickapi turn --help", true, async () => {
    await ensureDebugBuild();
    const r = await $`${debugBin()} turn --help`.cwd(REPO).nothrow();
    return { ok: r.exitCode === 0 && r.text().includes("stream"), detail: "cli turn subcommand" };
  });

  await runStep("pi-thin extension", true, async () => {
    const dir = join(REPO, "extensions", "pi-thin");
    if (!existsSync(join(dir, "thickapi-subs.test.ts"))) {
      return { ok: false, detail: "extensions/pi-thin tests missing" };
    }
    const r = await $`bun test`.cwd(dir).nothrow();
    return { ok: r.exitCode === 0, detail: r.text().split("\n").slice(-5).join("\n") };
  });

  const requiredFailed = steps.filter((s) => s.required && !s.ok);
  const passed = requiredFailed.length === 0;
  writeFileSync(
    REPORT,
    JSON.stringify(
      {
        at: new Date().toISOString(),
        passed,
        required_failed: requiredFailed.map((s) => s.name),
        steps,
      },
      null,
      2,
    ),
  );
  log(passed ? "VERIFY: ALL REQUIRED PASSED" : `VERIFY: FAILED: ${requiredFailed.map((s) => s.name).join(", ")}`);
  process.exit(passed ? 0 : 1);
}

main();
