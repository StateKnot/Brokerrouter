import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

// Deliberately synthetic contract fixtures; none are imported by src/ or shipped.
const tenant = "11111111-1111-4111-8111-111111111111";
const user = "22222222-2222-4222-8222-222222222222";
const key = "33333333-3333-4333-8333-333333333333";
const id = "44444444-4444-4444-8444-444444444444";
const csrf = "a".repeat(64);
const catalogModel = {
  id: "test/model",
  name: "测试逻辑模型",
  active: false,
  revision: "1",
};
const catalogEndpoint = {
  id,
  model_id: "test/model",
  name: "测试供应商端点",
  upstream_model: "qwen-test",
  active: false,
  revision: "2",
  currency: "CNY",
  input_per_million: "0.000000000001",
  output_per_million: "2.500000000000",
  context_tokens: 4096,
  max_output_tokens: 1024,
  priority: 100,
  operation: "chat",
  supports_tools: false,
  embedding_dimension: null,
  connection: {
    id: "approved",
    name: "预登记供应商",
    revision: "b".repeat(64),
  },
};
const totals = {
  requests: "6",
  succeeded: "4",
  failed: "1",
  reconciled: "0",
  in_progress: "0",
  submission_unknown: "1",
  known_cost_requests: "5",
  unsettled_cost_requests: "1",
  known_cost_cny: "0.000408",
  reserved_cny: "0.001000",
  mean_known_request_cost_cny: "0.0000816",
  token_usage_requests: "4",
  input_tokens: "351",
  output_tokens: "48",
  total_tokens: "399",
};
const row = {
  id,
  key_id: key,
  key_alias: "测试环境 Key",
  model_id: "test/qwen",
  model_name: "测试模型",
  status: "submission_unknown",
  created_at: "2026-09-14T00:00:00.123456Z",
  updated_at: "2026-09-14T00:01:00.123457Z",
  error_code: null,
  purpose: "model",
  cost_cny: null,
  reserved_cny: "0.001000",
  input_tokens: null,
  output_tokens: null,
};
const usage = {
  currency: "CNY",
  cost_basis: "configured_price_ledger_not_provider_invoice",
  time_basis: "request_created_at",
  observed_at: "2026-09-14T01:00:00Z",
  timezone: "Asia/Shanghai",
  totals,
  daily: Array.from({ length: 7 }, (_, i) => ({
    date: `2026-09-${String(i + 8).padStart(2, "0")}`,
    totals: {
      ...totals,
      known_cost_cny: [
        "0.000010",
        "0.000020",
        "0.000040",
        "0.000080",
        "0.000050",
        "0.000100",
        "0.000108",
      ][i],
    },
  })),
};
async function setup(page: Page, loggedIn = true) {
  await page.context().addCookies([
    {
      name: "__Host-br-csrf",
      value: csrf,
      domain: "localhost",
      path: "/",
      secure: true,
      sameSite: "Lax",
    },
  ]);
  await page.route("**/bff/v1/session", (route) =>
    route.fulfill({
      status: loggedIn ? 200 : 401,
      json: loggedIn
        ? {
            tenant_id: tenant,
            user_id: user,
            expires_at: new Date(Date.now() + 3_600_000).toISOString(),
            currency: "CNY",
          }
        : { error: { code: "browser_login_required" } },
    }),
  );
  await page.route("**/bff/v1/usage/query", (route) =>
    route.fulfill({ json: usage }),
  );
  await page.route("**/bff/v1/usage/groups/query", (route) =>
    route.fulfill({
      json: {
        currency: "CNY",
        dimension: route.request().postDataJSON().dimension,
        data: [{ object_id: "test/qwen", label: "测试模型", totals }],
        next_after: null,
      },
    }),
  );
  await page.route("**/bff/v1/requests/query", (route) =>
    route.fulfill({
      json: { currency: "CNY", data: [row], next_before: null },
    }),
  );
  await page.route("**/bff/v1/logout", (route) =>
    route.fulfill({ status: 204 }),
  );
}

async function catalogSetup(page: Page, write = true) {
  await setup(page);
  await page.route("**/bff/v1/catalog/models/query", (route) =>
    route.fulfill({
      json: { data: [catalogModel], next_after: null, can_write: write },
    }),
  );
  await page.route("**/bff/v1/catalog/endpoints/query", (route) =>
    route.fulfill({
      json: {
        model: catalogModel,
        data: [catalogEndpoint],
        next_after: null,
        can_write: write,
        connections: [catalogEndpoint.connection],
      },
    }),
  );
}
const managedKey = {
  id: key,
  name: "配置测试 Key",
  prefix: "sk_br_12345678",
  revision: "3",
  budget_cny: "10.000001",
  spent_cny: "0.000001",
  reserved_cny: "1.000000",
  expires_at: "2026-10-15T00:00:00.123456Z",
  revoked_at: null,
  created_at: "2026-09-15T00:00:00Z",
};
async function settingsSetup(page: Page, write = true) {
  await setup(page);
  await page.route("**/bff/v1/settings/keys/query", (route) =>
    route.fulfill({
      json: {
        currency: "CNY",
        data: [managedKey],
        next_after: null,
        can_write: write,
        tenant: {
          budget_cny: "20.000000",
          spent_cny: "0.000001",
          reserved_cny: "1.000000",
        },
      },
    }),
  );
  await page.route("**/bff/v1/settings/keys/detail", (route) =>
    route.fulfill({
      json: {
        key: managedKey,
        models: ["test/model"],
        grants_truncated: false,
        can_write: write,
      },
    }),
  );
  await page.route("**/bff/v1/settings/models/query", (route) =>
    route.fulfill({ json: { data: [catalogModel], next_after: null } }),
  );
  const current = {
    id: "10",
    usd_cny: "7.1234567890",
    source: "财务确认",
    effective_at: "2026-09-01T00:00:00.123456Z",
  };
  await page.route("**/bff/v1/settings/fx/query", (route) =>
    route.fulfill({
      json: {
        currency: "CNY",
        observed_at: "2026-09-15T00:00:00Z",
        latest_id: "11",
        current,
        data: [
          {
            id: "11",
            usd_cny: "7.2500000000",
            source: "计划版本",
            effective_at: "2026-10-01T00:00:00Z",
          },
          current,
        ],
        next_before: null,
        can_write: write,
      },
    }),
  );
}

const governedRail = {
  id: "sensitive-tool",
  name: "敏感工具审批",
  revision: "2",
  version: 2,
  active: true,
  stage: "tool_input",
  rule: {
    kind: "contains_any",
    terms: ["删除", "发送"],
    case_sensitive: false,
  },
};
const governedPolicy = {
  id,
  guardrail_id: governedRail.id,
  revision: "3",
  active: true,
  model_id: null,
  key_id: null,
  runtime_id: null,
  tool_name: null,
};
async function governanceSetup(page: Page, write = true) {
  await setup(page);
  await page.route("**/bff/v1/settings/guardrails/query", (route) =>
    route.fulfill({
      json: { data: [governedRail], next_after: null, can_write: write },
    }),
  );
  await page.route("**/bff/v1/settings/guardrails/history", (route) =>
    route.fulfill({ json: { data: [governedRail], next_before: null } }),
  );
  await page.route("**/bff/v1/settings/policies/query", (route) =>
    route.fulfill({
      json: { data: [governedPolicy], next_after: null, can_write: write },
    }),
  );
  await page.route("**/bff/v1/settings/governance/targets", (route) =>
    route.fulfill({
      json: {
        data: [{ id: "test/model", name: "测试引用", active: true }],
        next_after: null,
        can_write: write,
      },
    }),
  );
  await page.route("**/bff/v1/settings/policies/preview", (route) =>
    route.fulfill({
      json: {
        data: [{ ...governedRail, kind: governedRail.rule.kind }],
        runtime_id: "hermes-test",
        judge_limit_exceeded: false,
        evaluated_payload: false,
      },
    }),
  );
}
test("governance readers see immutable rules but no configuration mutations", async ({
  page,
}) => {
  await governanceSetup(page, false);
  await page.goto("/#governance");
  await expect(page.getByText("仅有治理配置读取权限。")).toBeVisible();
  await expect(
    page.getByRole("button", { name: "新建护栏", exact: true }),
  ).toHaveCount(0);
  await page
    .getByRole("button", { name: `历史版本 ${governedRail.id}` })
    .click();
  await page
    .getByText("v2 · 敏感工具审批 · 工具执行前", { exact: true })
    .click();
  await expect(
    page.getByRole("region", { name: "不可变规则历史" }).locator("pre"),
  ).toContainText("contains_any");
  await page.getByRole("button", { name: "匹配策略", exact: true }).click();
  await expect(page.getByText("全部（不限）")).toHaveCount(4);
  await expect(page.getByRole("button", { name: /编辑策略/ })).toHaveCount(0);
});
test("guardrail publication reviews full structured rule and safely retries identical content", async ({
  page,
}) => {
  await governanceSetup(page);
  const bodies: Record<string, unknown>[] = [];
  await page.route("**/bff/v1/settings/apply", (route) => {
    const body = route.request().postDataJSON();
    bodies.push(body);
    return bodies.length === 1
      ? route.abort()
      : route.fulfill({
          json: {
            operation_id: body.operation_id,
            object_id: "judge-rail",
            revision: "1",
          },
        });
  });
  await page.goto("/#governance");
  await page.getByRole("button", { name: "新建护栏", exact: true }).click();
  await page.getByLabel("护栏 ID", { exact: true }).fill("judge-rail");
  await page.getByLabel("护栏名称", { exact: true }).fill("判定测试");
  await page.getByLabel("检测方式").selectOption("model_judge");
  await page.getByLabel("判定模型 ID").fill("test/model");
  await page
    .getByLabel("风险判定指令")
    .fill("识别数据外发风险\n仅判定，不执行");
  await page.getByLabel("单次判定上限（人民币元）").fill("0.000001");
  await page.getByLabel("变更理由").fill("人工核对范围");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  expect(bodies).toHaveLength(0);
  await expect(page.getByRole("region", { name: "变更确认" })).toBeFocused();
  await expect(
    page.getByRole("region", { name: "变更确认" }).locator("pre"),
  ).toContainText('"maximum_cny": "0.000001"');
  await expect(
    page.getByRole("button", { name: "匹配策略", exact: true }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "确认提交配置", exact: true }).click();
  await page.getByRole("button", { name: "重试同一操作", exact: true }).click();
  expect(bodies).toHaveLength(2);
  expect(bodies[0]).toEqual(bodies[1]);
  expect(bodies[0]!.change).toMatchObject({
    active: false,
    expected_revision: null,
    rule: { kind: "model_judge", maximum_cny: "0.000001" },
  });
  await expect(page.getByText(/已提交配置版本 1/)).toBeVisible();
  expect(
    await page.evaluate(() => localStorage.length + sessionStorage.length),
  ).toBe(0);
});
test("policy editing preserves selector meaning and blocks stale revisions", async ({
  page,
}) => {
  await governanceSetup(page);
  let sent: Record<string, unknown> | null = null;
  await page.route("**/bff/v1/settings/apply", (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({
      status: 409,
      json: { error: { code: "configuration_conflict" } },
    });
  });
  await page.goto("/#governance");
  await page.getByRole("button", { name: "匹配策略", exact: true }).click();
  await page.getByRole("button", { name: `编辑策略 ${id}` }).click();
  await page
    .getByLabel("模型 ID（空值匹配全部）", { exact: true })
    .fill("test/model");
  await page.getByLabel("变更理由").fill("收窄模型范围");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  await expect(page.getByRole("region", { name: "变更确认" })).toContainText(
    "空会匹配",
  );
  await expect(
    page
      .getByRole("region", { name: "变更确认" })
      .getByText("全部（不限）", { exact: true }),
  ).toHaveCount(3);
  await page.getByRole("button", { name: "确认提交配置", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "重试同一操作", exact: true }),
  ).toBeDisabled();
  expect(sent).toMatchObject({
    change: {
      expected_revision: "3",
      model_id: "test/model",
      key_id: null,
      runtime_id: null,
      tool_name: null,
      active: true,
    },
  });
});
test("governance preview sends only context and never performs approval or model calls", async ({
  page,
}) => {
  await governanceSetup(page);
  const bodies: unknown[] = [];
  await page.route("**/bff/v1/settings/policies/preview", (route) => {
    bodies.push(route.request().postDataJSON());
    return route.fulfill({
      json: {
        data: [{ ...governedRail, kind: governedRail.rule.kind }],
        runtime_id: "hermes-test",
        judge_limit_exceeded: true,
        evaluated_payload: false,
      },
    });
  });
  await page.goto("/#governance");
  await page.getByRole("button", { name: "匹配预览", exact: true }).click();
  await page.getByLabel("智能体 Key UUID").fill(key);
  await page.getByLabel("模型 ID（可空）").fill("test/model");
  await page.getByLabel("预览阶段").selectOption("tool_input");
  await page.getByRole("button", { name: "查询匹配结果" }).click();
  await expect(page.getByText(/超过 8 个模型判定护栏/)).toBeVisible();
  expect(bodies).toEqual([
    {
      key_id: key,
      model_id: "test/model",
      tool_name: null,
      stage: "tool_input",
    },
  ]);
  await expect(
    page.getByRole("button", { name: /同意|批准|继续执行/ }),
  ).toHaveCount(0);
});
test("governance permission revocation clears configuration and pending review", async ({
  page,
}) => {
  await governanceSetup(page);
  await page.route("**/bff/v1/settings/apply", (route) =>
    route.fulfill({
      status: 403,
      json: { error: { code: "console_forbidden" } },
    }),
  );
  await page.goto("/#governance");
  await page
    .getByRole("button", { name: `编辑护栏 ${governedRail.id}` })
    .click();
  await page.getByLabel("变更理由").fill("审阅版本");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  await page.getByRole("button", { name: "确认提交配置", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "重新检查权限" }),
  ).toBeVisible();
  await expect(page.getByRole("region", { name: "治理配置" })).toHaveCount(0);
});
test("governance forms and previews remain accessible at narrow widths and 200 percent", async ({
  page,
}) => {
  await governanceSetup(page);
  await page.goto("/#governance");
  await page.getByRole("button", { name: "新建护栏", exact: true }).click();
  await page.getByLabel("检测方式").selectOption("model_judge");
  for (const width of [320, 640, 768, 1024, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    expect(
      await page
        .locator(
          "main input:visible, main textarea:visible, main button:visible, main select:visible",
        )
        .evaluateAll((nodes) =>
          nodes.some((n) => {
            const r = n.getBoundingClientRect();
            return r.left < 0 || r.right > innerWidth || r.height < 44;
          }),
        ),
    ).toBe(false);
  }
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.setViewportSize({ width: 720, height: 1000 });
  await page.evaluate(() => (document.documentElement.style.fontSize = "200%"));
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.getByLabel("护栏名称", { exact: true }).scrollIntoViewIfNeeded();
  await page.screenshot({
    path: "test-results/governance-rules.png",
    fullPage: false,
  });
  await page.evaluate(() => (document.documentElement.style.fontSize = "100%"));
  for (const mode of ["匹配策略", "匹配预览"]) {
    await page.getByRole("button", { name: mode, exact: true }).click();
    if (mode === "匹配策略")
      await page
        .getByRole("button", { name: "新建匹配策略", exact: true })
        .click();
    await page.setViewportSize({ width: 320, height: 1000 });
    expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  }
  await page
    .getByLabel("智能体 Key UUID", { exact: true })
    .scrollIntoViewIfNeeded();
  await page.screenshot({
    path: "test-results/governance-preview.png",
    fullPage: false,
  });
});
test("Key and FX read permissions do not expose write controls", async ({
  page,
}) => {
  await settingsSetup(page, false);
  await page.goto("/#keys");
  await expect(page.getByText("¥10.000001", { exact: true })).toBeVisible();
  await expect(page.getByText("创建虚拟 Key", { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "管理 配置测试 Key" }).click();
  await expect(
    page.getByText("授权模型：test/model", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("永久撤销 Key", { exact: true })).toHaveCount(0);
  await page.goto("/#fx");
  await expect(
    page.getByText("1 USD = 7.1234567890 CNY", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("发布新的人民币汇率", { exact: true }),
  ).toHaveCount(0);
  await expect(page.getByText("待生效", { exact: true })).toBeVisible();
});
test("Key secret stays browser-only and response-loss retry reuses its registration", async ({
  page,
}) => {
  await settingsSetup(page);
  const bodies: Record<string, unknown>[] = [];
  await page.route("**/bff/v1/settings/apply", (route) => {
    const b = route.request().postDataJSON();
    bodies.push(b);
    if (bodies.length === 1) return route.abort();
    return route.fulfill({
      json: { operation_id: b.operation_id, object_id: key, revision: "2" },
    });
  });
  await page.goto("/#keys");
  await page.getByText("创建虚拟 Key", { exact: true }).click();
  await page.getByRole("button", { name: "生成一次性密钥" }).click();
  const token = await page.getByLabel("待保存密钥").inputValue();
  expect(token).toMatch(/^sk_br_[a-f0-9]{64}$/);
  await page.getByLabel("Key 名称", { exact: true }).fill("新建测试 Key");
  await page.getByLabel("人民币预算（元）", { exact: true }).fill("0.000001");
  await page.getByLabel("允许的模型 ID", { exact: false }).fill("test/model");
  await page.getByLabel("我已将密钥保存到受控凭据库", { exact: false }).check();
  await page.getByLabel("变更理由", { exact: true }).fill("登记测试凭据");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  expect(bodies).toHaveLength(0);
  await expect(
    page.getByRole("button", { name: "查询 / 刷新 Key" }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "确认提交配置" }).click();
  await expect(
    page.getByText("暂时无法核对本次提交结果", { exact: false }),
  ).toBeVisible();
  expect(JSON.stringify(bodies[0])).not.toContain(token);
  expect(JSON.stringify(bodies[0])).not.toContain('"token"');
  await expect(page.getByLabel("待保存密钥")).toHaveValue(token);
  await page.getByRole("button", { name: "重试同一操作" }).click();
  await expect(
    page.getByText("已提交配置版本 2", { exact: false }),
  ).toBeVisible();
  expect(bodies).toHaveLength(2);
  expect(bodies[0]).toEqual(bodies[1]);
  const digest = await page.evaluate(
    async (t) =>
      Array.from(
        new Uint8Array(
          await crypto.subtle.digest("SHA-256", new TextEncoder().encode(t)),
        ),
        (v) => v.toString(16).padStart(2, "0"),
      ).join(""),
    token,
  );
  expect((bodies[0]?.change as Record<string, unknown>).key_hash).toBe(digest);
  await expect(page.getByLabel("待保存密钥")).toHaveCount(0);
  expect(
    await page.evaluate(() => localStorage.length + sessionStorage.length),
  ).toBe(0);
});
test("Key revoke requires explicit confirmation and stale version cannot be overwritten", async ({
  page,
}) => {
  await settingsSetup(page);
  const bodies: unknown[] = [];
  await page.route("**/bff/v1/settings/apply", (route) => {
    bodies.push(route.request().postDataJSON());
    return route.fulfill({
      status: 409,
      json: { error: { code: "configuration_conflict" } },
    });
  });
  await page.goto("/#keys");
  await page.getByRole("button", { name: "管理 配置测试 Key" }).click();
  await page.getByText("永久撤销 Key", { exact: true }).click();
  const panel = page.getByRole("region", { name: "Key 详情" });
  await panel.getByLabel("变更理由", { exact: true }).last().fill("疑似泄漏");
  await panel
    .getByRole("button", { name: "核对变更", exact: true })
    .last()
    .click();
  expect(bodies).toHaveLength(0);
  await panel.getByLabel("我确认永久撤销这个 Key").check();
  await panel
    .getByRole("button", { name: "核对变更", exact: true })
    .last()
    .click();
  await page.getByRole("button", { name: "确认提交配置" }).click();
  await expect(
    page.getByRole("button", { name: "重试同一操作" }),
  ).toBeDisabled();
  expect(bodies[0]).toMatchObject({
    change: { kind: "key_revoke", id: key, expected_revision: "3" },
  });
});
test("FX publication preserves decimal precision and prior version, with pending schedule warning", async ({
  page,
}) => {
  await settingsSetup(page);
  const bodies: unknown[] = [];
  await page.route("**/bff/v1/settings/apply", (route) => {
    const b = route.request().postDataJSON();
    bodies.push(b);
    return route.fulfill({
      json: { operation_id: b.operation_id, object_id: "12", revision: "12" },
    });
  });
  await page.goto("/#fx");
  await page.getByText("发布新的人民币汇率", { exact: true }).click();
  await page.getByLabel("1 USD 兑换人民币（CNY）").fill("7.0000000001");
  await page.getByLabel("汇率来源", { exact: true }).fill("财务批准版本");
  await page.getByLabel("变更理由", { exact: true }).fill("更新人民币结算换算");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  await expect(
    page.getByText("未来版本仍会按原定时间生效", { exact: false }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "刷新汇率", exact: true }),
  ).toBeDisabled();
  await page.getByRole("button", { name: "确认提交配置" }).click();
  await expect(
    page.getByText("已提交配置版本 12", { exact: false }),
  ).toBeVisible();
  expect(bodies[0]).toMatchObject({
    change: {
      kind: "fx_publish",
      expected_latest_id: "11",
      usd_cny: "7.0000000001",
      effective_at: null,
    },
  });
});
test("Key permission failures clear generated material and all previous data", async ({
  page,
}) => {
  await settingsSetup(page);
  await page.route("**/bff/v1/settings/apply", (route) =>
    route.fulfill({
      status: 403,
      json: { error: { code: "console_forbidden" } },
    }),
  );
  await page.goto("/#keys");
  await page.getByText("创建虚拟 Key", { exact: true }).click();
  await page.getByRole("button", { name: "生成一次性密钥" }).click();
  await page.getByLabel("Key 名称", { exact: true }).fill("test");
  await page.getByLabel("允许的模型 ID", { exact: false }).fill("test/model");
  await page.getByLabel("我已将密钥保存到受控凭据库", { exact: false }).check();
  await page.getByLabel("变更理由", { exact: true }).fill("test");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  await page.getByRole("button", { name: "确认提交配置" }).click();
  await expect(
    page.getByRole("button", { name: "重新检查权限" }),
  ).toBeVisible();
  await expect(page.getByLabel("待保存密钥")).toHaveCount(0);
  await expect(page.getByRole("list", { name: "Key 列表" })).toHaveCount(0);
});
test("Key and FX forms fit narrow and zoomed layouts and pass accessibility checks", async ({
  page,
}) => {
  await settingsSetup(page);
  await page.goto("/#keys");
  await page.getByRole("button", { name: "管理 配置测试 Key" }).click();
  await page.getByText("修改 Key 配置", { exact: true }).click();
  for (const width of [320, 640, 768, 1024, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    const clipped = await page
      .locator("main input:visible, main textarea:visible, main button:visible")
      .evaluateAll((nodes) =>
        nodes.some((n) => {
          const r = n.getBoundingClientRect();
          return r.left < 0 || r.right > innerWidth || r.height < 44;
        }),
      );
    expect(clipped).toBe(false);
  }
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.setViewportSize({ width: 720, height: 1000 });
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "200%";
  });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/settings-keys.png",
    fullPage: false,
  });
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "100%";
  });
  await page.goto("/#fx");
  await page.getByText("发布新的人民币汇率", { exact: true }).click();
  await page.setViewportSize({ width: 320, height: 1000 });
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/settings-fx.png",
    fullPage: false,
  });
  expect(
    await page
      .locator("main input:visible, main button:visible")
      .evaluateAll((nodes) =>
        nodes.some((n) => {
          const r = n.getBoundingClientRect();
          return r.left < 0 || r.right > innerWidth || r.height < 44;
        }),
      ),
  ).toBe(false);
});
test("catalog readonly permission hides mutations and preserves exact prices", async ({
  page,
}) => {
  await catalogSetup(page, false);
  await page.goto("/#catalog");
  await expect(
    page.getByText("当前为只读模式", { exact: false }),
  ).toBeVisible();
  await expect(page.getByText("新建逻辑模型", { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "管理 测试逻辑模型" }).click();
  await expect(
    page.getByText("CNY 0.000000000001", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("修改端点 测试供应商端点", { exact: true }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(() => localStorage.length + sessionStorage.length),
  ).toBe(0);
});
test("catalog mutation requires review and uncertain retry preserves operation and body", async ({
  page,
}) => {
  await catalogSetup(page);
  const bodies: unknown[] = [];
  await page.route("**/bff/v1/catalog/apply", (route) => {
    const body = route.request().postDataJSON();
    bodies.push(body);
    if (bodies.length === 1) return route.abort();
    return route.fulfill({
      json: {
        operation_id: body.operation_id,
        object_id: "new/model",
        revision: "1",
      },
    });
  });
  await page.goto("/#catalog");
  await page.getByText("新建逻辑模型", { exact: true }).click();
  await page.getByLabel("逻辑模型 ID", { exact: true }).fill("new/model");
  await page.getByLabel("模型名称", { exact: true }).fill("新测试模型");
  await page.getByLabel("变更理由", { exact: true }).fill("通过测试后再启用");
  await page.getByRole("button", { name: "核对变更", exact: true }).click();
  expect(bodies).toHaveLength(0);
  await expect(
    page.getByRole("button", { name: "查询 / 刷新模型" }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "管理 测试逻辑模型" }),
  ).toBeDisabled();
  await expect(page.getByRole("region", { name: "变更确认" })).toContainText(
    "新建模型（停用）",
  );
  await page.getByRole("button", { name: "确认提交配置" }).click();
  await expect(page.getByRole("alert")).toContainText("结果尚未核对");
  await page.getByRole("button", { name: "重试同一操作" }).click();
  await expect(
    page.getByRole("status").filter({ hasText: "已提交配置版本 1" }),
  ).toBeVisible();
  expect(bodies).toHaveLength(2);
  expect(bodies[0]).toEqual(bodies[1]);
  expect(JSON.stringify(bodies)).not.toContain("credential_env");
});
test("catalog stale revision is blocked and cannot silently retry overwrite", async ({
  page,
}) => {
  await catalogSetup(page);
  let body: Record<string, unknown> | undefined;
  await page.route("**/bff/v1/catalog/apply", (route) => {
    body = route.request().postDataJSON();
    return route.fulfill({
      status: 409,
      json: { error: { code: "configuration_conflict" } },
    });
  });
  await page.goto("/#catalog");
  await page.getByRole("button", { name: "管理 测试逻辑模型" }).click();
  await page.getByText("修改端点 测试供应商端点", { exact: true }).click();
  const editor = page.locator("details").filter({
    has: page.getByText("修改端点 测试供应商端点", { exact: true }),
  });
  await editor
    .getByLabel("输入价格 / 百万 Token", { exact: true })
    .fill("1.123456789012");
  await editor.getByLabel("变更理由", { exact: true }).fill("调整价格");
  await editor.getByRole("button", { name: "核对变更" }).click();
  await editor.getByRole("button", { name: "确认提交配置" }).click();
  await expect(page.getByRole("alert")).toContainText("配置已被其他人修改");
  await expect(
    page.getByRole("button", { name: "重试同一操作" }),
  ).toBeDisabled();
  expect(body?.change).toMatchObject({
    kind: "endpoint_update",
    expected_revision: "2",
    input_per_million: "1.123456789012",
  });
});
test("catalog mobile forms and audit have accessible boundaries and opaque pagination", async ({
  page,
}) => {
  await catalogSetup(page);
  await page.setViewportSize({ width: 375, height: 900 });
  await page.goto("/#catalog");
  await page.getByRole("button", { name: "管理 测试逻辑模型" }).click();
  await page.getByText("添加供应商端点", { exact: true }).click();
  await expect(
    page.getByRole("combobox", { name: "供应商连接", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("combobox", { name: "调用能力", exact: true })
    .selectOption("embedding");
  await expect(page.getByLabel("向量维度", { exact: true })).toBeVisible();
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  for (const width of [320, 375, 414, 640, 768, 1024, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    expect(
      await page.evaluate(() => {
        document.documentElement.style.overflow = "visible";
        document.body.style.overflow = "visible";
        const fits =
          document.documentElement.scrollWidth <= innerWidth &&
          document.body.scrollWidth <= innerWidth;
        document.documentElement.style.overflow = "";
        document.body.style.overflow = "";
        return fits;
      }),
    ).toBe(true);
  }
  const smallControls = await page
    .locator("button,input,select,summary")
    .evaluateAll((elements) =>
      elements
        .filter((e) => {
          const r = e.getBoundingClientRect();
          return r.height > 0 && (r.height < 43.9 || r.width < 43.9);
        })
        .map((e) => e.tagName),
    );
  expect(smallControls).toEqual([]);
  await page.screenshot({
    path: test.info().outputPath("catalog.png"),
    fullPage: true,
  });
  const cursor = {
    created_at: "2026-09-14T00:00:00.123456Z",
    id: "9007199254740993",
  };
  const queries: Record<string, unknown>[] = [];
  await page.route("**/bff/v1/audit/query", (route) => {
    queries.push(route.request().postDataJSON());
    return route.fulfill({
      json: {
        data: [
          {
            ...cursor,
            action: "catalog.endpoint_updated",
            object_id: id,
            database_actor: "brokerrouter_catalog",
            actor_user_id: user,
            change_reason: "调价审批后执行",
            operation_id: key,
            config_revision: "3",
          },
        ],
        next_before: queries.length === 1 ? cursor : null,
      },
    });
  });
  await page.goto("/#audit");
  await expect(page.getByText("调价审批后执行", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "下一页审计" }).click();
  await expect(page.getByRole("button", { name: "下一页审计" })).toBeDisabled();
  expect(queries[1]!.before).toEqual(cursor);
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([]);
  await page.screenshot({
    path: test.info().outputPath("audit.png"),
    fullPage: true,
  });
});
test("login validates tenant, sends no secret and permits only trusted HTTPS navigation", async ({
  page,
}) => {
  await setup(page, false);
  await page.route("**/bff/v1/login", async (route) => {
    expect(route.request().postDataJSON()).toEqual({ tenant_id: tenant });
    expect(route.request().headers().origin).toBe("https://localhost:4173");
    expect(route.request().headers().referer).toBe("https://localhost:4173/");
    expect(route.request().headers().authorization).toBeUndefined();
    await route.fulfill({
      json: { authorization_url: "https://idp.example/authorize?state=test" },
    });
  });
  await page.route("https://idp.example/**", (route) =>
    route.fulfill({ contentType: "text/html", body: "<h1>Test IdP</h1>" }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "使用组织身份登录" }).click();
  await expect(page.getByText("组织 ID 格式不正确")).toBeVisible();
  await page.getByLabel("组织 ID", { exact: true }).fill(tenant);
  await page.getByRole("button", { name: "使用组织身份登录" }).click();
  await expect(page).toHaveURL("https://idp.example/authorize?state=test");
});
test("renders exact CNY and secure same-origin report requests without persistent data", async ({
  page,
}) => {
  await setup(page);
  page.on("request", (request) => {
    if (request.url().endsWith("/usage/query")) {
      expect(request.headers()["x-br-csrf"]).toBe(csrf);
      expect(request.headers().authorization).toBeUndefined();
      expect(request.postDataJSON().tenant_id).toBeUndefined();
    }
  });
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await expect(page.getByText("5 笔费用已知 · 1 笔待结算")).toBeVisible();
  await page.getByText("查看每日精确金额与请求数").click();
  await expect(page.getByText("¥0.000010", { exact: true })).toBeVisible();
  expect(
    await page.evaluate(() => [localStorage.length, sessionStorage.length]),
  ).toEqual([0, 0]);
  expect(
    (await page.context().cookies()).every(
      (cookie) => !cookie.value.includes(tenant),
    ),
  ).toBe(true);
});
test("request pagination returns microsecond cursor verbatim and filters reset it", async ({
  page,
}) => {
  await setup(page);
  const cursor = { created_at: row.created_at, id };
  const seen: unknown[] = [];
  await page.route("**/bff/v1/requests/query", (route) => {
    const body = route.request().postDataJSON();
    seen.push(body);
    return route.fulfill({
      json: {
        currency: "CNY",
        data: [{ ...row, id: body.before ? user : id }],
        next_before: body.before ? null : cursor,
      },
    });
  });
  await page.goto("/#requests");
  await expect(page.getByText("待结算", { exact: true })).toBeVisible();
  await expect(page.getByText("输入 无证据")).toBeVisible();
  await page.getByRole("button", { name: "下一页" }).click();
  await expect(page.getByRole("button", { name: "下一页" })).toBeDisabled();
  expect(seen.at(-1)).toMatchObject({ before: cursor });
  await page.getByText("更多筛选").click();
  await page.getByLabel("逻辑模型 ID").fill("test/qwen");
  await page.getByRole("button", { name: "查询 / 刷新" }).click();
  await expect(page.getByRole("button", { name: "上一页" })).toBeDisabled();
  expect(seen.at(-1)).not.toHaveProperty("before");
  expect(seen.at(-1)).toMatchObject({ filter: { model_id: "test/qwen" } });
});
test("group permission denial clears already-rendered usage and does not auto-retry", async ({
  page,
}) => {
  await setup(page);
  let calls = 0;
  await page.route("**/bff/v1/usage/groups/query", (route) => {
    calls++;
    return route.fulfill({
      status: 403,
      json: { error: { code: "console_permission_denied" } },
    });
  });
  await page.goto("/");
  await expect(page.getByRole("alert")).toContainText("没有此项权限");
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
  expect(calls).toBe(1);
});
test("schema mismatch and network errors do not show stale numbers", async ({
  page,
}) => {
  await setup(page);
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await page.route("**/bff/v1/usage/query", (route) =>
    route.fulfill({
      json: { ...usage, totals: { ...totals, known_cost_cny: 0.000408 } },
    }),
  );
  await page.getByRole("button", { name: "查询 / 刷新" }).click();
  await expect(page.getByRole("alert")).toContainText("不兼容的数据");
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
  await page.route("**/bff/v1/usage/query", (route) => route.abort());
  await page.getByRole("button", { name: "重试", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("服务暂时不可用");
});
test("a delayed query cannot repopulate the page after logout", async ({
  page,
}) => {
  await setup(page);
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let started!: () => void;
  const start = new Promise<void>((resolve) => {
    started = resolve;
  });
  await page.route("**/bff/v1/usage/query", async (route) => {
    started();
    await gate;
    await route.fulfill({ json: usage }).catch(() => {});
  });
  await page.goto("/");
  await start;
  await page.getByRole("button", { name: "退出登录" }).click();
  await expect(page.getByText("已安全退出控制台。")).toBeVisible();
  release();
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
});
test("failed logout remains explicit and hides reports until a successful retry", async ({
  page,
}) => {
  await setup(page);
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await page.route("**/bff/v1/logout", (route) =>
    route.fulfill({
      status: 503,
      json: { error: { code: "persistence_error" } },
    }),
  );
  await page.getByRole("button", { name: "退出登录" }).click();
  await expect(page.getByRole("alert")).toContainText("会话可能仍有效");
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
  await page.route("**/bff/v1/logout", (route) =>
    route.fulfill({ status: 204 }),
  );
  await page.getByRole("button", { name: "重试", exact: true }).click();
  await expect(page.getByText("已安全退出控制台。")).toBeVisible();
});
test("session expiry clears report data; no provider, bearer or storage access", async ({
  page,
}) => {
  await setup(page);
  await page.clock.install();
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await page.clock.fastForward(3_601_000);
  await expect(page.getByText("登录已失效，请重新登录。")).toBeVisible();
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
});
test("renders untrusted labels as text, preserves huge integers and unknown states", async ({
  page,
}) => {
  await setup(page);
  await page.route("**/bff/v1/requests/query", (route) =>
    route.fulfill({
      json: {
        currency: "CNY",
        data: [
          {
            ...row,
            model_name: "<img src=x onerror=alert(1)>",
            cost_cny: "9007199254740993.123456",
            input_tokens: "9007199254740993",
          },
        ],
        next_before: null,
      },
    }),
  );
  await page.goto("/#requests");
  await expect(
    page.getByText("<img src=x onerror=alert(1)>", { exact: true }),
  ).toBeVisible();
  await expect(page.locator("img")).toHaveCount(0);
  await expect(
    page.getByText("¥9,007,199,254,740,993.123456", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("输入 9,007,199,254,740,993")).toBeVisible();
  await expect(
    page
      .getByRole("region", { name: "请求记录" })
      .getByText("提交不确定", { exact: true }),
  ).toBeVisible();
});
for (const width of [320, 375, 414, 768, 1440]) {
  test(`responsive and accessible workbench at ${width}px`, async ({
    page,
  }, testInfo) => {
    await setup(page);
    await page.setViewportSize({ width, height: 1000 });
    await page.emulateMedia({ reducedMotion: "reduce" });
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto("/");
    await expect(
      page.getByText("¥0.000408", { exact: true }).first(),
    ).toBeVisible();
    const checkWidth = () =>
      page.evaluate(() => {
        // Page-edge clipping is a visual safeguard, not permission to hide a
        // broken layout. Measure the intrinsic document with clipping removed.
        const nodes = [document.documentElement, document.body];
        const previous = nodes.map((node) => node.style.overflowX);
        nodes.forEach((node) => {
          node.style.overflowX = "visible";
        });
        const fits = nodes.every((node) => node.scrollWidth <= innerWidth);
        nodes.forEach((node, index) => {
          node.style.overflowX = previous[index]!;
        });
        return fits;
      });
    expect(await checkWidth()).toBe(true);
    await expect(page.locator(".chart-bar")).toHaveCount(7);
    for (const bar of await page.locator(".chart-bar").all()) {
      const box = await bar.boundingBox();
      expect(box?.height).toBeGreaterThan(0);
      expect(box?.width).toBeGreaterThan(0);
      expect(
        await bar.evaluate((element) => getComputedStyle(element).fill),
      ).not.toBe("none");
    }
    if (width === 1440) {
      for (let sample = 320; sample <= 1920; sample += 32) {
        await page.setViewportSize({ width: sample, height: 1000 });
        expect(await checkWidth(), `unclipped width ${sample}`).toBe(true);
      }
      await page.setViewportSize({ width, height: 1000 });
    }
    for (const control of await page
      .locator("input:visible, select:visible, button:visible")
      .all()) {
      const box = await control.boundingBox();
      expect(box?.height).toBeGreaterThanOrEqual(44);
    }
    const accessibility = await new AxeBuilder({ page })
      .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
      .analyze();
    expect(accessibility.violations).toEqual([]);
    await page.screenshot({
      path: testInfo.outputPath(`usage-${width}.png`),
      fullPage: true,
    });
    if (width < 960) await page.getByRole("button", { name: "菜单" }).click();
    await page.getByRole("link", { name: "请求日志", exact: true }).click();
    await expect(page.getByText("待结算", { exact: true })).toBeVisible();
    expect(await checkWidth()).toBe(true);
    await page.screenshot({
      path: testInfo.outputPath(`requests-${width}.png`),
      fullPage: true,
    });
    expect(
      (
        await new AxeBuilder({ page })
          .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
          .analyze()
      ).violations,
    ).toEqual([]);
    expect(errors).toEqual([]);
  });
}
test("31-day graph and expanded exact table fit a narrow viewport", async ({
  page,
}) => {
  await setup(page);
  await page.setViewportSize({ width: 320, height: 900 });
  await page.route("**/bff/v1/usage/query", (route) =>
    route.fulfill({
      json: {
        ...usage,
        daily: Array.from({ length: 31 }, (_, i) => ({
          date: `2026-08-${String(i + 1).padStart(2, "0")}`,
          totals,
        })),
      },
    }),
  );
  await page.goto("/");
  await expect(page.locator(".chart-bar")).toHaveCount(31);
  await page.getByText("查看每日精确金额与请求数").click();
  expect(
    await page.evaluate(
      () =>
        document.documentElement.scrollWidth <= innerWidth &&
        document.body.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(
    (
      await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
        .analyze()
    ).violations,
  ).toEqual([]);
});
test("logout broadcasts to another window and page restoration rechecks identity", async ({
  page,
  context,
}) => {
  await setup(page);
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  const other = await context.newPage();
  await setup(other);
  await other.goto("/");
  await expect(
    other.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await other.getByRole("button", { name: "退出登录" }).click();
  await expect(
    page.getByText("此账号已在其他窗口退出，请重新登录。"),
  ).toBeVisible();
  await expect(page.getByText("¥0.000408", { exact: true })).toHaveCount(0);
  await page.route("**/bff/v1/session", (route) =>
    route.fulfill({
      status: 401,
      json: { error: { code: "browser_login_required" } },
    }),
  );
  await page.evaluate(() =>
    window.dispatchEvent(
      new PageTransitionEvent("pageshow", { persisted: true }),
    ),
  );
  await expect(
    page.getByRole("heading", { name: "登录控制台", exact: true }),
  ).toBeVisible();
  await other.close();
});
test("invalid date range is local-only and explicit UTC filtering is preserved", async ({
  page,
}) => {
  await setup(page);
  const bodies: unknown[] = [];
  page.on("request", (request) => {
    if (request.url().endsWith("/usage/query"))
      bodies.push(request.postDataJSON());
  });
  await page.goto("/");
  await expect(
    page.getByText("¥0.000408", { exact: true }).first(),
  ).toBeVisible();
  await page.getByLabel("开始日期").fill("2026-08-01");
  await page.getByLabel("结束日期（含当日）").fill("2026-09-02");
  await page.getByRole("button", { name: "查询 / 刷新" }).click();
  await expect(page.getByRole("alert")).toContainText("1–31 天");
  expect(bodies).toHaveLength(1);
  await page.getByLabel("结束日期（含当日）").fill("2026-08-31");
  await page.getByLabel("时区", { exact: true }).selectOption("UTC");
  await page.getByRole("button", { name: "查询 / 刷新" }).click();
  await expect.poll(() => bodies.length).toBe(2);
  expect(bodies.at(-1)).toMatchObject({
    timezone: "UTC",
    filter: { start: "2026-08-01T00:00:00Z", end: "2026-09-01T00:00:00Z" },
  });
});
test("focus and control boundary contrast meets the three-to-one floor", async ({
  page,
}) => {
  await setup(page, false);
  await page.goto("/");
  await expect(page.getByLabel("组织 ID", { exact: true })).toBeVisible();
  const ratios = await page.evaluate(() => {
    const ctx = document
      .createElement("canvas")
      .getContext("2d", { willReadFrequently: true })!;
    const css = getComputedStyle(document.documentElement);
    const luminance = (token: string) => {
      ctx.fillStyle = css.getPropertyValue(token);
      ctx.fillRect(0, 0, 1, 1);
      const channels = [...ctx.getImageData(0, 0, 1, 1).data]
        .slice(0, 3)
        .map((v) => {
          const c = v / 255;
          return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
        });
      return (
        channels[0]! * 0.2126 + channels[1]! * 0.7152 + channels[2]! * 0.0722
      );
    };
    return [
      ["--color-focus", "--color-paper"],
      ["--color-focus", "--color-accent"],
      ["--color-control", "--color-surface"],
    ].map(([a, b]) => {
      const left = luminance(a!);
      const right = luminance(b!);
      return (Math.max(left, right) + 0.05) / (Math.min(left, right) + 0.05);
    });
  });
  for (const ratio of ratios) expect(ratio).toBeGreaterThanOrEqual(3);
});
test("keyboard login, focus ring and no external runtime assets", async ({
  page,
}, testInfo) => {
  await setup(page, false);
  await page.setViewportSize({ width: 1280, height: 800 });
  const requests: string[] = [];
  page.on("request", (request) => requests.push(request.url()));
  await page.goto("/");
  await expect(page.getByLabel("组织 ID", { exact: true })).toBeVisible();
  await page.keyboard.press("Tab");
  await expect(page.getByLabel("组织 ID", { exact: true })).toBeFocused();
  expect(
    await page
      .getByLabel("组织 ID", { exact: true })
      .evaluate((element) => getComputedStyle(element).outlineStyle),
  ).toBe("solid");
  expect(
    requests.every((url) => url.startsWith("https://localhost:4173/")),
  ).toBe(true);
  expect(
    (
      await new AxeBuilder({ page })
        .withTags(["wcag2a", "wcag2aa", "wcag21aa"])
        .analyze()
    ).violations,
  ).toEqual([]);
  await page.screenshot({
    path: testInfo.outputPath("login-1280.png"),
    fullPage: true,
  });
});
