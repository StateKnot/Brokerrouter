import { useRef, useState } from "react";
import type { FormEvent } from "react";
import { z } from "zod";
import { QueryState, useQuery } from "./Management";
import type { Denied } from "./Management";
import {
  chartRatio,
  cny,
  dateRange,
  integer,
  maximum,
  recentRange,
  time,
} from "./format";
import type { Timezone } from "./format";
import {
  monitorSchema,
  monitorRailsSchema,
  monitorStageLabels,
  monitorStateLabels,
} from "./monitoring-contracts";
import type { MonitorTotals, RailCursor } from "./monitoring-contracts";

const duration = (value: string | null) =>
  value === null ? "未采集" : `${value} ms`;
function Metrics({ t }: { t: MonitorTotals }) {
  return (
    <dl className="metrics">
      <div className="metric prominent">
        <dt>去重护栏检查</dt>
        <dd>
          <span>{integer(t.checks)}</span>
          <p>每个操作 × 护栏版本仅计一次</p>
        </dd>
      </div>
      <div className="metric">
        <dt>风险命中</dt>
        <dd>
          <span>{integer(t.risk)}</span>
          <p>检测失败 {integer(t.failed)} · 不含用户拒绝</p>
        </dd>
      </div>
      <div className="metric">
        <dt>明确分类通过率</dt>
        <dd>
          <span>
            {t.classified_pass_percent === null
              ? "无分类样本"
              : `${t.classified_pass_percent}%`}
          </span>
          <p>通过 ÷（通过＋风险）</p>
        </dd>
      </div>
      <div className="metric">
        <dt>护栏检测已知费用</dt>
        <dd>
          <span>{cny(t.known_cost_cny)}</span>
          <p>
            {integer(t.unsettled_cost_requests)} 笔待结算 · 预留{" "}
            {cny(t.reserved_cny)}
          </p>
        </dd>
      </div>
    </dl>
  );
}
function Counts({ t }: { t: MonitorTotals }) {
  return (
    <>
      <td>{integer(t.checks)}</td>
      <td>{integer(t.passed)}</td>
      <td>{integer(t.risk)}</td>
      <td>{integer(t.failed)}</td>
      <td>{integer((BigInt(t.pending) + BigInt(t.running)).toString())}</td>
      <td>{integer(t.skipped_initial)}</td>
    </>
  );
}
function Report({
  filter,
  zone,
  denied,
}: {
  filter: Record<string, unknown>;
  zone: Timezone;
  denied: Denied;
}) {
  const result = useQuery(
    "governance/monitor/query",
    monitorSchema,
    { filter, timezone: zone },
    0,
    denied,
  );
  const r = result.data;
  const max = r ? maximum(r.daily.map((d) => d.totals.checks)) : "0";
  return (
    <>
      <QueryState {...result} />
      {r && (
        <>
          <p className="observed">
            数据快照 {time(r.observed_at, zone)} ·
            按首次观测时间归档，状态与迟到结算在刷新后更新。
          </p>
          {r.history_may_be_partial && (
            <p className="notice" role="status">
              历史覆盖提示：{time(r.recorded_from, zone)}{" "}
              之前仅恢复了已留存的评估和事件，历史计数可能不完整，缺失耗时不补零。当前窗口有{" "}
              {integer(r.totals.legacy_checks)} 条历史观测。
            </p>
          )}
          <Metrics t={r.totals} />
          <div className="status-strip">
            <span>待检测 {integer(r.totals.pending)}</span>
            <span>
              检测中 {integer(r.totals.running)}（其中超时未归档{" "}
              {integer(r.totals.overdue_running)}）
            </span>
            <span>曾按批准跳过 {integer(r.totals.approval_skipped)}</span>
            <span>
              租户当前启用护栏 {integer(r.tenant_active_guardrails)}
              （非健康状态）
            </span>
          </div>
          <details className="monitor-notes">
            <summary>查看统计口径与耗时覆盖</summary>
            <p>
              通过、风险、检测失败、待检测、检测中、首次即跳过互斥，其合计为去重检查总数。“曾按批准跳过”可以与原始风险重叠，不加入通过率。批准不代表节点已经执行。
            </p>
            <p>
              即时检查平均 {duration(r.totals.mean_inline_ms)}（
              {integer(r.totals.inline_samples)}{" "}
              个样本，含规则和批准查询，不含事务提交）；语义检测生命周期平均{" "}
              {duration(r.totals.mean_judge_lifecycle_ms)}（
              {integer(r.totals.judge_samples)}{" "}
              个已终结样本，含排队）。媒体资产扫描引擎耗时不在即时检查中。
            </p>
            <p>
              费用以已配置价格账本为准，不是供应商发票。
              {integer(r.totals.judge_requests)} 笔语义检测请求，其中{" "}
              {integer(r.totals.known_cost_requests)}{" "}
              笔费用已知。输入正文、规则内容、审批意见和媒体地址不会在此加载。
            </p>
          </details>
          <section className="daily" aria-labelledby="monitor-daily-title">
            <div className="section-heading">
              <h2 id="monitor-daily-title">每日检查结果</h2>
              <span>按首次观测日 · 当下状态</span>
            </div>
            {r.totals.checks === "0" ? (
              <p className="empty">
                该范围没有已记录的护栏检查；不代表系统无风险。
              </p>
            ) : (
              <>
                <svg
                  className="chart"
                  viewBox={`0 0 ${r.daily.length * 32} 240`}
                  preserveAspectRatio="none"
                  role="img"
                  aria-label="每日护栏检查总数柱状图，精确结果分类见下方表格。"
                >
                  {r.daily.map((d, i) => {
                    const h = chartRatio(d.totals.checks, max) * 2.24;
                    return (
                      <g key={d.date}>
                        <title>
                          {d.date}：{integer(d.totals.checks)} 次检查，
                          {integer(d.totals.risk)} 次风险
                        </title>
                        <rect
                          className="chart-track"
                          x={i * 32 + 2}
                          y={16}
                          width={28}
                          height={224}
                        />
                        <rect
                          className="chart-bar"
                          x={i * 32 + 2}
                          y={240 - h}
                          width={28}
                          height={h}
                        />
                      </g>
                    );
                  })}
                </svg>
                <div className="chart-axis">
                  <span>{r.daily[0]?.date}</span>
                  <span>{r.daily.at(-1)?.date}</span>
                </div>
              </>
            )}
            <div
              className="table-scroll"
              tabIndex={0}
              role="region"
              aria-label="每日护栏检查明细，可横向滚动"
            >
              <table>
                <caption>每日精确计数（无记录日补零）</caption>
                <thead>
                  <tr>
                    <th scope="col">日期</th>
                    {[
                      "检查",
                      "通过",
                      "风险",
                      "检测失败",
                      "待检测 / 检测中",
                      "首次即跳过",
                    ].map((x) => (
                      <th scope="col" key={x}>
                        {x}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {r.daily.map((d) => (
                    <tr key={d.date}>
                      <th scope="row">{d.date}</th>
                      <Counts t={d.totals} />
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </section>
          <section className="management" aria-label="审批与执行状态">
            <h2>关联节点与审批</h2>
            <p>
              仅统计此筛选命中检查所关联的操作，每个操作去重。审批单另计，可多于操作数；展示当前状态，不是当日发生次数。“未过期”不保证当前仍有执行权限，继续前必须重新校验。
            </p>
            <div className="config-grid">
              <div>
                <h3>操作状态</h3>
                {r.operations.length === 0 ? (
                  <p>暂无关联操作。</p>
                ) : (
                  <dl className="monitor-state-list">
                    {r.operations.map((s) => (
                      <div key={s.state}>
                        <dt>{monitorStateLabels[s.state]}</dt>
                        <dd>
                          {integer(s.count)}
                          <small>
                            其中轮次已关闭 / 过期 {integer(s.closed_turn)}
                          </small>
                        </dd>
                      </div>
                    ))}
                  </dl>
                )}
              </div>
              <div>
                <h3>审批单状态</h3>
                {r.approvals.length === 0 ? (
                  <p>暂无关联审批单。</p>
                ) : (
                  <dl className="monitor-state-list">
                    {r.approvals.map((s) => (
                      <div key={s.state}>
                        <dt>{monitorStateLabels[s.state]}</dt>
                        <dd>{integer(s.count)}</dd>
                      </div>
                    ))}
                  </dl>
                )}
                <p className="helper">这里不接受审批决定，也不能恢复执行。</p>
              </div>
            </div>
          </section>
        </>
      )}
      <Rails filter={filter} zone={zone} denied={denied} />
    </>
  );
}
function Rails({
  filter,
  zone,
  denied,
}: {
  filter: Record<string, unknown>;
  zone: Timezone;
  denied: Denied;
}) {
  const [cursors, setCursors] = useState<RailCursor[]>([]);
  const result = useQuery(
    "governance/monitor/rails/query",
    monitorRailsSchema,
    { filter, limit: 20, after: cursors.at(-1) },
    0,
    denied,
  );
  return (
    <section className="management" aria-label="护栏版本表现">
      <h2>护栏版本表现</h2>
      <p>
        按护栏 ID /
        版本排序，保留旧版本记录；只列出窗口内有观测的版本。分页刷新可能反映更新后的状态。
      </p>
      <QueryState {...result} />
      {result.data && (
        <>
          <p className="observed">
            本页快照 {time(result.data.observed_at, zone)}
          </p>
          <div
            className="table-scroll"
            tabIndex={0}
            role="region"
            aria-label="护栏版本统计，可横向滚动"
          >
            <table>
              <caption>每个护栏版本的去重检查与人民币费用</caption>
              <thead>
                <tr>
                  {[
                    "护栏 / 版本",
                    "阶段 / 配置状态",
                    "检查",
                    "通过",
                    "风险",
                    "检测失败",
                    "待检测 / 检测中",
                    "首次即跳过",
                    "曾批准跳过",
                    "即时均耗时",
                    "语义生命周期",
                    "已知费用",
                  ].map((x) => (
                    <th scope="col" key={x}>
                      {x}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {result.data.data.map((r) => (
                  <tr key={`${r.guardrail_id}:${r.version}`}>
                    <th scope="row">
                      <strong>{r.name}</strong>
                      <small>
                        {r.guardrail_id} · v{r.version}
                      </small>
                    </th>
                    <td>
                      {monitorStageLabels[r.stage]}
                      <small>
                        {r.current_active ? "当前启用版本" : "历史或停用版本"}
                      </small>
                    </td>
                    <Counts t={r.totals} />
                    <td>{integer(r.totals.approval_skipped)}</td>
                    <td>
                      {duration(r.totals.mean_inline_ms)}
                      <small>{integer(r.totals.inline_samples)} 样本</small>
                    </td>
                    <td>
                      {duration(r.totals.mean_judge_lifecycle_ms)}
                      <small>{integer(r.totals.judge_samples)} 样本</small>
                    </td>
                    <td>
                      <span>{cny(r.totals.known_cost_cny)}</span>
                      <small>
                        {integer(r.totals.unsettled_cost_requests)} 笔待结算
                      </small>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {result.data.data.length === 0 && (
            <p className="empty">没有匹配的护栏版本观测。</p>
          )}
          <div className="pagination">
            <button
              disabled={cursors.length === 0}
              onClick={() => setCursors((v) => v.slice(0, -1))}
            >
              上一页护栏
            </button>
            <span>第 {cursors.length + 1} 页</span>
            <button
              disabled={!result.data.next_after}
              onClick={() => {
                if (result.data?.next_after)
                  setCursors((v) => [...v, result.data!.next_after!]);
              }}
            >
              下一页护栏
            </button>
          </div>
        </>
      )}
    </section>
  );
}
export default function Monitoring({ denied }: { denied: Denied }) {
  const initial = useRef(recentRange("Asia/Shanghai"));
  const [selection, setSelection] = useState({
    filter: dateRange(
      initial.current.start,
      initial.current.end,
      "Asia/Shanghai",
    ) as Record<string, unknown>,
    zone: "Asia/Shanghai" as Timezone,
    epoch: 0,
  });
  const [error, setError] = useState("");
  function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    try {
      const zone = z.enum(["Asia/Shanghai", "UTC"]).parse(f.get("timezone"));
      const filter: Record<string, unknown> = dateRange(
        String(f.get("start")),
        String(f.get("end")),
        zone,
      );
      for (const field of ["guardrail_id", "model_id", "stage", "key_id"]) {
        const v = String(f.get(field) || "").trim();
        if (v) filter[field] = field === "key_id" ? z.uuid().parse(v) : v;
      }
      setSelection((v) => ({ filter, zone, epoch: v.epoch + 1 }));
      setError("");
    } catch {
      setError(
        "请选择不超过 31 天的有效日期范围；虚拟 Key ID 必须为完整 UUID。",
      );
    }
  }
  return (
    <div className="monitoring">
      <form
        className="config-grid"
        onSubmit={submit}
        aria-describedby="monitor-filter-help"
      >
        <label>
          开始日期
          <input
            name="start"
            type="date"
            required
            defaultValue={initial.current.start}
          />
        </label>
        <label>
          结束日期（含）
          <input
            name="end"
            type="date"
            required
            defaultValue={initial.current.end}
          />
        </label>
        <label>
          统计时区
          <select name="timezone" defaultValue="Asia/Shanghai">
            <option value="Asia/Shanghai">北京时间 UTC+8</option>
            <option value="UTC">UTC</option>
          </select>
        </label>
        <label>
          护栏 ID
          <input name="guardrail_id" maxLength={100} />
        </label>
        <label>
          业务模型 ID
          <input name="model_id" maxLength={200} />
        </label>
        <label>
          虚拟 Key ID
          <input name="key_id" maxLength={36} />
        </label>
        <label>
          检查阶段
          <select name="stage">
            <option value="">全部阶段</option>
            {Object.entries(monitorStageLabels).map(([id, name]) => (
              <option key={id} value={id}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <button type="submit">查询 / 刷新监控</button>
        <p
          id="monitor-filter-help"
          className={error ? "field-error" : "helper"}
          role={error ? "alert" : undefined}
        >
          {error ||
            "ID 精确匹配；修改后点击查询生效。统计权限独立于护栏配置及审批权限。"}
        </p>
      </form>
      <Report
        key={selection.epoch}
        filter={selection.filter}
        zone={selection.zone}
        denied={denied}
      />
    </div>
  );
}
