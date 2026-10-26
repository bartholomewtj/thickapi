#!/usr/bin/env bun
/**
 * All-model tool smoke (#179, dialect cells #227). One low-effort Pi turn per (row, mode)
 * cell against the LIVE daemon (the one Pi's `subs` provider points at). A cell must drive every offered tool and leave the
 * right file. `--dialect` also requires each Claude alias on the access log.
 *
 * Run: bun scripts/tool-smoke.ts [--models a,b] [--modes named,session,plain,claude]
 *        [--tools a,b] [--dialect] [--repeat N] [--cell-seconds 600] [--out DIR]
 *
 * Not part of verify-all or CI: it needs the live daemon and real vendor accounts.
 * Never starts `thickapi serve`. Rows come from `pi --list-models subs`.
 * Output: <out>/<stamp>/{cells.jsonl, matrix.md} + per-cell events.jsonl, stderr.txt, result.json.
 */
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  appendFileSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

export const TOOLS = ["ls", "find", "grep", "read", "write", "edit", "bash", "todo"];
/** Claude preset: the eight, plus the two Pi tools the dialect rows replace with Agent and AskUserQuestion. */
export const CLAUDE_TOOLS = [
  "read", "write", "edit", "bash", "ls", "find", "grep", "todo", "subagent", "askuserquestion",
];
/** Vendor-side names `--dialect` requires at least once across a Claude cell. */
export const CLAUDE_ALIASES = [
  "Read", "Write", "Edit", "Bash", "Glob", "Grep", "TodoWrite", "Agent", "AskUserQuestion",
];
/** Only the `claude` prompt asks for a hand-off and a question; the eight-step prompts do not. */
const CLAUDE_ONLY_TOOLS = ["subagent", "askuserquestion"];
const CLAUDE_ONLY_ALIASES = ["Agent", "AskUserQuestion"];

/** The offered tools and aliases this mode's prompt asks for. Offered is not asked: the
 * eight-step modes still see the whole Claude preset, but cannot be failed for not handing off. */
export function expectedFor(mode: Mode, tools: string[]): { tools: string[]; aliases: string[] } {
  if (mode === "claude") return { tools, aliases: CLAUDE_ALIASES };
  return {
    tools: tools.filter((t) => !CLAUDE_ONLY_TOOLS.includes(t)),
    aliases: CLAUDE_ALIASES.filter((a) => !CLAUDE_ONLY_ALIASES.includes(a)),
  };
}
export type Mode = "named" | "session" | "plain" | "claude";
const ALL_MODES: Mode[] = ["named", "session", "plain", "claude"];

// Copied verbatim from the 29 Sep throwaway harness (toolsmoke/2026-09-29/run.py).
export const PROMPT = `This is a tool-coverage test. Work in the current directory. Do each step in order, one tool call per step, using exactly the tool named. Do not use bash for anything except step 7.

1. ls: list the directory \`src\`.
2. find: find every file matching \`*.txt\` under \`src\`.
3. grep: search \`src\` for the pattern \`MARKER-\\d+\`.
4. read: read \`src/input.txt\`.
5. write: create the NEW file \`src/out.txt\` (it does not exist yet) whose content is exactly the marker you found in input.txt, then a newline, then the word \`alpha\`.
6. edit: in \`src/out.txt\` replace \`alpha\` with \`omega\`.
7. bash: run \`wc -l src/out.txt\`.
8. todo: add one task titled \`tool smoke done\`.

When all eight steps are done, reply with one line: DONE <the marker>. Nothing else.`;

// Same eight tasks, no tool names, absolute paths (draftr-like).
export const NATURAL = `You are working in the repository at {repo}.

1. See what is in {src}.
2. Find every .txt file under {src}.
3. Search {src} for text matching the regex MARKER-\\d+.
4. Look at the full contents of {src}\\input.txt.
5. Create the new file {src}\\out.txt containing exactly the marker you found, a newline, then alpha.
6. Change alpha to omega in that file.
7. Run a shell command that counts the lines in {src}\\out.txt.
8. Add a task titled "tool smoke done" to your task list.

When done, reply with one line: DONE <the marker>. Nothing else.`;

// Same file work, plus the three calls only the Claude preset offers. No tool names:
// the dialect is what the access log must show. The ask errors under `-p`; the call still counts.
export const CLAUDE = `You are working in the repository at {repo}. Do the work yourself except where a step says to hand it off.

1. See what is in {src}.
2. Find every .txt file under {src}.
3. Search {src} for text matching the regex MARKER-\\d+.
4. Look at the full contents of {src}\\input.txt.
5. Create the new file {src}\\out.txt containing exactly the marker you found, a newline, then alpha.
6. Change alpha to omega in that file.
7. Run a shell command that counts the lines in {src}\\out.txt.
8. Keep a task list of exactly three items: "read the marker", "write the file", "confirm the line count". Mark the first completed and leave the other two pending. Set them in one update of the whole list.
9. Hand the mapping of {src} to one Explore subagent. The task is one line: list the files under {src} and report the marker. Wait for its report.
10. Ask the user which word to keep, omega or alpha, with those two as the only options. One question. Do not invent an answer.

When the ask has been made and the other steps are done, reply with one line: DONE <the marker>. Nothing else.`;

// ---------------------------------------------------------------- pure parts

export type Parsed = {
  requests: number;
  calls: string[];
  /** Pi `responseId` values (`chatcmpl-…`), the access log's `request_id`. */
  requestIds: string[];
  toolErrors: string[];
  modelErrors: string[];
  final: string;
};

/** Parse Pi `--mode json` stdout: only `message_end` events matter. */
export function parseEvents(text: string): Parsed {
  const p: Parsed = { requests: 0, calls: [], requestIds: [], toolErrors: [], modelErrors: [], final: "" };
  for (const line of text.split(/\r?\n/)) {
    if (!line.trim()) continue;
    let o: any;
    try {
      o = JSON.parse(line);
    } catch {
      continue;
    }
    if (o?.type !== "message_end" || !o.message) continue;
    const m = o.message;
    if (m.role === "assistant") {
      p.requests++;
      if (typeof m.responseId === "string" && m.responseId) p.requestIds.push(m.responseId);
      for (const c of Array.isArray(m.content) ? m.content : []) {
        if (c?.type === "toolCall") p.calls.push(String(c.name));
        if (c?.type === "text" && String(c.text ?? "").trim()) p.final = String(c.text).trim();
      }
      if (m.stopReason === "error" || m.stopReason === "aborted" || m.errorMessage) {
        p.modelErrors.push(`${m.stopReason}: ${String(m.errorMessage ?? "").slice(0, 300)}`);
      }
    } else if (m.role === "toolResult" && m.isError) {
      const txt = (Array.isArray(m.content) ? m.content : [])
        .filter((x: any) => x && typeof x === "object")
        .map((x: any) => x.text ?? "")
        .join(" ");
      p.toolErrors.push(`${m.toolName}: ${txt.slice(0, 200)}`);
    }
  }
  return p;
}

export type Verdict = {
  verdict: "pass" | "fail" | "timeout";
  missing: string[];
  file_ok: boolean;
  final_ok: boolean;
  /** Vendor aliases seen on the access log. Empty unless `--dialect` ran. */
  aliases: string[];
  /** Expected aliases absent from the cell's access lines. Empty when dialect is off. */
  alias_missing: string[];
};

/** `tools_out=Edit,Read` from one access line. `-` and a missing field are no names. */
export function toolsOut(line: string): string[] {
  const m = line.match(/tools_out=([^\s]+)/);
  if (!m || m[1] === "-" || m[1] === "null") return [];
  return m[1].split(",").map((s) => s.trim()).filter(Boolean);
}

/** The daemon's log file keeps tracing's ANSI colours (`request_id\x1b[0m\x1b[2m=\x1b[0m…`). */
export function stripAnsi(text: string): string {
  return text.replace(/\x1b\[[0-9;]*m/g, "");
}

/** Vendor aliases present on the access lines for this cell's request ids. A line with no
 * matching id is ignored, so a neighbouring cell's turn cannot cover a missing alias. */
export function aliasesIn(logText: string, requestIds: string[]): string[] {
  const seen = new Set<string>();
  if (!requestIds.length) return [];
  for (const line of stripAnsi(logText).split(/\r?\n/)) {
    if (!line.includes("thin request end")) continue;
    if (!requestIds.some((id) => line.includes(`request_id=${id}`))) continue;
    for (const name of toolsOut(line)) seen.add(name);
  }
  return [...seen];
}

/** pass = exit 0, out.txt is `MARKER-4172\nomega` (CRLF-tolerant), reply has `DONE MARKER-4172`,
 * and (named/session/claude) every offered tool called. Plain lets Cursor pick built-ins, so
 * coverage is recorded, not required. `claude` also accepts the ask as a call that errored.
 * `--dialect` adds the alias check: every expected vendor name at least once. */
export function judge(
  parsed: Parsed,
  rc: number | "timeout" | null,
  outTxt: string | null,
  mode: Mode,
  tools: string[] = TOOLS,
  aliases: string[] = [],
  expectAliases: string[] = [],
): Verdict {
  const called = new Set(parsed.calls);
  // An ask under `-p` errors and is still a call: Pi records it as a toolResult error.
  for (const err of parsed.toolErrors) {
    const name = err.split(":")[0]?.trim();
    if (name) called.add(name);
  }
  const missing = tools.filter((t) => !called.has(t));
  const file_ok = outTxt !== null && outTxt.replace(/\r\n/g, "\n").trim() === "MARKER-4172\nomega";
  const final_ok = parsed.final.includes("DONE MARKER-4172");
  const needAll = mode !== "plain";
  const alias_missing = expectAliases.filter((a) => !aliases.includes(a));
  const base = { missing, file_ok, final_ok, aliases, alias_missing };
  if (rc === "timeout") return { verdict: "timeout", ...base };
  const ok =
    rc === 0 && file_ok && final_ok && (!needAll || missing.length === 0) && alias_missing.length === 0;
  return { verdict: ok ? "pass" : "fail", ...base };
}

export type CellResult = Verdict & {
  model: string;
  mode: Mode;
  repeat: number;
  secs: number;
  rc: number | "timeout" | null;
  requests: number;
  calls: string[];
  request_ids: string[];
  tool_errors: string[];
  model_errors: string[];
  out_txt: string | null;
  final: string;
  stderr_tail: string;
};

export function matrixMd(cells: CellResult[], modes: Mode[]): string {
  const models = [...new Set(cells.map((c) => c.model))];
  const repeats = Math.max(1, ...cells.map((c) => c.repeat));
  const lines = [
    `| model | ${modes.join(" | ")} |`,
    `|---|${modes.map(() => "---").join("|")}|`,
  ];
  for (const model of models) {
    const cols = modes.map((mode) => {
      const rs = cells.filter((c) => c.model === model && c.mode === mode);
      if (!rs.length) return "-";
      return rs
        .map((c) => `${c.verdict} ${c.secs}s${c.missing.length && c.verdict !== "pass" ? ` (missing ${c.missing.length})` : ""}`)
        .join(", ");
    });
    lines.push(`| ${model} | ${cols.join(" | ")} |`);
  }
  const pass = cells.filter((c) => c.verdict === "pass").length;
  lines.push("", `${pass}/${cells.length} cells pass${repeats > 1 ? ` (${repeats} repeats)` : ""}.`);
  const covered = cells.filter((c) => c.aliases.length || c.alias_missing.length);
  if (covered.length) {
    lines.push("", "Alias coverage (`tools_out`, vendor-side names):");
    for (const c of covered) {
      const have = c.aliases.length ? c.aliases.join(",") : "-";
      const miss = c.alias_missing.length ? ` missing ${c.alias_missing.join(",")}` : "";
      lines.push(`- ${c.model} ${c.mode} r${c.repeat}: ${have}${miss}`);
    }
  }
  const bad = cells.filter((c) => c.verdict !== "pass");
  for (const c of bad) {
    const why =
      [...c.model_errors, ...c.tool_errors].slice(0, 2).join("; ") ||
      (c.alias_missing.length ? `aliases ${c.alias_missing.join(",")}` : `missing ${c.missing.join(",")}`);
    lines.push(`- ${c.model} ${c.mode} r${c.repeat}: ${c.verdict} — ${why.slice(0, 200)}`);
  }
  return lines.join("\n") + "\n";
}

// ---------------------------------------------------------------- runner

function piCli(): string {
  const r = spawnSync("npm root -g", { shell: true, encoding: "utf8" });
  if (r.status !== 0) throw new Error(`npm root -g failed: ${r.stderr}`);
  const cli = join(r.stdout.trim(), "@earendil-works", "pi-coding-agent", "dist", "bundle", "cli.js");
  if (!existsSync(cli)) throw new Error(`Pi cli not found at ${cli}`);
  return cli;
}

function listRows(cli: string): string[] {
  const r = spawnSync("node", [cli, "--list-models", "subs"], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  });
  const rows: string[] = [];
  for (const line of `${r.stdout}\n${r.stderr}`.split(/\r?\n/)) {
    const cols = line.trim().split(/\s+/);
    if (cols[0] === "subs" && cols[1] && cols[1] !== "model") rows.push(`subs/${cols[1]}`);
  }
  if (!rows.length) throw new Error("`pi --list-models subs` returned no subs rows");
  return rows;
}

function setupRepo(repo: string) {
  mkdirSync(join(repo, "src", "nested"), { recursive: true });
  writeFileSync(join(repo, "src", "input.txt"), "header\nMARKER-4172\nfooter\n");
  writeFileSync(join(repo, "src", "nested", "notes.txt"), "nothing here\n");
  writeFileSync(join(repo, "src", "main.py"), "print('hi')\n");
  spawnSync("git", ["init", "-q"], { cwd: repo, stdio: "ignore" });
}

function killTree(pid: number | undefined) {
  if (pid === undefined) return;
  if (process.platform === "win32") {
    spawnSync("taskkill", ["/F", "/T", "/PID", String(pid)], { stdio: "ignore" });
  } else {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* gone */
    }
  }
}

function accessLogText(t0: number): string {
  const dir = join(homedir(), ".thickapi", "logs");
  if (!existsSync(dir)) return "";
  // The cell may cross midnight; read today and yesterday.
  const days = [0, -1].map((n) => {
    const d = new Date(t0 + n * 86_400_000);
    return d.toISOString().slice(0, 10);
  });
  let text = "";
  for (const day of days) {
    const path = join(dir, `thickapi.${day}.log`);
    if (existsSync(path)) text += readFileSync(path, "utf8");
  }
  return text;
}

function runCell(
  cli: string,
  model: string,
  mode: Mode,
  repeat: number,
  cellSeconds: number,
  runDir: string,
  tools: string[],
  dialect: boolean,
): Promise<CellResult> {
  const cell = join(runDir, `${mode}-${model.replace(/\//g, "-")}${repeat > 1 ? `-r${repeat}` : ""}`);
  const repo = join(cell, "repo");
  mkdirSync(repo, { recursive: true });
  setupRepo(repo);
  const events = join(cell, "events.jsonl");
  const errf = join(cell, "stderr.txt");
  const sess =
    mode === "named"
      ? ["--no-session"]
      : ["--session-dir", join(cell, "sessions"), "--session-id", randomUUID()];
  const src = resolve(repo, "src");
  const prompt = (mode === "plain" ? NATURAL : mode === "claude" ? CLAUDE : PROMPT)
    .replaceAll("{repo}", resolve(repo))
    .replaceAll("{src}", src);
  const args = [
    cli, "-p", "--mode", "json", ...sess,
    "--model", `${model}:low`, "--thinking", "low",
    "--tools", tools.join(","),
    "--no-skills", "--no-prompt-templates", "--no-context-files",
    prompt,
  ];
  const fo = openSync(events, "w");
  const fe = openSync(errf, "w");
  const t0 = Date.now();
  return new Promise((done) => {
    const child = spawn("node", args, { cwd: repo, stdio: ["ignore", fo, fe], windowsHide: true });
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      killTree(child.pid);
    }, cellSeconds * 1000);
    const finish = (code: number | null) => {
      clearTimeout(timer);
      closeSync(fo);
      closeSync(fe);
      const rc: number | "timeout" | null = timedOut ? "timeout" : code;
      const secs = Math.round((Date.now() - t0) / 1000);
      const parsed = parseEvents(readFileSync(events, "utf8"));
      const outPath = join(repo, "src", "out.txt");
      const outTxt = existsSync(outPath) ? readFileSync(outPath, "utf8") : null;
      const claude = model.split("/")[1] === "claude";
      const checkDialect = dialect && claude;
      const aliases = checkDialect ? aliasesIn(accessLogText(t0), parsed.requestIds) : [];
      const expected = expectedFor(mode, tools);
      const expectAliases = checkDialect ? expected.aliases : [];
      const v = judge(parsed, rc, outTxt, mode, expected.tools, aliases, expectAliases);
      const res: CellResult = {
        ...v, model, mode, repeat, secs, rc,
        requests: parsed.requests, calls: parsed.calls, request_ids: parsed.requestIds,
        tool_errors: parsed.toolErrors, model_errors: parsed.modelErrors,
        out_txt: outTxt, final: parsed.final.slice(0, 300),
        stderr_tail: readFileSync(errf, "utf8").slice(-800),
      };
      writeFileSync(join(cell, "result.json"), JSON.stringify(res, null, 2));
      appendFileSync(join(runDir, "cells.jsonl"), JSON.stringify(res) + "\n");
      console.log(
        `${model} ${mode} r${repeat}: ${res.verdict} ${secs}s rc=${rc} missing=[${res.missing}] ` +
          `aliases=[${res.aliases}] alias_missing=[${res.alias_missing}] ` +
          `tool_errs=${res.tool_errors.length} model_errs=${res.model_errors.length}`,
      );
      done(res);
    };
    child.on("error", () => finish(null));
    child.on("close", (code) => finish(code));
  });
}

function vendorOf(model: string): string {
  return model.split("/")[1] ?? model;
}

function arg(name: string, dflt?: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : dflt;
}

function flag(name: string): boolean {
  return process.argv.includes(`--${name}`);
}

/** `--tools` list, or the Claude preset when a Claude row or the `claude` mode is in the run.
 * The default for the eight-tool modes stays the eight. */
export function toolsFor(argv: string[], modes: Mode[], models: string[]): string[] {
  const i = argv.indexOf("--tools");
  const listed = i >= 0 ? (argv[i + 1] ?? "").split(",").map((s) => s.trim()).filter(Boolean) : [];
  if (listed.length) return listed;
  const claude = models.some((m) => m.split("/")[1] === "claude") || modes.includes("claude");
  return claude ? CLAUDE_TOOLS : TOOLS;
}

async function main() {
  const cli = piCli();
  const wanted = arg("models")?.split(",").map((s) => s.trim()).filter(Boolean);
  const rows = wanted ?? listRows(cli);
  const modes = (arg("modes")?.split(",").map((s) => s.trim()) ?? ALL_MODES) as Mode[];
  for (const m of modes) if (!ALL_MODES.includes(m)) throw new Error(`unknown mode ${m}`);
  const tools = toolsFor(process.argv, modes, rows);
  const dialect = flag("dialect");
  const repeat = Number(arg("repeat", "1"));
  const cellSeconds = Number(arg("cell-seconds", "600"));
  const out = arg("out") ?? join(homedir(), ".thickapi", "toolsmoke");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const runDir = join(out, stamp);
  mkdirSync(runDir, { recursive: true });
  console.log(
    `tool-smoke: ${rows.length} rows x ${modes.length} modes x ${repeat} tools=${tools.join(",")} ` +
      `dialect=${dialect} -> ${runDir}`,
  );

  // Vendors in parallel; cells sequential within a vendor (Cursor rows share one account).
  const byVendor = new Map<string, string[]>();
  for (const r of rows) byVendor.set(vendorOf(r), [...(byVendor.get(vendorOf(r)) ?? []), r]);
  const results: CellResult[] = [];
  await Promise.all(
    [...byVendor.values()].map(async (vrows) => {
      for (let k = 1; k <= repeat; k++)
        for (const mode of modes)
          for (const model of vrows)
            results.push(await runCell(cli, model, mode, k, cellSeconds, runDir, tools, dialect));
    }),
  );
  const md = matrixMd(results, modes);
  writeFileSync(join(runDir, "matrix.md"), md);
  console.log("\n" + md);
  process.exit(results.every((c) => c.verdict === "pass") ? 0 : 1);
}

if (import.meta.main) {
  main().catch((e) => {
    console.error(String(e));
    process.exit(2);
  });
}
