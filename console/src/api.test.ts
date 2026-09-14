import { afterEach, describe, expect, it, vi } from "vitest";
import { z } from "zod";
import { ApiError, csrf, errorMessage, loginDestination, request } from "./api";

afterEach(() => vi.unstubAllGlobals());
describe("browser trust boundary", () => {
  it("requires exactly one valid readable CSRF cookie", () => {
    const token = "a".repeat(64);
    expect(csrf(`other=x; __Host-br-csrf=${token}`)).toBe(token);
    for (const value of [
      "",
      "__Host-br-csrf=x",
      `__Host-br-csrf=${token}; __Host-br-csrf=${token}`,
      `__Host-br-csrf=${token.toUpperCase()}`,
    ])
      expect(() => csrf(value)).toThrow(ApiError);
  });
  it("accepts only HTTPS login destinations without userinfo or fragments", () => {
    expect(loginDestination("https://idp.example/authorize?state=a")).toBe(
      "https://idp.example/authorize?state=a",
    );
    for (const value of [
      "javascript:alert(1)",
      "http://idp.example/",
      "https://a:b@idp.example/",
      "https://idp.example/#fragment",
    ])
      expect(() => loginDestination(value)).toThrow();
  });
  it("uses same-origin cookies, CSRF and no bearer proof", async () => {
    vi.stubGlobal("document", { cookie: `__Host-br-csrf=${"b".repeat(64)}` });
    const fetcher = vi
      .fn()
      .mockResolvedValue(Response.json({ value: "9007199254740993" }));
    vi.stubGlobal("fetch", fetcher);
    expect(
      await request("usage/query", z.object({ value: z.string() }), {
        filter: {},
      }),
    ).toEqual({ value: "9007199254740993" });
    const [path, options] = fetcher.mock.calls[0]!;
    expect(path).toBe("/bff/v1/usage/query");
    expect(options.credentials).toBe("same-origin");
    expect(options.mode).toBe("same-origin");
    expect(options.referrerPolicy).toBe("strict-origin");
    expect(options.cache).toBe("no-store");
    expect(options.redirect).toBe("error");
    expect(options.headers.get("X-BR-CSRF")).toBe("b".repeat(64));
    expect(options.headers.has("Authorization")).toBe(false);
  });
  it("does not send login to the server when CSRF is missing for logout", async () => {
    vi.stubGlobal("document", { cookie: "" });
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    await expect(request("logout", null)).rejects.toMatchObject({
      code: "csrf_rejected",
    });
    expect(fetcher).not.toHaveBeenCalled();
  });
  it("rejects schema drift, excessive responses and JSON-like wrong content types", async () => {
    for (const response of [
      Response.json({ value: 9007199254740993 }),
      Response.json({ value: "x".repeat(2 * 1024 * 1024) }),
      new Response("{}", { headers: { "content-type": "application/jsonp" } }),
    ]) {
      vi.stubGlobal("fetch", vi.fn().mockResolvedValue(response));
      await expect(
        request("session", z.object({ value: z.string() })),
      ).rejects.toBeInstanceOf(ApiError);
    }
  });
  it("never reflects server error messages or codes into user prose", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        Response.json(
          {
            error: {
              code: "console_permission_denied",
              message: "secret body",
            },
          },
          { status: 403 },
        ),
      ),
    );
    try {
      await request("session", z.object({}));
      throw new Error("unexpected success");
    } catch (error) {
      expect(errorMessage(error)).toContain("没有此项读取权限");
      expect(errorMessage(error)).not.toContain("secret");
    }
  });
});
