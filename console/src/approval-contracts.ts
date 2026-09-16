import { z } from "zod";
import { timestamp } from "./contracts";
const stage = z.enum([
  "pre",
  "post",
  "tool_input",
  "tool_output",
  "artifact",
  "media_output",
]);
const state = z.enum([
  "pending",
  "approved",
  "rejected",
  "feedback",
  "expired",
]);
const operation = z.enum([
  "ready",
  "checking",
  "waiting_approval",
  "rejected",
  "feedback",
  "executing",
  "execution_unknown",
  "completed",
]);
const cursor = z.object({ created_at: timestamp, id: z.uuid() });
export const inboxSchema = z.object({
  data: z
    .array(
      z.object({
        id: z.uuid(),
        operation_id: z.uuid(),
        turn_id: z.uuid(),
        stage,
        model_id: z.string().nullable(),
        tool_name: z.string().nullable(),
        state,
        operation_state: operation,
        expires_at: timestamp,
        created_at: timestamp,
      }),
    )
    .max(50),
  next_after: cursor.nullable(),
});
export const approvalSchema = z.object({
  id: z.uuid(),
  operation_id: z.uuid(),
  turn_id: z.uuid(),
  state,
  guardrails: z
    .array(
      z.object({
        id: z.string().max(200),
        version: z.number().int().positive(),
        name: z.string().max(200),
        stage,
        reason_code: z.string().max(200),
        evaluation_outcome: z.enum(["risk", "failed"]).optional(),
      }),
    )
    .min(1)
    .max(256),
  summary: z.object({
    session_id: z.string().max(200),
    run_id: z.string().max(200),
    generation: z.number().int(),
  }),
  expires_at: timestamp,
  scope: z.literal("current_turn_only"),
  resume_requires_recheck: z.literal(true),
  scope_notice: z.string().max(500),
  options: z.array(z.enum(["continue_this_turn", "reject", "feedback"])).max(3),
});
export const reviewSchema = z
  .object({
    approval: approvalSchema,
    review_id: z.uuid(),
    review_expires_at: timestamp,
    stage,
    model_id: z.string().nullable(),
    tool_name: z.string().nullable(),
    operation_state: operation,
    // Exact JSON numeric text: never parse + stringify operation content in JavaScript.
    payload_text: z.string().max(131072).nullable(),
    operation_sha256: z.string().regex(/^[a-f0-9]{64}$/),
    can_continue: z.boolean(),
    unavailable_reason: z.enum([
      "",
      "binary_review_not_supported",
      "content_unavailable",
      "content_too_large",
    ]),
    media: z
      .object({
        kind: z.literal("isolated_mp4"),
        sha256: z.string().regex(/^[a-f0-9]{64}$/),
        byte_length: z.number().int().min(1).max(67108864),
      })
      .optional(),
  })
  .refine(
    (v) =>
      !v.can_continue ||
      (v.unavailable_reason === "" &&
        (v.stage === "media_output"
          ? v.payload_text === null && v.media !== undefined
          : v.payload_text !== null && v.media === undefined)),
  );
export type Review = z.infer<typeof reviewSchema>;
export type Decision = {
  approval_id: string;
  review_id: string;
  acknowledged_scope: true;
  decision: {
    decision: "continue_this_turn" | "reject" | "feedback";
    approved_guardrail_ids: string[];
    feedback: string | null;
  };
};
export const approvalLabels: Record<string, string> = {
  pending: "待本人决定",
  approved: "已批准，尚需节点复核",
  rejected: "已拒绝",
  feedback: "已反馈，原轮次关闭",
  expired: "已过期",
  ready: "就绪，未执行",
  checking: "检测中",
  waiting_approval: "等待审批",
  executing: "执行中",
  execution_unknown: "执行结果不确定",
  completed: "执行已完成",
};
