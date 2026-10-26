import { describe, expect, test } from "bun:test";
import { label } from "./label";

describe("label", () => {
  test("lowercases and adds no bang", () => {
    expect(label("Pi")).toBe("pi");
  });
  test("keeps an empty name empty", () => {
    expect(label("")).toBe("");
  });
});
