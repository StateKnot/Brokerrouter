import { z } from "zod";

// All ledger numbers stay decimal strings across the browser boundary.
export const count = z.string().regex(/^(0|[1-9]\d{0,39})$/);
export const amount = z.string().regex(/^(0|[1-9]\d{0,39})(\.\d{1,12})?$/);
export const timestamp = z.iso.datetime({ offset: true });
const text = z.string().max(512);
export const statuses = [
  "reserved",
  "submitting",
  "submission_unknown",
  "succeeded",
  "failed",
  "reconciled",
] as const;
export const statusLabels: Record<(typeof statuses)[number], string> = {
  reserved: "已预留",
  submitting: "提交中",
  submission_unknown: "提交不确定",
  succeeded: "成功",
  failed: "失败",
  reconciled: "已核对",
};
export const totalsSchema = z.object({
  requests: count,
  succeeded: count,
  failed: count,
  reconciled: count,
  in_progress: count,
  submission_unknown: count,
  known_cost_requests: count,
  unsettled_cost_requests: count,
  known_cost_cny: amount,
  reserved_cny: amount,
  mean_known_request_cost_cny: amount.nullable(),
  token_usage_requests: count,
  input_tokens: count,
  output_tokens: count,
  total_tokens: count,
});
export type Totals = z.infer<typeof totalsSchema>;
export const sessionSchema = z.object({
  tenant_id: z.uuid(),
  user_id: z.uuid(),
  expires_at: timestamp,
  currency: z.literal("CNY"),
});
export type Session = z.infer<typeof sessionSchema>;
export const loginSchema = z.object({ authorization_url: z.url() });
export const usageSchema = z.object({
  currency: z.literal("CNY"),
  cost_basis: z.literal("configured_price_ledger_not_provider_invoice"),
  time_basis: z.literal("request_created_at"),
  observed_at: timestamp,
  timezone: z.enum(["Asia/Shanghai", "UTC"]),
  totals: totalsSchema,
  daily: z
    .array(z.object({ date: z.iso.date(), totals: totalsSchema }))
    .max(32),
});
export const groupsSchema = z.object({
  currency: z.literal("CNY"),
  dimension: z.enum(["model", "key"]),
  data: z
    .array(z.object({ object_id: text, label: text, totals: totalsSchema }))
    .max(100),
  next_after: text.nullable(),
});
export const requestCursorSchema = z.object({
  created_at: timestamp,
  id: z.uuid(),
});
export type RequestCursor = z.infer<typeof requestCursorSchema>;
export const requestsSchema = z.object({
  currency: z.literal("CNY"),
  data: z
    .array(
      z.object({
        id: z.uuid(),
        key_id: z.uuid(),
        key_alias: text,
        model_id: text,
        model_name: text,
        status: z.enum(statuses),
        purpose: z.enum(["model", "guardrail"]),
        created_at: timestamp,
        updated_at: timestamp,
        error_code: text.nullable(),
        cost_cny: amount.nullable(),
        reserved_cny: amount,
        input_tokens: count.nullable(),
        output_tokens: count.nullable(),
      }),
    )
    .max(100),
  next_before: requestCursorSchema.nullable(),
});
