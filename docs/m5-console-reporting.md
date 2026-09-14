# M5.1 控制台读取与人民币报表契约

实现档位：`console-reporting-v1`。本包是控制台的正式只读数据后端，不是 Web 页面、OIDC 登录服务或整个平台发布完成标记。所有结果从 PostgreSQL 中的请求、账本和审计事实查询，不读取采样 trace，不访问模型供应商。

## 服务与身份隔离

独立命令 `brokerrouter serve-console --bind 127.0.0.1:8086`；`/health/live` 为存活探针，`/health/ready` 检查全部迁移及 checksum。服务只需要自己的数据库登录凭据和受信 BFF 的 RSA 公钥，不得注入供应商密钥、模型结果/资产解密密钥、S3 凭据或 BFF 签名私钥。不得与 Agent 数据面或审批控制服务合并部署成共享万能身份。

认证采用 BFF 对本次只读操作签发的短期证明，不接受虚拟密钥、普通 OIDC ID Token、浏览器传来的自声明角色或现有 `br-human+jwt` 审批凭据：

- RS256，JWT `typ=br-console+jwt`，`kid` 必须命中静态信任的公钥集合。
- 独立 `BROKERROUTER_CONSOLE_ISSUER`（HTTPS）、`BROKERROUTER_CONSOLE_AUDIENCE`、`BROKERROUTER_CONSOLE_PUBLIC_KEYS`（kid → PEM 的 JSON）；与真人审批使用不同 audience，建议同时隔离签名密钥。
- Claims 结构复用 [human.rs](../src/human.rs) 的 `Claims`：准确绑定 `POST`、完整路径与原始 JSON 请求体 SHA-256；认证主体通过 `(issuer,subject,tenant_id)` 匹配已注册且有效的用户。
- `interactive=true`、证明有效期不超过 300 秒、用户认证时间不超过 1 小时；不根据模型消息或 Agent 传入的角色授权。同一个只读证明可在有效期内重放相同查询，但每次重查权限。
- BFF 必须先完成真实 OIDC 登录、会话与 CSRF/Origin 校验，再按服务端成员关系选租户和签名；不得为 Agent 暴露任意签名接口。此真实 BFF/登录集成尚未完成，不能把测试签名器当作生产 BFF。

控制台请求由数据库 `console_permissions` 显式授权：`usage_read` 可读租户内全部用量和请求元数据；`audit_read` 仅可读租户审计元数据。两者都不赋予修改配置、审批、执行或原文查看权限；Runtime 绑定和当前轮护栏豁免不隐含控制台权限。默认用户无权限。本包尚不提供开发者“只看自己的 Key”或团队级权限，不能给普通开发者授权租户全量读再用前端隐藏数据。

运维以独立 `brokerrouter_operator` 登录身份管理权限：

```sh
brokerrouter console-permission-set --tenant <TENANT_UUID> --user <GOVERNANCE_USER_UUID> \
  --permission usage_read --enabled true --reason '<工单或授权依据，不要填写密钥/业务原文>'
# 撤销时 --enabled false；audit_read 需另外明确授权。
```

权限变更与带前后状态、理由和数据库操作人身份的审计在同一事务提交；审计失败则权限不变，相同设置不重复生成变更记录。查询开始及数据库快照结束后分别检查有效租户/用户/权限/证明期限，查询期间发生的撤销会在释放响应前被拒绝。

## 数据库部署

1. 使用现有独立 schema owner/迁移身份应用 migration `0011_console_reporting.sql`。
2. 重新执行 [permissions.sql](../deploy/permissions.sql)。给一个独立、非超级用户、非 owner、非 BYPASSRLS 的登录身份仅授予 `brokerrouter_console`，不能加入 operator/runtime/control/reconciler 或其他用途组。
3. Console 身份只可读脱敏的 `console_request_facts` / `console_audit_facts` 视图，以及迁移和授权校验所需的有限表/列。不能读取源表中的密文、Key 哈希、价格/路由大 JSON、凭据引用或审计 `details`；更不能更新账本和权限。
4. 所有业务查询必须同时应用证明中的 tenant ID；视图提供用途/字段隔离，**不代表数据库已提供逐终端用户的 RLS**。数据库登录仅用于受控服务，不发给客户端。新增查询必须保留租户条件并运行跨租户回归。
5. 生产数据库使用验证服务器证书的 TLS，网关前设置独立内网/TLS 与 BFF 访问控制。权限组不清理 DBA 额外授权，部署前审查继承关系和直接授权。

迁移新增两个查询索引，普通建索引可能阻塞大型现有表；本项目仍使用明确维护窗口、排空和备份后的迁移发布，不宣称零停机 DDL。旧二进制会因迁移 checksum/版本集合不符而不就绪，不能混跑。生产数据规模上的索引计划、迁移时长、备份恢复和发布回滚待 LIVE-14/LIVE-07 验收。

## HTTP API

全部查询使用 POST，让筛选条件整体纳入 BFF 请求证明，避免把身份/业务筛选放入 URL；这些 POST 没有模型调用、收费或变更业务状态的副作用。响应包括失败均使用 `Cache-Control: no-store`、`nosniff`、禁止嵌入的 CSP 和 `no-referrer`。不开放 CORS，不用跨站浏览器直接持有证明代替 BFF。

| 接口 | 权限 | 返回 |
| --- | --- | --- |
| `/console/v1/usage/query` | usage_read | 同一快照的汇总、每日补零曲线、观察时间与明确计费口径 |
| `/console/v1/usage/groups/query` | usage_read | 按逻辑模型或虚拟 Key 的用量与费用、稳定游标分页 |
| `/console/v1/requests/query` | usage_read | 请求元数据、Key 别名、费用/预留/Token、状态/错误码及游标 |
| `/console/v1/audit/query` | audit_read | 审计 ID、时间、动作、对象和数据库操作人；不含任意 details |

用量查询示例（经 BFF 签名后发送，不含任何凭据）：

```json
{
  "filter": {
    "start": "2026-09-01T00:00:00+08:00",
    "end": "2026-09-08T00:00:00+08:00",
    "model_id": "logical/model"
  },
  "timezone": "Asia/Shanghai"
}
```

`filter` 还可包含 `key_id`、`request_id`、`status`、`purpose`。不接受 tenant ID；主体租户来自已验证证明。时间窗为 `[start,end)`，最多 31 天；timezone 只支持 `Asia/Shanghai`（默认）和 `UTC`，日界线使用数据库时区规则。状态枚举同持久请求表；purpose 为 `model` 或 `guardrail`，model 在这里包含普通模型与媒体生成费用，不等同于“仅文本”。

分组请求使用 `filter`、`dimension: model|key`、可选 `after` 与 `limit`（1–100，默认 50）；响应 `next_after` 非空时继续传回。请求日志用 `filter`、`limit`、可选 `before`（上一页的 `next_before`，包含 created_at 和 UUID）。审计查询用顶层 start/end、可选 action/object_id、limit、before。审计 ID 使用十进制字符串，上一页 `next_before` 可原样回传。精确筛选不做 SQL 字符串拼接，未知字段和非法枚举/时间窗直接拒绝。

单页内使用可重复读只读事务。游标采用时间+ID或对象 ID，不使用 offset；跨页是实时查询，不承诺长时间导出的冻结快照。新写入可能出现在下一次刷新，状态/费用可能因迟到结算变化；变更筛选条件时客户端必须丢弃旧游标。模型/Key 显示名取当前配置，原始逻辑 ID 不变。

## 指标口径

- `requests` 指已持久准入的逻辑请求，不是所有 HTTP 请求/attempt 数；鉴权、语法、预算准入前拒绝不在此表，纯工具动作和待审批操作也不冒充模型请求。
- 成功、失败、已核对、执行中和 `submission_unknown` 分列；后者不并入失败。重放已有请求不增加次数。
- 费用从唯一 `finalize` 账本项读取；未发送而释放的 failed 请求费用为已知零。`reserve/release` 不能相加当花费。未结算费用在请求日志中为 null，不是零。
- 汇总的 `known_cost_cny` 只表示已知入账金额，同时返回已知/未结算请求数量及 `reserved_cny`。平均费用 `mean_known_request_cost_cny` 的分母仅为费用已知请求（含已证实未发送的零元失败）；分母为零时为 null。不能把这个字段标成未知费用也已计入的“总平均费用”。
- 所有金额、Token、计数和审计 bigint ID 以字符串返回；金额在 Rust Decimal/PostgreSQL numeric 上计算，不经 JS 双精度累计。总额固定 6 位小数，均值展示最多 12 位，不用于反向生成账本。
- 总 Token 来自 finalize 中已有且合法的 input/output Token 证据；缺失证据的核对/媒体请求不伪造 Token 数，日志为 null，汇总同时返回 `token_usage_requests`。媒体单位量尚未聚合为图片张数/视频秒数/3D 数，不得把它们标为已覆盖。
- 时间筛选和每日归属均依据 request.created_at；已发生请求后续结算会更新原日的已知费用。不是账单结算日期报表，也不是供应商发票实付：返回 `cost_basis=configured_price_ledger_not_provider_invoice`。美元来源的费用已由原账本按其冻结汇率转换为人民币，不用当前汇率重算。
- 汇总和每日曲线在同一数据库快照计算；单服务并发上限 8、请求体 16 KiB、查询语句超时 5 秒、数据库锁等待 1 秒、HTTP 处理总 deadline 20 秒（含请求体读取）、响应分页最多 100 项，不允许无界导出拖垮账本数据库。前置代理还须配置连接总量与响应发送超时，限制慢接收客户端。生产容量仍需实测，必要时在不改变账本事实口径的条件下增加专门聚合存储。

## 本包验证与后续

新增 PostgreSQL/HTTP 回归包括签名类型/请求绑定、显式权限、撤销与查询竞态、跨租户与 SQL 注入式筛选、未知费用/稀缺 Token、日界线及空数据、分页、权限审计事务、真实受限 session 身份、超出 JS 安全整数的金额，以及真实 TCP HTTP 的 Body/CORS/撤销检查。完整本地测试结果写入 [里程碑](milestones.md)，没有再次使用真实供应商密钥。

另在上一轮专用真实联调数据库应用 0011，并以 `SET SESSION AUTHORIZATION brokerrouter_console` 验证脱敏视图：仍为 6 个请求、¥0.000408 已结算、0 预留、351 输入/48 输出 Token，与 [原联调记录](live-aliyun-2026-09-14.md)一致。只读取既有账本，没有再次调用供应商；此核对不代替真实 OIDC/BFF 的 HTTP 集成。

M5 后续：真实 OIDC/BFF/会话/CSRF、TypeScript 页面与权限展示、虚拟密钥/模型/端点/护栏/策略/汇率的受控写接口、护栏监控的完整事件口径、供应商/端点/团队/Agent 维度、媒体单位用量、任务治理与经验/评测样本。审计详情的显式脱敏与访问审计需要单独设计，本包只提供元数据。前次阿里云工具字段与 Embedding 端点限制缺陷仍保留在 LIVE-01，不因新增报表而关闭。
