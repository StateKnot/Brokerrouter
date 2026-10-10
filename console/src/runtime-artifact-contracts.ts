import { z } from "zod";
import { timestamp } from "./contracts";

const hash = z.string().regex(/^[a-f0-9]{64}$/);
const bytes = z.number().int().min(1).max(67108864);
const report = z.strictObject({
  sha256: hash,
  byte_length: bytes,
  profile_version: z.number().int().positive(),
  engine_revision: hash,
  technical: z.literal("clean"),
  content: z.enum(["pass", "risk", "unknown"]),
  media_type: z.literal("video/mp4"),
  reason_codes: z
    .array(
      z
        .string()
        .min(1)
        .max(64)
        .regex(/^[a-z0-9_]+$/),
    )
    .max(32),
  evidence_sha256: hash,
  scanned_at: timestamp,
});

// This is the exact server-owned native file branch. It does not convert or
// replace the existing current-turn-output-v1 media evidence.
export const runtimeArtifactOutput = z
  .strictObject({
    contract: z.literal("runtime-artifact-mp4-v1"),
    source: z.literal("runtime_artifact"),
    scan_id: z.uuid(),
    asset_id: z.uuid(),
    profile_id: z.uuid(),
    turn_id: z.uuid(),
    model_id: z.string().min(1).max(200),
    tool_name: z.string().min(1).max(200),
    batch_id: z.uuid(),
    input_operation_id: z.uuid(),
    ordinal: z.number().int().min(0).max(7),
    producer_revision: hash,
    original_payload_sha256: hash,
    input_payload_sha256: hash,
    sha256: hash,
    byte_length: bytes,
    chunk_count: z.number().int().min(1).max(16),
    chunk_bytes: z.literal(4194304),
    report_sha256: hash,
    scan_report: report,
    expires_at: timestamp,
    scope: z.literal("current_artifact_in_original_turn"),
  })
  .refine(
    (v) =>
      v.chunk_count === Math.ceil(v.byte_length / v.chunk_bytes) &&
      v.scan_report.sha256 === v.sha256 &&
      v.scan_report.byte_length === v.byte_length &&
      (v.scan_report.content === "pass" ||
        v.scan_report.reason_codes.length > 0) &&
      v.scan_report.reason_codes.every(
        (reason, i, codes) => i === 0 || (codes[i - 1] ?? "") < reason,
      ),
  );

export function sameRuntimeArtifact(a: unknown, b: unknown): boolean {
  const left = runtimeArtifactOutput.safeParse(a),
    right = runtimeArtifactOutput.safeParse(b);
  return (
    left.success &&
    right.success &&
    JSON.stringify(left.data) === JSON.stringify(right.data)
  );
}
