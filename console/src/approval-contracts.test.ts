import { describe, it, expect } from "vitest";
import { inboxSchema, reviewSchema } from "./approval-contracts";
describe("approval boundary", () => {
  it("rejects oversized inbox pages and unknown execution states", () => {
    expect(
      inboxSchema.safeParse({ data: [{}], next_after: null }).success,
    ).toBe(false);
    expect(
      inboxSchema.safeParse({ data: Array(51).fill({}), next_after: null })
        .success,
    ).toBe(false);
  });
  it("never accepts a result without a current-turn review contract", () => {
    expect(
      reviewSchema.safeParse({ can_continue: true, payload_text: "{}" })
        .success,
    ).toBe(false);
    expect(
      reviewSchema.safeParse({
        can_continue: true,
        payload_text: null,
        scope: "all_sessions",
      }).success,
    ).toBe(false);
  });
});
