import { describe, expect, it, vi } from "vitest";
import { decimal, expiry, modelGrants, mintKey } from "./settings-input";
describe("settings input", () => {
  it("keeps CNY / FX decimal values exact and rejects rounding", () => {
    expect(decimal("0.000001", 6, 1_000_000_000n)).toBe("0.000001");
    expect(decimal("7.1234567890", 10, 1_000_000n, true)).toBe("7.1234567890");
    for (const v of ["0.0000001", "-1", "1e2", "01", "1000000000.000001"])
      expect(() => decimal(v, 6, 1_000_000_000n)).toThrow();
    expect(() => decimal("0", 10, 1_000_000n, true)).toThrow();
  });
  it("requires explicit unique bounded model grants", () => {
    expect(modelGrants("a\r\nb")).toEqual(["a", "b"]);
    for (const v of [
      "",
      "a\na",
      "中".repeat(67),
      "a\u0000",
      "a\nb".repeat(101),
    ])
      expect(() => modelGrants(v)).toThrow();
  });
  it("preserves expiry microseconds and disallows backdates / silent submicrosecond rounding", () => {
    const day = new Date(Date.now() + 86400000).toISOString().slice(0, 19);
    expect(expiry(`${day}.123456Z`, false)).toBe(`${day}.123456Z`);
    expect(expiry("", true)).toBeNull();
    for (const v of [
      "",
      `${day}.1234567Z`,
      "2020-01-01T00:00:00Z",
      `${day}`,
      "2099-01-01T00:00:00Z",
    ])
      expect(() => expiry(v, false)).toThrow();
  });
  it("generates distinct high entropy keys with verifiable digest, never insecure fallback", async () => {
    vi.stubGlobal("isSecureContext", true);
    try {
      const a = await mintKey(),
        b = await mintKey();
      expect(a.token).toMatch(/^sk_br_[a-f0-9]{64}$/);
      expect(a.token).not.toBe(b.token);
      expect(a.prefix).toBe(a.token.slice(0, 14));
      const digest = Buffer.from(
        await crypto.subtle.digest(
          "SHA-256",
          new TextEncoder().encode(a.token),
        ),
      ).toString("hex");
      expect(a.hash).toBe(digest);
      vi.stubGlobal("isSecureContext", false);
      await expect(mintKey()).rejects.toThrow();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
