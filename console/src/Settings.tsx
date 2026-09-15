import { useContext, useEffect, useRef, useState } from "react";
import { Temporal } from "@js-temporal/polyfill";
import {
  keysSchema,
  keyDetailSchema,
  grantModelsSchema,
  fxSchema,
} from "./contracts";
import type { VirtualKey } from "./contracts";
import {
  MutationForm,
  PendingConfig,
  QueryState,
  useQuery,
} from "./Management";
import type { Denied } from "./Management";
import { cny, time } from "./format";
import { decimal, expiry, mintKey, modelGrants } from "./settings-input";

const value = (f: FormData, k: string) => String(f.get(k) ?? "").trim();
const keyReview =
  "提交后 Key 可用于明确授权的模型，仍受组织共享预算约束。撤销不可恢复；配置变更不会取消已准入请求、退款或撤回副作用。丢失明文后只能撤销并创建新 Key，不能恢复密钥。";
export function Keys({ denied }: { denied: Denied }) {
  const [owner, setOwner] = useState<string | null>(null);
  return (
    <PendingConfig.Provider value={{ owner, setOwner }}>
      <KeysBody denied={denied} />
    </PendingConfig.Provider>
  );
}
function KeysBody({ denied }: { denied: Denied }) {
  const { owner } = useContext(PendingConfig);
  const [query, setQuery] = useState<{
    limit: number;
    search?: string;
    after?: string | null;
  }>({ limit: 20 });
  const [selected, setSelected] = useState<string | null>(null);
  const [epoch, setEpoch] = useState(0),
    [message, setMessage] = useState("");
  const result = useQuery(
    "settings/keys/query",
    keysSchema,
    query,
    epoch,
    denied,
  );
  const saved = (m: string) => {
    setMessage(m);
    setEpoch((v) => v + 1);
  };
  return (
    <section className="management" aria-label="虚拟 Key 管理">
      <p>
        Key 预算以人民币为主，代表累计上限，不是充值或每月自动重置。多个 Key
        共享组织总预算，预算之和不代表可用余额。
      </p>
      {message && (
        <p role="status" className="notice">
          {message}
        </p>
      )}
      <form
        className="config-grid"
        onSubmit={(e) => {
          e.preventDefault();
          if (owner) return;
          setQuery({
            limit: 20,
            search: value(new FormData(e.currentTarget), "search") || undefined,
          });
          setSelected(null);
          setEpoch((v) => v + 1);
        }}
      >
        <label>
          搜索 Key 名称、完整 ID 或前缀
          <input name="search" maxLength={200} />
        </label>
        <button disabled={owner !== null}>查询 / 刷新 Key</button>
      </form>
      <QueryState {...result} />
      {result.data && (
        <>
          <dl className="config-facts">
            <div>
              <dt>组织预算</dt>
              <dd>{cny(result.data.tenant.budget_cny)}</dd>
            </div>
            <div>
              <dt>组织已花费</dt>
              <dd>{cny(result.data.tenant.spent_cny)}</dd>
            </div>
            <div>
              <dt>组织预留</dt>
              <dd>{cny(result.data.tenant.reserved_cny)}</dd>
            </div>
          </dl>
          {!result.data.can_write && (
            <p>只读模式。变更需 keys_read 与独立 keys_write 权限。</p>
          )}
          <ol className="record-list" aria-label="Key 列表">
            {result.data.data.map((k) => (
              <li key={k.id}>
                <div className="record-heading">
                  <strong>{k.name}</strong>
                  <span>
                    {k.revoked_at
                      ? "已永久撤销"
                      : k.expires_at &&
                          Temporal.Instant.compare(
                            k.expires_at,
                            Temporal.Now.instant(),
                          ) <= 0
                        ? "已过期"
                        : "未撤销"}
                  </span>
                </div>
                <p>
                  {k.prefix}… · 版本 {k.revision}
                </p>
                <dl className="config-facts">
                  <div>
                    <dt>Key ID</dt>
                    <dd>{k.id}</dd>
                  </div>
                  <div>
                    <dt>预算</dt>
                    <dd>{cny(k.budget_cny)}</dd>
                  </div>
                  <div>
                    <dt>已花费</dt>
                    <dd>{cny(k.spent_cny)}</dd>
                  </div>
                  <div>
                    <dt>预留</dt>
                    <dd>{cny(k.reserved_cny)}</dd>
                  </div>
                </dl>
                <button
                  disabled={owner !== null}
                  onClick={() => setSelected(k.id)}
                >
                  管理 {k.name}
                </button>
              </li>
            ))}
          </ol>
          {!result.data.data.length && <p>暂无匹配的虚拟 Key。</p>}
          <div className="config-actions">
            <button
              disabled={!query.after || owner !== null}
              onClick={() => setQuery((q) => ({ ...q, after: null }))}
            >
              返回首批 Key
            </button>
            <button
              disabled={!result.data.next_after || owner !== null}
              onClick={() =>
                setQuery((q) => ({ ...q, after: result.data!.next_after }))
              }
            >
              下一页 Key
            </button>
          </div>
          {selected && (
            <KeyDetails
              key={`${selected}-${epoch}`}
              id={selected}
              denied={denied}
              saved={saved}
            />
          )}
          {result.data.can_write && (
            <details className="config-panel">
              <summary>创建虚拟 Key</summary>
              <KeyEditor key={epoch} denied={denied} saved={saved} />
            </details>
          )}
          <details className="config-panel">
            <summary>查询可授权模型 ID</summary>
            <GrantLookup denied={denied} />
          </details>
        </>
      )}
    </section>
  );
}
function GrantLookup({ denied }: { denied: Denied }) {
  const { owner } = useContext(PendingConfig);
  const [q, setQ] = useState<{
    limit: number;
    search?: string;
    after?: string | null;
  }>({ limit: 20 });
  const result = useQuery(
    "settings/models/query",
    grantModelsSchema,
    q,
    0,
    denied,
  );
  return (
    <>
      <form
        className="config-grid"
        onSubmit={(e) => {
          e.preventDefault();
          if (owner) return;
          setQ({
            limit: 20,
            search: value(new FormData(e.currentTarget), "search") || undefined,
          });
        }}
      >
        <label>
          搜索可授权模型
          <input name="search" maxLength={200} />
        </label>
        <button disabled={owner !== null}>查找模型</button>
      </form>
      <QueryState {...result} />
      {result.data && (
        <>
          <ol className="record-list" aria-label="可授权模型">
            {result.data.data.map((m) => (
              <li key={m.id}>
                <strong>{m.name}</strong>
                <p>
                  {m.id} · {m.active ? "启用" : "停用，授权不会自动启用模型"}
                </p>
              </li>
            ))}
          </ol>
          {!result.data.data.length && <p>没有匹配模型。</p>}
          <div className="config-actions">
            <button
              disabled={!q.after || owner !== null}
              onClick={() => setQ((v) => ({ ...v, after: null }))}
            >
              首批可授权模型
            </button>
            <button
              disabled={!result.data.next_after || owner !== null}
              onClick={() =>
                setQ((v) => ({ ...v, after: result.data!.next_after }))
              }
            >
              下一页可授权模型
            </button>
          </div>
        </>
      )}
    </>
  );
}
function KeyDetails({
  id,
  denied,
  saved,
}: {
  id: string;
  denied: Denied;
  saved: (message: string) => void;
}) {
  const result = useQuery(
    "settings/keys/detail",
    keyDetailSchema,
    { id },
    0,
    denied,
  );
  return (
    <section className="config-panel" aria-label="Key 详情">
      <h2>Key 详情</h2>
      <QueryState {...result} />
      {result.data && (
        <>
          <p>
            {result.data.key.name} · {result.data.key.id} · 版本{" "}
            {result.data.key.revision}
          </p>
          <p>
            有效期：
            {result.data.key.expires_at
              ? `${time(result.data.key.expires_at, "Asia/Shanghai")}（北京时间）`
              : "历史无限期 Key"}
          </p>
          <p>授权模型：{result.data.models.join("、") || "无"}</p>
          {result.data.grants_truncated && (
            <p className="notice">
              此历史 Key 超过 100 项授权，网页仅展示前 100
              项，不能替换其配置；请运维迁移为更小的授权范围。仍可永久撤销。
            </p>
          )}
          {result.data.key.revoked_at ? (
            <p>已永久撤销，不能重新启用。历史用量和审计仍然保留。</p>
          ) : (
            result.data.can_write && (
              <>
                {!result.data.grants_truncated && (
                  <details>
                    <summary>修改 Key 配置</summary>
                    <KeyEditor
                      current={result.data.key}
                      models={result.data.models}
                      denied={denied}
                      saved={saved}
                    />
                  </details>
                )}
                <details>
                  <summary>永久撤销 Key</summary>
                  <MutationForm
                    path="settings/apply"
                    denied={denied}
                    saved={saved}
                    review={keyReview}
                    build={() => ({
                      kind: "key_revoke",
                      id,
                      expected_revision: result.data!.key.revision,
                    })}
                  >
                    <p className="wide">
                      撤销 {result.data.key.name}{" "}
                      后无法恢复。若凭据泄漏，应同时处理运行中任务；此操作不撤回已经产生的副作用。
                    </p>
                    <label className="checkbox-label wide">
                      <input name="revoke_ack" type="checkbox" required />
                      我确认永久撤销这个 Key
                    </label>
                  </MutationForm>
                </details>
              </>
            )
          )}
        </>
      )}
    </section>
  );
}
function KeyEditor({
  current,
  models = [],
  denied,
  saved,
}: {
  current?: VirtualKey;
  models?: string[];
  denied: Denied;
  saved: (message: string) => void;
}) {
  const { owner } = useContext(PendingConfig);
  const [secret, setSecret] = useState<Awaited<
    ReturnType<typeof mintKey>
  > | null>(null);
  const [busy, setBusy] = useState(false),
    [show, setShow] = useState(false),
    [message, setMessage] = useState("");
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  async function generate() {
    if (busy || owner) return;
    setBusy(true);
    setMessage("");
    try {
      const minted = await mintKey();
      if (alive.current) {
        setSecret(minted);
        setShow(false);
      }
    } catch {
      if (alive.current)
        setMessage(
          "无法生成密钥，请确认使用可信 HTTPS 和支持 Web Crypto 的浏览器。",
        );
    } finally {
      if (alive.current) setBusy(false);
    }
  }
  async function copy() {
    if (!secret) return;
    try {
      await navigator.clipboard.writeText(secret.token);
      if (alive.current)
        setMessage(
          "已复制密钥，请存入受控凭据库。系统剪贴板可能被其他应用读取。",
        );
    } catch {
      if (alive.current)
        setMessage(
          "复制未完成，可显示明文后手动保存；不要提交到聊天或代码仓库。",
        );
    }
  }
  return (
    <>
      {!current && (
        <section className="secret-panel" aria-label="一次性密钥">
          <p>
            浏览器使用 256
            位安全随机数生成密钥，仅向服务器提交摘要。请先保存，再创建；刷新、退出或离开页面后不可恢复。页面隐藏时也会清空。
          </p>
          <button
            disabled={busy || owner !== null || secret !== null}
            onClick={() => void generate()}
          >
            {busy ? "正在安全生成…" : "生成一次性密钥"}
          </button>
          {secret && (
            <>
              <label>
                待保存密钥
                <input
                  type={show ? "text" : "password"}
                  readOnly
                  value={secret.token}
                  autoComplete="off"
                  spellCheck={false}
                />
              </label>
              <div className="config-actions">
                <button onClick={() => setShow((v) => !v)}>
                  {show ? "隐藏密钥" : "显示密钥"}
                </button>
                <button onClick={() => void copy()}>复制密钥</button>
              </div>
            </>
          )}
          {message && <p role="status">{message}</p>}
        </section>
      )}
      {(current || secret) && (
        <MutationForm
          path="settings/apply"
          denied={denied}
          saved={saved}
          review={keyReview}
          build={(f) => ({
            kind: current ? "key_update" : "key_create",
            ...(current
              ? { id: current.id, expected_revision: current.revision }
              : { key_hash: secret!.hash, key_prefix: secret!.prefix }),
            name: value(f, "name"),
            budget_cny: decimal(value(f, "budget_cny"), 6, 1_000_000_000n),
            expires_at: expiry(
              value(f, "expires_at"),
              !!current && !current.expires_at,
            ),
            models: modelGrants(value(f, "models")),
          })}
        >
          <label>
            Key 名称
            <input
              name="name"
              required
              maxLength={200}
              defaultValue={current?.name}
            />
          </label>
          <label>
            人民币预算（元）
            <input
              name="budget_cny"
              required
              inputMode="decimal"
              defaultValue={current?.budget_cny ?? "0"}
            />
          </label>
          <label className="wide">
            有效期（RFC3339，含时区）
            <input
              name="expires_at"
              required={!current || !!current.expires_at}
              defaultValue={
                current
                  ? (current.expires_at ?? "")
                  : Temporal.Now.instant()
                      .add({ hours: 30 * 24 })
                      .toString({ smallestUnit: "second" })
              }
              placeholder="2026-12-01T00:00:00+08:00"
            />
          </label>
          <label className="wide">
            允许的模型 ID（每行一个，最多 100 个）
            <textarea
              name="models"
              required
              rows={4}
              defaultValue={models.join("\n")}
            />
          </label>
          <p className="wide muted">
            预算最多 6 位小数，不能低于实时已花费加预留，也不能超过组织总预算。0
            元表示不能准入有费用的请求。有效期不超过未来 366 天；可查询下方模型
            ID。
          </p>
          {!current && (
            <label className="checkbox-label wide">
              <input name="saved_ack" type="checkbox" required />
              我已将密钥保存到受控凭据库，理解创建后不可重新查看
            </label>
          )}
        </MutationForm>
      )}
    </>
  );
}
export function Fx({ denied }: { denied: Denied }) {
  const [owner, setOwner] = useState<string | null>(null);
  return (
    <PendingConfig.Provider value={{ owner, setOwner }}>
      <FxBody denied={denied} />
    </PendingConfig.Provider>
  );
}
function FxBody({ denied }: { denied: Denied }) {
  const { owner } = useContext(PendingConfig);
  const [before, setBefore] = useState<string | null>(null),
    [epoch, setEpoch] = useState(0),
    [message, setMessage] = useState("");
  const result = useQuery(
    "settings/fx/query",
    fxSchema,
    { limit: 20, before },
    epoch,
    denied,
  );
  return (
    <section className="management" aria-label="人民币汇率管理">
      <p>
        人民币是预算、账本与统计的本位币。填写 1
        美元可兑换的人民币数值；人民币报价直接结算，美元报价采用请求准入时的汇率版本。
      </p>
      {message && (
        <p role="status" className="notice">
          {message}
        </p>
      )}
      <button disabled={owner !== null} onClick={() => setEpoch((v) => v + 1)}>
        刷新汇率
      </button>
      <QueryState {...result} />
      {result.data && (
        <>
          <div className="config-panel">
            <h2>当前生效汇率</h2>
            {result.data.current ? (
              <>
                <p className="fx-current">
                  1 USD = {result.data.current.usd_cny} CNY
                </p>
                <p>
                  版本 {result.data.current.id} · 来源{" "}
                  {result.data.current.source}
                </p>
                <p>
                  生效于{" "}
                  {time(result.data.current.effective_at, "Asia/Shanghai")}
                  （北京时间）
                </p>
              </>
            ) : (
              <p className="notice">
                尚无生效汇率，美元报价的请求不能准入；人民币报价不受影响。
              </p>
            )}
            <p className="muted">
              查询时点：{time(result.data.observed_at, "Asia/Shanghai")}
              （北京时间）。到达计划生效时间后请刷新核对。
            </p>
          </div>
          <h2>版本记录（按发布编号倒序）</h2>
          <ol className="record-list" aria-label="汇率版本">
            {result.data.data.map((r) => (
              <li key={r.id}>
                <div className="record-heading">
                  <strong>
                    版本 {r.id} · 1 USD = {r.usd_cny} CNY
                  </strong>
                  <span>
                    {Temporal.Instant.compare(
                      r.effective_at,
                      result.data!.observed_at,
                    ) > 0
                      ? "待生效"
                      : r.id === result.data!.current?.id
                        ? "当前生效"
                        : "历史版本"}
                  </span>
                </div>
                <p>{r.source}</p>
                <p>{time(r.effective_at, "Asia/Shanghai")}（北京时间）</p>
              </li>
            ))}
          </ol>
          {!result.data.data.length && <p>没有汇率版本。</p>}
          <div className="config-actions">
            <button
              disabled={!before || owner !== null}
              onClick={() => setBefore(null)}
            >
              返回最新版本
            </button>
            <button
              disabled={!result.data.next_before || owner !== null}
              onClick={() => setBefore(result.data!.next_before)}
            >
              下一页汇率
            </button>
          </div>
          {result.data.can_write ? (
            <details className="config-panel">
              <summary>发布新的人民币汇率</summary>
              <MutationForm
                key={epoch}
                path="settings/apply"
                denied={denied}
                saved={(m) => {
                  setMessage(m);
                  setBefore(null);
                  setEpoch((v) => v + 1);
                }}
                review="汇率不可覆盖或删除。空生效时间表示服务端提交时生效；未来版本仍会按原定时间生效，不会被本次即时发布取消。已有预留、账单和存量请求的快照不变。请核对方向：1 USD = 所填数值 CNY。"
                build={(f) => ({
                  kind: "fx_publish",
                  expected_latest_id: result.data!.latest_id,
                  usd_cny: decimal(value(f, "usd_cny"), 10, 1_000_000n, true),
                  source: value(f, "source"),
                  effective_at: expiry(value(f, "effective_at"), true),
                })}
              >
                <label>
                  1 USD 兑换人民币（CNY）
                  <input
                    name="usd_cny"
                    required
                    inputMode="decimal"
                    placeholder="例如 7.2500000000"
                  />
                </label>
                <label>
                  汇率来源
                  <input
                    name="source"
                    required
                    maxLength={200}
                    placeholder="合同版本或财务确认记录"
                  />
                </label>
                <label className="wide">
                  计划生效时间（留空立即生效）
                  <input
                    name="effective_at"
                    placeholder="RFC3339，例如 2026-12-01T00:00:00+08:00"
                  />
                </label>
                <p className="wide muted">
                  最多 10 位小数、大于 0 且不超过 1,000,000。仅可发布未来 366
                  天内的计划；不自动获取市场实时汇率。
                </p>
              </MutationForm>
            </details>
          ) : (
            <p>只读模式。发布需 fx_read 与独立 fx_write 权限。</p>
          )}
        </>
      )}
    </section>
  );
}
