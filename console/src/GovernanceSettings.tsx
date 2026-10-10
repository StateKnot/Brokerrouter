import { useContext, useState } from "react";
import type { FormEvent } from "react";
import {
  MutationForm,
  PendingConfig,
  QueryState,
  useQuery,
} from "./Management";
import type { Denied } from "./Management";
import {
  stages,
  stageLabels,
  guardrailsSchema,
  policiesSchema,
  historySchema,
  targetsSchema,
  previewSchema,
} from "./governance-contracts";
import type { Guardrail, Policy, Rule } from "./governance-contracts";
import {
  buildRule,
  configText,
  optionalText,
  optionalUuid,
  value,
} from "./governance-input";

const review =
  "命中风险或检测失败时，智能体流程需要真实用户审批，不会由配置管理员自动放行。发布护栏新版本（包括启停）后旧版本的本轮豁免不适用于新版本；策略选择集合变更会使待审批内容失效。不会撤回已执行的工具副作用或改写历史费用。";
const ruleLabels: Record<Rule["kind"], string> = {
  always: "每次请求审批",
  contains_any: "包含任一检测词",
  model_judge: "模型语义判定（可能产生人民币费用）",
  asset_scan: "已发布的媒体扫描证据",
};
type ListQuery = { limit: number; search?: string; after?: string | null };
export default function GovernanceSettings({ denied }: { denied: Denied }) {
  const [owner, setOwner] = useState<string | null>(null);
  return (
    <PendingConfig.Provider value={{ owner, setOwner }}>
      <GovernanceBody denied={denied} />
    </PendingConfig.Provider>
  );
}
function GovernanceBody({ denied }: { denied: Denied }) {
  const { owner } = useContext(PendingConfig);
  const [mode, setMode] = useState<"guardrails" | "policies" | "preview">(
    "guardrails",
  );
  const [lookup, setLookup] = useState(false);
  return (
    <section className="management" aria-label="治理配置">
      <details className="config-panel">
        <summary>配置、审批与执行的边界</summary>
        <p>
          本页管理检测规则与匹配范围，不是审批弹窗。真正的暂停、反馈与恢复需要智能体接入治理协议；普通模型
          SDK 不会自动弹出审批。工具执行前的检查必须发生在产生副作用之前。
        </p>
      </details>
      <div className="config-actions" role="group" aria-label="治理配置页面">
        {(["guardrails", "policies", "preview"] as const).map((m) => (
          <button
            key={m}
            aria-pressed={mode === m}
            disabled={owner !== null}
            onClick={() => setMode(m)}
          >
            {
              {
                guardrails: "护栏配置",
                policies: "匹配策略",
                preview: "匹配预览",
              }[m]
            }
          </button>
        ))}
        <button
          disabled={owner !== null}
          aria-expanded={lookup}
          onClick={() => setLookup(!lookup)}
        >
          查找配置引用
        </button>
      </div>
      {lookup && <Targets denied={denied} />}
      {mode === "guardrails" ? (
        <Guardrails denied={denied} />
      ) : mode === "policies" ? (
        <Policies denied={denied} />
      ) : (
        <Preview denied={denied} />
      )}
    </section>
  );
}
function Search({
  query,
  setQuery,
  refresh,
}: {
  query: ListQuery;
  setQuery: (q: ListQuery) => void;
  refresh: () => void;
}) {
  const { owner } = useContext(PendingConfig);
  return (
    <form
      className="config-actions"
      onSubmit={(e) => {
        e.preventDefault();
        setQuery({
          limit: 20,
          search: value(new FormData(e.currentTarget), "search") || undefined,
        });
        refresh();
      }}
    >
      <label>
        搜索配置
        <input
          name="search"
          maxLength={200}
          defaultValue={query.search}
          disabled={owner !== null}
        />
      </label>
      <button disabled={owner !== null}>搜索 / 刷新</button>
    </form>
  );
}
function Paging({
  next,
  query,
  setQuery,
}: {
  next: string | null;
  query: ListQuery;
  setQuery: (q: ListQuery) => void;
}) {
  const { owner } = useContext(PendingConfig);
  return (
    <div className="config-actions">
      <button
        disabled={owner !== null || !query.after}
        onClick={() => setQuery({ ...query, after: null })}
      >
        返回首页
      </button>
      <button
        disabled={owner !== null || !next}
        onClick={() => setQuery({ ...query, after: next })}
      >
        下一页
      </button>
      <span>每页最多 {query.limit} 条</span>
    </div>
  );
}
function Guardrails({ denied }: { denied: Denied }) {
  const [query, setQuery] = useState<ListQuery>({ limit: 20 });
  const [epoch, setEpoch] = useState(0),
    [message, setMessage] = useState("");
  const [selected, setSelected] = useState<string | null>(null),
    [create, setCreate] = useState(false),
    [history, setHistory] = useState<string | null>(null);
  const { owner } = useContext(PendingConfig);
  const result = useQuery(
    "settings/guardrails/query",
    guardrailsSchema,
    query,
    epoch,
    denied,
  );
  const saved = (m: string) => {
    setMessage(m);
    setCreate(false);
    setEpoch((v) => v + 1);
  };
  return (
    <section aria-label="护栏配置列表">
      <h2>护栏版本</h2>
      <p>
        新护栏默认停用；名称、规则和启停都通过发布新版本保存。执行阶段不可改，需另建护栏
        ID。停用规则不是对历史风险的同意。
      </p>
      {message && (
        <p role="status" className="notice">
          {message}
        </p>
      )}
      <Search
        query={query}
        setQuery={(q) => {
          setSelected(null);
          setQuery(q);
        }}
        refresh={() => setEpoch((v) => v + 1)}
      />
      <QueryState {...result} />
      {result.data && (
        <>
          {!result.data.can_write && <p>仅有治理配置读取权限。</p>}
          {result.data.can_write && (
            <>
              <button
                disabled={owner !== null}
                aria-expanded={create}
                onClick={() => setCreate(!create)}
              >
                新建护栏
              </button>
              {create && (
                <div className="config-panel">
                  <GuardrailForm
                    key={`new-${epoch}`}
                    denied={denied}
                    saved={saved}
                  />
                </div>
              )}
            </>
          )}
          {!result.data.data.length && <p>当前搜索下没有护栏。</p>}
          <div className="record-list">
            {result.data.data.map((r) => (
              <article key={`${r.id}-${r.revision}`} className="config-panel">
                <h3>{r.name}</h3>
                <dl className="config-facts">
                  <div>
                    <dt>ID / 版本</dt>
                    <dd>
                      {r.id} · v{r.version} · 配置 {r.revision}
                    </dd>
                  </div>
                  <div>
                    <dt>状态 / 阶段</dt>
                    <dd>
                      {r.active ? "启用" : "停用"} · {stageLabels[r.stage]}
                    </dd>
                  </div>
                  <div>
                    <dt>检测类型</dt>
                    <dd>{ruleLabels[r.rule.kind]}</dd>
                  </div>
                </dl>
                <details>
                  <summary>查看当前完整规则</summary>
                  <pre className="config-json">
                    {JSON.stringify(r.rule, null, 2)}
                  </pre>
                </details>
                <div className="config-actions">
                  <button
                    disabled={owner !== null}
                    aria-expanded={history === r.id}
                    onClick={() => setHistory(history === r.id ? null : r.id)}
                  >
                    历史版本 {r.id}
                  </button>
                  {result.data!.can_write && (
                    <button
                      disabled={owner !== null}
                      aria-expanded={selected === r.id}
                      onClick={() =>
                        setSelected(selected === r.id ? null : r.id)
                      }
                    >
                      编辑护栏 {r.id}
                    </button>
                  )}
                </div>
                {history === r.id && (
                  <History id={r.id} denied={denied} epoch={epoch} />
                )}
                {selected === r.id && result.data!.can_write && (
                  <GuardrailForm rail={r} denied={denied} saved={saved} />
                )}
              </article>
            ))}
          </div>
          <Paging
            next={result.data.next_after}
            query={query}
            setQuery={setQuery}
          />
        </>
      )}
    </section>
  );
}
function GuardrailForm({
  rail,
  denied,
  saved,
}: {
  rail?: Guardrail;
  denied: Denied;
  saved: (m: string) => void;
}) {
  const [stage, setStage] = useState<Guardrail["stage"]>(rail?.stage ?? "pre");
  const [kind, setKind] = useState<Rule["kind"]>(rail?.rule.kind ?? "always");
  const initial = rail?.rule;
  const permitted: Rule["kind"][] =
    stage === "media_output"
      ? ["always", "asset_scan"]
      : stage === "artifact"
        ? ["always", "contains_any", "asset_scan"]
        : ["always", "contains_any", "model_judge"];
  return (
    <MutationForm
      path="settings/apply"
      review={review}
      denied={denied}
      saved={saved}
      build={(f) => ({
        kind: "guardrail_publish",
        id: rail?.id ?? configText(value(f, "id"), 100),
        expected_revision: rail?.revision ?? null,
        name: configText(value(f, "name"), 200),
        stage,
        rule: buildRule(f),
        active: rail ? f.get("active") === "on" : false,
      })}
    >
      {!rail && (
        <label>
          护栏 ID
          <input name="id" required maxLength={100} />
        </label>
      )}
      <label>
        护栏名称
        <input name="name" required maxLength={200} defaultValue={rail?.name} />
      </label>
      <label>
        执行阶段
        <select
          name="stage"
          value={stage}
          disabled={!!rail}
          onChange={(e) => {
            setStage(e.target.value as Guardrail["stage"]);
            setKind("always");
          }}
        >
          {stages.map((s) => (
            <option key={s} value={s}>
              {stageLabels[s]}
            </option>
          ))}
        </select>
      </label>
      <label className="wide">
        检测方式
        <select
          name="rule_kind"
          value={kind}
          onChange={(e) => setKind(e.target.value as Rule["kind"])}
        >
          {permitted.map((k) => (
            <option key={k} value={k}>
              {ruleLabels[k]}
            </option>
          ))}
        </select>
      </label>
      {kind === "contains_any" && (
        <>
          <label className="wide">
            检测词（每行一个，最多 64 个）
            <textarea
              name="terms"
              required
              rows={4}
              defaultValue={
                initial?.kind === "contains_any" ? initial.terms.join("\n") : ""
              }
            />
          </label>
          <label className="checkbox-label">
            <input
              type="checkbox"
              name="case_sensitive"
              defaultChecked={
                initial?.kind === "contains_any" && initial.case_sensitive
              }
            />
            区分大小写
          </label>
        </>
      )}
      {kind === "model_judge" && (
        <>
          <p className="wide">
            只检查 text_json，不读取图片、视频或 3D
            二进制内容。判定模型走服务器受限调用和人民币预算，失败需人工处理。启用前必须有可用
            chat 端点。
          </p>
          <label>
            判定模型 ID
            <input
              name="judge_model"
              required
              maxLength={200}
              defaultValue={
                initial?.kind === "model_judge" ? initial.model : ""
              }
            />
          </label>
          <label>
            单次判定上限（人民币元）
            <input
              name="maximum_cny"
              required
              inputMode="decimal"
              defaultValue={
                initial?.kind === "model_judge" ? initial.maximum_cny : "0.01"
              }
            />
          </label>
          <label>
            最大输入字节
            <input
              type="number"
              name="max_input_bytes"
              min={1}
              max={131072}
              step={1}
              required
              defaultValue={
                initial?.kind === "model_judge"
                  ? initial.max_input_bytes
                  : 32768
              }
            />
          </label>
          <label>
            最大输出 Token
            <input
              type="number"
              name="max_output_tokens"
              min={16}
              max={512}
              step={1}
              required
              defaultValue={
                initial?.kind === "model_judge" ? initial.max_output_tokens : 64
              }
            />
          </label>
          <label className="wide">
            风险判定指令
            <textarea
              name="instructions"
              rows={6}
              required
              defaultValue={
                initial?.kind === "model_judge" ? initial.instructions : ""
              }
            />
          </label>
        </>
      )}
      {kind === "asset_scan" && (
        <>
          <p className="wide">
            引用已发布扫描配置，策略必须精确选择该配置所属的模型。扫描技术安全不等于多模态语义合规，也不会因审批放行恶意或未验证资产。
          </p>
          <label className="wide">
            扫描配置 UUID
            <input
              name="profile_id"
              required
              defaultValue={
                initial?.kind === "asset_scan" ? initial.profile_id : ""
              }
            />
          </label>
        </>
      )}
      {rail && (
        <label className="checkbox-label">
          <input type="checkbox" name="active" defaultChecked={rail.active} />
          新版本启用（取消勾选会发布停用版本）
        </label>
      )}
    </MutationForm>
  );
}
function History({
  id,
  epoch,
  denied,
}: {
  id: string;
  epoch: number;
  denied: Denied;
}) {
  const [before, setBefore] = useState<number | null>(null);
  const result = useQuery(
    "settings/guardrails/history",
    historySchema,
    { id, before, limit: 10 },
    epoch,
    denied,
  );
  return (
    <section aria-label="不可变规则历史">
      <h4>不可变规则历史</h4>
      <p>历史内容只读；恢复旧规则也须作为新版本发布。启停操作见审计日志。</p>
      <QueryState {...result} />
      {result.data && (
        <>
          {result.data.data.map((v) => (
            <details key={v.version}>
              <summary>
                v{v.version} · {v.name} · {stageLabels[v.stage]}
              </summary>
              <pre className="config-json">
                {JSON.stringify(v.rule, null, 2)}
              </pre>
            </details>
          ))}
          <div className="config-actions">
            <button disabled={before === null} onClick={() => setBefore(null)}>
              最新版本
            </button>
            <button
              disabled={!result.data.next_before}
              onClick={() => setBefore(result.data!.next_before)}
            >
              更早版本
            </button>
          </div>
        </>
      )}
    </section>
  );
}
function Policies({ denied }: { denied: Denied }) {
  const [query, setQuery] = useState<ListQuery>({ limit: 20 });
  const [epoch, setEpoch] = useState(0),
    [message, setMessage] = useState("");
  const [selected, setSelected] = useState<string | null>(null),
    [create, setCreate] = useState(false);
  const { owner } = useContext(PendingConfig);
  const result = useQuery(
    "settings/policies/query",
    policiesSchema,
    query,
    epoch,
    denied,
  );
  const saved = (m: string) => {
    setMessage(m);
    setCreate(false);
    setEpoch((v) => v + 1);
  };
  return (
    <section aria-label="策略配置列表">
      <h2>匹配策略</h2>
      <p>
        一条策略内所有非空条件同时满足才匹配（AND）；多条策略取并集并按护栏去重（OR）。空值表示全部，不支持正则或通配符。命中相同护栏版本时，本轮已有豁免仍有效。
      </p>
      <p>
        策略引用护栏
        ID，始终跟随该护栏最新发布版本，不锁定当前规则内容。启用前请在护栏配置中核对规则与执行阶段。
      </p>
      {message && (
        <p role="status" className="notice">
          {message}
        </p>
      )}
      <Search
        query={query}
        setQuery={setQuery}
        refresh={() => setEpoch((v) => v + 1)}
      />
      <QueryState {...result} />
      {result.data && (
        <>
          {!result.data.can_write && <p>仅有治理配置读取权限。</p>}
          {result.data.can_write && (
            <>
              <button
                disabled={owner !== null}
                aria-expanded={create}
                onClick={() => setCreate(!create)}
              >
                新建匹配策略
              </button>
              {create && (
                <div className="config-panel">
                  <PolicyForm key={epoch} denied={denied} saved={saved} />
                </div>
              )}
            </>
          )}
          {!result.data.data.length && <p>当前搜索下没有策略。</p>}
          <div className="record-list">
            {result.data.data.map((p) => (
              <article key={`${p.id}-${p.revision}`} className="config-panel">
                <h3>
                  {p.guardrail_id} · {p.active ? "启用" : "停用"}
                </h3>
                <dl className="config-facts">
                  <div>
                    <dt>策略 ID / 配置版本</dt>
                    <dd>
                      {p.id} · {p.revision}
                    </dd>
                  </div>
                  {(
                    ["model_id", "key_id", "runtime_id", "tool_name"] as const
                  ).map((k) => (
                    <div key={k}>
                      <dt>
                        {
                          {
                            model_id: "模型",
                            key_id: "虚拟 Key",
                            runtime_id: "Runtime",
                            tool_name: "工具",
                          }[k]
                        }
                      </dt>
                      <dd>{p[k] ?? "全部（不限）"}</dd>
                    </div>
                  ))}
                </dl>
                {result.data!.can_write && (
                  <>
                    <button
                      disabled={owner !== null}
                      aria-expanded={selected === p.id}
                      onClick={() =>
                        setSelected(selected === p.id ? null : p.id)
                      }
                    >
                      编辑策略 {p.id}
                    </button>
                    {selected === p.id && (
                      <PolicyForm policy={p} denied={denied} saved={saved} />
                    )}
                  </>
                )}
              </article>
            ))}
          </div>
          <Paging
            next={result.data.next_after}
            query={query}
            setQuery={setQuery}
          />
        </>
      )}
    </section>
  );
}
function PolicyForm({
  policy,
  denied,
  saved,
}: {
  policy?: Policy;
  denied: Denied;
  saved: (m: string) => void;
}) {
  return (
    <MutationForm
      path="settings/apply"
      review={`${review} 所有选择条件留空会匹配该执行阶段的全部受管调用，请特别核对范围。新策略默认停用，启用前先启用护栏。`}
      denied={denied}
      saved={saved}
      build={(f) => ({
        kind: "policy_put",
        id: policy?.id ?? crypto.randomUUID(),
        expected_revision: policy?.revision ?? null,
        guardrail_id: configText(value(f, "guardrail_id"), 100),
        model_id: optionalText(f, "model_id"),
        key_id: optionalUuid(f, "key_id"),
        runtime_id: optionalText(f, "runtime_id"),
        tool_name: optionalText(f, "tool_name"),
        active: policy ? f.get("active") === "on" : false,
      })}
    >
      <label>
        绑定护栏 ID
        <input
          name="guardrail_id"
          required
          maxLength={100}
          defaultValue={policy?.guardrail_id}
        />
      </label>
      <Selectors policy={policy} />
      {policy && (
        <label className="checkbox-label">
          <input type="checkbox" name="active" defaultChecked={policy.active} />
          启用此策略
        </label>
      )}
    </MutationForm>
  );
}
function Selectors({ policy }: { policy?: Policy }) {
  return (
    <>
      {(["model_id", "key_id", "runtime_id", "tool_name"] as const).map((k) => (
        <label key={k}>
          {
            {
              model_id: "模型 ID（空值匹配全部）",
              key_id: "Key UUID（空值匹配全部）",
              runtime_id: "Runtime ID（空值匹配全部）",
              tool_name: "工具名（仅工具阶段）",
            }[k]
          }
          <input name={k} maxLength={200} defaultValue={policy?.[k] ?? ""} />
        </label>
      ))}
    </>
  );
}
function Targets({ denied }: { denied: Denied }) {
  const [kind, setKind] = useState("model"),
    [query, setQuery] = useState<ListQuery>({ limit: 20 }),
    [epoch, setEpoch] = useState(0);
  const result = useQuery(
    "settings/governance/targets",
    targetsSchema,
    { ...query, kind },
    epoch,
    denied,
  );
  return (
    <section className="config-panel" aria-label="可引用的配置">
      <h2>查找配置引用</h2>
      <p>
        可复制 ID 到表单。状态仅为查询时提示，提交时再次核验；这里不会创建
        Runtime、扫描器或模型。
      </p>
      <label>
        引用类型
        <select
          value={kind}
          onChange={(e) => {
            setKind(e.target.value);
            setQuery({ limit: 20 });
          }}
        >
          <option value="model">逻辑模型</option>
          <option value="key">虚拟 Key</option>
          <option value="runtime">已绑定 Runtime</option>
          <option value="scan_profile">扫描配置</option>
        </select>
      </label>
      <Search
        query={query}
        setQuery={setQuery}
        refresh={() => setEpoch((v) => v + 1)}
      />
      <QueryState {...result} />
      {result.data && (
        <>
          <ul>
            {result.data.data.map((t) => (
              <li key={t.id}>
                <strong>{t.name}</strong> · {t.active ? "可用" : "停用或过期"}
                <p className="identity">{t.id}</p>
              </li>
            ))}
          </ul>
          {!result.data.data.length && <p>没有符合条件的引用。</p>}
          <Paging
            next={result.data.next_after}
            query={query}
            setQuery={setQuery}
          />
        </>
      )}
    </section>
  );
}
function Preview({ denied }: { denied: Denied }) {
  const [input, setInput] = useState<Record<string, unknown> | null>(null),
    [epoch, setEpoch] = useState(0),
    [error, setError] = useState("");
  function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault();
    const f = new FormData(e.currentTarget);
    try {
      const key = optionalUuid(f, "key_id");
      if (!key) throw new Error("请选择已绑定且有效的智能体 Key UUID。");
      setInput({
        key_id: key,
        model_id: optionalText(f, "model_id"),
        tool_name: optionalText(f, "tool_name"),
        stage: value(f, "stage"),
      });
      setEpoch((v) => v + 1);
      setError("");
    } catch (e) {
      setInput(null);
      setError(e instanceof Error ? e.message : "请核对预览条件。");
    }
  }
  return (
    <section aria-label="已发布策略匹配预览">
      <h2>已发布策略匹配预览</h2>
      <p>
        仅显示当前配置的匹配结果，不发送内容、不调用模型、不产生费用，也不授予执行权限。需要真实已绑定
        Key；未保存的编辑不会参与预览。最多 256 个护栏，单次最多 8
        个模型判定护栏。
      </p>
      <form onSubmit={submit}>
        <fieldset className="config-grid">
          <legend>模拟节点上下文</legend>
          <label>
            智能体 Key UUID
            <input name="key_id" required />
          </label>
          <label>
            模型 ID（可空）
            <input name="model_id" maxLength={200} />
          </label>
          <label>
            工具名（可空）
            <input name="tool_name" maxLength={200} />
          </label>
          <label>
            预览阶段
            <select name="stage">
              {stages.map((s) => (
                <option key={s} value={s}>
                  {stageLabels[s]}
                </option>
              ))}
            </select>
          </label>
          <button>查询匹配结果</button>
        </fieldset>
      </form>
      {error && (
        <p role="alert" className="notice">
          {error}
        </p>
      )}
      {input && <PreviewResult input={input} epoch={epoch} denied={denied} />}
    </section>
  );
}
function PreviewResult({
  input,
  epoch,
  denied,
}: {
  input: Record<string, unknown>;
  epoch: number;
  denied: Denied;
}) {
  const result = useQuery(
    "settings/policies/preview",
    previewSchema,
    input,
    epoch,
    denied,
  );
  return (
    <div aria-live="polite">
      <QueryState {...result} />
      {result.data && (
        <>
          <p>
            Runtime：{result.data.runtime_id}。当前匹配{" "}
            {result.data.data.length} 个护栏，未执行内容检测。
          </p>
          <p className="identity">
            查询上下文：Key {String(input.key_id)} · 模型{" "}
            {String(input.model_id ?? "未指定")} · 工具{" "}
            {String(input.tool_name ?? "未指定")} · 阶段 {String(input.stage)}
          </p>
          {(input.stage === "media_output" || input.stage === "artifact") &&
            !result.data.data.some((r) => r.kind === "asset_scan") && (
              <p role="alert" className="notice">
                {input.stage === "artifact"
                  ? "当前没有匹配的 asset_scan 护栏，原生二进制产物不能交付。请配置与该模型当前扫描证据对应的策略。"
                  : "当前没有匹配的 asset_scan 护栏，不能完成隔离媒体交付。请先配置与该模型扫描证据对应的策略。"}
              </p>
            )}
          {result.data.judge_limit_exceeded && (
            <p role="alert" className="notice">
              超过 8 个模型判定护栏，实际调用会拒绝此配置，请缩小策略范围。
            </p>
          )}
          <ul>
            {result.data.data.map((r) => (
              <li key={r.id}>
                {r.name} · {r.id} · v{r.version} · {stageLabels[r.stage]} ·{" "}
                {ruleLabels[r.kind]}
              </li>
            ))}
          </ul>
          <p>
            这是配置时点的预览，不包含本轮审批豁免，也不证明预算、模型授权或执行权限可用。运行时会重新匹配。
          </p>
        </>
      )}
    </div>
  );
}
