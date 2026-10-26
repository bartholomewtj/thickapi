#!/usr/bin/env bun
/**
 * Claude knob cells (#227). One HTTP request per cell to the LIVE daemon (`THICKAPI_URL`,
 * default `http://127.0.0.1:8787`); never starts `thickapi serve`. The key is `providers.subs.apiKey` in `~/.pi/agent/models.json`.
 *
 * Per Claude row: `tool_choice` none, required, and named `edit`; `parallel_tool_calls:false`;
 * one image (the K7 PNG, `fixtures/images/blue-16.png`).
 *
 * Run: bun scripts/claude-knobs.ts [--models a,b] [--out DIR] [--dry-run]
 *
 * `--dry-run` writes the requests and does not call the daemon. Not part of verify-all or CI.
 * The live Claude cells wait for K10.
 */
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const DAEMON = `${process.env.THICKAPI_URL ?? "http://127.0.0.1:8787"}/v1/chat/completions`;
const PNG = join(import.meta.dir, "..", "fixtures", "images", "blue-16.png");

export type Knob = "none" | "required" | "named" | "parallel" | "image";
export const KNOBS: Knob[] = ["none", "required", "named", "parallel", "image"];

export type Expectation = {
  status: number;
  finish?: "stop" | "tool_calls";
  tool?: string;
  atMostOne?: boolean;
};

export const EXPECT: Record<Knob, Expectation> = {
  none: { status: 200, finish: "stop" },
  required: { status: 200, finish: "tool_calls" },
  named: { status: 200, finish: "tool_calls", tool: "edit" },
  parallel: { status: 200, finish: "tool_calls", atMostOne: true },
  image: { status: 200, finish: "stop" },
};

const EDIT = {
  type: "function",
  function: {
    name: "edit",
    description: "Replace one exact string in a file.",
    parameters: {
      type: "object",
      properties: { path: { type: "string" }, edits: { type: "array" } },
      required: ["path", "edits"],
    },
  },
};

const READ = {
  type: "function",
  function: {
    name: "read",
    description: "Read a file.",
    parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
  },
};

export function bodyFor(model: string, knob: Knob): Record<string, unknown> {
  const system = {
    role: "system",
    content: "You are a terse assistant under test. Call a tool when one fits.",
  };
  if (knob === "none") {
    return {
      model, stream: false, max_tokens: 256,
      messages: [system, { role: "user", content: "Reply with exactly: KNOB OK" }],
      tools: [EDIT, READ], tool_choice: "none",
    };
  }
  if (knob === "required") {
    return {
      model, stream: false, max_tokens: 256,
      messages: [system, { role: "user", content: "Change alpha to omega in notes.txt. You must call a tool." }],
      tools: [EDIT, READ], tool_choice: "required",
    };
  }
  if (knob === "named") {
    return {
      model, stream: false, max_tokens: 256,
      messages: [system, { role: "user", content: "Change alpha to omega in notes.txt." }],
      tools: [EDIT, READ],
      tool_choice: { type: "function", function: { name: "edit" } },
    };
  }
  if (knob === "parallel") {
    return {
      model, stream: false, max_tokens: 256,
      messages: [system, { role: "user", content: "Read a.txt and b.txt. Call the tool once, for a.txt only." }],
      tools: [READ], tool_choice: "required", parallel_tool_calls: false,
    };
  }
  const png = readFileSync(PNG);
  const url = `data:image/png;base64,${png.toString("base64")}`;
  return {
    model, stream: false, max_tokens: 64,
    messages: [
      { role: "system", content: "You are a terse assistant under test. Answer in one short sentence." },
      { role: "user", content: [
        { type: "text", text: "what colour is this square?" },
        { type: "image_url", image_url: { url } },
      ] },
    ],
  };
}

export type CellVerdict = {
  knob: Knob;
  model: string;
  verdict: "pass" | "fail" | "quota" | "dry-run";
  status: number | null;
  finish: string | null;
  tools: string[];
  why: string;
};

/** What one response must satisfy. A 429 is quota, not a fail. */
export function judgeKnob(knob: Knob, status: number, body: any): Omit<CellVerdict, "model"> {
  const expect = EXPECT[knob];
  const calls = body?.choices?.[0]?.message?.tool_calls ?? [];
  const tools = (Array.isArray(calls) ? calls : [])
    .map((c: any) => String(c?.function?.name ?? ""))
    .filter(Boolean);
  const finish = (body?.choices?.[0]?.finish_reason ?? null) as string | null;
  const code = String(body?.error?.code ?? body?.error?.type ?? "");
  const base = { knob, status, finish, tools, verdict: "fail" as const, why: "" };
  if (status === 429 || code.includes("rate_limit") || code.includes("out_of_credits")) {
    return { ...base, verdict: "quota", why: "429 quota; not a code fault" };
  }
  if (status !== expect.status) return { ...base, why: `status ${status}, want ${expect.status}` };
  if (expect.finish && finish !== expect.finish) return { ...base, why: `finish ${finish}, want ${expect.finish}` };
  if (expect.tool && !tools.includes(expect.tool)) return { ...base, why: `tools [${tools}], want ${expect.tool}` };
  if (expect.atMostOne && tools.length > 1) return { ...base, why: `${tools.length} calls, want at most one` };
  if (knob === "none" && tools.length) return { ...base, why: `tool_choice none returned ${tools}` };
  return { ...base, verdict: "pass", why: "" };
}

export function subsKey(modelsJson: string): string {
  const parsed = JSON.parse(modelsJson);
  const key = parsed?.providers?.subs?.apiKey;
  if (typeof key !== "string" || !key) throw new Error("providers.subs.apiKey missing in models.json");
  return key;
}

export function reportMd(cells: CellVerdict[]): string {
  const lines = ["| model | knob | verdict | finish | tools |", "|---|---|---|---|---|"];
  for (const c of cells) {
    lines.push(`| ${c.model} | ${c.knob} | ${c.verdict} | ${c.finish ?? "-"} | ${c.tools.join(",") || "-"} |`);
  }
  const pass = cells.filter((c) => c.verdict === "pass" || c.verdict === "dry-run").length;
  lines.push("", `${pass}/${cells.length} cells pass.`);
  for (const c of cells.filter((c) => c.verdict === "fail" || c.verdict === "quota")) {
    lines.push(`- ${c.model} ${c.knob}: ${c.verdict} — ${c.why}`);
  }
  return lines.join("\n") + "\n";
}

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function claudeRows(): string[] {
  const listed = arg("models")?.split(",").map((s) => s.trim()).filter(Boolean);
  if (listed?.length) return listed;
  return ["subs/claude/opus", "subs/claude/sonnet", "subs/claude/fable"];
}

async function main() {
  const dry = process.argv.includes("--dry-run");
  const rows = claudeRows();
  const out = arg("out") ?? join(homedir(), ".thickapi", "toolsmoke");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const dir = join(out, `${stamp}-knobs`);
  mkdirSync(dir, { recursive: true });
  const key = dry ? "" : subsKey(readFileSync(join(homedir(), ".pi", "agent", "models.json"), "utf8"));
  const cells: CellVerdict[] = [];
  let quota = false;
  for (const model of rows) {
    if (quota) break;
    for (const knob of KNOBS) {
      const body = bodyFor(model, knob);
      if (dry) {
        writeFileSync(join(dir, `${knob}-${model.replace(/\//g, "-")}.json`), JSON.stringify(body, null, 2));
        cells.push({ knob, model, verdict: "dry-run", status: null, finish: null, tools: [], why: "" });
        continue;
      }
      const res = await fetch(DAEMON, {
        method: "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      const text = await res.text();
      let parsed: any = {};
      try { parsed = JSON.parse(text); } catch { parsed = { error: { message: text.slice(0, 300) } }; }
      const v: CellVerdict = { ...judgeKnob(knob, res.status, parsed), model };
      cells.push(v);
      console.log(`${model} ${knob}: ${v.verdict} status=${v.status} finish=${v.finish} tools=[${v.tools}] ${v.why}`);
      if (v.verdict === "quota") {
        console.error("429 out_of_credits: stopping. Not a code fault.");
        quota = true;
        break;
      }
    }
  }
  const md = reportMd(cells);
  writeFileSync(join(dir, "knobs.md"), md);
  console.log("\n" + md);
  console.log(`wrote ${dir}`);
  process.exit(cells.every((c) => c.verdict === "pass" || c.verdict === "dry-run") ? 0 : 1);
}

if (import.meta.main) {
  main().catch((e) => {
    console.error(String(e));
    process.exit(2);
  });
}
