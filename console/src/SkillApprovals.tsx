import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { ApiError, errorMessage, needsLogin, request } from "./api";
import { QueryState, useQuery } from "./Management";
import type { Denied } from "./Management";
import {
  inbox,
  review as reviewSchema,
  receipt,
} from "./skill-approval-contracts";
import type { Decision, Review } from "./skill-approval-contracts";
import { time } from "./format";

export default function SkillApprovals({
  denied,
  onFrozenChange,
}: {
  denied: Denied;
  onFrozenChange: (value: boolean) => void;
}) {
  const [query, setQuery] = useState<Record<string, unknown>>({
    turn_id: null,
    pending_only: true,
    limit: 20,
    before_created_at: null,
    before_id: null,
  });
  const [epoch, setEpoch] = useState(0);
  const [opened, setOpened] = useState<Review | null>(null);
  const [frozen, setFrozen] = useState<Decision | null>(null);
  useEffect(() => onFrozenChange(frozen !== null), [frozen, onFrozenChange]);
  const [busy, setBusy] = useState(false);
  const [attempted, setAttempted] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [notice, setNotice] = useState("");
  const [expired, setExpired] = useState(false);
  const controller = useRef<AbortController | null>(null);
  const result = useQuery(
    "approvals/skills/query",
    inbox,
    query,
    epoch,
    denied,
  );
  useEffect(() => () => controller.current?.abort(), []);
  useEffect(() => {
    if (!opened) return;
    const timer = setTimeout(
      () => {
        setOpened(null);
        setExpired(true);
        setNotice("审阅凭据已过期，manifest 已清除。未提交的决定请重新审阅。");
      },
      Math.max(0, new Date(opened.review_expires_at).getTime() - Date.now()),
    );
    return () => clearTimeout(timer);
  }, [opened]);
  useEffect(() => {
    if (!frozen) return;
    const unload = (event: BeforeUnloadEvent) => event.preventDefault();
    const navigation = (event: MouseEvent) => {
      if (event.target instanceof Element && event.target.closest("a")) {
        event.preventDefault();
        setNotice("请先核对当前决定，避免不确定提交后创建新决定。");
      }
    };
    window.addEventListener("beforeunload", unload);
    document.addEventListener("click", navigation, true);
    return () => {
      window.removeEventListener("beforeunload", unload);
      document.removeEventListener("click", navigation, true);
    };
  }, [frozen]);
  function fail(value: unknown) {
    setError(value);
    if (
      needsLogin(value) ||
      (value instanceof ApiError && [401, 403].includes(value.status))
    )
      denied(value);
  }
  async function open(id: string) {
    if (busy || frozen) return;
    setOpened(null);
    setError(null);
    setNotice("");
    setExpired(false);
    setBusy(true);
    const c = new AbortController();
    controller.current = c;
    try {
      const data = await request(
        "approvals/skills/review",
        reviewSchema,
        { request_id: id },
        c.signal,
      );
      if (!c.signal.aborted) {
        setOpened(data);
        setAttempted(false);
      }
    } catch (e) {
      if (!c.signal.aborted) fail(e);
    } finally {
      if (!c.signal.aborted) setBusy(false);
    }
  }
  function prepare(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!opened || busy || expired) return;
    const form = new FormData(event.currentTarget);
    const decision = String(form.get("decision")) as Decision["decision"];
    if (!["approve_this_turn", "reject", "feedback"].includes(decision)) return;
    const feedback =
      decision === "feedback"
        ? String(form.get("feedback") ?? "").trim()
        : null;
    if (
      decision === "feedback" &&
      (!feedback || new TextEncoder().encode(feedback).length > 4000)
    ) {
      setNotice("反馈须为 1–4000 UTF-8 字节。");
      return;
    }
    setFrozen({
      request_id: opened.request.request_id,
      review_id: opened.review_id,
      acknowledged_scope: true,
      decision,
      feedback,
    });
    setError(null);
    setNotice("");
    setAttempted(false);
  }
  async function commit() {
    if (!frozen || busy || (expired && !attempted)) return;
    setBusy(true);
    setAttempted(true);
    setError(null);
    const c = new AbortController();
    controller.current = c;
    try {
      const value = await request(
        "approvals/skills/decide",
        receipt,
        frozen,
        c.signal,
      );
      if (c.signal.aborted) return;
      const expected =
        frozen.decision === "approve_this_turn"
          ? "approved"
          : frozen.decision === "reject"
            ? "rejected"
            : "feedback";
      if (value.request_id !== frozen.request_id || value.state !== expected)
        throw new ApiError(502, "invalid_response");
      setNotice(
        `决定已记录：${value.state}。Skill 尚不可读取或激活；后续必须验证完整资源内容及独立工具权限。`,
      );
      setFrozen(null);
      setOpened(null);
      setEpoch((v) => v + 1);
    } catch (e) {
      if (!c.signal.aborted) fail(e);
    } finally {
      if (!c.signal.aborted) setBusy(false);
    }
  }
  const conflict =
    error instanceof ApiError &&
    [400, 404, 409, 410, 422].includes(error.status);
  return (
    <div className="approval-workbench">
      <p className="notice">
        Skill 审批只针对本轮、指定来源和完整静态
        manifest。此页不读取文件、不执行代码、不授予工具权限。远端元数据按纯文本显示。
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (busy || frozen) return;
          const f = new FormData(e.currentTarget);
          const turn = String(f.get("turn") ?? "").trim();
          setOpened(null);
          setQuery({
            turn_id: turn || null,
            pending_only: f.get("pending") === "on",
            limit: 20,
            before_created_at: null,
            before_id: null,
          });
          setEpoch((v) => v + 1);
        }}
      >
        <fieldset disabled={busy || frozen !== null} className="config-grid">
          <legend>本人 Skill 审批</legend>
          <label>
            轮次 ID（可选）
            <input
              name="turn"
              pattern="[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
            />
          </label>
          <label className="check">
            <input type="checkbox" name="pending" defaultChecked />
            仅待处理
          </label>
          <button>刷新列表</button>
        </fieldset>
      </form>
      <QueryState {...result} />
      {result.data && (
        <section aria-label="本人 Skill 审批列表">
          <div className="approval-list">
            {result.data.data.map((item) => (
              <article key={item.request_id}>
                <h2>{item.uri}</h2>
                <p>
                  {item.state} · 来源 {item.server_id}
                </p>
                <p>
                  轮次 {item.turn_id} · 截至{" "}
                  {time(item.expires_at, "Asia/Shanghai")}
                </p>
                {item.state === "pending" && (
                  <button
                    disabled={busy || frozen !== null}
                    onClick={() => void open(item.request_id)}
                  >
                    审阅完整 manifest
                  </button>
                )}
              </article>
            ))}
          </div>
          {result.data.data.length === 0 && <p>当前没有匹配的 Skill 审批。</p>}
          {result.data.next_after && (
            <button
              disabled={busy || frozen !== null}
              onClick={() =>
                setQuery((q) => ({ ...q, ...result.data!.next_after }))
              }
            >
              下一页
            </button>
          )}
        </section>
      )}
      {busy && <p role="status">正在安全处理，请勿重复操作…</p>}
      {notice && (
        <p role="status" className="notice">
          {notice}
        </p>
      )}
      {error !== null && (
        <p role="alert" className="notice">
          {error instanceof ApiError && error.code === "policy_changed"
            ? "Skill manifest、来源配置或授权上下文已变化；请重新观察并申请新的审批。"
            : errorMessage(error)}{" "}
          {attempted
            ? conflict
              ? "决定未被接受。请刷新状态，再申请新审批。"
              : "结果不确定；仅可重试完全相同的决定。"
            : "请刷新并重新审阅。"}
        </p>
      )}
      {opened && (
        <section className="change-review" aria-label="Skill manifest 审阅">
          <h2>审阅完整 Skill manifest</h2>
          <p>
            来源 {opened.request.server_id} · URI {opened.request.uri} · 轮次{" "}
            {opened.request.turn_id}
          </p>
          <p>
            配置修订 {opened.request.server_revision} · manifest SHA-256{" "}
            <code>{opened.request.manifest_sha256}</code>
          </p>
          <p>
            审批指纹 <code>{opened.request.approval_fingerprint}</code> ·
            审阅截至 {time(opened.review_expires_at, "Asia/Shanghai")}
          </p>
          {opened.requests_tool_grant && (
            <p className="notice">
              该 Skill 请求
              allowed-tools；本次决定不会授予这些工具，须单独审批。
            </p>
          )}
          <h3>完整元数据与资源清单（仅纯文本，不含文件内容）</h3>
          <pre className="approval-payload" tabIndex={0}>
            {JSON.stringify(opened.entry, null, 2)}
          </pre>
          <form key={opened.review_id} onSubmit={prepare}>
            <fieldset disabled={busy || frozen !== null}>
              <legend>本人明确决定</legend>
              <label>
                审批意见
                <select name="decision" defaultValue="" required>
                  <option value="" disabled>
                    请选择，不预选批准
                  </option>
                  <option value="approve_this_turn">
                    批准该 manifest 用于本轮后续校验
                  </option>
                  <option value="reject">拒绝</option>
                  <option value="feedback">提出其他意见并关闭原轮次</option>
                </select>
              </label>
              <label>
                其他意见（选择反馈时必填）
                <textarea name="feedback" maxLength={4000} />
              </label>
              <label className="check">
                <input type="checkbox" required />
                <span>
                  我已核对来源、完整资源清单与指纹，理解批准不代表文件已验证、Skill
                  已激活或工具已授权。
                </span>
              </label>
              <button>核对本次决定</button>
            </fieldset>
          </form>
          {!frozen && (
            <button disabled={busy} onClick={() => setOpened(null)}>
              关闭并清除 manifest
            </button>
          )}
        </section>
      )}
      {frozen && (
        <section className="change-review" aria-label="确认 Skill 审批决定">
          <h2>确认提交</h2>
          <p>
            申请 {frozen.request_id} · 审阅 {frozen.review_id} · 决定{" "}
            {frozen.decision}
          </p>
          {frozen.feedback && (
            <pre className="approval-payload">{frozen.feedback}</pre>
          )}
          <p>
            批准仍不读取文件、不激活 Skill、不授予工具；资源变化必须重新审批。
          </p>
          <button
            className="primary"
            disabled={busy || conflict || (expired && !attempted)}
            onClick={() => void commit()}
          >
            {attempted ? "核对原决定（同内容重试）" : "确认并提交决定"}
          </button>
          <button
            disabled={busy}
            onClick={() => {
              setFrozen(null);
              setOpened(null);
              setNotice(
                attempted
                  ? "请刷新审批状态，不要盲目创建新决定。"
                  : "决定未提交，已清除审阅。",
              );
              setEpoch((v) => v + 1);
            }}
          >
            {attempted ? "结束核对" : "取消提交"}
          </button>
        </section>
      )}
    </div>
  );
}
