# M5.3b：虚拟 Key 与人民币汇率配置

实现版本：`key-fx-management-v1`，2026-09-15。Rust 配置服务、OIDC BFF 定向代理和 TypeScript 页面。已实现不等于真实部署已认证；发布闸门为 LIVE-16。

## 权限与部署

新增 `brokerrouter serve-settings --bind 127.0.0.1:8089`。仅部署在受控内网，由可信 HTTPS edge/BFF 访问。使用只属于 `brokerrouter_settings` 的独立数据库登录，验证数据库 TLS；不得附带 runtime、operator、catalog、console、governance_control 或 BFF 组。

服务配置：`DATABASE_URL`、`BROKERROUTER_SETTINGS_ISSUER`（BFF 公开 HTTPS origin）、`BROKERROUTER_SETTINGS_AUDIENCE`、`BROKERROUTER_SETTINGS_PUBLIC_KEYS`（kid 到 RSA 公钥 PEM 的 JSON）。没有供应商、存储或审批凭据。仅可修改 Key 配置/授权和追加 FX，不可读 Key hash、调用正文、端点凭据、审批或修改消费账本。

BFF 可选配置（四项全缺省为关闭；部分配置会启动失败）：

- `BROKERROUTER_BFF_SETTINGS_ORIGIN`：固定 HTTPS origin，部署 edge 反代到 settings listener。
- `BROKERROUTER_SETTINGS_AUDIENCE`。
- `BROKERROUTER_BFF_SETTINGS_SIGNING_KID`。
- `BROKERROUTER_BFF_SETTINGS_SIGNING_PRIVATE_KEY`：Secret 注入 RSA 私钥。

签名用途固定 `br-settings+jwt`，30 秒证明绑定 tenant、真人、POST、精确路径和 body hash；与只读 console、catalog 的 audience 和 RSA 密钥不得相同。settings 公钥不得加入 HUMAN 审批信任集合。BFF 仅开放下述精确路径，不接受浏览器 Bearer token、任意目标 URL 或泛用签名请求。现有 Secure Cookie、同源/CSRF、登录撤销和响应后会话复核不变。升级后的 BFF 配置指纹要求重新登录。

显式管理员权限使用现有 `console-permission-set` CLI 授予：

| 操作 | 必须具备的权限 |
| --- | --- |
| Key 列表/详情/可授权模型 ID 查询 | `keys_read` |
| 创建/改名/预算/有效期/模型授权/撤销 | `keys_read` + `keys_write` |
| 汇率当前值与历史 | `fx_read` |
| 发布汇率版本 | `fx_read` + `fx_write` |

权限以组织为范围，授予组织管理员，不是终端用户默认权限。不能用 Key、Agent token 或普通 OIDC ID token 直接调用配置服务；新增模型不自动添加到现有 Key 授权。

## API 与一致性

浏览器前缀 `/bff/v1/settings`，服务前缀 `/settings/v1`，全部为 POST JSON：

| 后缀 | 参数 |
| --- | --- |
| `/keys/query` | `limit` 1–100、可选 `after` UUID、`search` 名称子串/完整 ID/前缀 |
| `/keys/detail` | `id` UUID |
| `/models/query` | `limit` 1–100、可选 `after` 模型 ID、`search` |
| `/fx/query` | `limit` 1–100、可选 `before` 版本十进制字符串 |
| `/apply` | `operation_id` UUID、`reason`、下述 `change` |

变更类型：

- `key_create`：`name`、`key_hash`（小写 SHA-256 hex）、`key_prefix`、`budget_cny` 字符串、明确 `models`、必填 `expires_at`。
- `key_update`：`id`、`expected_revision` 字符串、`name`、`budget_cny`、完整 `models`、`expires_at`。不能更换原密钥或解除撤销。
- `key_revoke`：`id`、`expected_revision`；不可撤回。
- `fx_publish`：`expected_latest_id`（首次为 null）、`usd_cny` 字符串、`source`、`effective_at`（null 为数据库提交阶段当前时间）。

回执只有 `operation_id`、`object_id`、`revision`。配置/授权、前后审计、真人/理由和幂等回执在同一事务中提交。相同真人/组织/编号/内容返回原回执，编号换内容返回 409。撤销后重试旧创建只返回历史回执，不能复活 Key。版本落后返回 409，必须重新读取核对；不存在不返回外租户对象信息。

写入按 `tenant → user → permission → key` 获取有界锁，tenant 与计费共用顺序，避免先持共享锁再升级。撤权/停用与写事务串行；过期证明在提交前再次用数据库时间检查。默认 5 秒语句/1 秒锁等待、10 秒 HTTP、8 并发、16 KiB 请求限制，BFF 不自动重试。浏览器复核阶段冻结内容和编号，禁止切换配置对象；失联只提供显式同编号重试，不把“未收到响应”说成回滚。

在途 BFF 请求可能先提交、后因登出而抑制回执。登出/关闭/隐藏页面不是服务端取消令；先查询 Key/汇率与审计，再决定下一步。移动端不保证 beforeunload 弹窗出现，正确性依赖服务端事务和幂等，不依赖离开提醒。

## 密钥生命周期

网页仅在可信安全上下文使用 Web Crypto `getRandomValues` 生成 32 字节随机数，格式与现有网关 `sk_br_` 一致。原文不进入请求、服务端回执、审计、浏览器存储或 URL；服务器保存认证用 SHA-256 和短前缀。API 的调用方必须同样使用密码学安全随机源，不允许用户口令/可预测种子；服务端无法从摘要验证原始熵或前缀与原文的对应关系，摘要注册接口只授权可信管理员 BFF。

用户须先保存密钥，再勾选确认并提交。复制必须点击按钮，且明确提示系统剪贴板风险；不自动下载文件或写本地持久存储。提交失败时明文留在当前内存便于核对，提交成功/退出/页面隐藏/离开后移除。JavaScript 字符串与操作系统剪贴板无法保证安全擦除，生产须限制扩展、录屏和第三方脚本，使用受控凭据库及可信终端；不声称浏览器内存是硬件保险库。

响应丢失后，页面未关闭可复用相同摘要与操作编号；明文丢失后不可恢复，只能查找刚创建的 Key，撤销后重建。创建时组织管理员可用名称/ID/前缀查找，独立 audit_read 可按操作编号对应的审计记录核对。不得为方便恢复而把明文塞进幂等表。网关调用时仍通过原有 hash 认证、租户预算、Key 预算和明确模型授权。

Key 预算是人民币累计上限，最多 6 位小数、不超过 10 亿元，且不能超过组织预算；不是充值，多个 Key 额度可重叠但共享组织硬上限。修改必须大于等于实时已花费加预留，不重置消费。0 元不允许有费用准入，但不封禁零费用操作；停用凭据使用撤销。已准入调用保留资金预留和原快照，不能因撤销误记退款。

新建需未来 366 天内有效期；既有有限期不能改成无限期，历史无限期可保留或添加期限。时间含明确 RFC3339 时区、最多微秒，不做静默精度舍入。最多 100 个不重复授权模型，支持停用模型授权但不自动启用。超过 100 授权的历史 Key 仅显示前 100 项并明确标注不可网页编辑，避免截断导致授权丢失；可撤销或由运维显式迁移。资产配额、轮换自动化、月度充值/周期预算不在本包开放范围。

数据库配置版本仅在配置列修改和授权增删时推进，不因消费/预留/资产字节变化推进；CLI 撤销/授权变更同样使旧表单失效。网页仍在事务内重新检查实时用量，不用页面显示值做资金判断。

## 人民币与美元

方向固定 `1 USD = X CNY`，人民币为唯一预算/账本/展示本位币。`X > 0`、不超过 1,000,000、最多 10 位小数；传输和计算使用十进制字符串/Decimal，不经过 JS 浮点数。人民币报价不乘汇率，美元报价无生效版本时拒绝准入。历史账本及请求快照不重算。

FX 只追加，不能更新或删除。可以立即生效或计划未来 366 天内生效，网页禁止回溯生效；CLI 仍保留原有运维发布合同。每次发布必须匹配已看到的最新发布 ID，CLI 也获取相同 tenant 锁，因此不会无提示覆盖并发决策。当前值按生效时间选择，不是直接取最大 ID；分页按发布 ID 倒序，另返回全局最新 ID/当前值/查询时点。

即时纠错不取消早已计划的未来版本。页面明确显示待生效版本并在复核阶段警告；真正生效时请刷新核对。未来版本取消/作废工作流未开放，不能宣称有隐藏取消接口。此页面是合同/财务配置，不自动同步市场汇率，配置成本仍不等于供应商实际付款账单。

## 上线、回滚与验收

1. 预生产备份并验证恢复；确认 Key/授权表规模、DDL 锁窗口、审计留存和关键租户写入竞争 SLO。
2. 排空旧二进制后迁移 0014，重新应用完整权限脚本（CLI operator 增加 grant 触发器需要的 revision 更新权限），创建专用 settings 登录和网络规则。不得授予额外表/角色。
3. 同步部署新二进制/BFF/前端与独立签名信任，重新登录，先给测试组织显式读权限，再配置少量写权限验收。Key 页面提供 0 元预算，可不调用付费供应商完成控制面认证。
4. 回滚先关闭 BFF settings 代理并撤销新写权限，保留已创建 Key、FX、审计和回执。不得删除版本、回退消费或复活撤销 Key；修复后前向发布。二进制严格校验迁移，不能直接回退到不认识 0014 的版本；数据库整库回退仅在已核对无新资金/作业事实的独立恢复流程中执行。

本地验证记录在 milestones；实际 IdP/MFA、TLS、跨副本、浏览器人工无障碍和生产规模仍在 LIVE-16，不能以 fixture 代替。继续开发的能力包括护栏/策略配置、护栏监控、审批与治理 UI、Runtime 适配和其余协议；它们不是本包“只差测试”的功能。
