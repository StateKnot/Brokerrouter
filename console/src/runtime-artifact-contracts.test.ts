import { describe, it, expect } from "vitest";
import {
  runtimeArtifactOutput,
  sameRuntimeArtifact,
} from "./runtime-artifact-contracts";
import {
  approvalSchema,
  reviewSchema,
  isolatedReview,
} from "./approval-contracts";
import { mediaManifest } from "./MediaReview";

// Contract fixture only. This does not claim that bytes were scanned or reviewed.
const id = "11111111-1111-4111-8111-111111111111";
const other = "22222222-2222-4222-8222-222222222222";
const expires = "2026-10-11T12:00:00Z";
const output = {
  contract: "runtime-artifact-mp4-v1",
  source: "runtime_artifact",
  scan_id: id,
  asset_id: id,
  profile_id: id,
  turn_id: id,
  model_id: "native-model",
  tool_name: "bounded_mp4",
  batch_id: id,
  input_operation_id: id,
  ordinal: 0,
  producer_revision: "1".repeat(64),
  original_payload_sha256: "2".repeat(64),
  input_payload_sha256: "3".repeat(64),
  sha256: "4".repeat(64),
  byte_length: 1755,
  chunk_count: 1,
  chunk_bytes: 4194304,
  report_sha256: "5".repeat(64),
  scan_report: {
    sha256: "4".repeat(64),
    byte_length: 1755,
    profile_version: 1,
    engine_revision: "6".repeat(64),
    technical: "clean",
    content: "unknown",
    media_type: "video/mp4",
    reason_codes: ["content_not_certified"],
    evidence_sha256: "7".repeat(64),
    scanned_at: "2026-10-11T11:00:00Z",
  },
  expires_at: expires,
  scope: "current_artifact_in_original_turn",
};
const approval = {
  id,
  operation_id: id,
  turn_id: id,
  state: "pending",
  guardrails: [
    {
      id: "native-content",
      version: 1,
      name: "native content",
      stage: "artifact",
      reason_code: "content_unknown",
    },
  ],
  summary: {
    session_id: "native-session",
    run_id: "native-run",
    generation: 0,
    stage: "artifact",
    model_id: output.model_id,
    tool_name: output.tool_name,
    output,
  },
  expires_at: expires,
  scope: "current_artifact_in_original_turn",
  resume_requires_recheck: true,
  scope_notice: "this exact file only",
  options: ["continue_this_turn", "reject", "feedback"],
};
const review = {
  approval,
  review_id: id,
  review_expires_at: expires,
  stage: "artifact",
  model_id: output.model_id,
  tool_name: output.tool_name,
  operation_state: "waiting_approval",
  payload_text: null,
  operation_sha256: "8".repeat(64),
  can_continue: true,
  unavailable_reason: "",
  media: {
    kind: "isolated_mp4",
    source: "runtime_artifact",
    scope: output.scope,
    sha256: output.sha256,
    byte_length: output.byte_length,
    output,
  },
};

describe("exact native artifact review contract", () => {
  it("accepts complete native evidence without converting the legacy media branch", () => {
    expect(runtimeArtifactOutput.safeParse(output).success).toBe(true);
    const parsed = reviewSchema.parse(review);
    expect(isolatedReview(parsed)).toBe(true);
    expect(
      mediaManifest.safeParse({
        contract: "isolated-mp4-review-v1",
        session_id: id,
        approval_id: id,
        expires_at: expires,
        output,
      }).success,
    ).toBe(true);
  });
  it("cannot approve an artifact summary or JSON metadata as complete byte review", () => {
    expect(
      reviewSchema.safeParse({ ...review, media: undefined }).success,
    ).toBe(false);
    expect(
      reviewSchema.safeParse({
        ...review,
        media: undefined,
        payload_text: "{}",
      }).success,
    ).toBe(false);
    expect(
      reviewSchema.safeParse({
        ...review,
        media: undefined,
        can_continue: false,
        unavailable_reason: "binary_review_not_supported",
      }).success,
    ).toBe(true);
  });
  it("rejects stage, source, scope and original operation mismatches", () => {
    for (const value of [
      { ...review, stage: "media_output" },
      { ...review, approval: { ...approval, scope: "current_turn_only" } },
      { ...review, media: { ...review.media, source: "media_job" } },
      { ...review, media: { ...review.media, scope: "current_turn_only" } },
      {
        ...review,
        media: {
          ...review.media,
          output: { ...output, input_operation_id: other },
        },
      },
      {
        ...review,
        media: { ...review.media, output: { ...output, batch_id: other } },
      },
    ])
      expect(reviewSchema.safeParse(value).success).toBe(false);
  });
  it("rejects file, current scan, profile and full-report drift", () => {
    for (const patch of [
      { asset_id: other },
      { scan_id: other },
      { profile_id: other },
      { producer_revision: "9".repeat(64) },
      { report_sha256: "9".repeat(64) },
      { ordinal: 1 },
      { scan_report: { ...output.scan_report, profile_version: 2 } },
    ]) {
      const changed = { ...output, ...patch };
      expect(sameRuntimeArtifact(output, changed)).toBe(false);
      expect(
        reviewSchema.safeParse({
          ...review,
          media: { ...review.media, output: changed },
        }).success,
      ).toBe(false);
    }
    expect(
      sameRuntimeArtifact(
        output,
        Object.fromEntries(Object.entries(output).reverse()),
      ),
    ).toBe(true);
  });
  it("refuses technically unsafe media, inconsistent bytes and caller paths", () => {
    for (const patch of [
      { scan_report: { ...output.scan_report, technical: "unsafe" } },
      { scan_report: { ...output.scan_report, media_type: "text/html" } },
      { scan_report: { ...output.scan_report, sha256: "0".repeat(64) } },
      { scan_report: { ...output.scan_report, byte_length: 42 } },
      { scan_report: { ...output.scan_report, reason_codes: [] } },
      { chunk_count: 2 },
      { byte_length: 67108865 },
      { ordinal: 8 },
      { path: "file:///private/native-output" },
      { job_id: id },
    ])
      expect(
        runtimeArtifactOutput.safeParse({ ...output, ...patch }).success,
      ).toBe(false);
  });
  it("does not let malformed native evidence fall back to a turn-wide approval", () => {
    const malformed = {
      ...output,
      scan_report: { ...output.scan_report, technical: "unsafe" },
    };
    expect(
      approvalSchema.safeParse({
        ...approval,
        scope: "current_turn_only",
        summary: { ...approval.summary, output: malformed },
      }).success,
    ).toBe(false);
    expect(
      reviewSchema.safeParse({
        ...review,
        media: {
          kind: "isolated_mp4",
          sha256: output.sha256,
          byte_length: output.byte_length,
        },
      }).success,
    ).toBe(false);
  });
});
