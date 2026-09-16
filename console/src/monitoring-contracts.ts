import { z } from "zod";
import { amount, count, timestamp } from "./contracts";

export const monitorTotals = z.object({
  checks: count,
  passed: count,
  risk: count,
  failed: count,
  pending: count,
  running: count,
  overdue_running: count,
  skipped_initial: count,
  approval_skipped: count,
  legacy_checks: count,
  classified_pass_percent: amount.nullable(),
  inline_samples: count,
  mean_inline_ms: amount.nullable(),
  judge_samples: count,
  mean_judge_lifecycle_ms: amount.nullable(),
  judge_requests: count,
  known_cost_requests: count,
  unsettled_cost_requests: count,
  known_cost_cny: amount,
  reserved_cny: amount,
});
export type MonitorTotals = z.infer<typeof monitorTotals>;
export const monitorSchema = z.object({
  currency: z.literal("CNY"),
  cost_basis: z.literal("configured_price_ledger_not_provider_invoice"),
  time_basis: z.literal("first_observation_at"),
  observed_at: timestamp,
  recorded_from: timestamp,
  history_may_be_partial: z.boolean(),
  timezone: z.enum(["Asia/Shanghai", "UTC"]),
  totals: monitorTotals,
  daily: z
    .array(z.object({ date: z.iso.date(), totals: monitorTotals }))
    .max(32),
  operations: z
    .array(
      z.object({
        state: z.enum([
          "ready",
          "checking",
          "waiting_approval",
          "rejected",
          "feedback",
          "executing",
          "execution_unknown",
          "completed",
        ]),
        count,
        closed_turn: count,
      }),
    )
    .max(8),
  approvals: z
    .array(
      z.object({
        state: z.enum([
          "pending",
          "expired_pending",
          "approved",
          "rejected",
          "feedback",
        ]),
        count,
      }),
    )
    .max(5),
  tenant_active_guardrails: count,
});
export const railCursor = z.object({
  guardrail_id: z.string().min(1).max(100),
  version: z.number().int().positive().max(2147483647),
});
export type RailCursor = z.infer<typeof railCursor>;
export const monitorRailsSchema = z.object({
  observed_at: timestamp,
  data: z
    .array(
      z.object({
        ...railCursor.shape,
        name: z.string().max(200),
        stage: z.enum([
          "pre",
          "post",
          "tool_input",
          "tool_output",
          "artifact",
          "media_output",
        ]),
        kind: z.enum(["always", "contains_any", "model_judge", "asset_scan"]),
        current_active: z.boolean(),
        totals: monitorTotals,
      }),
    )
    .max(100),
  next_after: railCursor.nullable(),
});
export const monitorStateLabels: Record<string, string> = {
  ready: "可继续（未执行）",
  checking: "检测中",
  waiting_approval: "等待审批",
  rejected: "已拒绝",
  feedback: "另提意见",
  executing: "执行中",
  execution_unknown: "执行结果不确定",
  completed: "执行已确认",
  pending: "未过期待决定",
  expired_pending: "已过期待决定",
  approved: "已作出同意决定",
};
export const monitorStageLabels: Record<string, string> = {
  pre: "模型输入",
  post: "模型输出",
  tool_input: "工具输入",
  tool_output: "工具输出",
  artifact: "产物",
  media_output: "媒体交付",
};
