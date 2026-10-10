import { test, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import {
  artifactIds,
  artifactSha,
  jsonField,
  loadRuntimeArtifact,
  setupRuntimeArtifact,
} from "./runtime-artifact-fixture";

const approveOption = "批准本文件的所选护栏";
const viewedButton = "我已实际审阅视频，进入审批决定";
async function expectClosed(page: Page) {
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.locator("iframe")).toHaveCount(0);
  await expect(
    page.getByRole("option", { name: approveOption }),
  ).toHaveJSProperty("disabled", true);
}

test("native artifact decodes exact bytes and records review before a file-scoped decision", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page);
  await expect(
    page.getByRole("option", { name: approveOption }),
  ).toHaveJSProperty("disabled", true);
  await expect(
    page.getByText(/批准仅覆盖本文件、当前扫描与策略/),
  ).toBeVisible();
  await expect(
    page.getByRole("option", { name: "同意本轮跳过所选护栏" }),
  ).toHaveCount(0);
  await loadRuntimeArtifact(page);
  await expect(page.getByRole("button", { name: viewedButton })).toBeEnabled();
  await expect(page.locator("iframe")).toHaveAttribute(
    "sandbox",
    "allow-scripts",
  );
  await expect(page.locator("video")).toHaveCount(0);
  const viewer = page.frameLocator("iframe");
  await expect(viewer.getByRole("status")).toContainText("完整内容摘要一致");
  expect(
    await viewer
      .locator("video")
      .evaluate((v) => (v as HTMLVideoElement).readyState),
  ).toBeGreaterThanOrEqual(2);
  await expect(
    page.getByText(artifactIds.asset, { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText(artifactIds.input, { exact: true }),
  ).toBeVisible();
  expect(f.counts()).toEqual({
    starts: 1,
    chunks: 1,
    completions: 0,
    decisions: 0,
  });
  await page.getByRole("button", { name: viewedButton }).click();
  await expect(
    page.getByText(/媒体审阅已记录；尚未提交审批决定/),
  ).toBeVisible();
  expect(f.counts()).toEqual({
    starts: 1,
    chunks: 1,
    completions: 1,
    decisions: 0,
  });
  await page
    .getByRole("combobox", { name: "审批意见" })
    .selectOption("continue_this_turn");
  await page.getByRole("checkbox", { name: /原生产物内容审核/ }).check();
  await page.getByRole("checkbox", { name: /我已核对本次决定/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
  await expect(
    page.getByRole("region", { name: "审批决定确认" }),
  ).toContainText(approveOption);
  expect(f.counts().decisions).toBe(0);
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/决定已记录/)).toBeVisible();
  expect(f.counts().decisions).toBe(1);
  await expect(page.locator("iframe")).toHaveCount(0);
});

test("native artifact may be rejected without claiming byte review", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page);
  await page.getByRole("combobox", { name: "审批意见" }).selectOption("reject");
  await page.getByRole("checkbox", { name: /我已核对本次决定/ }).check();
  await page.getByRole("button", { name: "核对本次决定" }).click();
  await page.getByRole("button", { name: "确认并提交决定" }).click();
  await expect(page.getByText(/决定已记录：已拒绝/)).toBeVisible();
  expect(f.counts()).toEqual({
    starts: 0,
    chunks: 0,
    completions: 0,
    decisions: 1,
  });
});

for (const [field, value] of [
  ["contract", "future-runtime-artifact-v2"],
  ["source", "media_job"],
  ["scope", "current_turn_only"],
  ["asset_id", artifactIds.other],
  ["scan_id", artifactIds.other],
  ["input_operation_id", artifactIds.other],
  ["report_sha256", "9".repeat(64)],
  ["producer_revision", "9".repeat(64)],
] as const) {
  test(`native artifact refuses start evidence drift in ${field} before reading bytes`, async ({
    page,
  }) => {
    const f = await setupRuntimeArtifact(page, {
      manifest: (m) => {
        jsonField(m, "output")[field] = value;
      },
    });
    await loadRuntimeArtifact(page);
    await expectClosed(page);
    expect(f.counts()).toEqual({
      starts: 1,
      chunks: 0,
      completions: 0,
      decisions: 0,
    });
  });
}

for (const [field, value] of [
  ["batch_id", artifactIds.other],
  ["profile_id", artifactIds.other],
  ["report_sha256", "9".repeat(64)],
] as const) {
  test(`native artifact refuses internally consistent completion drift in ${field}`, async ({
    page,
  }) => {
    const f = await setupRuntimeArtifact(page, {
      complete: (r) => {
        jsonField(jsonField(jsonField(r, "approval"), "summary"), "output")[
          field
        ] = value;
        jsonField(jsonField(r, "media"), "output")[field] = value;
      },
    });
    await loadRuntimeArtifact(page);
    await expect(
      page.getByRole("button", { name: viewedButton }),
    ).toBeEnabled();
    await page.getByRole("button", { name: viewedButton }).click();
    await expectClosed(page);
    expect(f.counts()).toEqual({
      starts: 1,
      chunks: 1,
      completions: 1,
      decisions: 0,
    });
  });
}

test("native artifact compares the complete scan report across review receipts", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page, {
    complete: (r) => {
      for (const output of [
        jsonField(jsonField(jsonField(r, "approval"), "summary"), "output"),
        jsonField(jsonField(r, "media"), "output"),
      ])
        jsonField(output, "scan_report").profile_version = 2;
    },
  });
  await loadRuntimeArtifact(page);
  await expect(page.getByRole("button", { name: viewedButton })).toBeEnabled();
  await page.getByRole("button", { name: viewedButton }).click();
  await expectClosed(page);
  expect(f.counts().decisions).toBe(0);
});

for (const mode of ["summary-only", "json-metadata", "legacy-media"] as const) {
  test(`native artifact refuses ${mode} as a completed review`, async ({
    page,
  }) => {
    const f = await setupRuntimeArtifact(page, {
      complete: (r) => {
        if (mode === "legacy-media")
          r.media = {
            kind: "isolated_mp4",
            sha256: artifactSha,
            byte_length: 1755,
          };
        else {
          delete r.media;
          if (mode === "json-metadata") r.payload_text = "{}";
        }
      },
    });
    await loadRuntimeArtifact(page);
    await expect(
      page.getByRole("button", { name: viewedButton }),
    ).toBeEnabled();
    await page.getByRole("button", { name: viewedButton }).click();
    await expectClosed(page);
    expect(f.counts()).toEqual({
      starts: 1,
      chunks: 1,
      completions: 1,
      decisions: 0,
    });
  });
}

test("native artifact rejects a report digest that differs from actual MP4 bytes", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page, {
    output: (o) => {
      o.sha256 = "f".repeat(64);
      jsonField(o, "scan_report").sha256 = o.sha256;
    },
  });
  await loadRuntimeArtifact(page);
  await expectClosed(page);
  expect(f.counts()).toEqual({
    starts: 1,
    chunks: 1,
    completions: 0,
    decisions: 0,
  });
});

test("native artifact does not load a manifest whose review deadline has passed", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page, {
    manifest: (m) => {
      m.expires_at = new Date(Date.now() - 1000).toISOString();
    },
  });
  await loadRuntimeArtifact(page);
  await expectClosed(page);
  expect(f.counts()).toEqual({
    starts: 1,
    chunks: 0,
    completions: 0,
    decisions: 0,
  });
});

test("native artifact revocation during byte loading clears the isolated frame", async ({
  page,
}) => {
  const f = await setupRuntimeArtifact(page, { revoked: true });
  await loadRuntimeArtifact(page);
  await expect(
    page.getByRole("button", { name: "重新检查权限" }),
  ).toBeVisible();
  await expect(page.locator("iframe")).toHaveCount(0);
  expect(f.counts()).toEqual({
    starts: 1,
    chunks: 1,
    completions: 0,
    decisions: 0,
  });
});

test("native file approval remains accessible at narrow widths and enlarged text", async ({
  page,
}) => {
  await setupRuntimeArtifact(page);
  await loadRuntimeArtifact(page);
  await expect(page.getByRole("button", { name: viewedButton })).toBeEnabled();
  for (const width of [320, 768, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    const bounds = await page.locator("iframe").boundingBox();
    expect(bounds!.x).toBeGreaterThanOrEqual(0);
    expect(bounds!.width).toBeLessThanOrEqual(width);
  }
  await page.setViewportSize({ width: 768, height: 1000 });
  await page.evaluate(() => {
    document.documentElement.style.fontSize = "200%";
  });
  const a11y = await new AxeBuilder({ page })
    .include(".approval-workbench")
    .analyze();
  expect(a11y.violations).toEqual([]);
});
