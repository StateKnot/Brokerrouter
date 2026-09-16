# M5.4a：本人审批 BFF 与安全 JSON 审阅

契约 `human-approval-workbench-v1`，2026-09-16。交付 Rust 独立审批服务、定向 OIDC BFF 和 TypeScript 本人审批工作台。它是真实 API，不是模拟弹窗；生产 IdP/MFA、通道和运行环境的认证仍在 LIVE-19。普通 SDK 或未适配 Hermes/OpenClaw 不会因为新增网页就自动获得暂停/恢复功能。

## 权限与界面范围

`serve-approvals` 默认监听 `127.0.0.1:8090`，仅接受 `br-approvals+jwt`；独立 key/audience，不能使用 console/catalog/settings/human 的证明代替。部署角色 `brokerrouter_approvals` 能读本人审批服务需要的治理事实、解密受控 JSON、写审阅元数据/决定/同轮豁免及审计；不能读虚拟 Key 摘要或资产块，不能新建 turn、写 claim/执行回执、改模型/策略/预算或取得其他服务角色。审批服务的结果解密 keyring 不进入 BFF、浏览器或 Agent。

授权来自治理用户与 turn 的不可变本人归属，不来自 console 管理权限。登录绑定必须预先登记；不能使用邮箱认领、任意浏览器 tenant/user、管理员报表权限或 Agent Key 替用户审批。一个组织的管理员并不因此能批准其他人的任务。后端查询/审阅/决定均按签名用户和租户过滤；审阅与新决定还检查活跃 Runtime 绑定、Key、turn，决定继续复核模型授权与实际护栏集合。

列表仅选取元数据，不选取风险原文、反馈或护栏规则。用户主动点击“审阅风险原文”后，服务器完整解密并核对运行时的 `(turn, model, tool, payload)` 摘要，再把不超过 128 KiB 的 JSON **原始字符串**交给浏览器。页面不把它 parse/stringify，避免大整数改变；只用文本节点显示，不渲染 HTML、Markdown、脚本、链接、图片、播放器或自动下载。工具 JSON 审批的含义是审批所展示的参数/结果，不是证明引用资产内容已经审核。

当前可放行：文本 Chat `pre/post`（文本 messages/choices）与工具 `tool_input/tool_output` JSON。超大/已清理的内容不截断、不重新生成，不能继续；允许本人明确拒绝/反馈。`artifact` 只有描述而非真实二进制，本页不允许继续；`media_output` 的整个决定表单及服务端决定入口都禁用，必须等待专用隔离预览工作包，不能拿 JSON 描述冒充图片/视频/3D 审阅。

页面没有预选决定或预选护栏。用户必须选择意见、逐个选择要跳过的护栏、勾选范围确认，再核对冻结的决定并二次提交。展示护栏 ID/版本、检测失败标记、操作/轮次/会话/运行 ID、内容与上下文摘要、审批和审阅到期时间。所选护栏只在当前 turn、当前版本及批准有效期内豁免，可能影响本轮后续动作；不扩张工具权限、Key 模型授权、人民币预算或新 turn。

## 请求协议

浏览器路径 `/bff/v1/approvals/*`，后端 `/approvals/v1/*`，只接受 POST JSON；不提供任意 URL/路径代理、创建 turn、claim、receipt、reconcile 或签名令牌下载。

| 后缀 | 正文 | 结果 |
| --- | --- | --- |
| `query` | `limit` 1–50、`pending_only` 布尔；可选 `turn_id` 与 `after:{created_at,id}` | 本人倒序列表，微秒时间/UUID 游标，`next_after`；审批状态和执行状态分开 |
| `review` | `approval_id` | 待处理、未过期、当前快照的审阅；`review_id`、到期、`payload_text` 或明确不可用原因、`can_continue` |
| `decide` | `approval_id`、`review_id`、`acknowledged_scope:true`、嵌套 `decision` | 原底层审批回执；不会执行节点 |

`decision` 为 `{"decision":"continue_this_turn","approved_guardrail_ids":["rail-id"],"feedback":null}`，或 `reject` + 空列表/null，或 `feedback` + 空列表和 1–4000 UTF-8 字节意见。总正文 16 KiB；最多 256 个唯一护栏。不能同一审批改用不同决定。

审阅凭据不可变、绑定租户/本人/审批/操作摘要/护栏快照，最长 5 分钟且不超过审批和 turn 到期时间。它是“服务已授权并交付指定审阅内容”的凭据，不能证明人类实际读完；受控终端、明确交互与 MFA 是外层责任。凭据自身不授予任何跳过权。重新审阅创建新的元数据记录，不复活过期凭据；`review` 读写及 `approval.reviewed` 审计同事务提交。

新决定在持有 tenant→审批→操作/turn 锁的同一事务中校验审阅、有效期、模型/Key/Runtime、当前规则快照，提交审批、所选版本豁免、反馈/关轮及审计。规则集合改变返回 409，Runtime 需重新检查原操作。只有 read/write 完成后证明仍有效才提交；事务无供应商或网络调用。已有同一决定回执可重复返回，不重新发豁免，不能被过期审阅变成第二次执行。

`continue_this_turn` 记录批准，但操作仍等待 Runtime 重新 check/claim；部分批准可能触发剩余护栏的新申请。`reject` 拒绝该操作并清理受保护结果；`feedback` 关闭旧轮次，意见由 Runtime 按既有治理协议读取，新任务必须创建新 turn。未知执行副作用只能走独立核对协议，本页不能强制重跑。

## 失联、撤权与敏感信息

沿用 OIDC/PKCE、同源 CSRF、Secure Cookie、短时请求签名、固定目标、禁重定向和禁自动 HTTP 重试。BFF 请求前、响应交付前各检查持久会话。已签发且进入审批事务的在途决定可能先于并发 logout 生效；退出/关闭网页不是撤销已接受决定，也无法收回已显示字节。不能宣称跨服务全局原子取消：响应丢失或会话失效后，重新登录并查询同一审批记录确认，不创建新的外部动作。

提交后按钮冻结相同正文，网络/5xx/协议失败仅允许人工同内容重试；409/410 等明确冲突停止重试。服务若已提交会返回原回执，未提交则再次完整校验。原文只在内存，审阅到期、关闭审阅、切页、隐藏页面、会话失效/退出都会清除；不放进 URL、storage、遥测、错误消息或导出文件。离开/刷新可能丢失待核对正文，必须先看当前审批状态，不把内存清理当作取消事务。

`approval.reviewed` 和 `approval.decided` 审计不含正文或反馈，保留本人、操作/审批/审阅 ID 与状态/摘要；审计视图仅开放已允许的本人和操作 ID 元数据。完整审计事实由受控审计存储归档，审阅表无原文、不可修改/删除/清空。服务每 30 秒使用既有有界维护清理过期签名断言和超期加密反馈；审阅/审批决定属于长期事实，不自动删后允许重放。生产留存/归档和容量阈值需部署验收。

单服务并发上限 8、正文 16 KiB、HTTP 15 秒；query/review SQL 5 秒、锁 2 秒，决定沿用受控 DB 连接的有界等待。BFF 上游 12 秒、外层 40 秒、响应 2 MiB、证明最长 30 秒；超时即可能失联，不假定决定没生效。禁止记录请求/响应 body、Authorization、Cookie；审阅服务仅接受私网 BFF 入站，不向供应商或媒体源发请求。

## 部署与回滚

1. 停写/排空并核对备份、审批和未知执行清单。迁移账号应用 `0017`，DBA 应用 `deploy/permissions.sql`；给审批进程新建仅属于 `brokerrouter_approvals` 的 TLS 登录，不附加 runtime/control/operator/scanner/存储角色。生产先验证索引构建窗口、历史规模及恢复。
2. `serve-approvals` 注入 `BROKERROUTER_APPROVALS_ISSUER={BFF_ORIGIN}`、`BROKERROUTER_APPROVALS_AUDIENCE`、`BROKERROUTER_APPROVALS_PUBLIC_KEYS={kid:RSA公钥PEM}`、专用 `DATABASE_URL` 及原结果加密 keyring 的合法解密版本。私钥不得注入本服务。
3. BFF 全量配置四个可选变量：`BROKERROUTER_BFF_APPROVALS_ORIGIN`（精确 HTTPS origin）、`BROKERROUTER_APPROVALS_AUDIENCE`、`BROKERROUTER_BFF_APPROVALS_SIGNING_KID`、`BROKERROUTER_BFF_APPROVALS_SIGNING_PRIVATE_KEY`。不能只配置部分；RSA 模数和 audience 不能与 console/catalog/settings 相同，审批公钥也不得配置为 HUMAN/模型服务可信公钥。未配置时入口明确不可用，不降级为其他签名。
4. 所有服务与静态页面同版部署，精确 schema readiness 必须通过。新增目标进入配置指纹，旧会话需重新登录。部署边界使用既有 HTTPS edge，同源不承载用户上传内容；加密/解密密钥、DB 登录和日志权限分离。
5. 先安装新公钥，再轮换 BFF 私钥/kid；保持旧公钥直到 30 秒证明和滚动窗口过去。撤销本地治理用户/Runtime/Key 应立刻阻止新审阅/新决定；真实 IdP 停用需要同步本地成员状态，不能只依赖 IdP Cookie。高风险业务 IdP 强制 MFA/受控终端，不声称本包实现了任意 IdP 的 step-up。
6. 在独立租户做零费用确定性工具操作验证，包括跨用户/组织、重复决定、事务撤权/过期、响应丢失、当前版本变化、反馈/新轮次及事件恢复，再验证生产等价体量/SLO/审计增长、人工读屏和完整 Runtime。没有必要先要供应商 Key。
7. 回滚先冻结入口并核对已接受决定、同轮 grants 与外部执行。优先向前修复；恢复旧库/旧二进制必须共同对应同一恢复点，并对账之后发生的审批/成本/外部副作用，禁止只降级 UI 或删除新审阅事实。

尚未开发：媒体独立预览与媒体审批 BFF、Hermes/OpenClaw 原生交互和恢复、逐节点任务/调用/经验/评测页面、专用多模态审核、图片/3D/其他生成协议、SSE/Responses。本包不调用付费模型、不改变人民币账本或生产数据。验证记录见里程碑与 LIVE-19。
