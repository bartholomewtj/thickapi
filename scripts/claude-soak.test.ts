import { describe, expect, test } from "bun:test";
import { accessLines, forceCompaction, judgeSoak, logHealth, percentiles, reportMd, timing } from "./claude-soak";

describe("claude soak", () => {
  test("percentiles of spawn samples", () => {
    expect(percentiles([])).toEqual({ p50: null, p95: null });
    const p = percentiles([10, 20, 30, 40, 100]);
    expect(p.p50).toBe(30);
    expect(p.p95).toBe(100);
  });

  test("access lines are matched by request id, and spawn_ms is numeric", () => {
    const log = [
      "thin request end request_id=chatcmpl-a spawn_ms=120 cached_tokens=8 prompt_tokens=10 tools_out=TodoWrite",
      "thin request end request_id=chatcmpl-b spawn_ms=null tools_out=-",
      "thin request end request_id=chatcmpl-z spawn_ms=999 tools_out=Edit",
    ].join("\n");
    const lines = accessLines(log, ["chatcmpl-a", "chatcmpl-b"]);
    expect(lines.length).toBe(2);
    const t = timing(lines);
    expect(t.spawn).toEqual([120]);
    expect(t.ratio).toBeCloseTo(0.8);
    const health = logHealth(lines, lines.join("\n"));
    expect(health.tools).toEqual(["TodoWrite"]);
    expect(health.clean).toBe(true);
    expect(logHealth(lines, "dialect_error=Edit:bad").clean).toBe(false);
  });

  test("the log file's ANSI colours do not hide fields", () => {
    const kv = (k: string, v: string) => `\x1b[3m${k}\x1b[0m\x1b[2m=\x1b[0m${v}`;
    const log = `\x1b[32m INFO\x1b[0m thin request end ${kv("request_id", "chatcmpl-a")} ${kv("spawn_ms", "436")} ${kv("tools_out", "AskUserQuestion")}`;
    const lines = accessLines(log, ["chatcmpl-a"]);
    expect(lines.length).toBe(1);
    expect(timing(lines).spawn).toEqual([436]);
    expect(logHealth(lines, lines.join("\n")).tools).toEqual(["AskUserQuestion"]);
  });

  test("a cell passes only when every bar is met", () => {
    const ok = {
      model: "subs/claude/opus", steps: 30, compactions: 1, wall_s: 10,
      spawn_ms_p50: 1, spawn_ms_p95: 2, cached_over_prompt: 0.5,
      tool_calls: 30, subagent: 1, todo_write: 1, tests_green: true, log_clean: true,
    };
    expect(judgeSoak(ok).verdict).toBe("pass");
    expect(judgeSoak({ ...ok, compactions: 0 }).why).toContain("compaction");
    expect(judgeSoak({ ...ok, todo_write: 0 }).why).toContain("TodoWrite");
  });

  test("compaction settings force a threshold of one token", () => {
    const out = JSON.parse(forceCompaction(JSON.stringify({ theme: "dark", compaction: { enabled: false } })));
    expect(out.theme).toBe("dark");
    expect(out.compaction.enabled).toBe(true);
    expect(out.compaction.reserveTokens).toBeGreaterThan(200_000);
    expect(out.compaction.keepRecentTokens).toBe(256);
  });

  test("the report names the bars", () => {
    const md = reportMd([{
      model: "subs/claude/opus", verdict: "pass", steps: 40, compactions: 2, wall_s: 90,
      spawn_ms_p50: 800, spawn_ms_p95: 2100, cached_over_prompt: 0.42,
      tool_calls: 40, subagent: 1, todo_write: 3, tests_green: true, log_clean: true, why: "",
    }]);
    expect(md).toContain("| subs/claude/opus | pass | 40 | 2 | 90s | 800 | 2100 | 0.42 |");
  });
});
