import { Temporal } from "@js-temporal/polyfill";

export function decimal(
  value: string,
  scale: number,
  maximum: bigint,
  positive = false,
): string {
  if (!new RegExp(`^(0|[1-9]\\d{0,9})(\\.\\d{1,${scale}})?$`).test(value))
    throw new Error(`请输入普通十进制数，最多 ${scale} 位小数。`);
  const [whole = "0", fraction = ""] = value.split(".");
  const n =
    BigInt(whole) * 10n ** BigInt(scale) + BigInt(fraction.padEnd(scale, "0"));
  if (n > maximum * 10n ** BigInt(scale) || (positive && n === 0n))
    throw new Error("数值超出允许范围。");
  return value;
}
export function expiry(value: string, optional: boolean): string | null {
  if (!value && optional) return null;
  try {
    const instant = Temporal.Instant.from(value);
    const now = Temporal.Now.instant();
    if (
      instant.epochNanoseconds <= now.epochNanoseconds ||
      instant.epochNanoseconds > now.add({ hours: 366 * 24 }).epochNanoseconds
    )
      throw new Error();
    // Preserve microseconds; PostgreSQL cannot represent finer precision.
    if (instant.epochNanoseconds % 1000n !== 0n) throw new Error();
    return instant.toString();
  } catch {
    throw new Error(
      "请输入未来 366 天内的 RFC3339 时间（含 Z 或 +08:00），最多微秒精度。",
    );
  }
}
export function modelGrants(value: string): string[] {
  const models = value
    .split(/\r?\n/)
    .map((v) => v.trim())
    .filter(Boolean);
  if (
    !models.length ||
    models.length > 100 ||
    new Set(models).size !== models.length ||
    models.some(
      (v) =>
        new TextEncoder().encode(v).length > 200 ||
        /[\u0000-\u001f\u007f-\u009f]/u.test(v),
    )
  )
    throw new Error(
      "请逐行填写 1–100 个不重复的模型 ID，每项最多 200 个 UTF-8 字节。",
    );
  return models;
}
const hex = (bytes: Uint8Array) =>
  Array.from(bytes, (v) => v.toString(16).padStart(2, "0")).join("");
export async function mintKey(): Promise<{
  token: string;
  hash: string;
  prefix: string;
}> {
  if (!globalThis.isSecureContext || !crypto.subtle)
    throw new Error("密钥生成需要可信 HTTPS 安全上下文。");
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  const token = `sk_br_${hex(bytes)}`;
  bytes.fill(0);
  const encoded = new TextEncoder().encode(token);
  const hash = hex(
    new Uint8Array(await crypto.subtle.digest("SHA-256", encoded)),
  );
  encoded.fill(0);
  return { token, hash, prefix: token.slice(0, 14) };
}
