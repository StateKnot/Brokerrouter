import { expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import { createHash } from "node:crypto";
import { video } from "./media-fixture";

// Browser contract fixture only: this synthetic report is not scan certification.
// The unchanged deterministic MP4 bytes are really decoded in the isolated frame.
export const artifactIds = {
  approval: "11111111-1111-4111-8111-111111111111",
  operation: "22222222-2222-4222-8222-222222222222",
  turn: "33333333-3333-4333-8333-333333333333",
  review: "44444444-4444-4444-8444-444444444444",
  asset: "55555555-5555-4555-8555-555555555555",
  scan: "66666666-6666-4666-8666-666666666666",
  profile: "77777777-7777-4777-8777-777777777777",
  batch: "88888888-8888-4888-8888-888888888888",
  input: "99999999-9999-4999-8999-999999999999",
  other: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
};
export const artifactSha = createHash("sha256").update(video).digest("hex");
type Json = Record<string, unknown>;
export type ArtifactFixtureOptions = {
  output?: (output: Json) => void;
  review?: (review: Json) => void;
  manifest?: (manifest: Json) => void;
  complete?: (review: Json) => void;
  revoked?: boolean;
};

export async function setupRuntimeArtifact(
  page: Page,
  options: ArtifactFixtureOptions = {},
) {
  const expires = new Date(Date.now() + 240_000).toISOString();
  const output: Json = {
    contract: "runtime-artifact-mp4-v1",
    source: "runtime_artifact",
    scan_id: artifactIds.scan,
    asset_id: artifactIds.asset,
    profile_id: artifactIds.profile,
    turn_id: artifactIds.turn,
    model_id: "native-fixture-model",
    tool_name: "bounded_mp4",
    batch_id: artifactIds.batch,
    input_operation_id: artifactIds.input,
    ordinal: 0,
    producer_revision: "1".repeat(64),
    original_payload_sha256: "2".repeat(64),
    input_payload_sha256: "3".repeat(64),
    sha256: artifactSha,
    byte_length: video.length,
    chunk_count: 1,
    chunk_bytes: 4194304,
    report_sha256: "5".repeat(64),
    scan_report: {
      sha256: artifactSha,
      byte_length: video.length,
      profile_version: 1,
      engine_revision: "6".repeat(64),
      technical: "clean",
      content: "unknown",
      media_type: "video/mp4",
      reason_codes: ["content_not_certified"],
      evidence_sha256: "7".repeat(64),
      scanned_at: new Date(Date.now() - 1000).toISOString(),
    },
    expires_at: expires,
    scope: "current_artifact_in_original_turn",
  };
  options.output?.(output);
  const approval = {
    id: artifactIds.approval,
    operation_id: artifactIds.operation,
    turn_id: artifactIds.turn,
    state: "pending",
    guardrails: [
      {
        id: "native-content",
        version: 1,
        name: "原生产物内容审核",
        stage: "artifact",
        reason_code: "asset_content_unknown",
        evaluation_outcome: "risk",
      },
    ],
    summary: {
      session_id: "native-fixture-session",
      run_id: "native-fixture-run",
      generation: 1,
      stage: "artifact",
      model_id: output.model_id,
      tool_name: output.tool_name,
      output,
    },
    expires_at: expires,
    scope: "current_artifact_in_original_turn",
    resume_requires_recheck: true,
    scope_notice: "本次批准仅覆盖原轮次中的当前文件，其他产物需重新核对。",
    options: ["continue_this_turn", "reject", "feedback"],
  };
  const review: Json = {
    approval,
    review_id: artifactIds.review,
    review_expires_at: expires,
    stage: "artifact",
    model_id: output.model_id,
    tool_name: output.tool_name,
    operation_state: "waiting_approval",
    payload_text: null,
    operation_sha256: "8".repeat(64),
    can_continue: false,
    unavailable_reason: "binary_review_not_supported",
  };
  let session = "",
    starts = 0,
    chunks = 0,
    completions = 0,
    decisions = 0;
  await page.context().addCookies([
    {
      name: "__Host-br-csrf",
      value: "a".repeat(64),
      domain: "localhost",
      path: "/",
      secure: true,
      sameSite: "Lax",
    },
  ]);
  await page.route("**/bff/v1/session", (r) =>
    r.fulfill({
      json: {
        tenant_id: artifactIds.approval,
        user_id: artifactIds.operation,
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
        media_preview_origin: "https://127.0.0.1:4174",
      },
    }),
  );
  await page.route("**/bff/v1/approvals/query", (r) =>
    r.fulfill({
      json: {
        data: [
          {
            id: approval.id,
            operation_id: approval.operation_id,
            turn_id: approval.turn_id,
            stage: "artifact",
            model_id: output.model_id,
            tool_name: output.tool_name,
            state: "pending",
            operation_state: "waiting_approval",
            expires_at: expires,
            created_at: new Date().toISOString(),
          },
        ],
        next_after: null,
      },
    }),
  );
  await page.route("**/bff/v1/approvals/review", (r) => {
    const response = structuredClone(review);
    options.review?.(response);
    return r.fulfill({ json: response });
  });
  await page.route("**/bff/v1/media-review/start", (r) => {
    starts++;
    const body = r.request().postDataJSON();
    expect(body.approval_id).toBe(artifactIds.approval);
    expect(typeof body.session_id).toBe("string");
    session = body.session_id;
    const manifest: Json = {
      contract: "isolated-mp4-review-v1",
      session_id: session,
      approval_id: artifactIds.approval,
      expires_at: expires,
      output: structuredClone(output),
    };
    options.manifest?.(manifest);
    return r.fulfill({ json: manifest });
  });
  await page.route("**/bff/v1/media-review/chunk", (r) => {
    chunks++;
    expect(r.request().postDataJSON()).toEqual({
      session_id: session,
      ordinal: 0,
    });
    return options.revoked
      ? r.fulfill({ status: 403, json: { error: { code: "denied" } } })
      : r.fulfill({ contentType: "application/octet-stream", body: video });
  });
  await page.route("**/bff/v1/media-review/complete", (r) => {
    completions++;
    expect(r.request().postDataJSON()).toEqual({
      session_id: session,
      sha256: artifactSha,
      reviewed: true,
    });
    const response: Json = {
      ...structuredClone(review),
      review_id: session,
      can_continue: true,
      unavailable_reason: "",
      media: {
        kind: "isolated_mp4",
        source: "runtime_artifact",
        scope: "current_artifact_in_original_turn",
        sha256: artifactSha,
        byte_length: video.length,
        output: structuredClone(output),
      },
    };
    options.complete?.(response);
    return r.fulfill({ json: response });
  });
  await page.route("**/bff/v1/approvals/decide", (r) => {
    decisions++;
    const body = r.request().postDataJSON();
    const reject = body.decision.decision === "reject";
    expect(body.approval_id).toBe(artifactIds.approval);
    expect(body.review_id).toBe(reject ? artifactIds.review : session);
    expect(body.acknowledged_scope).toBe(true);
    expect(body.decision.approved_guardrail_ids).toEqual(
      reject ? [] : ["native-content"],
    );
    return r.fulfill({
      json: { ...approval, state: reject ? "rejected" : "approved" },
    });
  });
  await page.goto("/#approvals");
  await page.getByRole("button", { name: "审阅风险原文" }).click();
  return { counts: () => ({ starts, chunks, completions, decisions }) };
}

export async function loadRuntimeArtifact(page: Page) {
  await page
    .getByRole("button", { name: "加载隔离视频（读取敏感内容）" })
    .click();
}

export function jsonField(value: Json, key: string): Json {
  return value[key] as Json;
}
