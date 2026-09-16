import { describe, expect, it } from "vitest";
import { monitorTotals, monitorRailsSchema } from "./monitoring-contracts";

describe("monitoring contracts", () => {
  it("preserves exact counts, tiny costs and unavailable durations", () => {
    const input = Object.fromEntries(
      Object.keys(monitorTotals.shape).map((k) => [k, "0"]),
    );
    Object.assign(input, {
      checks: "9007199254740993",
      known_cost_cny: "0.000001",
      classified_pass_percent: null,
      mean_inline_ms: null,
      mean_judge_lifecycle_ms: null,
    });
    const parsed = monitorTotals.parse(input);
    expect(parsed.checks).toBe("9007199254740993");
    expect(parsed.known_cost_cny).toBe("0.000001");
    expect(parsed.mean_inline_ms).toBeNull();
    expect(
      monitorTotals.safeParse({ ...input, checks: 9007199254740993 }).success,
    ).toBe(false);
    expect(
      monitorTotals.safeParse({ ...input, known_cost_cny: "-1" }).success,
    ).toBe(false);
  });
  it("rejects invalid or unbounded version cursors", () => {
    const data = {
      observed_at: "2026-09-16T00:00:00Z",
      data: [],
      next_after: { guardrail_id: "rail", version: 2147483647 },
    };
    expect(monitorRailsSchema.parse(data).next_after?.version).toBe(2147483647);
    for (const version of [0, -1, 2147483648, "1"]) {
      expect(
        monitorRailsSchema.safeParse({
          ...data,
          next_after: { guardrail_id: "rail", version },
        }).success,
      ).toBe(false);
    }
  });
});
