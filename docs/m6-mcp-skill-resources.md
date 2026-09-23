# M6.2c2c：MCP Skill 资源按需读取

状态：2026-09-23 实现 **Streamable HTTPS 上游**的逐资源读取、安全校验与按轮次隔离密文缓存。该阶段没有 Skill 激活、`allowed-tools` 继承、跨上游读取、目录抓取或智能体自动审批；这些入口均保持关闭。

## 生产契约

- Runtime 先持有本轮真人明确批准的 Skill request。需要某个文件时，以同一虚拟 Key 提交 `POST /v1/governance/skills/requests/{request_id}/resources`，请求体为 `{"read_id":"UUID","turn_id":"UUID","uri":"manifest 中的完整 URI"}`；重复 `read_id` 只能重读相同绑定。每轮最多 1024 次读取、累计 manifest 预期字节最多 64 MiB。**发现、`skills/get`、审批和连接建立都不预取文件。**
- 公网 Runtime 只入队，独立 `mcp-worker` 才读取上游凭据。worker 发起 `resources/read` 前检查当前活跃 turn、真人决定、服务器 revision、最新直接 `skills/get` 的完整 entry 与授权上下文；读取后在提交前再检查一次。HTTP 强制 HTTPS、DNS/IP 固定、无代理/重定向，读取响应上限 24 MiB（含 Base64 包装）。stdio 子进程资源帧尚无可靠字节上限，因此此阶段拒绝 stdio Skill 内容读取。
- 对上游响应只接受 `complete`、单条且 URI 完全相等的 `contents`、合法 TTL/cacheScope；`input_required` 不自动应答。先按原始 UTF-8/解码后 blob 字节验证 manifest 中大小及 SHA-256，再对根 `SKILL.md` 的 YAML frontmatter 逐字段与保留 entry 比较。动态或未列资源拒绝。`ttlMs=0` 在异步持久缓存路径中失败关闭。
- 每次读取仅写入一条按 tenant/key/turn/request/server/revision/manifest/URI 绑定的密文缓存事实。AES-GCM AAD 包含租户与 `read_id`；触发器禁止覆盖已验证内容；返回时仍重新解密并校验密文摘要及完整 manifest。结果有效期是上游 `resources/read` TTL 与 turn 到期的较小值，到期由 worker 清除密文。轮次审批过期、Key 撤销、服务器或 manifest 变化会立即阻断返回，不等清理任务。
- `GET /v1/governance/skills/resources/{read_id}` 轮询 `queued`/`reading`/`ready`/`failed`；`ready` 才提供 `text` 或 Base64 `blob`，同时注明 `content_trust=untrusted`、`activation=disabled`、`tool_grant=none`。读取失败有稳定错误码；仅新 `read_id` 能重试，永不把不确定或不完整内容伪装成成功。Skill 内容不得放入本地 Skill 扫描目录。
- 数据库权限分离：Runtime 仅可提交和读取内容事实，不能写验证结果；worker 可领取和完成读取，但不能决定审批或查询审批反馈密文；审批服务不能读取缓存。审计仅记录请求、摘要、大小与错误码，不记录正文。

## 部署顺序和发布闸门

先应用 `0028_mcp_skill_resources.sql`，重放 `deploy/permissions.sql`，再升级 Runtime 与 `mcp-worker`。现有结果加密密钥须同时提供给两者，MCP 配置解密密钥及上游 Secret resolver 只给 worker；不要向 BFF、下游 MCP server 或模型进程分发上游凭据。旧版本不认识新路由，不应把目录元数据误认为可执行 Skill。

本地测试包括真实 2026-07-28 TCP `resources/read` 与 `_meta`、无预取，PostgreSQL 迁移/受限角色、批准前拒绝、未列 URI、密文与不可改写、返回时篡改/TTL 拒绝、manifest 更新后的读取与 worker 失败关闭。**尚待**受管 HTTPS+私有 CA、Bearer/OAuth 轮换及断线/租约的完整预生产联调；这些是开启 Skill 激活前的强制闸门。官方 MCP/Skills 一致性、MRTR 和 Hermes/OpenClaw 原生暂停恢复属于 M6.2d。

规范依据：[MCP 资源读取](https://modelcontextprotocol.io/specification/2026-07-28/server/resources)、[MCP Skills 扩展](https://modelcontextprotocol.io/extensions/skills/overview)。
