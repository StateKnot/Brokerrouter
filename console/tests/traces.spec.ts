import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
const id = "11111111-1111-4111-8111-111111111111",
  user = "22222222-2222-4222-8222-222222222222",
  op = "33333333-3333-4333-8333-333333333333",
  at = "2026-09-16T02:00:00.123456Z";
const turn = {
  id,
  key_id: id,
  user_id: user,
  session_id: "session-main",
  run_id: "run-main",
  generation: 0,
  recorded_state: "active",
  state: "active",
  created_at: at,
  expires_at: at,
  event_clock: "9007199254740993",
  acknowledged_sequence: "1",
  runtime_id: "hermes",
  agent_id: "agent-one",
  key_alias: "governed-key",
  identity_active: true,
};
const detail = {
  observed_at: at,
  currency: "CNY",
  cost_basis: "configured_price_ledger_not_provider_invoice",
  turn,
  costs: [
    {
      purpose: "model",
      requests: "1",
      known_cost_requests: "1",
      unsettled_cost_requests: "0",
      known_cost_cny: "0.000001",
      reserved_cny: "0",
      token_usage_requests: "1",
      input_tokens: "9007199254740993",
      output_tokens: "2",
    },
  ],
  nodes: [{ state: "ready", count: "1" }],
  approvals: [{ state: "approved", count: "1" }],
};
const node = {
  kind: "nodes",
  id: op,
  turn_id: id,
  stage: "tool_input",
  model_id: "alpha",
  tool_name: "send_message",
  created_at: at,
  execution_deadline: null,
  recorded_state: "ready",
  state: "ready",
  operation_sha256: "a".repeat(64),
};
const rows: Record<string, unknown[]> = {
  nodes: [node],
  requests: [
    {
      kind: "requests",
      id,
      turn_id: id,
      model_id: "alpha",
      model_name: "Alpha",
      purpose: "model",
      status: "succeeded",
      created_at: at,
      updated_at: at,
      error_code: null,
      cost_cny: "0.000001",
      reserved_cny: "0",
      input_tokens: "10",
      output_tokens: "20",
      judge_operation_id: null,
      media_state: null,
      media_endpoint_id: null,
      cancel_requested: null,
      dispatch_deadline: null,
      result_expires_at: at,
      attempts: [
        {
          id,
          endpoint_id: id,
          endpoint_name: "阿里端点",
          ordinal: 0,
          status: "succeeded",
          http_status: 200,
          error_code: null,
          cost_cny: "0.000001",
          created_at: at,
          finished_at: at,
        },
      ],
    },
  ],
  checks: [
    {
      kind: "checks",
      guardrail_id: "pii",
      version: 1,
      name: "敏感信息",
      stage: "tool_input",
      rule_kind: "always",
      outcome: "risk",
      current_active: true,
      created_at: at,
      approval_skipped_at: at,
      source: "live",
      request_id: null,
      judge_deadline: null,
    },
  ],
  approvals: [
    {
      kind: "approvals",
      id,
      turn_id: id,
      operation_id: op,
      recorded_state: "approved",
      state: "approved",
      created_at: at,
      expires_at: at,
      decided_at: at,
      decided_by: user,
      grants: [{ guardrail_id: "pii", version: 1, expires_at: at }],
    },
  ],
  events: [
    {
      kind: "events",
      sequence: "9007199254740993",
      created_at: at,
      event_type: "operation.claimed",
      operation_id: op,
      approval_id: null,
      reconciliation: null,
    },
  ],
};
async function setup(page: Page) {
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
        user_id: user,
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
      },
    }),
  );
  await page.route("**/bff/v1/traces/turns/query", (r) =>
    r.fulfill({ json: { observed_at: at, data: [turn], next_before: null } }),
  );
  await page.route("**/bff/v1/traces/turns/detail", (r) =>
    r.fulfill({ json: detail }),
  );
  await page.route("**/bff/v1/traces/turns/items", (r) => {
    const q = r.request().postDataJSON();
    return r.fulfill({
      json: {
        section: q.section,
        observed_at: at,
        data: rows[q.section],
        next_after: null,
        event_fence: q.section === "events" ? "9007199254740993" : null,
      },
    });
  });
}
async function openDetail(page: Page) {
  await page.goto("/#traces");
  await page.getByRole("button", { name: `查看轮次 ${id}` }).click();
  await expect(
    page.getByRole("heading", { name: "轮次治理详情" }),
  ).toBeFocused();
}
test("traces drill down real fact categories without execution authority", async ({
  page,
}) => {
  await setup(page);
  await openDetail(page);
  await expect(page.getByText("¥0.000001", { exact: true })).toBeVisible();
  await expect(page.getByText(/9,007,199,254,740,993 输入/)).toBeVisible();
  await expect(page.getByText(/模型调用成功不代表输出获准释放/)).toBeVisible();
  await expect(
    page.getByRole("button", { name: /批准|同意|恢复|执行/ }),
  ).toHaveCount(0);
  await page.getByRole("button", { name: "查看护栏", exact: true }).click();
  await expect(page.getByLabel("记录类别")).toBeFocused();
  await expect(page.getByRole("cell", { name: /^风险命中/ })).toBeVisible();
  await expect(page.getByText(/原始结论不因后续同意而改写/)).toBeVisible();
  await page.getByLabel("记录类别").selectOption("requests");
  await expect(page.getByText(/阿里端点/)).toBeVisible();
  await page.getByLabel("记录类别").selectOption("approvals");
  await expect(page.getByText(/pii v1/)).toBeVisible();
  await page.getByLabel("记录类别").selectOption("events");
  await expect(page.getByText("领取单次执行权", { exact: true })).toBeVisible();
  await expect(
    page.getByRole("link", { name: "前往本人审批" }),
  ).toHaveAttribute("href", "#approvals");
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
});
test("traces preserve opaque microsecond cursors and AND exact filters", async ({
  page,
}) => {
  await setup(page);
  const queries: Record<string, unknown>[] = [];
  await page.route("**/bff/v1/traces/turns/query", (r) => {
    queries.push(r.request().postDataJSON());
    return r.fulfill({
      json: {
        observed_at: at,
        data: [turn],
        next_before: { created_at: at, id },
      },
    });
  });
  await page.goto("/#traces");
  await page.getByRole("button", { name: "下一批轮次" }).click();
  await expect
    .poll(() => queries.at(-1)?.before)
    .toEqual({ created_at: at, id });
  await page.getByLabel("开始日期", { exact: true }).fill("2026-09-01");
  await page.getByLabel("结束日期", { exact: true }).fill("2026-09-01");
  await page.getByLabel("Runtime", { exact: true }).fill(" hermes ");
  await page.getByLabel("Agent", { exact: true }).fill(" agent-one ");
  await page.getByRole("button", { name: "查询任务" }).click();
  await expect
    .poll(() => queries.at(-1))
    .toEqual({
      filter: {
        start: "2026-08-31T16:00:00Z",
        end: "2026-09-01T16:00:00Z",
        runtime_id: "hermes",
        agent_id: "agent-one",
      },
      before: null,
      limit: 20,
    });
  await page.getByLabel("Key ID", { exact: true }).fill("invalid");
  await page.getByRole("button", { name: "查询任务" }).click();
  await expect(page.getByRole("alert")).toContainText("完整 UUID");
  await page.getByRole("button", { name: `查看轮次 ${id}` }).click();
  await page.getByRole("button", { name: "返回任务列表" }).click();
  await expect(page.getByLabel("开始日期", { exact: true })).toHaveValue(
    "2026-09-01",
  );
  await expect(
    page.getByRole("heading", { name: "任务轮次", exact: true }),
  ).toBeFocused();
});
test("trace event pages retain fence and reset on refresh", async ({
  page,
}) => {
  await setup(page);
  const queries: Record<string, unknown>[] = [];
  const cursor = {
    kind: "event",
    sequence: "9007199254740993",
    through: "9007199254740994",
  };
  await page.route("**/bff/v1/traces/turns/items", (r) => {
    const q = r.request().postDataJSON();
    queries.push(q);
    return r.fulfill({
      json: {
        section: q.section,
        observed_at: at,
        data: rows[q.section],
        next_after: q.section === "events" ? cursor : null,
        event_fence: q.section === "events" ? cursor.through : null,
      },
    });
  });
  await openDetail(page);
  await page.getByLabel("记录类别").selectOption("events");
  await page.getByRole("button", { name: "下一批记录" }).click();
  await expect.poll(() => queries.at(-1)?.after).toEqual(cursor);
  await page.getByRole("button", { name: "刷新轮次记录" }).click();
  await expect.poll(() => queries.at(-1)?.after).toBeNull();
  await page.getByLabel("记录类别").selectOption("approvals");
  await expect.poll(() => queries.at(-1)?.limit).toBe(5);
});
test("trace permission revocation clears all detailed facts", async ({
  page,
}) => {
  await setup(page);
  await openDetail(page);
  await expect(page.getByText("¥0.000001", { exact: true })).toBeVisible();
  await page.route("**/bff/v1/traces/turns/detail", (r) =>
    r.fulfill({
      status: 403,
      json: { error: { code: "console_permission_denied" } },
    }),
  );
  await page.getByRole("button", { name: "刷新轮次记录" }).click();
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText("¥0.000001", { exact: true })).toHaveCount(0);
  await expect(page.getByText("send_message", { exact: false })).toHaveCount(0);
});
test("malformed trace section fails closed without raw response rendering", async ({
  page,
}) => {
  await setup(page);
  await page.route("**/bff/v1/traces/turns/items", (r) =>
    r.fulfill({
      json: {
        section: "nodes",
        observed_at: at,
        data: [{ ...node, kind: "events", payload: "secret-provider-body" }],
        next_after: null,
        event_fence: null,
      },
    }),
  );
  await openDetail(page);
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText("secret-provider-body")).toHaveCount(0);
});
for (const width of [320, 768, 1440])
  test(`trace tables keyboard and layout ${width}px at 200% text`, async ({
    page,
  }, info) => {
    await page.setViewportSize({ width, height: 900 });
    await setup(page);
    await openDetail(page);
    await page.evaluate(() => {
      for (const sheet of Array.from(document.styleSheets)) {
        try {
          sheet.insertRule(
            "html { font-size: 200% !important; }",
            sheet.cssRules.length,
          );
          break;
        } catch {
          /* cross-origin sheet */
        }
      }
    });
    await expect(
      page.getByText("send_message", { exact: false }),
    ).toBeVisible();
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= window.innerWidth + 1,
      ),
    ).toBe(true);
    const region = page.getByRole("region", { name: "受控节点，可横向滚动" });
    await region.focus();
    await expect(region).toBeFocused();
    await page.keyboard.press("ArrowRight");
    expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
    await page.screenshot({
      path: info.outputPath(`traces-${width}.png`),
      fullPage: true,
    });
  });
