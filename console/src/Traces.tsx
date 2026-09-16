import { useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { z } from "zod";
import { QueryState, useQuery } from "./Management";
import type { Denied } from "./Management";
import { cny, dateRange, integer, recentRange, time } from "./format";
import { statusLabels } from "./contracts";
import { monitorStageLabels, monitorStateLabels } from "./monitoring-contracts";
import {
  turnsSchema,
  traceDetailSchema,
  traceItemsSchema,
  turnLabels,
  sectionLabels,
  eventLabels,
} from "./trace-contracts";
import type { Section, TraceCursor, TraceItem } from "./trace-contracts";

const zone = "Asia/Shanghai";
const fields = {
  turn_id: "轮次 ID",
  user_id: "用户 ID",
  key_id: "Key ID",
  runtime_id: "Runtime",
  agent_id: "Agent",
  session_id: "会话 ID",
  run_id: "运行 ID",
};
const stamp = (value: string | null) => (value ? time(value, zone) : "未记录");
const checkLabels: Record<string, string> = {
  pending: "待检测",
  running: "检测中",
  passed: "通过",
  risk: "风险命中",
  failed: "检测失败",
  skipped_by_user_approval: "按本轮用户授权跳过",
};

export default function Traces({
  denied,
  user,
}: {
  denied: Denied;
  user: string;
}) {
  const initial = useRef(recentRange(zone));
  const [dates, setDates] = useState(initial.current);
  const [filter, setFilter] = useState<Record<string, unknown>>(() =>
    dateRange(initial.current.start, initial.current.end, zone),
  );
  const [before, setBefore] =
    useState<z.infer<typeof turnsSchema>["next_before"]>(null);
  const [epoch, setEpoch] = useState(0),
    [error, setError] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (!selected) heading.current?.focus();
  }, [selected]);
  function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const form = new FormData(e.currentTarget);
    try {
      const next: Record<string, unknown> = dateRange(
        String(form.get("start")),
        String(form.get("end")),
        zone,
      );
      for (const key of Object.keys(fields)) {
        const value = String(form.get(key) ?? "").trim();
        if (!value) continue;
        if (["turn_id", "user_id", "key_id"].includes(key)) {
          if (!z.uuid().safeParse(value).success)
            throw new Error(
              `${fields[key as keyof typeof fields]} 需为完整 UUID。`,
            );
        } else if (
          new TextEncoder().encode(value).length > 200 ||
          /[\u0000-\u001f\u007f]/.test(value)
        )
          throw new Error("筛选文本需为不超过 200 字节的单行标识。");
        next[key] = value;
      }
      const state = String(form.get("state") ?? "");
      if (state) next.state = state;
      setFilter(next);
      setDates({
        start: String(form.get("start")),
        end: String(form.get("end")),
      });
      setBefore(null);
      setEpoch((v) => v + 1);
      setError("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "请检查筛选条件。");
    }
  }
  if (selected)
    return (
      <Detail
        id={selected}
        denied={denied}
        user={user}
        back={() => {
          setSelected(null);
          setEpoch((v) => v + 1);
        }}
      />
    );
  return (
    <section className="traces" aria-label="任务与调用治理">
      <h2 ref={heading} tabIndex={-1}>
        任务轮次
      </h2>
      <p>
        独立 traces_read
        权限可读取本组织治理元数据，不提供请求原文或执行权限。这里展示网关记录的节点，不推断运行时未上报的父子流程。
      </p>
      <form className="filters" onSubmit={submit}>
        <label>
          开始日期
          <input type="date" name="start" defaultValue={dates.start} required />
        </label>
        <label>
          结束日期
          <input type="date" name="end" defaultValue={dates.end} required />
        </label>
        {Object.entries(fields).map(([key, label]) => (
          <label key={key}>
            {label}
            <input
              name={key}
              maxLength={200}
              autoComplete="off"
              defaultValue={String(filter[key] ?? "")}
            />
          </label>
        ))}
        <label>
          轮次状态
          <select name="state" defaultValue={String(filter.state ?? "")}>
            <option value="">全部</option>
            {Object.entries(turnLabels).map(([id, name]) => (
              <option value={id} key={id}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <button className="primary">查询任务</button>
      </form>
      {error && (
        <p role="alert" className="notice">
          {error}
        </p>
      )}
      <p className="helper">
        北京时间，最多 31 天；条件按 AND
        精确匹配。列表按轮次创建时间筛选，详情包含该轮次所有已记录调用（含后续入账）。
      </p>
      <TurnList
        filter={filter}
        before={before}
        epoch={epoch}
        denied={denied}
        select={setSelected}
        page={setBefore}
      />
    </section>
  );
}
function TurnList({
  filter,
  before,
  epoch,
  denied,
  select,
  page,
}: {
  filter: Record<string, unknown>;
  before: z.infer<typeof turnsSchema>["next_before"];
  epoch: number;
  denied: Denied;
  select: (id: string) => void;
  page: (v: z.infer<typeof turnsSchema>["next_before"]) => void;
}) {
  const result = useQuery(
    "traces/turns/query",
    turnsSchema,
    { filter, before, limit: 20 },
    epoch,
    denied,
  );
  const r = result.data;
  return (
    <>
      <QueryState {...result} />
      {r && (
        <>
          <p className="helper">
            观测时间 {stamp(r.observed_at)} · 共 {r.data.length} 条本页记录
          </p>
          <div
            className="table-scroll"
            role="region"
            aria-label="轮次列表，可横向滚动"
            tabIndex={0}
          >
            <table>
              <thead>
                <tr>
                  {[
                    "轮次 / 创建时间",
                    "Runtime / Agent",
                    "会话 / 运行",
                    "用户 / Key",
                    "轮次状态",
                    "详情",
                  ].map((v) => (
                    <th key={v}>{v}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {r.data.map((t) => (
                  <tr key={t.id}>
                    <td>
                      {t.id}
                      <p>{stamp(t.created_at)}</p>
                    </td>
                    <td>
                      {t.runtime_id}
                      <p>{t.agent_id}</p>
                    </td>
                    <td>
                      {t.session_id}
                      <p>
                        {t.run_id} · 第 {t.generation} 代
                      </p>
                    </td>
                    <td>
                      {t.user_id}
                      <p>
                        {t.key_alias} · {t.key_id}
                      </p>
                    </td>
                    <td>
                      {turnLabels[t.state]}
                      <p>
                        {t.identity_active
                          ? "当前身份绑定有效"
                          : "身份绑定失效"}
                      </p>
                    </td>
                    <td>
                      <button
                        onClick={() => select(t.id)}
                        aria-label={`查看轮次 ${t.id}`}
                      >
                        查看节点与调用
                      </button>
                    </td>
                  </tr>
                ))}
                {!r.data.length && (
                  <tr>
                    <td colSpan={6}>当前条件下无已记录轮次。</td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
          <div className="trace-actions">
            <button disabled={!before} onClick={() => page(null)}>
              返回首批轮次
            </button>
            <button
              disabled={!r.next_before}
              onClick={() => page(r.next_before)}
            >
              下一批轮次
            </button>
          </div>
        </>
      )}
    </>
  );
}
function Detail({
  id,
  denied,
  user,
  back,
}: {
  id: string;
  denied: Denied;
  user: string;
  back: () => void;
}) {
  const [epoch, setEpoch] = useState(0),
    [section, setSection] = useState<Section>("nodes"),
    [operation, setOperation] = useState<string | null>(null);
  const category = useRef<HTMLSelectElement>(null);
  useEffect(() => {
    if (operation) category.current?.focus();
  }, [section, operation]);
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  const result = useQuery(
      "traces/turns/detail",
      traceDetailSchema,
      { turn_id: id },
      epoch,
      denied,
    ),
    r = result.data;
  const pick = (id: string, section: Section) => {
    setOperation(id);
    setSection(section);
  };
  return (
    <section className="traces" aria-label="轮次治理详情">
      <div className="trace-actions">
        <button onClick={back}>返回任务列表</button>
        <button onClick={() => setEpoch((v) => v + 1)}>刷新轮次记录</button>
      </div>
      <h2 ref={heading} tabIndex={-1}>
        轮次治理详情
      </h2>
      <QueryState {...result} />
      {r && (
        <>
          <p className="trace-metadata">
            轮次 {r.turn.id} · {turnLabels[r.turn.state]} ·{" "}
            {r.turn.identity_active ? "身份绑定有效" : "身份绑定失效"}
          </p>
          <p className="trace-metadata">
            {r.turn.runtime_id} / {r.turn.agent_id} · 会话 {r.turn.session_id} ·
            运行 {r.turn.run_id} · 第 {r.turn.generation} 代
          </p>
          <p>
            轮次期限 {stamp(r.turn.expires_at)} · 观测于 {stamp(r.observed_at)}
          </p>
          <p>
            已提交事件 {integer(r.turn.event_clock)} · Runtime 已确认消费{" "}
            {integer(r.turn.acknowledged_sequence)}
            。消费确认仅为事件游标，不代表工具副作用执行成功。
          </p>
          <div className="notice">
            <p>
              模型调用成功不代表输出获准释放；用户同意不代表节点已执行；轮次关闭不抹除不确定执行结果。下方事实分别记录，不推断整体成功。
            </p>
          </div>
          <dl className="metrics">
            {r.costs.map((c) => (
              <div className="metric" key={c.purpose}>
                <dt>
                  {c.purpose === "model" ? "业务模型" : "护栏模型"}已知费用
                </dt>
                <dd>
                  <span>{cny(c.known_cost_cny)}</span>
                  <p>
                    {integer(c.requests)} 个唯一请求 ·{" "}
                    {integer(c.unsettled_cost_requests)} 待结算 · 预留{" "}
                    {cny(c.reserved_cny)}
                  </p>
                  <p>
                    已知 Token {integer(c.input_tokens)} 输入 /{" "}
                    {integer(c.output_tokens)} 输出（
                    {integer(c.token_usage_requests)} 个请求有记录）
                  </p>
                </dd>
              </div>
            ))}
          </dl>
          {!r.costs.length && <p>尚无归属本轮的已准入模型调用。</p>}
          <p>
            节点：
            {r.nodes
              .map((n) => `${monitorStateLabels[n.state]} ${integer(n.count)}`)
              .join(" · ") || "尚无已记录节点"}
          </p>
          <p>
            审批：
            {r.approvals
              .map((n) => `${monitorStateLabels[n.state]} ${integer(n.count)}`)
              .join(" · ") || "尚无审批记录"}
          </p>
          {r.turn.user_id === user && (
            <p>
              <a href="#approvals">前往本人审批</a>
              （独立校验本人身份和当前轮次授权，历史同意不是再次执行凭证）
            </p>
          )}
          <div className="trace-actions">
            <label>
              记录类别
              <select
                ref={category}
                value={section}
                onChange={(e) => {
                  const v = e.target.value as Section;
                  setSection(v);
                  if (v === "nodes") setOperation(null);
                }}
              >
                {Object.entries(sectionLabels).map(([id, label]) => (
                  <option
                    value={id}
                    key={id}
                    disabled={id === "checks" && !operation}
                  >
                    {label}
                  </option>
                ))}
              </select>
            </label>
            {operation && (
              <>
                <span className="trace-metadata">限定节点 {operation}</span>
                <button
                  onClick={() => {
                    setOperation(null);
                    if (section === "checks") setSection("nodes");
                  }}
                >
                  清除节点筛选
                </button>
              </>
            )}
          </div>
          <Items
            key={`${epoch}:${section}:${operation}`}
            turn={id}
            section={section}
            operation={operation}
            denied={denied}
            pick={pick}
          />
        </>
      )}
    </section>
  );
}
const headers: Record<Section, string[]> = {
  nodes: [
    "节点 / 创建时间",
    "阶段 / 目标",
    "执行状态",
    "期限 / 指纹",
    "关联记录",
  ],
  requests: [
    "请求 / 模型",
    "用途 / 状态",
    "人民币账本",
    "Token / 期限",
    "端点尝试",
  ],
  approvals: [
    "审批 / 节点",
    "审批状态",
    "时间",
    "决定人",
    "实际生成的本轮授权",
  ],
  events: ["序号", "时间", "事件", "节点 / 审批", "核对结果"],
  checks: [
    "护栏 / 版本",
    "阶段 / 类型",
    "检测结论",
    "时间 / 后续审批",
    "检测来源 / 调用",
  ],
};
function Items({
  turn,
  section,
  operation,
  denied,
  pick,
}: {
  turn: string;
  section: Section;
  operation: string | null;
  denied: Denied;
  pick: (id: string, s: Section) => void;
}) {
  const [after, setAfter] = useState<TraceCursor | null>(null);
  const result = useQuery(
      "traces/turns/items",
      traceItemsSchema,
      {
        turn_id: turn,
        section,
        operation_id: operation,
        after,
        limit: section === "approvals" ? 5 : 20,
      },
      0,
      denied,
    ),
    r = result.data;
  return (
    <>
      <QueryState {...result} />
      {r && (
        <>
          <p className="helper">
            {sectionLabels[section]} · 观测于 {stamp(r.observed_at)}。
            {r.event_fence !== null
              ? `本批事件固定截至序号 ${r.event_fence}，新事件需刷新轮次。`
              : "各页是查询时的状态，新增或变化的记录请刷新轮次。"}
          </p>
          <div
            className="table-scroll"
            role="region"
            aria-label={`${sectionLabels[section]}，可横向滚动`}
            tabIndex={0}
          >
            <table>
              <thead>
                <tr>
                  {headers[section].map((h) => (
                    <th key={h}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {r.data.map((row, index) => (
                  <Rows key={index} row={row} pick={pick} />
                ))}
                {!r.data.length && (
                  <tr>
                    <td colSpan={5}>暂无已记录数据；无记录不代表执行成功。</td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
          <div className="trace-actions">
            <button disabled={!after} onClick={() => setAfter(null)}>
              返回首批记录
            </button>
            <button
              disabled={!r.next_after}
              onClick={() => setAfter(r.next_after)}
            >
              下一批记录
            </button>
          </div>
        </>
      )}
    </>
  );
}
function Rows({
  row: r,
  pick,
}: {
  row: TraceItem;
  pick: (id: string, s: Section) => void;
}) {
  switch (r.kind) {
    case "nodes":
      return (
        <tr>
          <td>
            {r.id}
            <p>{stamp(r.created_at)}</p>
          </td>
          <td>
            {monitorStageLabels[r.stage]}
            <p>
              {r.model_id ?? "无模型"} / {r.tool_name ?? "无工具"}
            </p>
          </td>
          <td>
            {monitorStateLabels[r.state]}
            {r.state !== r.recorded_state && (
              <p>
                存储状态：{monitorStateLabels[r.recorded_state]}
                ，截止期限后按不确定展示
              </p>
            )}
          </td>
          <td>
            {stamp(r.execution_deadline)}
            <details>
              <summary>载荷 SHA-256</summary>
              <code>{r.operation_sha256}</code>
            </details>
          </td>
          <td>
            <div className="trace-actions">
              <button onClick={() => pick(r.id, "checks")}>查看护栏</button>
              <button onClick={() => pick(r.id, "requests")}>查看调用</button>
              <button onClick={() => pick(r.id, "approvals")}>查看审批</button>
            </div>
          </td>
        </tr>
      );
    case "requests":
      return (
        <tr>
          <td>
            {r.id}
            <p>
              {r.model_name} · {r.model_id}
            </p>
            <p>{stamp(r.created_at)}</p>
          </td>
          <td>
            {r.purpose === "model" ? "业务模型" : "护栏模型"}
            <p>
              {statusLabels[r.status]}
              {r.error_code && ` · ${r.error_code}`}
            </p>
            {r.media_state && (
              <p>
                媒体：{r.media_state}
                {r.cancel_requested
                  ? " · 已请求取消（不代表供应商已取消）"
                  : ""}
              </p>
            )}
          </td>
          <td>
            {cny(r.cost_cny)}
            <p>预留 {cny(r.reserved_cny)}</p>
          </td>
          <td>
            输入 {r.input_tokens === null ? "未知" : integer(r.input_tokens)} /
            输出 {r.output_tokens === null ? "未知" : integer(r.output_tokens)}
            <p>派发期限 {stamp(r.dispatch_deadline)}</p>
            <p>加密结果期限 {stamp(r.result_expires_at)}</p>
          </td>
          <td>
            {r.attempts.length ? (
              <ol>
                {r.attempts.map((a) => (
                  <li key={a.id}>
                    {a.endpoint_name} · {a.endpoint_id}
                    <p>
                      第 {a.ordinal + 1} 次 · {a.status} · HTTP{" "}
                      {a.http_status ?? "未记录"} {a.error_code ?? ""}
                    </p>
                    <p>
                      {cny(a.cost_cny)} · {stamp(a.finished_at)}
                    </p>
                  </li>
                ))}
              </ol>
            ) : (
              <p>
                {r.media_endpoint_id
                  ? `异步媒体端点 ${r.media_endpoint_id}`
                  : "无已记录文本派发尝试"}
              </p>
            )}
            <p className="helper">尝试用于追踪，不重复加入请求账本。</p>
          </td>
        </tr>
      );
    case "approvals":
      return (
        <tr>
          <td>
            {r.id}
            <p>节点 {r.operation_id}</p>
          </td>
          <td>{monitorStateLabels[r.state]}</td>
          <td>
            创建 {stamp(r.created_at)}
            <p>期限 {stamp(r.expires_at)}</p>
            <p>决定 {stamp(r.decided_at)}</p>
          </td>
          <td>{r.decided_by ?? "尚无真人决定"}</td>
          <td>
            {r.grants.length ? (
              <ul>
                {r.grants.map((g) => (
                  <li key={`${g.guardrail_id}:${g.version}`}>
                    {g.guardrail_id} v{g.version} · 截至 {stamp(g.expires_at)}
                  </li>
                ))}
              </ul>
            ) : (
              "本记录未新增授权"
            )}
            <p className="helper">
              仅为历史事实；当前执行仍须校验轮次、身份、策略与单次执行权。
            </p>
          </td>
        </tr>
      );
    case "events":
      return (
        <tr>
          <td>{integer(r.sequence)}</td>
          <td>{stamp(r.created_at)}</td>
          <td>{eventLabels[r.event_type]}</td>
          <td>
            {r.operation_id ?? "无节点关联"}
            <p>{r.approval_id ?? "无审批关联"}</p>
          </td>
          <td>
            {r.reconciliation === "confirmed_executed"
              ? "已人工确认执行"
              : r.reconciliation === "confirmed_not_executed"
                ? "已人工确认未执行"
                : "无核对结果"}
          </td>
        </tr>
      );
    case "checks":
      return (
        <tr>
          <td>
            {r.name}
            <p>
              {r.guardrail_id} v{r.version} ·{" "}
              {r.current_active ? "当前启用版本" : "历史或停用版本"}
            </p>
          </td>
          <td>
            {monitorStageLabels[r.stage]}
            <p>{r.rule_kind}</p>
          </td>
          <td>
            {checkLabels[r.outcome]}
            <p>原始结论不因后续同意而改写</p>
          </td>
          <td>
            {stamp(r.created_at)}
            <p>本轮批准跳过 {stamp(r.approval_skipped_at)}</p>
          </td>
          <td>
            {r.source === "live" ? "在线检查记录" : "历史回填（可能不完整）"}
            <p>{r.request_id ?? "无模型检测调用"}</p>
            <p>检测期限 {stamp(r.judge_deadline)}</p>
          </td>
        </tr>
      );
  }
}
