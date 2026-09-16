import { z } from "zod";
import {
  amount,
  count,
  timestamp,
  requestCursorSchema,
  statuses,
} from "./contracts";
const text = z.string().max(512);
const turnStates = z.enum([
  "active",
  "expired",
  "completed",
  "cancelled",
  "failed",
]);
const nodeStates = z.enum([
  "ready",
  "checking",
  "waiting_approval",
  "rejected",
  "feedback",
  "executing",
  "execution_unknown",
  "completed",
]);
const stages = z.enum([
  "pre",
  "post",
  "tool_input",
  "tool_output",
  "artifact",
  "media_output",
]);
const approvalStates = z.enum([
  "pending",
  "expired_pending",
  "approved",
  "rejected",
  "feedback",
]);
export const turnSchema = z.object({
  id: z.uuid(),
  key_id: z.uuid(),
  user_id: z.uuid(),
  session_id: text,
  run_id: text,
  generation: z.number().int().nonnegative(),
  recorded_state: z.enum(["active", "completed", "cancelled", "failed"]),
  state: turnStates,
  created_at: timestamp,
  expires_at: timestamp,
  event_clock: count,
  acknowledged_sequence: count,
  runtime_id: text,
  agent_id: text,
  key_alias: text,
  identity_active: z.boolean(),
});
export type Turn = z.infer<typeof turnSchema>;
export const turnsSchema = z.object({
  observed_at: timestamp,
  data: z.array(turnSchema).max(50),
  next_before: requestCursorSchema.nullable(),
});
export const traceDetailSchema = z.object({
  observed_at: timestamp,
  currency: z.literal("CNY"),
  cost_basis: z.literal("configured_price_ledger_not_provider_invoice"),
  turn: turnSchema,
  costs: z
    .array(
      z.object({
        purpose: z.enum(["model", "guardrail"]),
        requests: count,
        known_cost_requests: count,
        unsettled_cost_requests: count,
        known_cost_cny: amount,
        reserved_cny: amount,
        token_usage_requests: count,
        input_tokens: count,
        output_tokens: count,
      }),
    )
    .max(2),
  nodes: z.array(z.object({ state: nodeStates, count })).max(8),
  approvals: z.array(z.object({ state: approvalStates, count })).max(5),
});
export const sectionSchema = z.enum([
  "nodes",
  "requests",
  "approvals",
  "events",
  "checks",
]);
export type Section = z.infer<typeof sectionSchema>;
export const traceCursor = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("time"), ...requestCursorSchema.shape }),
  z.object({ kind: z.literal("event"), sequence: count, through: count }),
  z.object({
    kind: z.literal("check"),
    guardrail_id: text,
    version: z.number().int().positive(),
  }),
]);
export type TraceCursor = z.infer<typeof traceCursor>;
const attempt = z.object({
  id: z.uuid(),
  endpoint_id: z.uuid(),
  endpoint_name: text,
  ordinal: z.number().int().min(0).max(2),
  status: z.enum([
    "submitting",
    "not_sent",
    "submission_unknown",
    "succeeded",
    "reconciled",
  ]),
  http_status: z.number().int().min(100).max(599).nullable(),
  error_code: text.nullable(),
  cost_cny: amount.nullable(),
  created_at: timestamp,
  finished_at: timestamp.nullable(),
});
export const traceItem = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("nodes"),
    id: z.uuid(),
    turn_id: z.uuid(),
    stage: stages,
    model_id: text.nullable(),
    tool_name: text.nullable(),
    created_at: timestamp,
    execution_deadline: timestamp.nullable(),
    recorded_state: nodeStates,
    state: nodeStates,
    operation_sha256: z.string().regex(/^[a-f0-9]{64}$/),
  }),
  z.object({
    kind: z.literal("requests"),
    id: z.uuid(),
    turn_id: z.uuid(),
    model_id: text,
    model_name: text,
    purpose: z.enum(["model", "guardrail"]),
    status: z.enum(statuses),
    created_at: timestamp,
    updated_at: timestamp,
    error_code: text.nullable(),
    cost_cny: amount.nullable(),
    reserved_cny: amount,
    input_tokens: count.nullable(),
    output_tokens: count.nullable(),
    judge_operation_id: z.uuid().nullable(),
    media_state: text.nullable(),
    media_endpoint_id: z.uuid().nullable(),
    cancel_requested: z.boolean().nullable(),
    dispatch_deadline: timestamp.nullable(),
    result_expires_at: timestamp.nullable(),
    attempts: z.array(attempt).max(3),
  }),
  z.object({
    kind: z.literal("approvals"),
    id: z.uuid(),
    turn_id: z.uuid(),
    operation_id: z.uuid(),
    recorded_state: z.enum(["pending", "approved", "rejected", "feedback"]),
    state: approvalStates,
    created_at: timestamp,
    expires_at: timestamp,
    decided_at: timestamp.nullable(),
    decided_by: z.uuid().nullable(),
    grants: z
      .array(
        z.object({
          guardrail_id: text,
          version: z.number().int().positive(),
          expires_at: timestamp,
        }),
      )
      .max(256),
  }),
  z.object({
    kind: z.literal("events"),
    sequence: count,
    created_at: timestamp,
    event_type: z.enum([
      "turn.created",
      "turn.closed",
      "guardrail.started",
      "guardrail.finished",
      "approval.required",
      "approval.decided",
      "guardrails.evaluated",
      "operation.claimed",
      "operation.receipted",
      "operation.reconciled",
      "unrecognized",
    ]),
    operation_id: z.uuid().nullable(),
    approval_id: z.uuid().nullable(),
    reconciliation: z
      .enum(["confirmed_executed", "confirmed_not_executed"])
      .nullable(),
  }),
  z.object({
    kind: z.literal("checks"),
    guardrail_id: text,
    version: z.number().int().positive(),
    name: text,
    stage: stages,
    rule_kind: z.enum(["always", "contains_any", "model_judge", "asset_scan"]),
    outcome: z.enum([
      "pending",
      "running",
      "passed",
      "risk",
      "failed",
      "skipped_by_user_approval",
    ]),
    current_active: z.boolean(),
    created_at: timestamp,
    approval_skipped_at: timestamp.nullable(),
    source: z.enum(["live", "legacy_event", "legacy_evaluation"]),
    request_id: z.uuid().nullable(),
    judge_deadline: timestamp.nullable(),
  }),
]);
export type TraceItem = z.infer<typeof traceItem>;
export const traceItemsSchema = z
  .object({
    section: sectionSchema,
    observed_at: timestamp,
    data: z.array(traceItem).max(50),
    next_after: traceCursor.nullable(),
    event_fence: count.nullable(),
  })
  .superRefine((v, ctx) => {
    if (
      v.data.some((r) => r.kind !== v.section) ||
      (v.section === "approvals" && v.data.length > 5)
    )
      ctx.addIssue({ code: "custom", message: "trace section mismatch" });
    if (
      v.next_after &&
      v.next_after.kind !==
        (v.section === "events"
          ? "event"
          : v.section === "checks"
            ? "check"
            : "time")
    )
      ctx.addIssue({ code: "custom", message: "trace cursor mismatch" });
    if ((v.section === "events") !== (v.event_fence !== null))
      ctx.addIssue({ code: "custom", message: "event fence required" });
  });
export const turnLabels: Record<z.infer<typeof turnStates>, string> = {
  active: "轮次进行中",
  expired: "轮次已过期",
  completed: "轮次已关闭：完成",
  cancelled: "轮次已取消",
  failed: "轮次已关闭：失败",
};
export const sectionLabels: Record<Section, string> = {
  nodes: "受控节点",
  requests: "模型与护栏调用",
  approvals: "审批历史",
  events: "持久事件",
  checks: "节点护栏记录",
};
export const eventLabels: Record<string, string> = {
  "turn.created": "创建轮次",
  "turn.closed": "关闭轮次",
  "guardrail.started": "模型护栏开始检测",
  "guardrail.finished": "模型护栏结束检测",
  "approval.required": "请求真人审批",
  "approval.decided": "记录真人决定",
  "guardrails.evaluated": "护栏检查完成",
  "operation.claimed": "领取单次执行权",
  "operation.receipted": "记录执行回执",
  "operation.reconciled": "人工核对执行结果",
  unrecognized: "未识别的历史事件（不展示原文）",
};
