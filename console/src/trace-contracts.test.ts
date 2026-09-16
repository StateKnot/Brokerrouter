import { describe, expect, it } from "vitest";
import { traceItemsSchema, traceItem, traceCursor } from "./trace-contracts";
const id = "11111111-1111-4111-8111-111111111111",
  at = "2026-09-16T02:00:00.123456Z";
describe("trace wire contracts", () => {
  it("preserves event and timestamp precision", () => {
    const event = {
      kind: "events",
      sequence: "9007199254740993",
      created_at: at,
      event_type: "unrecognized",
      operation_id: null,
      approval_id: null,
      reconciliation: null,
    };
    const parsed = traceItemsSchema.parse({
      section: "events",
      observed_at: at,
      data: [event],
      next_after: {
        kind: "event",
        sequence: event.sequence,
        through: event.sequence,
      },
      event_fence: event.sequence,
    });
    expect(parsed.data[0]).toEqual(event);
    expect(parsed.observed_at).toBe(at);
    expect(
      traceItem.safeParse({ ...event, sequence: 9007199254740993 }).success,
    ).toBe(false);
  });
  it("rejects wrong section cursors and missing event fences", () => {
    const base = {
      section: "nodes",
      observed_at: at,
      data: [],
      next_after: null,
      event_fence: null,
    };
    expect(traceItemsSchema.parse(base).section).toBe("nodes");
    expect(
      traceItemsSchema.safeParse({
        ...base,
        next_after: { kind: "event", sequence: "1", through: "2" },
      }).success,
    ).toBe(false);
    expect(
      traceItemsSchema.safeParse({ ...base, section: "events" }).success,
    ).toBe(false);
    expect(
      traceCursor.safeParse({ kind: "time", created_at: at, id }).success,
    ).toBe(true);
  });
  it("keeps CNY and unknown usage as distinct values and bounds children", () => {
    const row = {
      kind: "requests",
      id,
      turn_id: id,
      model_id: "m",
      model_name: "model",
      purpose: "model",
      status: "succeeded",
      created_at: at,
      updated_at: at,
      error_code: null,
      cost_cny: "0.000001",
      reserved_cny: "0",
      input_tokens: null,
      output_tokens: null,
      judge_operation_id: null,
      media_state: null,
      media_endpoint_id: null,
      cancel_requested: null,
      dispatch_deadline: null,
      result_expires_at: null,
      attempts: [],
    };
    expect(traceItem.parse(row)).toEqual(row);
    expect(traceItem.safeParse({ ...row, cost_cny: 0.000001 }).success).toBe(
      false,
    );
    const event = {
      kind: "events",
      sequence: "1",
      created_at: at,
      event_type: "turn.created",
      operation_id: null,
      approval_id: null,
      reconciliation: null,
    };
    expect(
      traceItemsSchema.safeParse({
        section: "nodes",
        observed_at: at,
        data: [event],
        next_after: null,
        event_fence: null,
      }).success,
    ).toBe(false);
  });
});
