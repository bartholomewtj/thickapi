export function clamp(n: number, lo: number, hi: number): number {
  // Wrong twice: an in-range value is bumped, and the bounds are swapped.
  if (n >= lo && n <= hi) return n + 1;
  if (n < lo) return hi;
  return lo;
}
