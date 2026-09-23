import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import { createHash } from "node:crypto";
import AxeBuilder from "@axe-core/playwright";
import { video } from "./media-fixture";
const id = "11111111-1111-4111-8111-111111111111",
  op = "22222222-2222-4222-8222-222222222222",
  turn = "33333333-3333-4333-8333-333333333333",
  old = "44444444-4444-4444-8444-444444444444";
const sha = createHash("sha256").update(video).digest("hex");
async function setup(page: Page, mode = "normal") {
  const expires = new Date(
    Date.now() + (mode === "expiry" ? 2500 : 240000),
  ).toISOString();
  let session = "",
    chunks = 0,
    completions = 0,
    decisions = 0;
  const approval = {
    id,
    operation_id: op,
    turn_id: turn,
    state: "pending",
    guardrails: [
      {
        id: "media-risk",
        version: 1,
        name: "视频内容审核",
        stage: "media_output",
        reason_code: "asset_content_unknown",
        evaluation_outcome: "failed",
      },
    ],
    summary: { session_id: "session", run_id: "run", generation: 0 },
    expires_at: expires,
    scope: "current_turn_only",
    resume_requires_recheck: true,
    scope_notice: "本轮所选版本护栏豁免，不扩大执行权限。",
    options: ["continue_this_turn", "reject", "feedback"],
  };
  const review = {
    approval,
    review_id: old,
    review_expires_at: expires,
    stage: "media_output",
    model_id: "video-model",
    tool_name: null,
    operation_state: "waiting_approval",
    payload_text: null,
    operation_sha256: "a".repeat(64),
    can_continue: false,
    unavailable_reason: "binary_review_not_supported",
  };
  await page.context().addCookies([
    {
      name: "__Host-br-csrf",
      value: "a".repeat(64),
      domain: "localhost",
      path: "/",
      secure: true,
      sameSite: "Lax",
    },
    {
      name: "__Host-br-session",
      value: "b".repeat(64),
      domain: "localhost",
      path: "/",
      secure: true,
      sameSite: "Lax",
      httpOnly: true,
    },
  ]);
  await page.route("**/bff/v1/session", (r) =>
    r.fulfill({
      json: {
        tenant_id: id,
        user_id: op,
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
        media_preview_origin:
          mode === "same-host"
            ? "https://localhost:4174"
            : "https://127.0.0.1:4174",
      },
    }),
  );
  await page.route("**/bff/v1/approvals/query", (r) =>
    r.fulfill({
      json: {
        data: [
          {
            ...approval,
            stage: "media_output",
            model_id: "video-model",
            tool_name: null,
            operation_state: "waiting_approval",
            created_at: new Date().toISOString(),
          },
        ],
        next_after: null,
      },
    }),
  );
  await page.route("**/bff/v1/approvals/review", (r) =>
    r.fulfill({ json: review }),
  );
  await page.route("**/bff/v1/media-review/start", (r) => {
    session = r.request().postDataJSON().session_id;
    return r.fulfill({
      json: {
        contract: "isolated-mp4-review-v1",
        session_id: session,
        approval_id: id,
        expires_at: expires,
        output: {
          contract: "current-turn-output-v1",
          sha256: mode === "digest" ? "f".repeat(64) : sha,
          byte_length: video.length,
          chunk_count: 1,
          chunk_bytes: 4194304,
          expires_at: expires,
          scope: "current_turn_only",
        },
      },
    });
  });
  await page.route("**/bff/v1/media-review/chunk", (r) => {
    chunks++;
    expect(r.request().postDataJSON()).toEqual({
      session_id: session,
      ordinal: 0,
    });
    return mode === "revoked"
      ? r.fulfill({ status: 403, json: { error: { code: "denied" } } })
      : r.fulfill({ contentType: "application/octet-stream", body: video });
  });
  await page.route("**/bff/v1/media-review/complete", (r) => {
    completions++;
    expect(r.request().postDataJSON()).toEqual({
      session_id: session,
      sha256: sha,
      reviewed: true,
    });
    return r.fulfill({
      json: {
        ...review,
        review_id: session,
        can_continue: true,
        unavailable_reason: "",
        media: { kind: "isolated_mp4", sha256: sha, byte_length: video.length },
      },
    });
  });
  await page.route("**/bff/v1/approvals/decide", (r) => {
    decisions++;
    const body = r.request().postDataJSON();
    const rejecting = body.decision.decision === "reject";
    expect(body.review_id).toBe(rejecting ? old : session);
    if (rejecting) expect(body.decision.approved_guardrail_ids).toEqual([]);
    return r.fulfill({
      json: { ...approval, state: rejecting ? "rejected" : "approved" },
    });
  });
  await page.goto("/#approvals");
  await page.getByRole("button", { name: "审阅风险原文" }).click();
  return { counts: () => ({ chunks, completions, decisions }) };
}
async function load(page: Page) {
  await page
    .getByRole("button", { name: "加载隔离视频（读取敏感内容）" })
    .click();
}
test("isolated MP4 requires real decode and explicit review before separate double-confirmed decision", async ({
  page,
}) => {
  const remote: Array<{
    url: string;
    cookie: string | undefined;
    referer: string | undefined;
  }> = [];
  page.on("request", (r) => {
    if (r.url().startsWith("https://127.0.0.1:4174"))
      remote.push({
        url: r.url(),
        cookie: r.headers().cookie,
        referer: r.headers().referer,
      });
  });
  const f = await setup(page);
  expect(f.counts()).toEqual({ chunks: 0, completions: 0, decisions: 0 });
  await expect(
    page.getByRole("option", { name: "同意本轮跳过所选护栏" }),
  ).toHaveJSProperty("disabled", true);
  await load(page);
  const confirm = page.getByRole("button", {
    name: "我已实际审阅视频，进入审批决定",
  });
  await expect(confirm).toBeEnabled();
  const iframe = page.locator("iframe");
  await expect(iframe).toHaveAttribute("sandbox", "allow-scripts");
  expect(await page.locator("video").count()).toBe(0);
  const viewer = page.frameLocator("iframe");
  await expect(viewer.getByRole("status")).toContainText("完整内容摘要一致");
  const isolated = await viewer.locator("video").evaluate((v) => {
    let cookie = false,
      storage = false,
      parent = false;
    try {
      void document.cookie;
    } catch {
      cookie = true;
    }
    try {
      void localStorage.length;
    } catch {
      storage = true;
    }
    try {
      void window.parent.document;
    } catch {
      parent = true;
    }
    return {
      cookie,
      storage,
      parent,
      ready: (v as HTMLVideoElement).readyState,
    };
  });
  expect(isolated).toMatchObject({
    cookie: true,
    storage: true,
    parent: true,
  });
  expect(isolated.ready).toBeGreaterThanOrEqual(2);
  expect(remote.length).toBe(3);
  expect(
    remote.every((r) => r.cookie === undefined && r.referer === undefined),
  ).toBe(true);
  await confirm.click();
  // The completion response replaces the reviewed receipt and remounts the
  // decision form. Do not select while that network transition is in flight.
  await expect(
    page.getByText("媒体审阅已记录；尚未提交审批决定，请在下方选择意见。"),
  ).toBeVisible();
  await expect(page.getByRole("combobox", { name: "审批意见" })).toBeEnabled();
  expect(f.counts()).toEqual({ chunks: 1, completions: 1, decisions: 0 });
  await page
    .getByRole("combobox", { name: "审批意见" })
    .selectOption("continue_this_turn");
  await page.getByRole("checkbox", { name: /视频内容审核/ }).check();
  await page.getByRole("checkbox", { name: /我已核对操作与风险/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
  expect(f.counts().decisions).toBe(0);
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/决定已记录/)).toBeVisible();
  expect(f.counts().decisions).toBe(1);
  await expect(iframe).toHaveCount(0);
});
test("digest mismatch clears frame and never produces approval receipt", async ({
  page,
}) => {
  const f = await setup(page, "digest");
  await load(page);
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.locator("iframe")).toHaveCount(0);
  expect(f.counts().completions).toBe(0);
  await expect(
    page.getByRole("option", { name: "同意本轮跳过所选护栏" }),
  ).toHaveJSProperty("disabled", true);
});
test("revocation during chunk loading clears sensitive review", async ({
  page,
}) => {
  const f = await setup(page, "revoked");
  await load(page);
  await expect(
    page.getByRole("button", { name: "重新检查权限" }),
  ).toBeVisible();
  await expect(page.locator("iframe")).toHaveCount(0);
  expect(f.counts().completions).toBe(0);
});
test("same-host preview and untrusted window messages cannot enable approval", async ({
  page,
}) => {
  const f = await setup(page, "same-host");
  await expect(page.getByText(/独立媒体预览站点尚未配置/)).toBeVisible();
  await page.evaluate(() =>
    window.postMessage(
      { contract: "isolated-mp4-viewer-v1", state: "rendered" },
      "*",
    ),
  );
  await expect(
    page.getByRole("option", { name: "同意本轮跳过所选护栏" }),
  ).toHaveJSProperty("disabled", true);
  expect(f.counts()).toEqual({ chunks: 0, completions: 0, decisions: 0 });
});
test("review deadline removes isolated player and prevents submission", async ({
  page,
}) => {
  const f = await setup(page, "expiry");
  await load(page);
  await expect(page.locator("iframe")).toHaveCount(1);
  await expect(page.locator("iframe")).toHaveCount(0, { timeout: 10000 });
  expect(f.counts().completions).toBe(0);
});
test("owner may reject risky media without loading or attesting to viewing it", async ({
  page,
}) => {
  const f = await setup(page);
  await page.getByRole("combobox", { name: "审批意见" }).selectOption("reject");
  await page.getByRole("checkbox", { name: /我已核对操作与风险/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
  expect(f.counts()).toEqual({ chunks: 0, completions: 0, decisions: 0 });
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/决定已记录：已拒绝/)).toBeVisible();
  expect(f.counts()).toEqual({ chunks: 0, completions: 0, decisions: 1 });
});
test("isolated review is keyboard-accessible at narrow widths and enlarged text", async ({
  page,
}, info) => {
  await setup(page);
  await load(page);
  await expect(
    page.getByRole("button", { name: "我已实际审阅视频，进入审批决定" }),
  ).toBeEnabled();
  for (const width of [320, 768, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    const bounds = await page.locator("iframe").boundingBox();
    expect(bounds!.width).toBeLessThanOrEqual(width);
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
  }
  await page.setViewportSize({ width: 768, height: 1000 });
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "200%";
  });
  const a11y = await new AxeBuilder({ page })
    .include(".approval-workbench")
    .analyze();
  expect(a11y.violations).toEqual([]);
  await page.evaluate(() => {
    (document.activeElement as HTMLElement)?.blur();
    window.scrollTo(0, 0);
  });
  await page.screenshot({
    path: info.outputPath("isolated-media-review.png"),
    fullPage: true,
  });
});
