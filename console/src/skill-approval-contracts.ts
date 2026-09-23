import { z } from "zod";
import { timestamp } from "./contracts";

const state = z.enum([
  "pending",
  "approved",
  "rejected",
  "feedback",
  "expired",
  "stale",
]);
export const inbox = z.object({
  data: z
    .array(
      z.object({
        request_id: z.uuid(),
        turn_id: z.uuid(),
        server_id: z.uuid(),
        uri: z.string().max(4096),
        state,
        expires_at: timestamp,
        created_at: timestamp,
      }),
    )
    .max(50),
  next_after: z
    .object({ before_created_at: timestamp, before_id: z.uuid() })
    .nullable(),
});
const request = z.object({
  request_id: z.uuid(),
  turn_id: z.uuid(),
  server_id: z.uuid(),
  observation_id: z.uuid(),
  server_revision: z.string(),
  uri: z.string().max(4096),
  approval_fingerprint: z.string().regex(/^[a-f0-9]{64}$/),
  manifest_sha256: z.string().regex(/^[a-f0-9]{64}$/),
  state,
  feedback: z.string().nullable(),
  expires_at: timestamp,
  activation: z.literal(
    "disabled_until_verified_resource_read_and_explicit_tool_grant",
  ),
});
const resource = z.object({
  uri: z.string().max(4096),
  digest: z.string().max(100),
  size: z.number().int().min(0).max(16777216),
});
export const review = z.object({
  request,
  review_id: z.uuid(),
  review_expires_at: timestamp,
  entry: z
    .object({
      uri: z.string().max(4096),
      frontmatter: z.record(z.string(), z.unknown()),
      resources: z.array(resource).min(1).max(512),
    })
    .passthrough(),
  requests_tool_grant: z.boolean(),
  scope: z.string().max(200),
});
export const receipt = request;
export type Review = z.infer<typeof review>;
export type Decision = {
  request_id: string;
  review_id: string;
  acknowledged_scope: true;
  decision: "approve_this_turn" | "reject" | "feedback";
  feedback: string | null;
};
