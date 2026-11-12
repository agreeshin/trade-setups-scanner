import { expect, it } from "vitest";
import { formatElapsedDuration } from "./format-duration";

it.each([
  [undefined, "—"], [null, "—"], [NaN, "—"], [Infinity, "—"], [-Infinity, "—"],
  [-60, "Менее 1 мин"], [0, "Менее 1 мин"], [59.9, "Менее 1 мин"],
  [60, "1 мин"], [3599, "59 мин"], [3600, "1 ч"], [3661, "1 ч 1 мин"],
  [86400, "1 дн"], [90061, "1 дн 1 ч 1 мин"],
  [10_000 * 86400, "10000 дн"], [Number.MAX_SAFE_INTEGER, "104249991374 дн 7 ч 36 мин"],
] as const)("formats bounded elapsed seconds %s as %s", (seconds, expected) => {
  expect(formatElapsedDuration(seconds)).toBe(expected);
});

it("shows the complete number of days even for the largest finite input", () => {
  const formatted = formatElapsedDuration(Number.MAX_VALUE);
  expect(formatted.startsWith(`${BigInt(Number.MAX_VALUE) / 86400n} дн`)).toBe(true);
  expect(formatted).not.toMatch(/Infinity|NaN|e\+|дн\+/);
  expect(formatted.length).toBeLessThan(330);
});
