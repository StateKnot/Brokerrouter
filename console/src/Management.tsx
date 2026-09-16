import {
  createContext,
  useContext,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import type { FormEvent, ReactNode } from "react";
import type { z } from "zod";
import { ApiError, errorMessage, needsLogin, request } from "./api";
import {
  auditSchema,
  modelsSchema,
  endpointsSchema,
  mutationSchema,
} from "./contracts";
import type { Model, Endpoint } from "./contracts";
import { dateRange, recentRange, time } from "./format";
import type { Timezone } from "./format";

export type Denied = (error: unknown) => void;
export const PendingConfig = createContext<{
  owner: string | null;
  setOwner: (id: string | null) => void;
}>({ owner: null, setOwner: () => {} });
type QueryPath =
  | "approvals/query"
  | "audit/query"
  | "governance/monitor/query"
  | "governance/monitor/rails/query"
  | "catalog/models/query"
  | "catalog/endpoints/query"
  | "settings/keys/query"
  | "settings/keys/detail"
  | "settings/models/query"
  | "settings/fx/query"
  | "settings/guardrails/query"
  | "settings/guardrails/history"
  | "settings/policies/query"
  | "settings/policies/preview"
  | "settings/governance/targets";
export function useQuery<T>(
  path: QueryPath,
  schema: z.ZodType<T>,
  body: unknown,
  epoch: number,
  denied: Denied,
) {
  const [state, setState] = useState<{
    identity?: string;
    data?: T;
    error?: unknown;
    loading: boolean;
  }>({ loading: true });
  const encoded = JSON.stringify(body);
  const identity = `${path}:${encoded}:${epoch}`;
  useEffect(() => {
    const controller = new AbortController();
    setState({ identity, loading: true });
    void request(path, schema, JSON.parse(encoded), controller.signal)
      .then((data) => {
        if (!controller.signal.aborted)
          setState({ identity, data, loading: false });
      })
      .catch((error) => {
        if (controller.signal.aborted) return;
        setState({ identity, error, loading: false });
        if (
          needsLogin(error) ||
          (error instanceof ApiError && error.status === 403)
        )
          denied(error);
      });
    return () => controller.abort();
  }, [path, schema, encoded, identity, denied]);
  return state.identity === identity ? state : { loading: true };
}
export function QueryState({
  loading,
  error,
}: {
  loading: boolean;
  error?: unknown;
}) {
  if (loading) return <p role="status">正在加载最新记录…</p>;
  if (error)
    return (
      <p className="notice" role="alert">
        {errorMessage(error)}
      </p>
    );
  return null;
}
export function Audit({ denied }: { denied: Denied }) {
  const initial = useRef(recentRange("Asia/Shanghai"));
  const [query, setQuery] = useState<Record<string, unknown>>({
    ...dateRange(initial.current.start, initial.current.end, "Asia/Shanghai"),
    limit: 30,
  });
  const [zone, setZone] = useState<Timezone>("Asia/Shanghai");
  const [epoch, setEpoch] = useState(0);
  const [error, setError] = useState("");
  const result = useQuery("audit/query", auditSchema, query, epoch, denied);
  function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget),
      timezone = f.get("timezone") as Timezone;
    try {
      const range = dateRange(
        String(f.get("start")),
        String(f.get("end")),
        timezone,
      );
      setQuery({
        ...range,
        limit: 30,
        ...(f.get("action") ? { action: String(f.get("action")).trim() } : {}),
        ...(f.get("object")
          ? { object_id: String(f.get("object")).trim() }
          : {}),
      });
      setZone(timezone);
      setEpoch((v) => v + 1);
      setError("");
    } catch {
      setError("日期无效，请选择不超过 31 天的范围。");
    }
  }
  return (
    <section className="management" aria-label="审计查询">
      <form className="config-grid" onSubmit={submit}>
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
          结束日期
          <input
            name="end"
            type="date"
            required
            defaultValue={initial.current.end}
          />
        </label>
        <label>
          时区
          <select name="timezone">
            <option value="Asia/Shanghai">北京时间 UTC+8</option>
            <option value="UTC">UTC</option>
          </select>
        </label>
        <label>
          动作（精确匹配）
          <input
            name="action"
            maxLength={200}
            placeholder="catalog.endpoint_updated"
          />
        </label>
        <label>
          对象 ID（精确匹配）
          <input name="object" maxLength={200} />
        </label>
        <button type="submit">查询 / 刷新审计</button>
      </form>
      {error && (
        <p role="alert" className="notice">
          {error}
        </p>
      )}
      <p className="muted">
        数据库执行身份不是最终操作人。配置变更另列操作人、理由与操作编号；不展示原始请求或任意审计载荷。
      </p>
      <QueryState {...result} />
      {result.data && (
        <>
          <ol className="record-list" aria-label="审计记录">
            {result.data.data.map((r) => (
              <li key={r.id}>
                <div className="record-heading">
                  <strong>{r.action}</strong>
                  <time dateTime={r.created_at}>
                    {time(r.created_at, zone)}
                  </time>
                </div>
                <dl className="config-facts">
                  <div>
                    <dt>对象</dt>
                    <dd>{r.object_id}</dd>
                  </div>
                  <div>
                    <dt>审计编号</dt>
                    <dd>{r.id}</dd>
                  </div>
                  <div>
                    <dt>数据库执行身份</dt>
                    <dd>{r.database_actor}</dd>
                  </div>
                  {r.actor_user_id && (
                    <div>
                      <dt>操作人</dt>
                      <dd>{r.actor_user_id}</dd>
                    </div>
                  )}
                  {r.operation_id && (
                    <div>
                      <dt>操作编号</dt>
                      <dd>{r.operation_id}</dd>
                    </div>
                  )}
                  {r.config_revision && (
                    <div>
                      <dt>配置版本</dt>
                      <dd>{r.config_revision}</dd>
                    </div>
                  )}
                  {r.change_reason && (
                    <div>
                      <dt>变更理由</dt>
                      <dd>{r.change_reason}</dd>
                    </div>
                  )}
                </dl>
              </li>
            ))}
          </ol>
          {result.data.data.length === 0 && <p>此范围内没有审计记录。</p>}
          <div className="config-actions">
            <button
              disabled={!query.before}
              onClick={() => {
                setQuery((q) => ({ ...q, before: undefined }));
                setEpoch((v) => v + 1);
              }}
            >
              返回最新记录
            </button>
            <button
              disabled={!result.data.next_before}
              onClick={() =>
                setQuery((q) => ({ ...q, before: result.data!.next_before }))
              }
            >
              下一页审计
            </button>
          </div>
        </>
      )}
    </section>
  );
}

type Change = Record<string, unknown>;
const labels: Record<string, string> = {
  kind: "变更类型",
  id: "对象 ID",
  model_id: "逻辑模型",
  expected_revision: "依据版本",
  name: "名称",
  active: "启用",
  priority: "路由优先级",
  currency: "价格币种",
  input_per_million: "输入 / 百万 Token",
  output_per_million: "输出 / 百万 Token",
  connection_id: "供应商连接",
  connection_revision: "连接版本",
  upstream_model: "上游模型",
  context_tokens: "上下文上限",
  max_output_tokens: "输出上限",
  operation: "能力",
  supports_tools: "工具调用",
  embedding_dimension: "向量维度",
  key_hash: "密钥摘要（不是密钥）",
  key_prefix: "密钥前缀",
  budget_cny: "人民币预算（元）",
  models: "允许的模型 ID",
  expires_at: "有效期",
  expected_latest_id: "依据最新汇率版本",
  usd_cny: "1 USD 兑换 CNY",
  source: "汇率来源",
  effective_at: "生效时间（空值代表提交时）",
  stage: "执行阶段",
  rule: "完整检测规则",
  guardrail_id: "护栏 ID",
  key_id: "虚拟 Key ID（空值匹配全部）",
  runtime_id: "Runtime ID（空值匹配全部）",
  tool_name: "工具名（空值匹配全部）",
};
const kinds: Record<string, string> = {
  model_create: "新建模型（停用）",
  model_update: "修改模型",
  endpoint_create: "新建端点（停用）",
  endpoint_update: "修改端点",
  key_create: "创建虚拟 Key（提交后可用）",
  key_update: "修改虚拟 Key",
  key_revoke: "永久撤销虚拟 Key",
  fx_publish: "发布 USD/CNY 汇率版本",
  guardrail_publish: "发布护栏新版本（包含启停状态）",
  policy_put: "保存护栏匹配策略",
};
function mutationError(error: unknown): string {
  if (error instanceof ApiError && (error.status === 0 || error.status >= 500))
    return "暂时无法核对本次提交结果，请保留操作编号，不要重新创建相同配置。";
  if (
    error instanceof ApiError &&
    (error.status === 400 || error.status === 422)
  )
    return "配置未被接受，请核对字段范围、价格与逻辑模型的能力约束。";
  return errorMessage(error);
}
export function MutationForm({
  children,
  build,
  saved,
  denied,
  path = "catalog/apply",
  review = "配置会影响后续调用。存量请求保留准入时的路由、价格与汇率快照。新增模型和端点默认停用，联调验证后再启用。",
}: {
  children: ReactNode;
  build: (form: FormData) => Change;
  saved: (message: string) => void;
  denied: Denied;
  path?: "catalog/apply" | "settings/apply";
  review?: string;
}) {
  const id = useId();
  const { owner, setOwner } = useContext(PendingConfig);
  const [pending, setPending] = useState<{
    operation_id: string;
    reason: string;
    change: Change;
  } | null>(null);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState<unknown>(null);
  const [validation, setValidation] = useState("");
  const controller = useRef<AbortController | null>(null);
  const reviewRef = useRef<HTMLElement | null>(null);
  useEffect(() => () => controller.current?.abort(), []);
  useEffect(() => {
    if (!pending) return;
    reviewRef.current?.focus();
    const unload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
    };
    const navigate = (e: MouseEvent) => {
      if (!(e.target instanceof Element) || !e.target.closest("a[href]"))
        return;
      if (
        !window.confirm(
          "仍有待确认或结果未核对的配置操作。离开后请先查询配置和审计记录，不要直接重复创建。确定离开？",
        )
      ) {
        e.preventDefault();
        e.stopPropagation();
      }
    };
    window.addEventListener("beforeunload", unload);
    document.addEventListener("click", navigate, true);
    return () => {
      window.removeEventListener("beforeunload", unload);
      document.removeEventListener("click", navigate, true);
    };
  }, [pending]);
  function prepare(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (owner !== null) return;
    const form = new FormData(e.currentTarget);
    try {
      const reason = String(form.get("reason") ?? "").trim();
      if (!reason) throw new Error("请填写变更理由。");
      const encoder = new TextEncoder();
      if (
        encoder.encode(reason).length > 500 ||
        /[\u0000-\u001f\u007f-\u009f]/u.test(reason)
      )
        throw new Error("变更理由最多 500 个 UTF-8 字节，不能包含控制字符。");
      const change = build(form);
      for (const [key, value] of Object.entries(change))
        if (
          typeof value === "string" &&
          (encoder.encode(value).length > 200 ||
            /[\u0000-\u001f\u007f-\u009f]/u.test(value))
        )
          throw new Error(
            `${labels[key] ?? "配置字段"}最多 200 个 UTF-8 字节，不能包含控制字符。`,
          );
      const prepared = {
        operation_id: crypto.randomUUID(),
        reason,
        change,
      };
      if (encoder.encode(JSON.stringify(prepared)).length > 16384)
        throw new Error("配置内容超过 16 KiB，请缩小本次授权范围。");
      setPending(prepared);
      setOwner(id);
      setError(null);
      setValidation("");
    } catch (cause) {
      setValidation(
        cause instanceof Error ? cause.message : "请检查配置字段。",
      );
    }
  }
  async function commit() {
    if (!pending || busy) return;
    setBusy(true);
    setError(null);
    const active = new AbortController();
    controller.current = active;
    try {
      const receipt = await request(
        path,
        mutationSchema,
        pending,
        active.signal,
      );
      if (active.signal.aborted) return;
      setPending(null);
      setOwner(null);
      setBusy(false);
      saved(
        `已提交配置版本 ${receipt.revision}。对象 ${receipt.object_id}。操作编号 ${receipt.operation_id}。请核对最新状态；配置不改写历史调用。`,
      );
    } catch (cause) {
      if (active.signal.aborted) return;
      setError(cause);
      setBusy(false);
      if (
        needsLogin(cause) ||
        (cause instanceof ApiError && cause.status === 403)
      )
        denied(cause);
    }
  }
  const conflict = error instanceof ApiError && error.status === 409;
  return (
    <div className="mutation-form">
      <form onSubmit={prepare}>
        <fieldset
          disabled={pending !== null || (owner !== null && owner !== id)}
          className="config-grid"
        >
          <legend className="sr-only">配置字段</legend>
          {children}
          <label className="wide">
            变更理由
            <input name="reason" required maxLength={500} />
          </label>
          <button type="submit">核对变更</button>
        </fieldset>
      </form>
      {validation && (
        <p role="alert" className="notice">
          {validation}
        </p>
      )}
      {pending && (
        <section
          className="change-review"
          aria-label="变更确认"
          tabIndex={-1}
          ref={reviewRef}
        >
          <h3>确认这次配置变更</h3>
          <p>{review}</p>
          <dl className="config-facts">
            {Object.entries(pending.change).map(([key, value]) => (
              <div key={key}>
                <dt>{labels[key] ?? key}</dt>
                <dd>
                  {key === "kind" ? (
                    kinds[String(value)]
                  ) : value === null ? (
                    pending.change.kind === "policy_put" &&
                    ["model_id", "key_id", "runtime_id", "tool_name"].includes(
                      key,
                    ) ? (
                      "全部（不限）"
                    ) : (
                      "不适用"
                    )
                  ) : typeof value === "boolean" ? (
                    value ? (
                      "是"
                    ) : (
                      "否"
                    )
                  ) : typeof value === "object" && !Array.isArray(value) ? (
                    <pre className="config-json">
                      {JSON.stringify(value, null, 2)}
                    </pre>
                  ) : (
                    String(value)
                  )}
                </dd>
              </div>
            ))}
            <div>
              <dt>变更理由</dt>
              <dd>{pending.reason}</dd>
            </div>
            <div>
              <dt>操作编号</dt>
              <dd>{pending.operation_id}</dd>
            </div>
          </dl>
          {error !== null && (
            <p role="alert" className="notice">
              {mutationError(error)}{" "}
              {!conflict &&
                "结果尚未核对；重试将复用相同内容和操作编号，不会自动再次创建。"}
            </p>
          )}
          <div className="config-actions">
            <button disabled={busy || conflict} onClick={() => void commit()}>
              {busy ? "正在提交…" : error ? "重试同一操作" : "确认提交配置"}
            </button>
            <button
              disabled={busy}
              onClick={() => {
                setPending(null);
                setOwner(null);
                setError(null);
                if (error)
                  saved("已停止重试，请核对当前配置和审计后再决定下一步。");
              }}
            >
              取消并重新核对
            </button>
          </div>
        </section>
      )}
    </div>
  );
}
const value = (f: FormData, name: string) => String(f.get(name) ?? "").trim();
function number(f: FormData, name: string) {
  const v = value(f, name);
  if (
    !/^\d+$/.test(v) ||
    !Number.isSafeInteger(Number(v)) ||
    Number(v) > 2147483647
  )
    throw new Error("数量与优先级必须是有效整数。");
  return Number(v);
}
function price(f: FormData, name: string) {
  const v = value(f, name);
  if (
    !/^(0|[1-9]\d{0,9})(\.\d{1,12})?$/.test(v) ||
    BigInt(v.split(".")[0]!) > 1000000000n ||
    (v.startsWith("1000000000.") && /[1-9]/.test(v.split(".")[1]!))
  )
    throw new Error("价格必须是 0 到 10 亿之间的十进制，最多 12 位小数。");
  return v;
}
function PriceFields({ endpoint }: { endpoint?: Endpoint }) {
  return (
    <>
      <label>
        价格币种
        <select name="currency" defaultValue={endpoint?.currency ?? "CNY"}>
          <option value="CNY">CNY · 人民币</option>
          <option value="USD">USD · 美元（按已配置汇率折算）</option>
        </select>
      </label>
      <label>
        输入价格 / 百万 Token
        <input
          name="input_per_million"
          inputMode="decimal"
          required
          defaultValue={endpoint?.input_per_million ?? ""}
        />
      </label>
      <label>
        输出价格 / 百万 Token
        <input
          name="output_per_million"
          inputMode="decimal"
          required
          defaultValue={endpoint?.output_per_million ?? ""}
        />
      </label>
      <label>
        路由优先级（较小优先）
        <input
          name="priority"
          type="number"
          min="0"
          max="2147483647"
          step="1"
          required
          defaultValue={endpoint?.priority ?? 100}
        />
      </label>
    </>
  );
}
function prices(f: FormData) {
  return {
    currency: value(f, "currency"),
    input_per_million: price(f, "input_per_million"),
    output_per_million: price(f, "output_per_million"),
    priority: number(f, "priority"),
  };
}
function ModelEdit({
  model,
  saved,
  denied,
}: {
  model?: Model;
  saved: (message: string) => void;
  denied: Denied;
}) {
  return (
    <MutationForm
      denied={denied}
      saved={saved}
      build={(f) =>
        model
          ? {
              kind: "model_update",
              id: model.id,
              expected_revision: model.revision,
              name: value(f, "name"),
              active: value(f, "active") === "true",
            }
          : { kind: "model_create", id: value(f, "id"), name: value(f, "name") }
      }
    >
      {!model && (
        <label>
          逻辑模型 ID
          <input name="id" required maxLength={200} autoComplete="off" />
        </label>
      )}
      <label>
        模型名称
        <input
          name="name"
          required
          maxLength={200}
          defaultValue={model?.name}
        />
      </label>
      {model && (
        <label>
          模型状态
          <select name="active" defaultValue={String(model.active)}>
            <option value="false">停用</option>
            <option value="true">启用（需已有可用端点与 Key 授权）</option>
          </select>
        </label>
      )}
    </MutationForm>
  );
}
function EndpointEdit({
  endpoint,
  saved,
  denied,
}: {
  endpoint: Endpoint;
  saved: (message: string) => void;
  denied: Denied;
}) {
  return (
    <MutationForm
      denied={denied}
      saved={saved}
      build={(f) => ({
        kind: "endpoint_update",
        id: endpoint.id,
        expected_revision: endpoint.revision,
        name: value(f, "name"),
        active: value(f, "active") === "true",
        ...prices(f),
      })}
    >
      <label>
        端点名称
        <input
          name="name"
          required
          maxLength={200}
          defaultValue={endpoint.name}
        />
      </label>
      <label>
        端点状态
        <select name="active" defaultValue={String(endpoint.active)}>
          <option value="false">停用</option>
          <option value="true">启用（仅在联调验收后）</option>
        </select>
      </label>
      <PriceFields endpoint={endpoint} />
    </MutationForm>
  );
}
function EndpointCreate({
  model,
  connections,
  saved,
  denied,
}: {
  model: string;
  connections: z.infer<typeof endpointsSchema>["connections"];
  saved: (message: string) => void;
  denied: Denied;
}) {
  const [operation, setOperation] = useState("chat");
  return (
    <MutationForm
      denied={denied}
      saved={saved}
      build={(f) => {
        const c = connections.find((c) => c.id === value(f, "connection"));
        if (!c) throw new Error("请选择预登记的供应商连接。");
        const context = number(f, "context_tokens"),
          output = number(f, "max_output_tokens");
        if (context < 1 || output < 1 || output > context)
          throw new Error("输出上限不能超过上下文上限。");
        return {
          kind: "endpoint_create",
          model_id: model,
          connection_id: c.id,
          connection_revision: c.revision,
          name: value(f, "name"),
          upstream_model: value(f, "upstream_model"),
          ...prices(f),
          context_tokens: context,
          max_output_tokens: output,
          operation,
          supports_tools:
            operation === "chat" && value(f, "supports_tools") === "true",
          embedding_dimension:
            operation === "embedding" ? number(f, "embedding_dimension") : null,
        };
      }}
    >
      <label>
        供应商连接
        <select name="connection" required defaultValue="">
          <option value="" disabled>
            请选择运维预登记连接
          </option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name}
            </option>
          ))}
        </select>
      </label>
      <label>
        端点名称
        <input name="name" required maxLength={200} />
      </label>
      <label>
        上游模型 ID
        <input name="upstream_model" required maxLength={200} />
      </label>
      <label>
        调用能力
        <select
          value={operation}
          onChange={(e) => setOperation(e.target.value)}
        >
          <option value="chat">Chat · 文本对话</option>
          <option value="embedding">Embedding · 向量</option>
        </select>
      </label>
      <label>
        上下文 Token 上限
        <input
          name="context_tokens"
          type="number"
          min="1"
          max="2147483647"
          required
        />
      </label>
      <label>
        输出 Token 上限
        <input
          name="max_output_tokens"
          type="number"
          min="1"
          max="2147483647"
          required
        />
      </label>
      {operation === "embedding" ? (
        <label>
          向量维度
          <input
            name="embedding_dimension"
            type="number"
            min="1"
            max="65536"
            required
          />
        </label>
      ) : (
        <label>
          支持工具调用
          <select name="supports_tools" defaultValue="false">
            <option value="false">否</option>
            <option value="true">是</option>
          </select>
        </label>
      )}
      <PriceFields />
    </MutationForm>
  );
}
export function Catalog({ denied }: { denied: Denied }) {
  const [owner, setOwner] = useState<string | null>(null);
  return (
    <PendingConfig.Provider value={{ owner, setOwner }}>
      <CatalogBody denied={denied} />
    </PendingConfig.Provider>
  );
}
function CatalogBody({ denied }: { denied: Denied }) {
  const { owner } = useContext(PendingConfig);
  const [query, setQuery] = useState<{
    limit: number;
    search?: string;
    after?: string;
  }>({ limit: 20 });
  const [epoch, setEpoch] = useState(0),
    [selected, setSelected] = useState<string | null>(null),
    [message, setMessage] = useState("");
  const result = useQuery(
    "catalog/models/query",
    modelsSchema,
    query,
    epoch,
    denied,
  );
  const saved = (message: string) => {
    setMessage(message);
    setEpoch((v) => v + 1);
  };
  return (
    <section className="management" aria-label="模型和端点配置">
      <p>
        本页管理文本对话与向量端点。每个逻辑模型可以挂载多个供应商；图片、视频、3D
        连接器另行管理。金额以人民币入账，美元端点使用准入时生效的汇率快照。
      </p>
      {message && (
        <p className="notice" role="status">
          {message}
        </p>
      )}
      <form
        className="config-actions"
        onSubmit={(e) => {
          e.preventDefault();
          if (owner !== null) return;
          setSelected(null);
          setQuery({
            limit: 20,
            search: value(new FormData(e.currentTarget), "search") || undefined,
          });
          setEpoch((v) => v + 1);
        }}
      >
        <label>
          搜索模型 ID 或名称
          <input name="search" maxLength={200} />
        </label>
        <button type="submit" disabled={owner !== null}>
          查询 / 刷新模型
        </button>
      </form>
      <QueryState {...result} />
      {result.data && (
        <>
          {!result.data.can_write && (
            <p>当前为只读模式；变更需要独立的 catalog_write 权限。</p>
          )}
          <ol className="record-list" aria-label="模型列表">
            {result.data.data.map((m) => (
              <li key={m.id}>
                <div className="record-heading">
                  <div>
                    <strong>{m.name}</strong>
                    <p>
                      {m.id} · {m.active ? "启用" : "停用"} · 版本 {m.revision}
                    </p>
                  </div>
                  <button
                    disabled={owner !== null}
                    onClick={() => setSelected(m.id)}
                  >
                    管理 {m.name}
                  </button>
                </div>
              </li>
            ))}
          </ol>
          {result.data.data.length === 0 && <p>没有匹配的模型。</p>}
          <div className="config-actions">
            <button
              disabled={!query.after || owner !== null}
              onClick={() => setQuery((q) => ({ ...q, after: undefined }))}
            >
              返回首批模型
            </button>
            <button
              disabled={!result.data.next_after || owner !== null}
              onClick={() =>
                setQuery((q) => ({ ...q, after: result.data!.next_after! }))
              }
            >
              下一页模型
            </button>
          </div>
          {result.data.can_write && (
            <details className="config-panel">
              <summary>新建逻辑模型</summary>
              <ModelEdit key={epoch} saved={saved} denied={denied} />
            </details>
          )}
          {selected && (
            <ModelDetail
              key={`${selected}-${epoch}`}
              id={selected}
              denied={denied}
              saved={saved}
            />
          )}
        </>
      )}
    </section>
  );
}
function ModelDetail({
  id,
  denied,
  saved,
}: {
  id: string;
  denied: Denied;
  saved: (message: string) => void;
}) {
  const { owner } = useContext(PendingConfig);
  const [after, setAfter] = useState<string | null>(null);
  const result = useQuery(
    "catalog/endpoints/query",
    endpointsSchema,
    { model_id: id, after, limit: 30 },
    0,
    denied,
  );
  return (
    <section className="config-panel" aria-label="模型详情">
      <h2>模型 {id}</h2>
      <QueryState {...result} />
      {result.data && (
        <>
          <p>
            {result.data.model.active ? "模型启用" : "模型停用"} · 版本{" "}
            {result.data.model.revision}
            。停用不取消在途请求，也不撤销已有副作用。
          </p>
          {result.data.can_write && (
            <details>
              <summary>修改模型名称与状态</summary>
              <ModelEdit
                model={result.data.model}
                saved={saved}
                denied={denied}
              />
            </details>
          )}
          <h3>供应商端点</h3>
          <ol className="record-list" aria-label="端点列表">
            {result.data.data.map((e) => (
              <li key={e.id}>
                <div className="record-heading">
                  <strong>{e.name}</strong>
                  <span>
                    {e.active ? "启用" : "停用"} · 版本 {e.revision}
                  </span>
                </div>
                <p>
                  {e.upstream_model} · {e.operation} · 优先级 {e.priority}
                </p>
                <dl className="config-facts">
                  <div>
                    <dt>端点 ID</dt>
                    <dd>{e.id}</dd>
                  </div>
                  <div>
                    <dt>供应商连接</dt>
                    <dd>
                      {e.connection
                        ? `${e.connection.name} · ${e.connection.id}`
                        : "历史 CLI 端点（无登记快照）"}
                    </dd>
                  </div>
                  <div>
                    <dt>输入 / 百万 Token</dt>
                    <dd>
                      {e.currency} {e.input_per_million}
                    </dd>
                  </div>
                  <div>
                    <dt>输出 / 百万 Token</dt>
                    <dd>
                      {e.currency} {e.output_per_million}
                    </dd>
                  </div>
                  <div>
                    <dt>上下文 / 输出上限</dt>
                    <dd>
                      {e.context_tokens} / {e.max_output_tokens}
                    </dd>
                  </div>
                  <div>
                    <dt>工具调用 / 向量维度</dt>
                    <dd>
                      {e.supports_tools ? "支持" : "不支持"} /{" "}
                      {e.embedding_dimension ?? "不适用"}
                    </dd>
                  </div>
                </dl>
                {result.data!.can_write && (
                  <details>
                    <summary>修改端点 {e.name}</summary>
                    <p>
                      供应商连接、上游模型和能力不可原地替换；需要变更时新建端点，再停用旧端点。
                    </p>
                    <EndpointEdit endpoint={e} saved={saved} denied={denied} />
                  </details>
                )}
              </li>
            ))}
          </ol>
          {result.data.data.length === 0 && (
            <p>暂无文本或向量端点。新建端点不会自动授予虚拟 Key 调用权限。</p>
          )}
          <div className="config-actions">
            <button
              disabled={!after || owner !== null}
              onClick={() => setAfter(null)}
            >
              返回首批端点
            </button>
            <button
              disabled={!result.data.next_after || owner !== null}
              onClick={() => setAfter(result.data!.next_after)}
            >
              下一页端点
            </button>
          </div>
          {result.data.can_write && (
            <details className="config-panel">
              <summary>添加供应商端点</summary>
              {result.data.connections.length ? (
                <EndpointCreate
                  model={id}
                  connections={result.data.connections}
                  saved={saved}
                  denied={denied}
                />
              ) : (
                <p>
                  当前组织没有预登记的供应商连接。请运维配置连接白名单；网页不接收上游密钥、环境变量名或任意地址。
                </p>
              )}
            </details>
          )}
        </>
      )}
    </section>
  );
}
