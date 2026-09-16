import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const totals = {
  checks: "6",
  passed: "2",
  risk: "1",
  failed: "1",
  pending: "0",
  running: "1",
  overdue_running: "1",
  skipped_initial: "1",
  approval_skipped: "2",
  legacy_checks: "2",
  classified_pass_percent: "66.6667",
  inline_samples: "3",
  mean_inline_ms: "0.012345",
  judge_samples: "1",
  mean_judge_lifecycle_ms: "1000.123456",
  judge_requests: "2",
  known_cost_requests: "1",
  unsettled_cost_requests: "1",
  known_cost_cny: "0.000001",
  reserved_cny: "0.001000",
};
const summary = {
  currency: "CNY",
  cost_basis: "configured_price_ledger_not_provider_invoice",
  time_basis: "first_observation_at",
  observed_at: "2026-09-16T02:00:00Z",
  recorded_from: "2026-09-15T02:00:00Z",
  history_may_be_partial: true,
  timezone: "Asia/Shanghai",
  totals,
  daily: [{ date: "2026-09-16", totals }],
  operations: [
    { state: "ready", count: "1", closed_turn: "0" },
    { state: "waiting_approval", count: "2", closed_turn: "1" },
  ],
  approvals: [
    { state: "approved", count: "1" },
    { state: "pending", count: "1" },
    { state: "expired_pending", count: "1" },
  ],
  tenant_active_guardrails: "11",
};
const rail = {
  guardrail_id: "semantic-review",
  version: 1,
  name: "语义风险检测",
  stage: "tool_input",
  kind: "model_judge",
  current_active: false,
  totals,
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
        tenant_id: "11111111-1111-4111-8111-111111111111",
        user_id: "22222222-2222-4222-8222-222222222222",
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
      },
    }),
  );
  await page.route("**/bff/v1/governance/monitor/query", (r) =>
    r.fulfill({ json: summary }),
  );
  await page.route("**/bff/v1/governance/monitor/rails/query", (r) =>
    r.fulfill({
      json: {
        observed_at: summary.observed_at,
        data: [rail],
        next_after: null,
      },
    }),
  );
}
test("monitoring distinguishes risk approval execution history and precise CNY", async ({
  page,
}) => {
  await setup(page);
  await page.goto("/#monitoring");
  await expect(
    page.getByRole("heading", { name: "护栏监控", exact: true }),
  ).toBeVisible();
  await expect(page.getByText("66.6667%", { exact: true })).toBeVisible();
  await expect(page.getByText("¥0.000001", { exact: true })).toHaveCount(2);
  await expect(
    page.getByText("可继续（未执行）", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("已作出同意决定", { exact: true })).toBeVisible();
  await expect(page.getByText(/历史覆盖提示/)).toBeVisible();
  await page.getByText("查看统计口径与耗时覆盖", { exact: true }).click();
  await expect(page.getByText(/即时检查平均 0.012345 ms/)).toBeVisible();
  await expect(
    page.getByRole("button", { name: /批准|同意|执行|放行/ }),
  ).toHaveCount(0);
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
});
test("monitoring filters use exact IDs half-open CNY dates and reset version pagination", async ({
  page,
}) => {
  await setup(page);
  const queries: Record<string, unknown>[] = [];
  await page.route("**/bff/v1/governance/monitor/rails/query", (r) => {
    queries.push(r.request().postDataJSON());
    return r.fulfill({
      json: {
        observed_at: summary.observed_at,
        data: [rail],
        next_after: { guardrail_id: "semantic-review", version: 2147483647 },
      },
    });
  });
  await page.goto("/#monitoring");
  await page.getByRole("button", { name: "下一页护栏" }).click();
  await expect
    .poll(() => queries.at(-1)?.after)
    .toEqual({ guardrail_id: "semantic-review", version: 2147483647 });
  await page.getByLabel("开始日期", { exact: true }).fill("2026-09-01");
  await page.getByLabel("结束日期（含）", { exact: true }).fill("2026-09-01");
  await page.getByLabel("业务模型 ID", { exact: true }).fill(" text/model ");
  await page
    .getByRole("combobox", { name: "检查阶段", exact: true })
    .selectOption("tool_input");
  await page.getByRole("button", { name: "查询 / 刷新监控" }).click();
  await expect
    .poll(() => queries.at(-1))
    .toEqual({
      filter: {
        start: "2026-08-31T16:00:00Z",
        end: "2026-09-01T16:00:00Z",
        model_id: "text/model",
        stage: "tool_input",
      },
      limit: 20,
    });
  await expect(page.getByRole("button", { name: "上一页护栏" })).toBeDisabled();
  await page.getByLabel("虚拟 Key ID", { exact: true }).fill("not-uuid");
  await page.getByRole("button", { name: "查询 / 刷新监控" }).click();
  await expect(page.getByRole("alert")).toContainText("完整 UUID");
});
test("monitoring empty and missing latency never imply healthy or zero latency", async ({
  page,
}) => {
  await setup(page);
  const empty = {
    ...totals,
    ...Object.fromEntries(Object.keys(totals).map((k) => [k, "0"])),
    classified_pass_percent: null,
    mean_inline_ms: null,
    mean_judge_lifecycle_ms: null,
  };
  await page.route("**/bff/v1/governance/monitor/query", (r) =>
    r.fulfill({
      json: {
        ...summary,
        totals: empty,
        daily: [{ date: "2026-09-16", totals: empty }],
        operations: [],
        approvals: [],
      },
    }),
  );
  await page.route("**/bff/v1/governance/monitor/rails/query", (r) =>
    r.fulfill({
      json: { observed_at: summary.observed_at, data: [], next_after: null },
    }),
  );
  await page.goto("/#monitoring");
  await expect(page.getByText("无分类样本", { exact: true })).toBeVisible();
  await expect(page.getByText(/不代表系统无风险/)).toBeVisible();
  await page.getByText("查看统计口径与耗时覆盖", { exact: true }).click();
  await expect(page.getByText(/即时检查平均 未采集/)).toBeVisible();
  await expect(page.getByText("Healthy", { exact: true })).toHaveCount(0);
});
test("monitoring clears reports on error and removes data after permission revocation", async ({
  page,
}) => {
  await setup(page);
  await page.goto("/#monitoring");
  await expect(page.getByText("66.6667%", { exact: true })).toBeVisible();
  await page.route("**/bff/v1/governance/monitor/query", (r) =>
    r.fulfill({ status: 503, json: { error: { code: "console_timeout" } } }),
  );
  await page.getByRole("button", { name: "查询 / 刷新监控" }).click();
  await expect(page.getByText("66.6667%", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("alert")).toContainText("已清除旧数据");
  await page.route("**/bff/v1/governance/monitor/query", (r) =>
    r.fulfill({ status: 403, json: { error: { code: "console_forbidden" } } }),
  );
  await page.getByRole("button", { name: "查询 / 刷新监控" }).click();
  await expect(page.getByText("语义风险检测", { exact: true })).toHaveCount(0);
  await expect(page.getByText(/当前账号没有此项权限/)).toBeVisible();
});
test("monitoring responsive tables keyboard and 200 percent text remain accessible", async ({
  page,
}, info) => {
  await setup(page);
  await page.goto("/#monitoring");
  await expect(page.getByText("语义风险检测", { exact: true })).toBeVisible();
  for (const width of [320, 640, 768, 1024, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    expect(
      await page.locator("button,input,select,summary").evaluateAll((es) =>
        es
          .filter((e) => {
            const r = e.getBoundingClientRect();
            return (
              r.height > 0 &&
              (r.width < 43.9 ||
                r.height < 43.9 ||
                r.left < 0 ||
                r.right > innerWidth + 0.1)
            );
          })
          .map((e) => e.outerHTML),
      ),
    ).toEqual([]);
  }
  await page.setViewportSize({ width: 320, height: 1000 });
  const table = page.getByRole("region", {
    name: "每日护栏检查明细，可横向滚动",
  });
  await table.focus();
  await expect(table).toBeFocused();
  await page.keyboard.press("ArrowRight");
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.screenshot({
    path: info.outputPath("monitoring-320.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "200%";
  });
  expect(
    await page
      .locator(".monitoring .metric dd > span")
      .evaluateAll((elements) =>
        elements
          .filter(
            (element) =>
              element.getBoundingClientRect().height >
              parseFloat(getComputedStyle(element).lineHeight) + 1,
          )
          .map((element) => element.textContent),
      ),
  ).toEqual([]);
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.screenshot({
    path: info.outputPath("monitoring-200.png"),
    fullPage: true,
  });
});
