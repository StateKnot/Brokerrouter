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
