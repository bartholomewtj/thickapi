import { describe, expect, test } from "bun:test";
import { bodyFor, judgeKnob, KNOBS, reportMd, subsKey, type CellVerdict } from "./claude-knobs";

describe("claude knob cells", () => {
  test("every knob builds a request the daemon can parse", () => {
    for (const knob of KNOBS) {
      const body = bodyFor("subs/claude/opus", knob);
      expect(body.model).toBe("subs/claude/opus");
      expect(body.stream).toBe(false);
    }
    expect(bodyFor("m", "none").tool_choice).toBe("none");
    expect(bodyFor("m", "required").tool_choice).toBe("required");
    expect((bodyFor("m", "named").tool_choice as any).function.name).toBe("edit");
    expect(bodyFor("m", "parallel").parallel_tool_calls).toBe(false);
    const image = bodyFor("m", "image");
    const parts = (image.messages as any[])[1].content;
    expect(parts[1].type).toBe("image_url");
    expect(String(parts[1].image_url.url).startsWith("data:image/png;base64,")).toBe(true);
  });

  test("judge checks finish reason and tool name; a 429 is quota", () => {
    expect(judgeKnob("none", 200, { choices: [{ finish_reason: "stop", message: {} }] }).verdict).toBe("pass");
    expect(judgeKnob("none", 200, { choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ function: { name: "edit" } }] } }] }).verdict).toBe("fail");
    const named = judgeKnob("named", 200, { choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ function: { name: "edit" } }] } }] });
    expect(named.verdict).toBe("pass");
    expect(named.tools).toEqual(["edit"]);
    expect(judgeKnob("named", 200, { choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ function: { name: "read" } }] } }] }).verdict).toBe("fail");
    expect(judgeKnob("parallel", 200, { choices: [{ finish_reason: "tool_calls", message: { tool_calls: [{ function: { name: "read" } }, { function: { name: "read" } }] } }] }).verdict).toBe("fail");
    expect(judgeKnob("image", 429, { error: { code: "rate_limit_error" } }).verdict).toBe("quota");
  });

  test("the subs key is providers.subs.apiKey", () => {
    expect(subsKey(JSON.stringify({ providers: { subs: { apiKey: "ta_x" } } }))).toBe("ta_x");
    expect(() => subsKey("{}")).toThrow(/apiKey/);
  });

  test("the report sits beside the smoke matrix", () => {
    const cells: CellVerdict[] = [
      { knob: "named", model: "subs/claude/opus", verdict: "pass", status: 200, finish: "tool_calls", tools: ["edit"], why: "" },
    ];
    expect(reportMd(cells)).toContain("| subs/claude/opus | named | pass | tool_calls | edit |");
  });
});
