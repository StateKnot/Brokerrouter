import { Temporal } from "@js-temporal/polyfill";
import { amount, count } from "./contracts";

export type Timezone = "Asia/Shanghai" | "UTC";
export function integer(value: string): string {
  return count.parse(value).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}
export function cny(value: string | null): string {
  if (value === null) return "待结算";
  const [whole = "0", fraction = ""] = amount.parse(value).split(".");
  // Keep every supplied digit; never round a small real charge to zero.
  return `¥${integer(whole)}.${fraction.padEnd(2, "0")}`;
}
function units(v: string): bigint {
  const [whole = "0", fraction = ""] = amount.parse(v).split(".");
  return BigInt(whole) * 10n ** 12n + BigInt(fraction.padEnd(12, "0"));
}
export function chartRatio(value: string, maximum: string): number {
  const max = units(maximum);
  // Only the bounded graphical ratio becomes Number. Labels retain exact values.
  return max === 0n ? 0 : Number((units(value) * 10000n) / max) / 100;
}
export function maximum(values: string[]): string {
  return values.reduce(
    (max, value) => (units(value) > units(max) ? value : max),
    "0",
  );
}
export function dateRange(start: string, end: string, timezone: Timezone) {
  const first = Temporal.PlainDate.from(start);
  const last = Temporal.PlainDate.from(end);
  if (first.toString() !== start || last.toString() !== end)
    throw new Error("请填写完整日期。");
  const days = first.until(last).days;
  if (days < 0 || days > 30)
    throw new Error("日期范围需为 1–31 天，结束日期不能早于开始日期。");
  const instant = (day: Temporal.PlainDate) =>
    day.toZonedDateTime(timezone).toInstant().toString();
  return { start: instant(first), end: instant(last.add({ days: 1 })) };
}
export function recentRange(timezone: Timezone, days = 7) {
  const end = Temporal.Now.plainDateISO(timezone);
  return {
    start: end.subtract({ days: days - 1 }).toString(),
    end: end.toString(),
  };
}
export function time(value: string, timezone: Timezone): string {
  // Display only; raw timestamps/cursors are never reconstructed from this string.
  return new Intl.DateTimeFormat("zh-CN", {
    timeZone: timezone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  }).format(new Date(value));
}
