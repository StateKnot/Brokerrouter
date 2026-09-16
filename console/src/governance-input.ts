import { z } from "zod";
import { ruleSchema } from "./governance-contracts";
import { decimal } from "./settings-input";
export const value = (f: FormData, k: string) => String(f.get(k) ?? "").trim();
export function configText(s: string, max: number): string {
  if (
    !s.trim() ||
    new TextEncoder().encode(s).length > max ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(s)
  )
    throw new Error(`配置字段需要 1–${max} 个 UTF-8 字节，不能包含控制字符。`);
  return s;
}
export function optionalText(f: FormData, k: string): string | null {
  const s = value(f, k);
  return s ? configText(s, 200) : null;
}
export function optionalUuid(f: FormData, k: string): string | null {
  const s = value(f, k);
  if (!s) return null;
  if (!z.uuid().safeParse(s).success)
    throw new Error("Key ID 必须是有效 UUID，不要填写密钥明文。");
  return s;
}
export function buildRule(f: FormData) {
  const kind = value(f, "rule_kind");
  let rule: unknown;
  if (kind === "always") rule = { kind };
  else if (kind === "contains_any") {
    const terms = value(f, "terms")
      .split(/\r?\n/)
      .map((s) => configText(s.trim(), 256));
    if (terms.length > 64 || new Set(terms).size !== terms.length)
      throw new Error("请填写 1–64 个不重复的检测词，每行一个。");
    rule = { kind, terms, case_sensitive: f.get("case_sensitive") === "on" };
  } else if (kind === "model_judge") {
    const instructions = value(f, "instructions");
    if (
      !instructions ||
      new TextEncoder().encode(instructions).length > 8000 ||
      /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/u.test(
        instructions,
      )
    )
      throw new Error(
        "判定指令最多 8000 个 UTF-8 字节，只允许换行和制表控制符。",
      );
    rule = {
      kind,
      model: configText(value(f, "judge_model"), 200),
      instructions,
      input_format: "text_json",
      max_input_bytes: Number(value(f, "max_input_bytes")),
      max_output_tokens: Number(value(f, "max_output_tokens")),
      maximum_cny: decimal(value(f, "maximum_cny"), 6, 9223372036854n),
    };
  } else if (kind === "asset_scan")
    rule = { kind, profile_id: value(f, "profile_id") };
  const parsed = ruleSchema.safeParse(rule);
  if (!parsed.success)
    throw new Error(
      "规则字段不完整或超出范围，请核对数字上限与扫描配置 UUID。",
    );
  return parsed.data;
}
