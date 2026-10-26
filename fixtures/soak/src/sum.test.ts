import { describe, expect, test } from "bun:test";
import { sum } from "./sum";

describe("sum", () => {
  test("adds two numbers", () => {
    expect(sum([1, 2])).toBe(3);
  });
  test("adds an empty list to zero", () => {
    expect(sum([])).toBe(0);
  });
});
