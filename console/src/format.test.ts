import { describe, expect, it } from "vitest";
import { cny, chartRatio, dateRange, integer, maximum, time } from "./format";
import { amount, requestCursorSchema, totalsSchema } from "./contracts";

describe("exact ledger display", () => {
  it("preserves tiny charges, means and integers beyond Number precision", () => {
    expect(cny("0.000001")).toBe("¥0.000001");
    expect(cny("9007199254740993.123456")).toBe(
      "¥9,007,199,254,740,993.123456",
    );
    expect(cny("0.000000000001")).toBe("¥0.000000000001");
    expect(cny("0")).toBe("¥0.00");
    expect(integer("9007199254740993")).toBe("9,007,199,254,740,993");
    expect(cny(null)).toBe("待结算");
  });
  it("rejects float coercion, exponents, negatives and excessive precision", () => {
    for (const value of [
      1,
      NaN,
      Infinity,
      "1e9",
      "-1",
      "0.1234567890123",
      "<script>",
      "00.1",
    ])
      expect(amount.safeParse(value).success).toBe(false);
    expect(totalsSchema.safeParse({ requests: 1 }).success).toBe(false);
  });
  it("converts only a bounded chart ratio; maximum selection stays exact", () => {
    expect(chartRatio("0", "0")).toBe(0);
    expect(chartRatio("0.000001", "0.000002")).toBe(50);
    expect(
      maximum(["9007199254740993.000001", "9007199254740993.000002"]),
    ).toBe("9007199254740993.000002");
    expect(maximum([])).toBe("0");
  });
});
describe("calendar and cursor contracts", () => {
  it("uses inclusive displayed end day and exclusive API boundary", () => {
    expect(dateRange("2026-09-01", "2026-09-07", "Asia/Shanghai")).toEqual({
      start: "2026-08-31T16:00:00Z",
      end: "2026-09-07T16:00:00Z",
    });
    expect(dateRange("2026-09-01", "2026-09-01", "UTC")).toEqual({
      start: "2026-09-01T00:00:00Z",
      end: "2026-09-02T00:00:00Z",
    });
  });
  it("validates impossible, reversed and overlong ranges", () => {
    for (const pair of [
      ["2026-02-30", "2026-03-01"],
      ["", "2026-09-01"],
      ["2026-09-02", "2026-09-01"],
      ["2026-09-01", "2026-10-02"],
    ])
      expect(() => dateRange(pair[0]!, pair[1]!, "UTC")).toThrow();
    expect(() => dateRange("2026-08-01", "2026-08-31", "UTC")).not.toThrow();
  });
  it("preserves microseconds in raw cursors", () => {
    const cursor = {
      created_at: "2026-09-14T00:00:00.123456Z",
      id: "11111111-1111-4111-8111-111111111111",
    };
    expect(requestCursorSchema.parse(cursor)).toEqual(cursor);
    expect(time(cursor.created_at, "Asia/Shanghai")).toContain("08:00:00");
  });
});
