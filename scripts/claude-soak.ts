#!/usr/bin/env bun
/**
 * Claude soak (#227). One long Pi session per row against the LIVE daemon (the one Pi's `subs` provider points at); never
 * starts `thickapi serve`, and never edits `~/.pi/agent`. Pi runs on the real agent dir, so
 * its installed packages (the local `todo` fork the TodoWrite mapping needs) load as in daily
 * use; compaction settings go in the temp repo's project `.pi/settings.json`, which Pi merges
 * over the global file. Each row's `events.jsonl` is kept beside the report.
 *
 * Pass: `bun test` green, >=30 tool calls, compaction >=1, a subagent >=1, a TodoWrite >=1
 * (from the access log's `tools_out`), and zero `vendor_drift` / `empty_completion` / dialect
 * errors in the run window.
 *
 * Run: bun scripts/claude-soak.ts [--models a,b] [--out DIR] [--cell-seconds 1800] [--dry-run]
 *
 * `--dry-run` copies the fixture, checks it fails, writes the report shape, and does not
 * spawn Pi. Not part of verify-all or CI. The live Claude rows wait for K10.
 */
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { stripAnsi } from "./tool-smoke";

const FIXTURE = join(import.meta.dir, "..", "fixtures", "soak");
const PROMPT = `Fix every failing test in this repository. The tests are correct; the sources are not. Do not edit a test file.

Before editing, use one Explore subagent to map the code: one line, list the source files and what each exports. Wait for its report.

Keep a todo list of the files you will fix, and mark each item completed as you finish it.

Then read the sources, edit them, and run bun test until it is green. Reply with one line: SOAK GREEN.`;

export type SoakReport = {
  model: string;
  verdict: "pass" | "fail" | "dry-run";
  steps: number;
  compactions: number;
  wall_s: number;
  spawn_ms_p50: number | null;
  spawn_ms_p95: number | null;
  cached_over_prompt: number | null;
  tool_calls: number;
  subagent: number;
  todo_write: number;
  tests_green: boolean;
  log_clean: boolean;
  why: string;
};

/** p50 / p95 of a list of millisecond samples. Empty is null. */
export function percentiles(xs: number[]): { p50: number | null; p95: number | null } {
  if (!xs.length) return { p50: null, p95: null };
  const s = [...xs].sort((a, b) => a - b);
  const at = (p: number) => s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
  return { p50: at(50), p95: at(95) };
}

/** Access lines for this run's request ids, without the log file's ANSI colours. */
export function accessLines(logText: string, requestIds: string[]): string[] {
  if (!requestIds.length) return [];
  return stripAnsi(logText).split(/\r?\n/).filter(
    (l) => l.includes("thin request end") && requestIds.some((id) => l.includes(`request_id=${id}`)),
  );
}

export function field(line: string, name: string): string | null {
  const at = line.indexOf(`${name}=`);
  if (at < 0) return null;
  const rest = line.slice(at + name.length + 1);
  const end = rest.search(/\s/);
  return end < 0 ? rest : rest.slice(0, end);
}

/** `spawn_ms` samples and the cache ratio across the run's access lines. */
export function timing(lines: string[]): { spawn: number[]; ratio: number | null } {
  const spawn: number[] = [];
  let cached = 0;
  let prompt = 0;
  for (const line of lines) {
    const ms = field(line, "spawn_ms");
    if (ms && ms !== "null" && ms !== "-") spawn.push(Number(ms));
    const c = field(line, "cached_tokens");
    const p = field(line, "prompt_tokens");
    if (c && c !== "null") cached += Number(c);
    if (p && p !== "null") prompt += Number(p);
  }
  return { spawn, ratio: prompt > 0 ? cached / prompt : null };
}

/** Vendor names across the run, and whether the window logged a fault. */
export function logHealth(lines: string[], windowText: string): { tools: string[]; clean: boolean } {
  const tools: string[] = [];
  for (const line of lines) {
    const out = field(line, "tools_out");
    if (out && out !== "-" && out !== "null") tools.push(...out.split(","));
  }
  const fault = /vendor_drift|empty_completion|dialect_error=(?!-)/.test(windowText);
  return { tools, clean: !fault };
}

export function judgeSoak(r: Omit<SoakReport, "verdict" | "why">): { verdict: "pass" | "fail"; why: string } {
  const why: string[] = [];
  if (!r.tests_green) why.push("bun test not green");
  if (r.tool_calls < 30) why.push(`${r.tool_calls} tool calls, want >=30`);
  if (r.compactions < 1) why.push("no compaction");
  if (r.subagent < 1) why.push("no subagent");
  if (r.todo_write < 1) why.push("no TodoWrite in tools_out");
  if (!r.log_clean) why.push("log has vendor_drift, empty_completion or a dialect error");
  return { verdict: why.length ? "fail" : "pass", why: why.join("; ") };
}

export function reportMd(rows: SoakReport[]): string {
  const head = "| model | verdict | steps | compactions | wall | spawn p50 | spawn p95 | cache |";
  const lines = [head, "|---|---|---|---|---|---|---|---|"];
  for (const r of rows) {
    const ratio = r.cached_over_prompt === null ? "-" : r.cached_over_prompt.toFixed(2);
    lines.push(
      `| ${r.model} | ${r.verdict} | ${r.steps} | ${r.compactions} | ${r.wall_s}s | ${r.spawn_ms_p50 ?? "-"} | ${r.spawn_ms_p95 ?? "-"} | ${ratio} |`,
    );
  }
  for (const r of rows.filter((r) => r.verdict === "fail")) lines.push(`- ${r.model}: ${r.why}`);
  return lines.join("\n") + "\n";
}

/**
 * Settings whose compaction threshold is one token, so a session that does real work
 * compacts at least once. `reserveTokens` above the context window trips
 * `contextTokens > contextWindow - reserveTokens` immediately.
 */
export function forceCompaction(settingsText: string): string {
  const settings = JSON.parse(settingsText || "{}");
  settings.compaction = {
    ...(settings.compaction ?? {}),
    enabled: true,
    reserveTokens: 100_000_000,
    keepRecentTokens: 256,
  };
  return JSON.stringify(settings, null, 2);
}

function piCli(): string {
  const r = spawnSync("npm root -g", { shell: true, encoding: "utf8" });
  if (r.status !== 0) throw new Error(`npm root -g failed: ${r.stderr}`);
  const cli = join(r.stdout.trim(), "@earendil-works", "pi-coding-agent", "dist", "bundle", "cli.js");
  if (!existsSync(cli)) throw new Error(`Pi cli not found at ${cli}`);
  return cli;
}

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

/** Project settings in the temp repo. A copied agent dir would lose `npm/` and `git/`, and Pi
 * would install the published packages instead of the ones in daily use. */
function writeProjectSettings(repo: string): void {
  mkdirSync(join(repo, ".pi"), { recursive: true });
  writeFileSync(join(repo, ".pi", "settings.json"), forceCompaction("{}"));
}

type Parsed = { calls: string[]; requestIds: string[]; compactions: number; final: string };

function parseEvents(text: string): Parsed {
  const p: Parsed = { calls: [], requestIds: [], compactions: 0, final: "" };
  for (const line of text.split(/\r?\n/)) {
    if (!line.trim()) continue;
    let o: any;
    try { o = JSON.parse(line); } catch { continue; }
    if (o?.type === "compaction_end") p.compactions++;
    if (o?.type !== "message_end" || !o.message) continue;
    const m = o.message;
    if (m.role === "assistant") {
      if (typeof m.responseId === "string") p.requestIds.push(m.responseId);
      for (const c of Array.isArray(m.content) ? m.content : []) {
        if (c?.type === "toolCall") p.calls.push(String(c.name));
        if (c?.type === "text" && String(c.text ?? "").trim()) p.final = String(c.text).trim();
      }
    }
  }
  return p;
}

function accessLog(t0: number): string {
  const dir = join(homedir(), ".thickapi", "logs");
  if (!existsSync(dir)) return "";
  let text = "";
  for (const n of [0, -1]) {
    const d = new Date(t0 + n * 86_400_000).toISOString().slice(0, 10);
    const path = join(dir, `thickapi.${d}.log`);
    if (existsSync(path)) text += readFileSync(path, "utf8");
  }
  return text;
}

function runPi(cli: string, model: string, repo: string, seconds: number): Promise<string> {
  const events = join(repo, "..", "events.jsonl");
  const args = [
    cli, "-p", "--mode", "json",
    "--session-dir", join(repo, "..", "sessions"), "--session-id", randomUUID(),
    "--model", `${model}:high`, "--thinking", "high",
    "--tools", "read,write,edit,bash,grep,find,ls,todo,subagent",
    "--no-skills", "--no-prompt-templates", "--no-context-files",
    PROMPT,
  ];
  return new Promise((done) => {
    const child = spawn("node", args, {
      cwd: repo,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    const chunks: Buffer[] = [];
    child.stdout?.on("data", (b) => chunks.push(b));
    const timer = setTimeout(() => {
      if (process.platform === "win32") spawnSync("taskkill", ["/F", "/T", "/PID", String(child.pid)], { stdio: "ignore" });
      else try { process.kill(child.pid ?? 0, "SIGKILL"); } catch { /* gone */ }
    }, seconds * 1000);
    child.on("close", () => {
      clearTimeout(timer);
      const text = Buffer.concat(chunks).toString("utf8");
      writeFileSync(events, text);
      done(text);
    });
  });
}

async function soak(cli: string, model: string, seconds: number, outDir: string): Promise<SoakReport> {
  const root = join(tmpdir(), `thickapi-soak-repo-${randomUUID()}`);
  const repo = join(root, "repo");
  mkdirSync(repo, { recursive: true });
  cpSync(FIXTURE, repo, { recursive: true });
  writeProjectSettings(repo);
  const t0 = Date.now();
  try {
    const events = await runPi(cli, model, repo, seconds);
    writeFileSync(join(outDir, `events-${model.replace(/\//g, "-")}.jsonl`), events);
    const parsed = parseEvents(events);
    const test = spawnSync("bun", ["test"], { cwd: repo, encoding: "utf8" });
    const lines = accessLines(accessLog(t0), parsed.requestIds);
    const t = timing(lines);
    const pct = percentiles(t.spawn);
    const health = logHealth(lines, lines.join("\n"));
    const base = {
      model,
      steps: parsed.calls.length,
      compactions: parsed.compactions,
      wall_s: Math.round((Date.now() - t0) / 1000),
      spawn_ms_p50: pct.p50,
      spawn_ms_p95: pct.p95,
      cached_over_prompt: t.ratio,
      tool_calls: parsed.calls.length,
      subagent: parsed.calls.filter((c) => c === "subagent").length,
      todo_write: health.tools.filter((n) => n === "TodoWrite").length,
      tests_green: test.status === 0,
      log_clean: health.clean,
    };
    const judged = judgeSoak(base);
    return { ...base, verdict: judged.verdict, why: judged.why };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

async function main() {
  const dry = process.argv.includes("--dry-run");
  const models = arg("models")?.split(",").map((s) => s.trim()).filter(Boolean)
    ?? ["subs/claude/opus", "subs/claude/sonnet", "subs/claude/fable"];
  const seconds = Number(arg("cell-seconds") ?? "1800");
  const out = arg("out") ?? join(homedir(), ".thickapi", "toolsmoke");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const dir = join(out, `${stamp}-soak`);
  mkdirSync(dir, { recursive: true });
  if (dry) {
    const probe = join(dir, "probe");
    cpSync(FIXTURE, probe, { recursive: true });
    const test = spawnSync("bun", ["test"], { cwd: probe, encoding: "utf8" });
    const rows: SoakReport[] = models.map((model) => ({
      model, verdict: "dry-run", steps: 0, compactions: 0, wall_s: 0,
      spawn_ms_p50: null, spawn_ms_p95: null, cached_over_prompt: null,
      tool_calls: 0, subagent: 0, todo_write: 0,
      tests_green: false, log_clean: true, why: test.status === 0 ? "fixture unexpectedly green" : "fixture fails, as it should",
    }));
    writeFileSync(join(dir, "soak.md"), reportMd(rows));
    console.log(reportMd(rows));
    console.log(`fixture bun test rc=${test.status} (want non-zero)`);
    process.exit(test.status === 0 ? 1 : 0);
  }
  const cli = piCli();
  const rows: SoakReport[] = [];
  for (const model of models) {
    const r = await soak(cli, model, seconds, dir);
    rows.push(r);
    console.log(`${model}: ${r.verdict} steps=${r.steps} compactions=${r.compactions} ${r.why}`);
  }
  writeFileSync(join(dir, "soak.md"), reportMd(rows));
  console.log("\n" + reportMd(rows));
  console.log(`wrote ${dir}`);
  process.exit(rows.every((r) => r.verdict === "pass") ? 0 : 1);
}

if (import.meta.main) {
  main().catch((e) => {
    console.error(String(e));
    process.exit(2);
  });
}
