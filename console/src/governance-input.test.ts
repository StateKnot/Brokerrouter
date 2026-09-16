import { describe, it, expect } from "vitest";
import { buildRule, configText, optionalUuid } from "./governance-input";
function form(values: Record<string, string>) {
  const f = new FormData();
  for (const [k, v] of Object.entries(values)) f.set(k, v);
  return f;
}
describe("bounded declarative governance inputs", () => {
  it("accepts exact CNY judge limits without floating point conversion", () => {
    const r = buildRule(
      form({
        rule_kind: "model_judge",
        judge_model: "test/model",
        instructions: "检查敏感操作\n只做分类",
        max_input_bytes: "1024",
        max_output_tokens: "64",
        maximum_cny: "0.000001",
      }),
    );
    expect(r).toMatchObject({
      maximum_cny: "0.000001",
      input_format: "text_json",
      max_output_tokens: 64,
    });
    expect(() =>
      buildRule(
        form({
          rule_kind: "model_judge",
          judge_model: "model",
          instructions: "risk",
          max_input_bytes: "1",
          max_output_tokens: "513",
          maximum_cny: "0.1",
        }),
      ),
    ).toThrow();
  });
  it("bounds UTF-8 and rejects unknown rules and ambiguous detection terms", () => {
    expect(() => configText("中".repeat(34), 100)).toThrow();
    for (const s of ["", "a\na", "a\n\nb", "\u0000bad"])
      expect(() =>
        buildRule(form({ rule_kind: "contains_any", terms: s })),
      ).toThrow();
    expect(
      buildRule(
        form({
          rule_kind: "contains_any",
          terms: "密码\n密钥",
          case_sensitive: "on",
        }),
      ),
    ).toEqual({
      kind: "contains_any",
      terms: ["密码", "密钥"],
      case_sensitive: true,
    });
    expect(() => buildRule(form({ rule_kind: "custom_script" }))).toThrow();
  });
  it("validates UUID references and never treats credentials as identifiers", () => {
    expect(optionalUuid(form({}), "key")).toBeNull();
    expect(() =>
      optionalUuid(form({ key: "sk_br_not_an_id" }), "key"),
    ).toThrow();
    expect(() =>
      buildRule(
        form({ rule_kind: "asset_scan", profile_id: "https://unsafe.example" }),
      ),
    ).toThrow();
    expect(buildRule(form({ rule_kind: "always" }))).toEqual({
      kind: "always",
    });
  });
});
