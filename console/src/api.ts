import type { z } from "zod";

type Path =
  | "login"
  | "session"
  | "logout"
  | "usage/query"
  | "usage/groups/query"
  | "requests/query"
  | "audit/query"
  | "catalog/models/query"
  | "catalog/endpoints/query"
  | "catalog/apply"
  | "settings/keys/query"
  | "settings/keys/detail"
  | "settings/models/query"
  | "settings/fx/query"
  | "settings/apply";
export class ApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
  ) {
    super(code);
  }
}
export function csrf(cookie: string): string {
  const values = cookie
    .split(";")
    .map((v) => v.trim())
    .filter((v) => v.startsWith("__Host-br-csrf="));
  const token = values[0]?.slice("__Host-br-csrf=".length);
  if (values.length !== 1 || !token || !/^[a-f0-9]{64}$/.test(token))
    throw new ApiError(403, "csrf_rejected");
  return token;
}
export function loginDestination(value: string): string {
  const url = new URL(value);
  if (url.protocol !== "https:" || url.username || url.password || url.hash)
    throw new ApiError(502, "invalid_response");
  return url.href;
}
export async function request<T>(
  path: Path,
  schema: z.ZodType<T> | null,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  const headers = new Headers();
  if (path !== "session") {
    headers.set("Content-Type", "application/json");
    if (path !== "login") headers.set("X-BR-CSRF", csrf(document.cookie));
  }
  const timeout = AbortSignal.timeout(45_000);
  let response: Response;
  try {
    response = await fetch(`/bff/v1/${path}`, {
      method: path === "session" ? "GET" : "POST",
      headers,
      credentials: "same-origin",
      mode: "same-origin",
      // Fetch may serialize Origin as null for non-CORS POST + no-referrer
      // (notably WebKit). Send only the origin as Referer, never page paths/query.
      referrerPolicy: "strict-origin",
      cache: "no-store",
      redirect: "error",
      body: path === "session" ? undefined : JSON.stringify(body ?? {}),
      signal: signal ? AbortSignal.any([signal, timeout]) : timeout,
    });
    if (response.status === 204 && schema === null) return undefined as T;
    if (
      response.headers
        .get("content-type")
        ?.split(";")[0]
        ?.trim()
        .toLowerCase() !== "application/json"
    )
      throw new ApiError(response.status, "invalid_response");
    const reader = response.body?.getReader();
    if (!reader) throw new ApiError(502, "invalid_response");
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      while (true) {
        const part = await reader.read();
        if (part.done) break;
        size += part.value.byteLength;
        if (size > 2 * 1024 * 1024)
          throw new ApiError(502, "response_too_large");
        chunks.push(part.value);
      }
    } finally {
      await reader.cancel().catch(() => {});
      reader.releaseLock();
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    const json: unknown = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(bytes),
    );
    if (!response.ok) {
      let code = "request_failed";
      if (typeof json === "object" && json !== null && "error" in json) {
        const error = json.error;
        if (
          typeof error === "object" &&
          error !== null &&
          "code" in error &&
          typeof error.code === "string" &&
          /^[a-z_]{1,80}$/.test(error.code)
        )
          code = error.code;
      }
      throw new ApiError(response.status, code);
    }
    if (schema === null) throw new ApiError(502, "invalid_response");
    const parsed = schema.safeParse(json);
    if (!parsed.success) throw new ApiError(502, "invalid_response");
    return parsed.data;
  } catch (error) {
    if (signal?.aborted) throw signal.reason;
    if (error instanceof ApiError) throw error;
    throw new ApiError(
      0,
      timeout.aborted ? "request_timeout" : "network_or_protocol_error",
    );
  }
}
export function errorMessage(error: unknown): string {
  if (!(error instanceof ApiError))
    return "页面未能完成操作，请重新加载后重试。";
  if (error.code === "browser_login_required")
    return "登录已失效，请重新登录。";
  if (error.code === "csrf_rejected")
    return "安全校验未通过，请重新登录后重试。";
  if (error.status === 403)
    return "当前账号没有此项权限，请联系组织管理员授权。";
  if (error.code === "configuration_conflict")
    return "配置已被其他人修改，请重新加载并核对后再提交。";
  if (error.code === "idempotency_conflict")
    return "操作编号已用于其他内容，已拒绝提交。请核对审计记录。";
  if (error.status === 401)
    return "服务身份校验失败，请联系管理员检查 BFF 信任配置。";
  if (error.status === 400 || error.status === 422)
    return "查询条件未被接受，请检查日期、模型 ID 和请求 ID。";
  if (error.status === 429) return "查询过于频繁，请稍后手动重试。";
  if (error.code === "invalid_response" || error.code === "response_too_large")
    return "服务返回了不兼容的数据，已停止展示，请联系管理员。";
  return "服务暂时不可用或请求超时，已清除旧数据。请稍后重试。";
}
export function needsLogin(error: unknown): boolean {
  return (
    error instanceof ApiError &&
    (error.code === "browser_login_required" || error.code === "csrf_rejected")
  );
}
