import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import type { FormEvent, ReactNode } from "react";
import type { z } from "zod";
import {
  ApiError,
  errorMessage,
  loginDestination,
  needsLogin,
  request,
} from "./api";
import {
  groupsSchema,
  loginSchema,
  requestsSchema,
  sessionSchema,
  statuses,
  statusLabels,
  usageSchema,
} from "./contracts";
import type { RequestCursor, Session, Totals } from "./contracts";
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
const Audit = lazy(() =>
  import("./Management").then((module) => ({ default: module.Audit })),
);
const Catalog = lazy(() =>
  import("./Management").then((module) => ({ default: module.Catalog })),
);

type Page = "usage" | "requests" | "audit" | "catalog";
const titles: Record<Page, string> = {
  usage: "人民币用量",
  requests: "请求日志",
  audit: "审计日志",
  catalog: "模型与端点",
};
type Auth =
  | { state: "checking" }
  | { state: "login"; message?: string }
  | { state: "error"; error: unknown }
  | { state: "ready"; session: Session };
const pageFromHash = (): Page => {
  const page = location.hash.slice(1);
  return page === "requests" || page === "audit" || page === "catalog"
    ? page
    : "usage";
};

function Notice({
  children,
  retry,
}: {
  children: ReactNode;
  retry?: () => void;
}) {
  return (
    <div className="notice" role="alert">
      <p>{children}</p>
      {retry && <button onClick={retry}>重试</button>}
    </div>
  );
}
function Loading() {
  return (
    <div className="loading" role="status">
      <p>正在安全加载…</p>
      <div className="skeleton" />
      <div className="skeleton short" />
    </div>
  );
}
function Brand() {
  return (
    <span className="wordmark">
      Brokerrouter
      <span className="brand-dot" aria-hidden="true">
        .
      </span>
    </span>
  );
}

export default function App() {
  const [auth, setAuth] = useState<Auth>({ state: "checking" });
  const generation = useRef(0);
  const pending = useRef<AbortController | null>(null);
  const channel = useRef<BroadcastChannel | null>(null);
  const invalidate = useCallback((message = "登录已失效，请重新登录。") => {
    generation.current++;
    pending.current?.abort();
    setAuth({ state: "login", message });
  }, []);
  const check = useCallback(async () => {
    const current = ++generation.current;
    pending.current?.abort();
    const controller = new AbortController();
    pending.current = controller;
    setAuth({ state: "checking" });
    try {
      const session = await request(
        "session",
        sessionSchema,
        undefined,
        controller.signal,
      );
      if (new Date(session.expires_at).getTime() <= Date.now())
        throw new ApiError(401, "browser_login_required");
      if (current === generation.current) setAuth({ state: "ready", session });
    } catch (error) {
      if (controller.signal.aborted || current !== generation.current) return;
      if (needsLogin(error)) setAuth({ state: "login" });
      else setAuth({ state: "error", error });
    }
  }, []);
  useEffect(() => {
    // No session, report or tenant data is placed in browser persistent storage.
    if ("BroadcastChannel" in window) {
      channel.current = new BroadcastChannel("brokerrouter-session");
      channel.current.onmessage = (event) => {
        if (event.data === "signed-out")
          invalidate("此账号已在其他窗口退出，请重新登录。");
      };
    }
    void check();
    const visibility = () => {
      if (document.hidden) {
        generation.current++;
        pending.current?.abort();
        setAuth({ state: "checking" });
      } else void check();
    };
    const pageshow = (event: PageTransitionEvent) => {
      if (event.persisted) void check();
    };
    document.addEventListener("visibilitychange", visibility);
    window.addEventListener("pageshow", pageshow);
    return () => {
      generation.current++;
      pending.current?.abort();
      channel.current?.close();
      document.removeEventListener("visibilitychange", visibility);
      window.removeEventListener("pageshow", pageshow);
    };
  }, [check, invalidate]);
  useEffect(() => {
    if (auth.state !== "ready") return;
    const timer = window.setTimeout(
      () => invalidate(),
      Math.max(0, new Date(auth.session.expires_at).getTime() - Date.now()),
    );
    return () => clearTimeout(timer);
  }, [auth, invalidate]);
  const loginNeeded = useCallback(
    (error: unknown) => invalidate(errorMessage(error)),
    [invalidate],
  );
  if (auth.state === "checking")
    return (
      <main className="entry">
        <Brand />
        <Loading />
      </main>
    );
  if (auth.state === "error")
    return (
      <main className="entry">
        <Brand />
        <h1>无法连接控制台</h1>
        <Notice retry={() => void check()}>{errorMessage(auth.error)}</Notice>
      </main>
    );
  if (auth.state === "login") return <Login message={auth.message} />;
  return (
    <Workspace
      session={auth.session}
      loginNeeded={loginNeeded}
      onLogout={() => {
        channel.current?.postMessage("signed-out");
        invalidate("已安全退出控制台。");
      }}
    />
  );
}

function Login({ message }: { message?: string }) {
  const [tenant, setTenant] = useState("");
  const [touched, setTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const controller = useRef<AbortController | null>(null);
  useEffect(() => () => controller.current?.abort(), []);
  const valid = sessionSchema.shape.tenant_id.safeParse(tenant.trim()).success;
  async function login(event: FormEvent) {
    event.preventDefault();
    setTouched(true);
    setError(null);
    if (!valid || busy) return;
    const active = new AbortController();
    controller.current = active;
    setBusy(true);
    try {
      const result = await request(
        "login",
        loginSchema,
        { tenant_id: tenant.trim() },
        active.signal,
      );
      // This destination comes only from the trusted same-origin BFF, never a query parameter.
      location.assign(loginDestination(result.authorization_url));
    } catch (cause) {
      if (!active.signal.aborted) {
        setError(cause);
        setBusy(false);
      }
    }
  }
  return (
    <main className="entry login-layout">
      <section className="login-intro">
        <Brand />
        <h1>
          AI 调用，
          <br />
          人民币记账。
        </h1>
        <p>查看模型用量与请求记录。使用组织身份登录，读取权限由管理员分配。</p>
        <p className="muted">统一网关 · 可追溯账本 · 独立身份</p>
      </section>
      <section className="login-form" aria-labelledby="login-title">
        <h2 id="login-title">登录控制台</h2>
        {message && <p role="status">{message}</p>}
        {new URLSearchParams(location.search).get("login") === "failed" && (
          <Notice>
            组织登录未完成或已过期，请重新登录；如果重复出现，请联系管理员检查身份映射。
          </Notice>
        )}
        <form onSubmit={login} noValidate>
          <label htmlFor="tenant">组织 ID</label>
          <input
            id="tenant"
            value={tenant}
            autoComplete="off"
            autoCapitalize="none"
            spellCheck={false}
            maxLength={36}
            required
            aria-required="true"
            aria-invalid={touched && !valid}
            aria-describedby="tenant-help"
            onBlur={() => setTouched(true)}
            onChange={(event) => setTenant(event.target.value)}
          />
          <p
            id="tenant-help"
            className={touched && !valid ? "field-error" : "helper"}
          >
            {touched && !valid
              ? "组织 ID 格式不正确，请向管理员获取完整 UUID。"
              : "由组织管理员提供；这里不填写供应商密钥。"}
          </p>
          <button className="primary" disabled={busy} aria-busy={busy}>
            {busy ? "正在前往组织登录…" : "使用组织身份登录"}
          </button>
        </form>
        {error !== null && <Notice>{errorMessage(error)}</Notice>}
        <p className="muted">
          登录不会赋予智能体审批权限，也不会触发模型调用。
        </p>
      </section>
    </main>
  );
}

type Filter = {
  start: string;
  end: string;
  model_id?: string;
  key_id?: string;
  request_id?: string;
  status?: string;
  purpose?: string;
};
type Selection = { filter: Filter; timezone: Timezone; revision: number };
function initialSelection(): Selection {
  const dates = recentRange("Asia/Shanghai");
  return {
    filter: dateRange(dates.start, dates.end, "Asia/Shanghai"),
    timezone: "Asia/Shanghai",
    revision: 0,
  };
}
function Workspace({
  session,
  loginNeeded,
  onLogout,
}: {
  session: Session;
  loginNeeded: (error: unknown) => void;
  onLogout: () => void;
}) {
  const [page, setPage] = useState<Page>(pageFromHash);
  const [menu, setMenu] = useState(false);
  const [selection, setSelection] = useState(initialSelection);
  const [loggingOut, setLoggingOut] = useState(false);
  const [logoutError, setLogoutError] = useState<unknown>(null);
  const [permissionError, setPermissionError] = useState<unknown>(null);
  const [reportEpoch, setReportEpoch] = useState(0);
  const heading = useRef<HTMLHeadingElement>(null);
  const logoutController = useRef<AbortController | null>(null);
  const reportDenied = useCallback(
    (error: unknown) => {
      if (needsLogin(error)) loginNeeded(error);
      else setPermissionError(error);
    },
    [loginNeeded],
  );
  useEffect(() => {
    const change = () => {
      setPage(pageFromHash());
      setMenu(false);
      setPermissionError(null);
    };
    window.addEventListener("hashchange", change);
    return () => {
      window.removeEventListener("hashchange", change);
      logoutController.current?.abort();
    };
  }, []);
  useEffect(() => {
    document.title = `${titles[page]} · Brokerrouter`;
    heading.current?.focus();
  }, [page]);
  async function logout() {
    if (loggingOut) return;
    setLoggingOut(true);
    setLogoutError(null);
    setReportEpoch((v) => v + 1);
    const active = new AbortController();
    logoutController.current = active;
    try {
      await request("logout", null, {}, active.signal);
      if (!active.signal.aborted) onLogout();
    } catch (error) {
      if (active.signal.aborted) return;
      if (needsLogin(error)) {
        loginNeeded(error);
        return;
      }
      setLogoutError(error);
      setLoggingOut(false);
    }
  }
  return (
    <div className="workspace">
      <a className="skip-link" href="#main">
        跳至内容
      </a>
      <aside className="sidebar">
        <div className="brand-row">
          <Brand />
          <button
            className="menu-toggle"
            aria-expanded={menu}
            aria-controls="navigation"
            onClick={() => setMenu(!menu)}
          >
            菜单
          </button>
        </div>
        <nav
          id="navigation"
          aria-label="控制台"
          className={menu ? "navigation expanded" : "navigation"}
        >
          <a href="#usage" aria-current={page === "usage" ? "page" : undefined}>
            人民币用量
          </a>
          <a
            href="#requests"
            aria-current={page === "requests" ? "page" : undefined}
          >
            请求日志
          </a>
          <a href="#audit" aria-current={page === "audit" ? "page" : undefined}>
            审计日志
          </a>
          <a
            href="#catalog"
            aria-current={page === "catalog" ? "page" : undefined}
          >
            模型与端点
          </a>
        </nav>
        <div className="sidebar-note">
          <span>结算本位币</span>
          <strong>CNY · 人民币</strong>
          <span>按权限开放配置</span>
        </div>
      </aside>
      <div className="workspace-body">
        <header className="session-bar">
          <div>
            <span className="muted">当前组织</span>
            <span className="identity">{session.tenant_id}</span>
          </div>
          <button onClick={() => void logout()} disabled={loggingOut}>
            {loggingOut ? "正在退出…" : "退出登录"}
          </button>
        </header>
        <main id="main" tabIndex={-1}>
          <header className="page-header">
            <h1 ref={heading} tabIndex={-1}>
              {titles[page]}
            </h1>
            <p>
              {page === "usage"
                ? "按请求创建时间核对已入账费用、预留与模型用量。"
                : page === "audit"
                  ? "追溯组织操作记录与配置变更，区分真人操作与数据库执行身份。"
                  : page === "catalog"
                    ? "统一逻辑模型与多供应商端点，使用独立配置权限和可追溯变更。"
                    : "查看已准入请求的状态与账本记录，不含请求原文。"}
            </p>
          </header>
          {logoutError !== null && (
            <Notice retry={() => void logout()}>
              退出未完成，会话可能仍有效。{errorMessage(logoutError)}
            </Notice>
          )}
          {(page === "usage" || page === "requests") && (
            <FilterForm
              onApply={(next) => {
                setPermissionError(null);
                setSelection((current) => ({
                  ...next,
                  revision: current.revision + 1,
                }));
              }}
            />
          )}
          <div className="context-line">
            <span>
              时区：{selection.timezone === "UTC" ? "UTC" : "北京时间 UTC+8"}
            </span>
            <span>用户 {session.user_id}</span>
            <span>会话截至 {time(session.expires_at, selection.timezone)}</span>
          </div>
          {permissionError !== null && (
            <Notice>
              {errorMessage(permissionError)} 获得授权后重新加载。
              <button
                onClick={() => {
                  setPermissionError(null);
                  setReportEpoch((v) => v + 1);
                }}
              >
                重新检查权限
              </button>
            </Notice>
          )}
          {!loggingOut && logoutError === null && permissionError === null && (
            <div key={`${page}-${selection.revision}-${reportEpoch}`}>
              <Suspense fallback={<Loading />}>
                {page === "usage" ? (
                  <Usage selection={selection} loginNeeded={reportDenied} />
                ) : page === "audit" ? (
                  <Audit denied={reportDenied} />
                ) : page === "catalog" ? (
                  <Catalog denied={reportDenied} />
                ) : (
                  <Requests selection={selection} loginNeeded={reportDenied} />
                )}
              </Suspense>
            </div>
          )}
          {loggingOut && <Loading />}
        </main>
        <footer>
          Brokerrouter · 费用以配置价格账本为准，非供应商实付账单。
        </footer>
      </div>
    </div>
  );
}

function FilterForm({
  onApply,
}: {
  onApply: (selection: Omit<Selection, "revision">) => void;
}) {
  const initial = useRef(recentRange("Asia/Shanghai"));
  const [start, setStart] = useState(initial.current.start);
  const [end, setEnd] = useState(initial.current.end);
  const [timezone, setTimezone] = useState<Timezone>("Asia/Shanghai");
  const [model, setModel] = useState("");
  const [key, setKey] = useState("");
  const [id, setId] = useState("");
  const [status, setStatus] = useState("");
  const [purpose, setPurpose] = useState("");
  const [error, setError] = useState("");
  function apply(event: FormEvent) {
    event.preventDefault();
    try {
      for (const value of [key, id])
        if (
          value.trim() &&
          !sessionSchema.shape.tenant_id.safeParse(value.trim()).success
        )
          throw new Error("虚拟 Key ID 和请求 ID 必须是完整 UUID。");
      if (/[\u0000-\u001f\u007f]/.test(model))
        throw new Error("模型 ID 不能包含控制字符。");
      onApply({
        filter: {
          ...dateRange(start, end, timezone),
          ...(model.trim() ? { model_id: model.trim() } : {}),
          ...(key.trim() ? { key_id: key.trim() } : {}),
          ...(id.trim() ? { request_id: id.trim() } : {}),
          ...(status ? { status } : {}),
          ...(purpose ? { purpose } : {}),
        },
        timezone,
      });
      setError("");
    } catch (cause) {
      setError(
        cause instanceof RangeError
          ? "日期无效，请填写完整的开始和结束日期。"
          : cause instanceof Error
            ? cause.message
            : "请检查查询条件。",
      );
    }
  }
  return (
    <form
      className="filters"
      onSubmit={apply}
      aria-label="报表筛选"
      aria-describedby="filter-help"
    >
      <div className="filter-primary">
        <label>
          开始日期
          <input
            type="date"
            required
            value={start}
            onChange={(event) => setStart(event.target.value)}
          />
        </label>
        <label>
          结束日期（含当日）
          <input
            type="date"
            required
            value={end}
            onChange={(event) => setEnd(event.target.value)}
          />
        </label>
        <label>
          时区
          <select
            aria-label="时区"
            value={timezone}
            onChange={(event) => setTimezone(event.target.value as Timezone)}
          >
            <option value="Asia/Shanghai">北京时间</option>
            <option value="UTC">UTC</option>
          </select>
        </label>
        <button
          type="button"
          onClick={() => {
            const dates = recentRange(timezone);
            setStart(dates.start);
            setEnd(dates.end);
          }}
        >
          最近 7 天
        </button>
        <button type="submit" className="primary">
          查询 / 刷新
        </button>
      </div>
      <details>
        <summary>更多筛选</summary>
        <div className="filter-extra">
          <label>
            逻辑模型 ID
            <input
              value={model}
              maxLength={200}
              onChange={(event) => setModel(event.target.value)}
            />
          </label>
          <label>
            虚拟 Key ID
            <input
              value={key}
              maxLength={36}
              onChange={(event) => setKey(event.target.value)}
            />
          </label>
          <label>
            请求 ID
            <input
              value={id}
              maxLength={36}
              onChange={(event) => setId(event.target.value)}
            />
          </label>
          <label>
            请求状态
            <select
              value={status}
              onChange={(event) => setStatus(event.target.value)}
            >
              <option value="">全部状态</option>
              {statuses.map((item) => (
                <option value={item} key={item}>
                  {statusLabels[item]}
                </option>
              ))}
            </select>
          </label>
          <label>
            费用用途
            <select
              value={purpose}
              onChange={(event) => setPurpose(event.target.value)}
            >
              <option value="">全部用途</option>
              <option value="model">模型（含媒体）</option>
              <option value="guardrail">护栏检测</option>
            </select>
          </label>
        </div>
      </details>
      <p
        id="filter-help"
        className={error ? "field-error" : "helper"}
        role={error ? "alert" : undefined}
      >
        {error ||
          "最多查询 31 天；修改条件后点击“查询 / 刷新”生效。模型和 ID 为精确匹配。"}
      </p>
    </form>
  );
}

function useReport<T>(
  path: "usage/query" | "usage/groups/query" | "requests/query",
  schema: z.ZodType<T>,
  body: unknown,
  loginNeeded: (error: unknown) => void,
) {
  const [revision, setRevision] = useState(0);
  const serialized = JSON.stringify(body);
  const identity = `${path}:${serialized}:${revision}`;
  const [result, setResult] = useState<{
    identity: string;
    data: T | null;
    error: unknown;
  }>({ identity: "", data: null, error: null });
  useEffect(() => {
    const controller = new AbortController();
    setResult({ identity, data: null, error: null });
    void request(path, schema, JSON.parse(serialized), controller.signal)
      .then((result) => {
        if (!controller.signal.aborted)
          setResult({ identity, data: result, error: null });
      })
      .catch((cause) => {
        if (controller.signal.aborted) return;
        if (
          needsLogin(cause) ||
          (cause instanceof ApiError && cause.status === 403)
        )
          loginNeeded(cause);
        else setResult({ identity, data: null, error: cause });
      });
    return () => controller.abort();
  }, [path, schema, serialized, identity, loginNeeded]);
  return {
    data: result.identity === identity ? result.data : null,
    error: result.identity === identity ? result.error : null,
    retry: () => setRevision((v) => v + 1),
  };
}
function Empty({ message, retry }: { message: string; retry: () => void }) {
  return (
    <div className="empty">
      <span className="empty-mark" aria-hidden="true">
        —
      </span>
      <p>{message}</p>
      <p className="muted">调整日期或筛选条件后重新查询。</p>
      <button onClick={retry}>刷新记录</button>
    </div>
  );
}
function Metric({
  label,
  value,
  note,
  prominent = false,
}: {
  label: string;
  value: string;
  note: string;
  prominent?: boolean;
}) {
  return (
    <div className={prominent ? "metric prominent" : "metric"}>
      <dt>{label}</dt>
      <dd>
        <span>{value}</span>
        <p>{note}</p>
      </dd>
    </div>
  );
}
function Usage({
  selection,
  loginNeeded,
}: {
  selection: Selection;
  loginNeeded: (error: unknown) => void;
}) {
  const report = useReport(
    "usage/query",
    usageSchema,
    { filter: selection.filter, timezone: selection.timezone },
    loginNeeded,
  );
  if (report.error !== null)
    return <Notice retry={report.retry}>{errorMessage(report.error)}</Notice>;
  if (!report.data) return <Loading />;
  const { totals, daily, observed_at } = report.data;
  const max = maximum(daily.map((day) => day.totals.known_cost_cny));
  return (
    <>
      <p className="observed">
        数据快照 {time(observed_at, selection.timezone)} ·
        刷新后可能包含迟到结算
      </p>
      <dl className="metrics">
        <Metric
          prominent
          label="已知入账费用"
          value={cny(totals.known_cost_cny)}
          note={`${integer(totals.known_cost_requests)} 笔费用已知 · ${integer(totals.unsettled_cost_requests)} 笔待结算`}
        />
        <Metric
          label="已准入请求"
          value={integer(totals.requests)}
          note={`成功 ${integer(totals.succeeded)} · 失败 ${integer(totals.failed)}`}
        />
        <Metric
          label="Token 合计"
          value={integer(totals.total_tokens)}
          note={`${integer(totals.token_usage_requests)} 笔有 Token 证据`}
        />
        <Metric
          label="已知请求平均费用"
          value={
            totals.mean_known_request_cost_cny === null
              ? "暂无已知费用"
              : cny(totals.mean_known_request_cost_cny)
          }
          note="仅以费用已知的请求为分母"
        />
      </dl>
      <div className="status-strip">
        <span>预留 {cny(totals.reserved_cny)}</span>
        <span>执行中 {integer(totals.in_progress)}</span>
        <span>提交不确定 {integer(totals.submission_unknown)}</span>
        <span>已核对 {integer(totals.reconciled)}</span>
      </div>
      <section className="daily" aria-labelledby="daily-title">
        <div className="section-heading">
          <h2 id="daily-title">每日入账费用</h2>
          <span className="muted">人民币 · 单日峰值 {cny(max)}</span>
        </div>
        {totals.requests === "0" ? (
          <Empty message="此时间范围内没有已准入请求。" retry={report.retry} />
        ) : (
          <>
            <svg
              className="chart"
              viewBox={`0 0 ${Math.max(daily.length, 1) * 32} 240`}
              preserveAspectRatio="none"
              role="img"
              aria-label="每日已知人民币费用柱状图。精确金额见下方每日明细。"
            >
              {daily.map((day, index) => {
                const height =
                  chartRatio(day.totals.known_cost_cny, max) * 2.24;
                return (
                  <g key={day.date}>
                    <title>{`${day.date} ${cny(day.totals.known_cost_cny)}`}</title>
                    <rect
                      className="chart-track"
                      x={index * 32 + 2}
                      y={16}
                      width={28}
                      height={224}
                    />
                    <rect
                      className="chart-bar"
                      x={index * 32 + 2}
                      y={240 - height}
                      width={28}
                      height={height}
                    />
                  </g>
                );
              })}
            </svg>
            <div className="chart-axis">
              <span>{daily[0]?.date}</span>
              <span>{daily.at(-1)?.date}</span>
            </div>
            <details className="daily-details">
              <summary>查看每日精确金额与请求数</summary>
              <table className="responsive-table">
                <caption className="sr-only">每日账本明细</caption>
                <thead>
                  <tr>
                    <th>日期</th>
                    <th>已知费用</th>
                    <th>请求数</th>
                    <th>待结算</th>
                  </tr>
                </thead>
                <tbody>
                  {daily.map((day) => (
                    <tr key={day.date}>
                      <th scope="row">{day.date}</th>
                      <td data-label="已知费用">
                        {cny(day.totals.known_cost_cny)}
                      </td>
                      <td data-label="请求数">
                        {integer(day.totals.requests)}
                      </td>
                      <td data-label="待结算">
                        {integer(day.totals.unsettled_cost_requests)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </details>
          </>
        )}
      </section>
      <Groups selection={selection} loginNeeded={loginNeeded} />
      <p className="footnote">
        美元来源费用使用请求冻结汇率折算，不随当前汇率重算。缺少 Token
        的媒体请求不计入 Token 合计；此处不是图片张数或视频秒数统计。
      </p>
    </>
  );
}

function Pager({
  hasPrevious,
  hasNext,
  previous,
  next,
}: {
  hasPrevious: boolean;
  hasNext: boolean;
  previous: () => void;
  next: () => void;
}) {
  return (
    <div className="pager">
      <span>每页最多 20 行 · 游标分页</span>
      <div>
        <button disabled={!hasPrevious} onClick={previous}>
          上一页
        </button>
        <button disabled={!hasNext} onClick={next}>
          下一页
        </button>
      </div>
    </div>
  );
}
function Groups({
  selection,
  loginNeeded,
}: {
  selection: Selection;
  loginNeeded: (error: unknown) => void;
}) {
  const [dimension, setDimension] = useState<"model" | "key">("model");
  return (
    <section className="group-section" aria-labelledby="groups-title">
      <div className="section-heading">
        <h2 id="groups-title">按维度统计</h2>
        <label>
          分组维度
          <select
            value={dimension}
            onChange={(event) =>
              setDimension(event.target.value as "model" | "key")
            }
          >
            <option value="model">逻辑模型</option>
            <option value="key">虚拟 Key</option>
          </select>
        </label>
      </div>
      <GroupRows
        key={dimension}
        selection={selection}
        dimension={dimension}
        loginNeeded={loginNeeded}
      />
    </section>
  );
}
function GroupRows({
  selection,
  dimension,
  loginNeeded,
}: {
  selection: Selection;
  dimension: "model" | "key";
  loginNeeded: (error: unknown) => void;
}) {
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const report = useReport(
    "usage/groups/query",
    groupsSchema,
    { filter: selection.filter, dimension, limit: 20, after: cursors.at(-1) },
    loginNeeded,
  );
  if (report.error !== null)
    return <Notice retry={report.retry}>{errorMessage(report.error)}</Notice>;
  if (!report.data) return <Loading />;
  const { data, next_after } = report.data;
  return (
    <>
      <div className="table-wrap">
        <table className="responsive-table">
          <caption className="sr-only">
            {dimension === "model" ? "模型" : "虚拟 Key"}费用统计
          </caption>
          <thead>
            <tr>
              <th>对象</th>
              <th>已知费用 / 预留</th>
              <th>请求状态</th>
              <th>Token</th>
            </tr>
          </thead>
          <tbody>
            {data.map((row) => (
              <tr key={row.object_id}>
                <td data-label="对象">
                  <strong>{row.label}</strong>
                  <span className="identifier">{row.object_id}</span>
                </td>
                <td data-label="费用">
                  <span>{cny(row.totals.known_cost_cny)}</span>
                  <span className="muted">
                    预留 {cny(row.totals.reserved_cny)}
                  </span>
                  <span className="muted">
                    待结算 {integer(row.totals.unsettled_cost_requests)} 笔
                  </span>
                </td>
                <td data-label="状态">
                  <StatusTotals totals={row.totals} />
                </td>
                <td data-label="Token">
                  <span>{integer(row.totals.total_tokens)}</span>
                  <span className="muted">
                    覆盖 {integer(row.totals.token_usage_requests)} 笔
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!data.length && (
        <Empty message="此分组没有匹配记录。" retry={report.retry} />
      )}
      <Pager
        hasPrevious={cursors.length > 1}
        hasNext={next_after !== null}
        previous={() => setCursors((v) => v.slice(0, -1))}
        next={() => {
          if (next_after !== null) setCursors((v) => [...v, next_after]);
        }}
      />
    </>
  );
}
function StatusTotals({ totals }: { totals: Totals }) {
  return (
    <>
      <span>
        共 {integer(totals.requests)} · 成功 {integer(totals.succeeded)} · 失败{" "}
        {integer(totals.failed)}
      </span>
      <span className="muted">
        执行中 {integer(totals.in_progress)} · 不确定{" "}
        {integer(totals.submission_unknown)} · 已核对{" "}
        {integer(totals.reconciled)}
      </span>
    </>
  );
}
function Requests({
  selection,
  loginNeeded,
}: {
  selection: Selection;
  loginNeeded: (error: unknown) => void;
}) {
  const [cursors, setCursors] = useState<(RequestCursor | undefined)[]>([
    undefined,
  ]);
  const report = useReport(
    "requests/query",
    requestsSchema,
    { filter: selection.filter, limit: 20, before: cursors.at(-1) },
    loginNeeded,
  );
  if (report.error !== null)
    return <Notice retry={report.retry}>{errorMessage(report.error)}</Notice>;
  if (!report.data) return <Loading />;
  const { data, next_before } = report.data;
  return (
    <section aria-label="请求记录">
      <div className="table-wrap">
        <table className="responsive-table requests">
          <caption className="sr-only">已准入请求日志</caption>
          <thead>
            <tr>
              <th>时间 / 请求 ID</th>
              <th>模型 / 虚拟 Key</th>
              <th>状态 / 用途</th>
              <th>费用 / Token</th>
            </tr>
          </thead>
          <tbody>
            {data.map((row) => (
              <tr key={row.id}>
                <td data-label="请求">
                  <time dateTime={row.created_at}>
                    {time(row.created_at, selection.timezone)}
                  </time>
                  <span className="identifier">{row.id}</span>
                  <details>
                    <summary>完整时间</summary>
                    <span className="identifier">创建 {row.created_at}</span>
                    <span className="identifier">更新 {row.updated_at}</span>
                  </details>
                </td>
                <td data-label="模型与 Key">
                  <strong>{row.model_name}</strong>
                  <span className="identifier">{row.model_id}</span>
                  <span>{row.key_alias}</span>
                  <span className="identifier">{row.key_id}</span>
                </td>
                <td data-label="状态">
                  <span className={`badge status-${row.status}`}>
                    {statusLabels[row.status]}
                  </span>
                  <span className="muted">
                    {row.purpose === "model" ? "模型调用" : "护栏检测"}
                  </span>
                  {row.error_code && (
                    <span className="identifier">{row.error_code}</span>
                  )}
                </td>
                <td data-label="费用与用量">
                  <strong>{cny(row.cost_cny)}</strong>
                  <span className="muted">预留 {cny(row.reserved_cny)}</span>
                  <span>
                    输入{" "}
                    {row.input_tokens === null
                      ? "无证据"
                      : integer(row.input_tokens)}
                  </span>
                  <span>
                    输出{" "}
                    {row.output_tokens === null
                      ? "无证据"
                      : integer(row.output_tokens)}
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!data.length && (
        <Empty message="没有匹配的已准入请求。" retry={report.retry} />
      )}
      <Pager
        hasPrevious={cursors.length > 1}
        hasNext={next_before !== null}
        previous={() => setCursors((v) => v.slice(0, -1))}
        next={() => {
          if (next_before !== null) setCursors((v) => [...v, next_before]);
        }}
      />
      <p className="footnote">
        鉴权或准入前拒绝不在此表。“提交不确定”不等同于失败，请勿据此重新发起模型调用。跨页查询为实时数据，不是冻结导出。
      </p>
    </section>
  );
}
