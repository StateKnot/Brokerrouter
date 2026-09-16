import { z } from "zod";
import { count } from "./contracts";
export const stages = [
  "pre",
  "post",
  "tool_input",
  "tool_output",
  "artifact",
  "media_output",
] as const;
export const stageLabels: Record<(typeof stages)[number], string> = {
  pre: "模型输入",
  post: "模型输出",
  tool_input: "工具执行前",
  tool_output: "工具执行后",
  artifact: "智能体产物描述",
  media_output: "隔离媒体输出",
};
export const ruleSchema = z.discriminatedUnion("kind", [
  z.strictObject({ kind: z.literal("always") }),
  z.strictObject({
    kind: z.literal("contains_any"),
    terms: z.array(z.string().min(1).max(256)).min(1).max(64),
    case_sensitive: z.boolean(),
  }),
  z.strictObject({
    kind: z.literal("model_judge"),
    model: z.string().min(1).max(200),
    instructions: z.string().min(1).max(8000),
    input_format: z.literal("text_json"),
    max_input_bytes: z.number().int().min(1).max(131072),
    max_output_tokens: z.number().int().min(16).max(512),
    maximum_cny: z.string().max(40),
  }),
  z.strictObject({ kind: z.literal("asset_scan"), profile_id: z.uuid() }),
]);
export type Rule = z.infer<typeof ruleSchema>;
export const versionSchema = z.object({
  id: z.string().max(100),
  version: z.number().int().min(1).max(2147483647),
  name: z.string().max(200),
  stage: z.enum(stages),
  rule: ruleSchema,
});
export const guardrailSchema = versionSchema.extend({
  revision: count,
  active: z.boolean(),
});
export type Guardrail = z.infer<typeof guardrailSchema>;
export const guardrailsSchema = z.object({
  data: z.array(guardrailSchema).max(100),
  next_after: z.string().nullable(),
  can_write: z.boolean(),
});
export const policySchema = z.object({
  id: z.uuid(),
  guardrail_id: z.string().max(100),
  revision: count,
  active: z.boolean(),
  model_id: z.string().max(200).nullable(),
  key_id: z.uuid().nullable(),
  runtime_id: z.string().max(200).nullable(),
  tool_name: z.string().max(200).nullable(),
});
export type Policy = z.infer<typeof policySchema>;
export const policiesSchema = z.object({
  data: z.array(policySchema).max(100),
  next_after: z.uuid().nullable(),
  can_write: z.boolean(),
});
export const historySchema = z.object({
  data: z.array(versionSchema).max(100),
  next_before: z.number().int().positive().nullable(),
});
export const targetsSchema = z.object({
  data: z
    .array(
      z.object({
        id: z.string().max(200),
        name: z.string().max(300),
        active: z.boolean(),
      }),
    )
    .max(100),
  next_after: z.string().nullable(),
  can_write: z.boolean(),
});
export const previewSchema = z.object({
  data: z
    .array(
      versionSchema.omit({ rule: true }).extend({
        kind: z.enum(["always", "contains_any", "model_judge", "asset_scan"]),
      }),
    )
    .max(256),
  runtime_id: z.string().max(200),
  judge_limit_exceeded: z.boolean(),
  evaluated_payload: z.literal(false),
});
