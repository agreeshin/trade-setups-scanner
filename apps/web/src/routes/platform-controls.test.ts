import { describe, expect, it } from "vitest";
import { replayRange } from "./platform-controls";

describe("local replay time", () => {
  it("converts local wall time to Unix seconds", () => {
    expect(replayRange("2026-10-10T12:00", "2026-10-10T13:00")).toEqual({
      fromUnix: new Date(2026, 9, 10, 12, 0).getTime() / 1000,
      toUnix: new Date(2026, 9, 10, 13, 0).getTime() / 1000,
    });
  });
  it.each([
    ["", "2026-10-10T13:00"],
    ["2026-02-30T12:00", "2026-03-10T13:00"],
    ["2026-10-10T13:00", "2026-10-10T12:00"],
    ["2026-10-10T12:00", "2026-10-10T12:00"],
    ["not-a-date", "2026-10-10T13:00"],
  ])("rejects invalid or reversed range %s to %s", (from, to) => {
    expect(() => replayRange(from, to)).toThrow("корректные даты");
  });
});
