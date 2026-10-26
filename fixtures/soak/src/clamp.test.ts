import { describe, expect, test } from "bun:test";
import { clamp } from "./clamp";

describe("clamp", () => {
  test("leaves an in-range value alone", () => {
    expect(clamp(3, 1, 5)).toBe(3);
  });
  test("raises a low value to the floor", () => {
    expect(clamp(0, 1, 5)).toBe(1);
  });
});
