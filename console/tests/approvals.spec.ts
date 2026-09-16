import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
const id = "11111111-1111-4111-8111-111111111111";
const op = "22222222-2222-4222-8222-222222222222";
const turn = "33333333-3333-4333-8333-333333333333";
const reviewId = "44444444-4444-4444-8444-444444444444";
function fixture() {
  const expires = new Date(Date.now() + 300000).toISOString();
  const approval = {
    id,
    operation_id: op,
    turn_id: turn,
    state: "pending",
    guardrails: [
      {
        id: "risk-a",
        version: 2,
        name: "外发敏感信息",
        stage: "tool_input",
        reason_code: "configured_rule_match",
      },
      {
        id: "risk-b",
        version: 3,
        name: "语义检测",
        stage: "tool_input",
        reason_code: "judge_unavailable",
        evaluation_outcome: "failed",
      },
    ],
    summary: { session_id: "session", run_id: "run", generation: 0 },
    expires_at: expires,
    scope: "current_turn_only",
    resume_requires_recheck: true,
    scope_notice:
      "继续本轮后，该轮后续动作也会跳过所选护栏；不扩大工具权限，不适用于新轮次。",
    options: ["continue_this_turn", "reject", "feedback"],
  };
  return {
    approval,
    review_id: reviewId,
    review_expires_at: expires,
    stage: "tool_input",
    model_id: null,
    tool_name: "send_message",
    operation_state: "waiting_approval",
    payload_text:
      '{"integer":9007199254740993,"text":"private-only <script>alert(1)</script>","url":"https://example.invalid/never-fetch"}',
    operation_sha256: "a".repeat(64),
    can_continue: true,
    unavailable_reason: "",
  };
}
async function setup(page: Page, review = fixture()) {
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
        tenant_id: id,
        user_id: op,
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
      },
    }),
  );
  await page.route("**/bff/v1/approvals/query", (r) =>
    r.fulfill({
      json: {
        data: [
          {
            ...review.approval,
            stage: review.stage,
            model_id: null,
            tool_name: "send_message",
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
  await page.goto("/#approvals");
  await expect(
    page.getByRole("button", { name: "审阅风险原文" }),
  ).toBeVisible();
}
async function open(page: Page) {
  await page.getByRole("button", { name: "审阅风险原文" }).click();
  await expect(page.getByRole("region", { name: "风险审阅" })).toBeVisible();
}
async function prepare(page: Page, choice = "continue_this_turn") {
  await page.getByRole("combobox", { name: "审批意见" }).selectOption(choice);
  if (choice === "continue_this_turn")
    await page.getByRole("checkbox", { name: /外发敏感信息/ }).check();
  if (choice === "feedback")
    await page
      .getByRole("textbox", { name: /其他意见/ })
      .fill("请改为私有目标，不要发送原文");
  await page.getByRole("checkbox", { name: /我已核对操作与风险/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
}
test("approval reads only on explicit action and preserves inert exact content", async ({
  page,
}) => {
  let reviews = 0;
  page.on("request", (r) => {
    if (r.url().endsWith("/approvals/review")) reviews++;
  });
  const outbound: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes("example.invalid")) outbound.push(r.url());
  });
  await setup(page);
  expect(reviews).toBe(0);
  await expect(page.getByText(/private-only/)).toHaveCount(0);
  await open(page);
  expect(reviews).toBe(1);
  await expect(page.locator(".approval-payload")).toContainText(
    "9007199254740993",
  );
  await expect(page.locator(".approval-payload")).toContainText(
    "<script>alert(1)</script>",
  );
  expect(outbound).toEqual([]);
  await expect(
    page.getByRole("checkbox", { name: /外发敏感信息/ }),
  ).not.toBeChecked();
  await expect(page.getByRole("combobox", { name: "审批意见" })).toHaveValue(
    "",
  );
  await page.getByRole("button", { name: "关闭并清除原文" }).click();
  await expect(page.getByText(/private-only/)).toHaveCount(0);
  expect(
    await page.evaluate(() => [localStorage.length, sessionStorage.length]),
  ).toEqual([0, 0]);
});
test("explicit partial approval has confirmation and immutable uncertain retry", async ({
  page,
}) => {
  const bodies: string[] = [];
  const data = fixture();
  await setup(page, data);
  await page.route("**/bff/v1/approvals/decide", (r) => {
    bodies.push(r.request().postData()!);
    return bodies.length === 1
      ? r.abort()
      : r.fulfill({ json: { ...data.approval, state: "approved" } });
  });
  await open(page);
  await prepare(page);
  expect(bodies).toHaveLength(0);
  await expect(
    page.getByRole("region", { name: "审批决定确认" }),
  ).toBeFocused();
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByRole("alert")).toContainText("决定结果尚未核对");
  await expect(page.getByRole("combobox", { name: "审批意见" })).toBeDisabled();
  await page.getByRole("link", { name: "护栏监控", exact: true }).click();
  expect(page.url()).toContain("#approvals");
  await page.getByRole("button", { name: "核对原决定（同内容重试）" }).click();
  await expect(page.getByText(/决定已记录：已批准/)).toBeVisible();
  expect(bodies).toHaveLength(2);
  expect(bodies[0]).toBe(bodies[1]);
  expect(JSON.parse(bodies[0]!)).toEqual({
    approval_id: id,
    review_id: reviewId,
    acknowledged_scope: true,
    decision: {
      decision: "continue_this_turn",
      approved_guardrail_ids: ["risk-a"],
      feedback: null,
    },
  });
  await expect(page.getByText(/private-only/)).toHaveCount(0);
});
test("policy conflict stops retry and requires a fresh review", async ({
  page,
}) => {
  await setup(page);
  await page.route("**/bff/v1/approvals/decide", (r) =>
    r.fulfill({ status: 409, json: { error: { code: "policy_changed" } } }),
  );
  await open(page);
  await prepare(page);
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "护栏快照或审阅凭据已失效",
  );
  await expect(
    page.getByRole("button", { name: "核对原决定（同内容重试）" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "结束核对，刷新记录" }).click();
  await expect(page.getByText(/private-only/)).toHaveCount(0);
});
test("feedback closes old turn without selecting grants", async ({ page }) => {
  const data = fixture();
  await setup(page, data);
  let body: unknown;
  await page.route("**/bff/v1/approvals/decide", (r) => {
    body = r.request().postDataJSON();
    return r.fulfill({ json: { ...data.approval, state: "feedback" } });
  });
  await open(page);
  await prepare(page, "feedback");
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/决定已记录：已反馈，原轮次关闭/)).toBeVisible();
  expect(body).toMatchObject({
    decision: {
      decision: "feedback",
      approved_guardrail_ids: [],
      feedback: "请改为私有目标，不要发送原文",
    },
  });
});
test("binary media cannot be approved from JSON placeholders", async ({
  page,
}) => {
  const data = fixture();
  const media = {
    ...data,
    stage: "media_output",
    can_continue: false,
    unavailable_reason: "binary_review_not_supported",
    payload_text: null,
  } as unknown as ReturnType<typeof fixture>;
  await setup(page, media);
  await open(page);
  await expect(
    page.getByRole("button", { name: "核对本次决定" }),
  ).toBeDisabled();
  await expect(page.getByText(/本页不提交媒体决定/)).toBeVisible();
  await expect(page.locator(".approval-payload")).toHaveCount(0);
});
test("review expiry and permission revocation clear sensitive content", async ({
  page,
}) => {
  const data = fixture();
  data.review_expires_at = new Date(Date.now() + 2500).toISOString();
  await setup(page, data);
  await open(page);
  await expect(page.getByText(/审阅凭据已过期，风险原文已清除/)).toBeVisible({
    timeout: 10000,
  });
  await expect(page.getByText(/private-only/)).toHaveCount(0);
  await page.route("**/bff/v1/approvals/review", (r) =>
    r.fulfill({ status: 403, json: { error: { code: "denied" } } }),
  );
  await page.getByRole("button", { name: "审阅风险原文" }).click();
  await expect(
    page.getByRole("button", { name: "重新检查权限" }),
  ).toBeVisible();
  await expect(page.getByRole("region", { name: "本人审批列表" })).toHaveCount(
    0,
  );
});
test("approval review accessible at narrow widths and enlarged text", async ({
  page,
}, testInfo) => {
  await setup(page);
  await open(page);
  for (const width of [320, 768, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  }
  await page.setViewportSize({ width: 1280, height: 1000 });
  await page.evaluate(() => (document.documentElement.style.fontSize = "200%"));
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.evaluate(() => {
    (document.activeElement as HTMLElement)?.blur();
    window.scrollTo(0, 0);
  });
  await page.screenshot({
    path: testInfo.outputPath("approval-review-200.png"),
    fullPage: true,
  });
  await page.evaluate(() => (document.documentElement.style.fontSize = "100%"));
  await page.setViewportSize({ width: 320, height: 900 });
  await page.screenshot({
    path: testInfo.outputPath("approval-review-320.png"),
    fullPage: true,
  });
  const small = await page
    .locator("main button,main input,main select,main textarea")
    .evaluateAll((nodes) =>
      nodes
        .filter((n) => {
          const r = n.getBoundingClientRect();
          return r.width > 0 && r.height > 0 && (r.width < 44 || r.height < 44);
        })
        .map((n) => n.outerHTML),
    );
  expect(small).toEqual([]);
});
