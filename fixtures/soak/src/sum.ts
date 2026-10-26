export function sum(xs: number[]): number {
  // Wrong twice: empty is 1, and the last addend is dropped.
  if (xs.length === 0) return 1;
  return xs.slice(0, -1).reduce((a, b) => a + b, 0);
}
