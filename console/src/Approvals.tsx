import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { ApiError, errorMessage, needsLogin, request } from "./api";
import { QueryState, useQuery } from "./Management";
import type { Denied } from "./Management";
import {
  approvalLabels,
  approvalSchema,
  inboxSchema,
  reviewSchema,
} from "./approval-contracts";
import type { Decision, Review } from "./approval-contracts";
import { time } from "./format";
import MediaReview from "./MediaReview";
import SkillApprovals from "./SkillApprovals";

export default function Approvals({
  denied,
  previewOrigin,
}: {
  denied: Denied;
  previewOrigin: string | null;
}) {
  const [kind, setKind] = useState<"guardrail" | "skill">("guardrail");
  const [locked, setLocked] = useState(false);
  return (
    <>
      <div className="tabs" role="tablist" aria-label="审批类型">
        <button
          type="button"
          role="tab"
          aria-selected={kind === "guardrail"}
          disabled={locked}
          onClick={() => setKind("guardrail")}
        >
          护栏审批
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={kind === "skill"}
          disabled={locked}
          onClick={() => setKind("skill")}
        >
          Skill 审批
        </button>
      </div>
      {kind === "guardrail" ? (
        <GuardrailApprovals
          denied={denied}
          previewOrigin={previewOrigin}
          onFrozenChange={setLocked}
        />
      ) : (
        <SkillApprovals denied={denied} onFrozenChange={setLocked} />
      )}
    </>
  );
}

function GuardrailApprovals({
  denied,
  previewOrigin,
  onFrozenChange,
}: {
  denied: Denied;
  previewOrigin: string | null;
  onFrozenChange: (value: boolean) => void;
}) {
  const [query, setQuery] = useState<Record<string, unknown>>({
    limit: 20,
    pending_only: true,
  });
  const [epoch, setEpoch] = useState(0);
  const [review, setReview] = useState<Review | null>(null);
  const [frozen, setFrozen] = useState<Decision | null>(null);
  useEffect(() => onFrozenChange(frozen !== null), [frozen, onFrozenChange]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [notice, setNotice] = useState("");
  const [attempted, setAttempted] = useState(false);
  const [expired, setExpired] = useState(false);
  const controller = useRef<AbortController | null>(null);
  const focus = useRef<HTMLElement | null>(null);
  const result = useQuery("approvals/query", inboxSchema, query, epoch, denied);
  useEffect(() => () => controller.current?.abort(), []);
  useEffect(() => {
    focus.current?.focus();
  }, [review, frozen, notice]);
  useEffect(() => {
    if (!review) return;
    const timeout = setTimeout(
      () => {
        setReview(null);
        setExpired(true);
        setNotice(
          "审阅凭据已过期，风险原文已清除。已提交但未收到回执的决定仍可用原内容核对；未提交的决定请重新审阅。",
        );
      },
      Math.max(0, new Date(review.review_expires_at).getTime() - Date.now()),
    );
    return () => clearTimeout(timeout);
  }, [review]);
  useEffect(() => {
    if (!frozen) return;
    const unload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
    };
    const navigation = (e: MouseEvent) => {
      const a = e.target instanceof Element ? e.target.closest("a") : null;
      if (a) {
        e.preventDefault();
        setNotice(
          "请先完成当前决定或明确结束核对；不要离开后创建新的审批决定。",
        );
      }
    };
    window.addEventListener("beforeunload", unload);
    document.addEventListener("click", navigation, true);
    return () => {
      window.removeEventListener("beforeunload", unload);
      document.removeEventListener("click", navigation, true);
    };
  }, [frozen]);
  function failure(e: unknown) {
    setError(e);
    if (
      needsLogin(e) ||
      (e instanceof ApiError && (e.status === 401 || e.status === 403))
    )
      denied(e);
  }
  async function open(id: string) {
    if (busy || frozen) return;
    setReview(null);
    setError(null);
    setNotice("");
    setExpired(false);
    setBusy(true);
    const c = new AbortController();
    controller.current = c;
    try {
      const data = await request(
        "approvals/review",
        reviewSchema,
        { approval_id: id },
        c.signal,
      );
      if (!c.signal.aborted) {
        setReview(data);
        setAttempted(false);
      }
    } catch (e) {
      if (!c.signal.aborted) failure(e);
    } finally {
      if (!c.signal.aborted) setBusy(false);
    }
  }
  function prepare(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (!review || busy || expired) return;
    const f = new FormData(e.currentTarget);
    const decision = String(
      f.get("decision"),
    ) as Decision["decision"]["decision"];
    const ids =
      decision === "continue_this_turn"
        ? f.getAll("rail").map(String).sort()
        : [];
    const feedback =
      decision === "feedback" ? String(f.get("feedback") ?? "").trim() : null;
    if (
      decision === "continue_this_turn" &&
      (!review.can_continue || ids.length === 0)
    ) {
      setNotice("请明确选择至少一个要跳过的护栏；没有预选放行。");
      return;
    }
    if (
      decision === "feedback" &&
      (!feedback || new TextEncoder().encode(feedback).length > 4000)
    ) {
      setNotice("请填写 1–4000 UTF-8 字节的反馈意见。");
      return;
    }
    const body: Decision = {
      approval_id: review.approval.id,
      review_id: review.review_id,
      acknowledged_scope: true,
      decision: { decision, approved_guardrail_ids: ids, feedback },
    };
    if (new TextEncoder().encode(JSON.stringify(body)).length > 16384) {
      setNotice("本次决定超过 16 KiB，请缩小所选护栏范围。");
      return;
    }
    setFrozen(body);
    setError(null);
    setNotice("");
    setAttempted(false);
  }
  async function commit() {
    if (!frozen || busy || (!attempted && expired)) return;
    setBusy(true);
    setAttempted(true);
    setError(null);
    const c = new AbortController();
    controller.current = c;
    try {
      const receipt = await request(
        "approvals/decide",
        approvalSchema,
        frozen,
        c.signal,
      );
      if (c.signal.aborted) return;
      if (
        receipt.id !== frozen.approval_id ||
        receipt.state !==
          (
            {
              continue_this_turn: "approved",
              reject: "rejected",
              feedback: "feedback",
            } as const
          )[frozen.decision.decision]
      )
        throw new ApiError(502, "invalid_response");
      setNotice(
        `决定已记录：${approvalLabels[receipt.state]}。审批编号 ${receipt.id}。批准不是执行完成；Runtime 必须重新检查原操作，不能盲目重发外部动作。`,
      );
      setFrozen(null);
      setReview(null);
      setEpoch((v) => v + 1);
    } catch (e) {
      if (!c.signal.aborted) failure(e);
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
        仅显示本人名下的审批。打开审阅会读取敏感原文并留下审计记录，请在受控终端操作。链接、HTML
        和工具参数仅显示为文字，不会在浏览器执行。
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (busy || frozen) return;
          const f = new FormData(e.currentTarget);
          const turn = String(f.get("turn") ?? "").trim();
          setReview(null);
          setQuery({
            limit: 20,
            pending_only: f.get("pending") === "on",
            ...(turn ? { turn_id: turn } : {}),
          });
          setEpoch((v) => v + 1);
        }}
      >
        <fieldset disabled={busy || frozen !== null} className="config-grid">
          <legend>审批列表筛选</legend>
          <label>
            轮次 ID（可选）
            <input
              name="turn"
              pattern="[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
            />
          </label>
          <label className="check">
            <input type="checkbox" name="pending" defaultChecked />
            仅待处理
          </label>
          <button>刷新本人审批</button>
        </fieldset>
      </form>
      <QueryState {...result} />
      {result.data && (
        <section aria-label="本人审批列表">
          <div className="approval-list">
            {result.data.data.map((a) => (
              <article key={a.id}>
                <h2>{a.tool_name ?? a.model_id ?? "受控操作"}</h2>
                <p>
                  {a.stage} · {approvalLabels[a.state]} ·{" "}
                  {approvalLabels[a.operation_state]}
                </p>
                <dl className="config-facts">
                  <div>
                    <dt>审批 ID</dt>
                    <dd>{a.id}</dd>
                  </div>
                  <div>
                    <dt>轮次</dt>
                    <dd>{a.turn_id}</dd>
                  </div>
                  <div>
                    <dt>审批截至</dt>
                    <dd>{time(a.expires_at, "Asia/Shanghai")}</dd>
                  </div>
                </dl>
                {a.state === "pending" && (
                  <button
                    disabled={busy || frozen !== null}
                    onClick={() => void open(a.id)}
                  >
                    审阅风险原文
                  </button>
                )}
              </article>
            ))}
          </div>
          {result.data.data.length === 0 && <p>当前条件下没有本人审批。</p>}
          {result.data.next_after && (
            <button
              disabled={busy || frozen !== null}
              onClick={() => {
                setReview(null);
                setQuery((q) => ({ ...q, after: result.data!.next_after }));
              }}
            >
              下一页
            </button>
          )}
        </section>
      )}
      {busy && <p role="status">正在安全处理，请勿重复操作…</p>}
      {notice && (
        <section ref={focus} tabIndex={-1} role="status" className="notice">
          {notice}
        </section>
      )}
      {error !== null && (
        <p role="alert" className="notice">
          {errorMessage(error)}{" "}
          {attempted
            ? conflict
              ? "决定未获本次接口接受。请核对最新记录；策略变化时需 Runtime 重新检查原操作。"
              : "决定结果尚未核对。不要改选其他意见；仅可用下面按钮重试完全相同的决定。"
            : "未取得有效审阅，请刷新后重试。"}
        </p>
      )}
      {review && (
        <section
          className="change-review"
          aria-label="风险审阅"
          ref={focus}
          tabIndex={-1}
        >
          <h2>审阅这一个受控节点</h2>
          <p>
            轮次 {review.approval.turn_id} · 会话{" "}
            {review.approval.summary.session_id} · 运行{" "}
            {review.approval.summary.run_id}
          </p>
          <p>
            操作 {review.approval.operation_id} · {review.stage} ·{" "}
            {review.tool_name ?? review.model_id ?? "无模型/工具标识"}
          </p>
          <p className="notice">
            {review.approval.scope_notice}{" "}
            同轮豁免不超过审批/轮次有效期。批准后仍需重新校验预算、权限与护栏；本页不执行节点。
          </p>
          <p>
            审阅有效至 {time(review.review_expires_at, "Asia/Shanghai")}；原文
            与上下文 SHA-256 <code>{review.operation_sha256}</code>
          </p>
          {review.stage === "media_output" ? (
            <MediaReview
              key={review.approval.id}
              approvalId={review.approval.id}
              previewOrigin={previewOrigin}
              disabled={frozen !== null || busy}
              completed={review.media !== undefined}
              onComplete={setReview}
              denied={denied}
            />
          ) : review.payload_text !== null ? (
            <>
              <h3>完整操作 JSON 原文（不执行、不自动打开链接）</h3>
              <pre className="approval-payload" tabIndex={0}>
                {review.payload_text}
              </pre>
              <p>
                工具 JSON
                仅代表本次调用参数/结果，不表示已查看引用的图片、视频或附件。
              </p>
            </>
          ) : (
            <p className="notice">
              原文不可用于本页放行：{review.unavailable_reason}
              。没有截断内容或用链接替代真实媒体审阅。
            </p>
          )}
          <form key={review.review_id} onSubmit={prepare}>
            <fieldset disabled={frozen !== null || busy}>
              <legend>本人明确决定</legend>
              <fieldset>
                <legend>需要处理的护栏（默认不选择）</legend>
                {review.approval.guardrails.map((r) => (
                  <label className="check" key={r.id}>
                    <input
                      type="checkbox"
                      name="rail"
                      value={r.id}
                      disabled={!review.can_continue}
                    />
                    <span>
                      {r.name} · {r.id} · v{r.version} · {r.reason_code}
                      {r.evaluation_outcome === "failed"
                        ? "（检测失败，不代表安全）"
                        : ""}
                    </span>
                  </label>
                ))}
              </fieldset>
              <label>
                审批意见
                <select name="decision" defaultValue="" required>
                  <option value="" disabled>
                    请选择，不预选放行
                  </option>
                  <option
                    value="continue_this_turn"
                    disabled={!review.can_continue}
                  >
                    同意本轮跳过所选护栏
                  </option>
                  <option value="reject">拒绝本次操作</option>
                  <option value="feedback">提出其他意见，关闭原轮次</option>
                </select>
              </label>
              <label>
                其他意见（选择反馈时必填）
                <textarea name="feedback" maxLength={4000} />
              </label>
              <label className="check">
                <input type="checkbox" required />
                <span>
                  我已核对操作与风险，并理解同轮后续动作也可能跳过所选版本护栏；审批不等于执行完成。
                </span>
              </label>
              <button>核对本次决定</button>
            </fieldset>
          </form>
          {review.stage === "media_output" && !review.media && (
            <p className="notice">
              放行前必须完整加载并确认隔离审阅；JSON
              描述不能代替视频。可不观看内容而明确拒绝或反馈，不发放豁免。
            </p>
          )}
          {!frozen && (
            <button disabled={busy} onClick={() => setReview(null)}>
              关闭并清除原文
            </button>
          )}
        </section>
      )}
      {frozen && (
        <section
          className="change-review"
          aria-label="审批决定确认"
          ref={focus}
          tabIndex={-1}
        >
          <h2>确认提交这个决定</h2>
          <p>
            审批 {frozen.approval_id} · 审阅 {frozen.review_id}
          </p>
          <p>
            {frozen.decision.decision === "continue_this_turn"
              ? "同意本轮跳过所选护栏"
              : frozen.decision.decision === "reject"
                ? "拒绝本次操作"
                : "提出其他意见并关闭原轮次"}
          </p>
          <p>
            所选护栏：
            {frozen.decision.approved_guardrail_ids.join("、") ||
              "无，不发放豁免"}
          </p>
          {frozen.decision.feedback && (
            <pre className="approval-payload">{frozen.decision.feedback}</pre>
          )}
          <p>
            审批不执行节点，也不改变工具权限或人民币预算；没有自动重试外部动作。
          </p>
          <button
            className="primary"
            disabled={busy || conflict || (!attempted && expired)}
            onClick={() => void commit()}
          >
            {attempted ? "核对原决定（同内容重试）" : "确认并提交决定"}
          </button>
          <button
            disabled={busy}
            onClick={() => {
              setFrozen(null);
              setReview(null);
              setError(null);
              setNotice(
                attempted
                  ? "已结束本页核对；请先刷新审批记录确认结果，不要盲目创建新决定。"
                  : "决定尚未提交，已清除审阅。请重新打开风险原文。",
              );
              setEpoch((v) => v + 1);
            }}
          >
            {attempted ? "结束核对，刷新记录" : "取消提交并清除审阅"}
          </button>
        </section>
      )}
    </div>
  );
}
