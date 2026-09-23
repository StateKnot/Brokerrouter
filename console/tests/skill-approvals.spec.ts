import { test, expect } from "@playwright/test";

const tenant = "11111111-1111-4111-8111-111111111111";
const user = "22222222-2222-4222-8222-222222222222";
const turn = "33333333-3333-4333-8333-333333333333";
const server = "44444444-4444-4444-8444-444444444444";
const observation = "55555555-5555-4555-8555-555555555555";
const requestId = "66666666-6666-4666-8666-666666666666";
const reviewId = "77777777-7777-4777-8777-777777777777";
const uri = "skill://reviewed/SKILL.md";

test("Skill consent shows the complete inert manifest only after owner review and never preselects approval", async ({
  page,
}) => {
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
  await page.route("**/bff/v1/session", (route) =>
    route.fulfill({
      json: {
        tenant_id: tenant,
        user_id: user,
        expires_at: new Date(Date.now() + 3600000).toISOString(),
        currency: "CNY",
      },
    }),
  );
  const expires = new Date(Date.now() + 300000).toISOString();
  const item = {
    request_id: requestId,
    turn_id: turn,
    server_id: server,
    uri,
    state: "pending",
    expires_at: expires,
    created_at: new Date().toISOString(),
  };
  const scope = {
    request_id: requestId,
    turn_id: turn,
    server_id: server,
    observation_id: observation,
    server_revision: "1",
    uri,
    approval_fingerprint: "a".repeat(64),
    manifest_sha256: "b".repeat(64),
    state: "pending",
    feedback: null,
    expires_at: expires,
    activation: "disabled_until_verified_resource_read_and_explicit_tool_grant",
  };
  const entry = {
    uri,
    frontmatter: {
      name: "reviewed",
      description: "<script>window.evil=true</script>",
      "allowed-tools": "send_message",
    },
    resources: [{ uri, digest: `sha256:${"c".repeat(64)}`, size: 42 }],
  };
  let reviewed = 0;
  let decided: unknown;
  const outbound: string[] = [];
  page.on("request", (req) => {
    if (req.url().includes("example.invalid")) outbound.push(req.url());
  });
  await page.route("**/bff/v1/approvals/skills/query", (route) =>
    route.fulfill({ json: { data: [item], next_after: null } }),
  );
  await page.route("**/bff/v1/approvals/skills/review", (route) => {
    reviewed++;
    return route.fulfill({
      json: {
        request: scope,
        review_id: reviewId,
        review_expires_at: expires,
        entry,
        requests_tool_grant: true,
        scope:
          "this exact manifest in this turn only; no file read, activation or tool grant",
      },
    });
  });
  await page.route("**/bff/v1/approvals/skills/decide", (route) => {
    decided = route.request().postDataJSON();
    return route.fulfill({ json: { ...scope, state: "approved" } });
  });
  await page.goto("/#approvals");
  await page.getByRole("tab", { name: "Skill 审批" }).click();
  await expect(
    page.getByRole("button", { name: "审阅完整 manifest" }),
  ).toBeVisible();
  expect(reviewed).toBe(0);
  await expect(page.getByText(/window.evil/)).toHaveCount(0);
  await page.getByRole("button", { name: "审阅完整 manifest" }).click();
  expect(reviewed).toBe(1);
  await expect(page.locator(".approval-payload")).toContainText(
    "<script>window.evil=true</script>",
  );
  await expect(page.getByText(/该 Skill 请求 allowed-tools/)).toBeVisible();
  expect(
    await page.evaluate(
      () => (window as typeof window & { evil?: boolean }).evil,
    ),
  ).toBeUndefined();
  expect(outbound).toEqual([]);
  await expect(page.getByRole("combobox", { name: "审批意见" })).toHaveValue(
    "",
  );
  await page
    .getByRole("combobox", { name: "审批意见" })
    .selectOption("approve_this_turn");
  await page.getByRole("checkbox", { name: /我已核对来源/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
  await expect(page.getByRole("tab", { name: "护栏审批" })).toBeDisabled();
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/Skill 尚不可读取或激活/)).toBeVisible();
  expect(decided).toEqual({
    request_id: requestId,
    review_id: reviewId,
    acknowledged_scope: true,
    decision: "approve_this_turn",
    feedback: null,
  });
  await expect(page.getByText(/window.evil/)).toHaveCount(0);
});
