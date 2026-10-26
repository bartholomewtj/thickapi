import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import {
  aliasesIn,
  CLAUDE_ALIASES,
  CLAUDE_TOOLS,
  expectedFor,
  judge,
  matrixMd,
  parseEvents,
  TOOLS,
  toolsFor,
  toolsOut,
  type CellResult,
} from "./tool-smoke";

const fx = (n: string) => readFileSync(join(import.meta.dir, "fixtures", n), "utf8");

describe("tool-smoke event parsing", () => {
  test("pass cell: eight tools, DONE reply", () => {
    const p = parseEvents(fx("tool-smoke-pass.events.jsonl"));
    expect(p.calls).toEqual(["ls", "find", "grep", "read", "write", "edit", "bash", "todo"]);
    expect(p.requests).toBe(9);
    expect(p.modelErrors).toEqual([]);
    expect(p.final).toContain("DONE MARKER-4172");
    const v = judge(p, 0, "MARKER-4172\r\nomega\r\n", "named");
    expect(v.verdict).toBe("pass");
    expect(v.missing).toEqual([]);
  });

  test("fail cell: cursor vendor_down, no calls", () => {
    const p = parseEvents(fx("tool-smoke-fail.events.jsonl"));
    expect(p.calls).toEqual([]);
    expect(p.modelErrors[0]).toContain("vendor_down");
    const v = judge(p, 0, null, "named");
    expect(v.verdict).toBe("fail");
    expect(v.missing.length).toBe(8);
    expect(v.file_ok).toBe(false);
  });

  test("wrong file content or nonzero exit fails; plain does not need all tools", () => {
    const p = parseEvents(fx("tool-smoke-pass.events.jsonl"));
    expect(judge(p, 0, "MARKER-4172\nalpha\n", "named").verdict).toBe("fail");
    expect(judge(p, 1, "MARKER-4172\nomega", "named").verdict).toBe("fail");
    const partial = { ...p, calls: ["bash"] };
    expect(judge(partial, 0, "MARKER-4172\nomega", "named").verdict).toBe("fail");
    expect(judge(partial, 0, "MARKER-4172\nomega", "plain").verdict).toBe("pass");
  });

  test("timeout verdict and matrix rendering", () => {
    const p = parseEvents("");
    expect(judge(p, "timeout", null, "named").verdict).toBe("timeout");
    const cell = (verdict: CellResult["verdict"]): CellResult => ({
      ...judge(p, 0, null, "named"), verdict, model: "subs/x/y", mode: "named", repeat: 1, secs: 3,
      rc: 0, requests: 0, calls: [], request_ids: [], tool_errors: [], model_errors: [],
      out_txt: null, final: "", stderr_tail: "",
    });
    const md = matrixMd([cell("pass"), { ...cell("fail"), mode: "plain" }], ["named", "plain"]);
    expect(md).toContain("| subs/x/y | pass 3s | fail 3s (missing 8) |");
    expect(md).toContain("1/2 cells pass");
  });

  test("garbage lines are skipped", () => {
    expect(parseEvents("not json\n{\"type\":\"agent_start\"}\n").requests).toBe(0);
  });

  test("responseId is the access-log request id", () => {
    const p = parseEvents(fx("tool-smoke-pass.events.jsonl"));
    expect(p.requestIds[0]).toBe("chatcmpl-125a8afcec994151a9a86832e87bfc69");
    expect(p.requestIds.length).toBe(p.requests);
  });
});

describe("dialect alias coverage", () => {
  const log = [
    "INFO thin request end request_id=chatcmpl-aaa tools_out=Edit,Read",
    "INFO thin request end request_id=chatcmpl-other tools_out=Bash",
    "INFO thin request end request_id=chatcmpl-bbb tools_out=TodoWrite,Agent",
    "INFO thin request end request_id=chatcmpl-ccc tools_out=-",
  ].join("\n");

  test("tools_out parses a comma list and treats a dash as none", () => {
    expect(toolsOut("tools_out=Edit,Read finish_reason=tool_calls")).toEqual(["Edit", "Read"]);
    expect(toolsOut("tools_out=-")).toEqual([]);
    expect(toolsOut("no field")).toEqual([]);
  });

  test("only the cell's request ids count", () => {
    expect(aliasesIn(log, ["chatcmpl-aaa", "chatcmpl-bbb"]).sort()).toEqual(
      ["Agent", "Edit", "Read", "TodoWrite"].sort(),
    );
    expect(aliasesIn(log, ["chatcmpl-ccc"])).toEqual([]);
    expect(aliasesIn(log, [])).toEqual([]);
  });

  test("only the claude prompt is held to the hand-off and the ask", () => {
    const eight = expectedFor("named", CLAUDE_TOOLS);
    expect(eight.tools.sort()).toEqual([...TOOLS].sort());
    expect(eight.aliases).not.toContain("Agent");
    expect(eight.aliases).not.toContain("AskUserQuestion");
    expect(eight.aliases).toContain("TodoWrite");
    expect(expectedFor("plain", TOOLS).tools.sort()).toEqual([...TOOLS].sort());
    const all = expectedFor("claude", CLAUDE_TOOLS);
    expect(all.tools).toEqual(CLAUDE_TOOLS);
    expect(all.aliases).toEqual(CLAUDE_ALIASES);
  });

  test("the log file's ANSI colours do not hide request_id or tools_out", () => {
    const kv = (k: string, v: string) => `\x1b[3m${k}\x1b[0m\x1b[2m=\x1b[0m${v}`;
    const coloured = `\x1b[32m INFO\x1b[0m thin request end ${kv("request_id", "chatcmpl-aaa")} ${kv("tools_out", "Read,Bash")}`;
    expect(aliasesIn(coloured, ["chatcmpl-aaa"]).sort()).toEqual(["Bash", "Read"]);
  });

  test("a missing alias fails the cell; coverage is recorded either way", () => {
    const p = parseEvents(fx("tool-smoke-pass.events.jsonl"));
    const seen = ["Read", "Edit"];
    const v = judge(p, 0, "MARKER-4172\nomega", "claude", CLAUDE_TOOLS, seen, ["Read", "Edit", "Agent"]);
    expect(v.verdict).toBe("fail");
    expect(v.alias_missing).toEqual(["Agent"]);
    expect(v.aliases).toEqual(seen);
    const whole = judge(p, 0, "MARKER-4172\nomega", "named", TOOLS, seen, []);
    expect(whole.verdict).toBe("pass");
  });

  test("matrix shows alias coverage", () => {
    const p = parseEvents("");
    const cell: CellResult = {
      ...judge(p, 0, null, "claude", CLAUDE_TOOLS, ["Edit"], ["Edit", "Read"]),
      verdict: "fail", model: "subs/claude/opus", mode: "claude", repeat: 1, secs: 4,
      rc: 0, requests: 1, calls: [], request_ids: [], tool_errors: [], model_errors: [],
      out_txt: null, final: "", stderr_tail: "",
    };
    const md = matrixMd([cell], ["claude"]);
    expect(md).toContain("Alias coverage");
    expect(md).toContain("subs/claude/opus claude r1: Edit missing Read");
  });

  test("tools default to the eight, and a claude mode takes the preset", () => {
    expect(toolsFor(["bun", "tool-smoke.ts"], ["named"], ["subs/grok/grok-4.7"])).toEqual(TOOLS);
    expect(toolsFor(["bun", "tool-smoke.ts"], ["claude"], ["subs/claude/opus"])).toEqual(CLAUDE_TOOLS);
    expect(toolsFor(["bun", "tool-smoke.ts", "--tools", "read,edit"], ["claude"], [])).toEqual([
      "read",
      "edit",
    ]);
  });
});
