# MCP Skill 真人审批（M6.2c2b）

状态：2026-09-23 实现逐轮次、来源和完整静态 manifest 绑定的真人审批事实及工作台。**本阶段不提供 `resources/read`、内容缓存、Skill 激活或 `allowed-tools` 授权。**批准仅意味着未来校验路径可检查这条事实，不能直接执行 Skill。

## 生产安全契约

- Runtime 使用其虚拟密钥和当前活跃、已绑定真人的 turn 调用 `POST /v1/governance/skills/requests`，提供随机 `request_id`、`turn_id`、`server_id` 与完整 `uri`。URI 是 MCP 方法标识，不作为 URL 抓取。申请数据只取自该服务器当前版本最新、尚未过期的直接 `skills/get` 观察，Runtime 不能自报 manifest 或摘要。每轮最多 128 条申请；同一 ID/相同来源可幂等重试，不同内容冲突。
- 请求事实冻结租户、虚拟密钥、用户、turn、服务器、配置版本、观察 ID、完整 entry 的 SHA-256、审批指纹与私有授权上下文摘要。审批有效期不超过原 turn。数据库触发器禁止修改绑定或终态决定。申请与不可修改的 turn 事件同事务提交；真人审阅/决定另与审计事实同事务提交。Runtime 不取得全局审计表写权限，以维持既有对账权限边界。
- 真人通过独立 purpose-scoped `br-approvals+jwt`、BFF 会话与 CSRF 调用 `/bff/v1/approvals/skills/query|review|decide`。查询只展示本人元数据；review 才显示完整 entry，并生成 5 分钟、本人专属且不可改写的审阅回执。浏览器将不可信元数据按纯文本展示，不运行 HTML/链接；不预选批准。
- 决定必须引用回执并确认作用范围。支持 `approve_this_turn`、`reject`、`feedback`；反馈加密存储并关闭原 turn。审批前再次验证用户、密钥/绑定、turn、活动服务器版本和最新直接观察。刷新后的观察 ID 可以变化，但完整 entry、资源集合、指纹及授权上下文必须逐项完全相同；否则失败关闭并需新申请。
- Runtime 使用 `GET /v1/governance/skills/requests/{id}` 轮询。仅当前密钥/turn 可读取，`approved` 也仅在当前最新观察仍新鲜且与批准完全一致时返回；否则为 `stale` 或 `expired`。查询不返回 manifest、文件内容或上游凭据。没有任何路由会因该批准读取文件或授予工具。
- 数据库身份分离：Runtime 只能提交申请和读取自己服务所需事实，不可决定；审批服务只能审阅/决定，不能代发申请；下游 MCP server 无权读这两张表。权限矩阵由隔离 PostgreSQL 测试验证。

## 部署与使用

部署时按既有迁移流程应用 `0027_mcp_skill_approvals.sql`，重放 `deploy/permissions.sql`，再升级 Runtime、审批服务、BFF 和控制台。四个服务必须使用各自数据库/签名身份；不得将 approvals 端口暴露给智能体。原有环境变量与 BFF OIDC 配置不变。控制台「本人审批」新增独立的「Skill 审批」页签。

直接 URI 观察 TTL 是发现 TTL 和 Skill TTL 的较小值。真人审阅或决定遇到 `policy_changed` 时，由受管 MCP client 重新运行 `mcp-skill-get`；内容完全一致才可继续同一审批，否则申请新的审批。旧批准即使曾成功，也不会在新 manifest 或过期观察上被认作可用。

## 下一包

M6.2c2c 实现按需 `resources/read`、原始字节大小与 SHA-256 校验、完整 `SKILL.md` frontmatter 对照和不可变隔离缓存。读取门必须重新验证此审批事实，且不能把“已批准”视为自动激活或工具授权。随后 M6.2d 处理 MRTR 与 Hermes/OpenClaw 暂停恢复及官方一致性认证。
