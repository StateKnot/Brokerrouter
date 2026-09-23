# MCP 2026-07-28 与 Skills 接入契约

状态：2026-09-23 已完成 M6.2a 安全契约、[M6.2b1 连接目录](m6-mcp-connection-catalog.md)、[M6.2b2a HTTP 上游发现](m6-mcp-http-discovery.md)、[M6.2b2b1 受管 stdio](m6-mcp-stdio-discovery.md)、[M6.2b2b2a 下游目录](m6-mcp-downstream-catalog.md)、[M6.2b2b2b OAuth 资源认证](m6-mcp-oauth-resource.md)、[M6.2b2c 持久治理工具调用](m6-mcp-governed-tool-calls.md)、[M6.2c1 Skill 元数据目录](m6-mcp-skill-catalog.md)、[M6.2c2a 直接 URI 元数据查询](m6-mcp-skill-get.md)和 [M6.2c2b 真人 Skill 审批](m6-mcp-skill-approval.md)。`POST /mcp` 已开放受保护目录与单次审批后 `tools/call`；MRTR、Skill 激活/内容读取、Hermes/OpenClaw 恢复及官方认证仍未完成，因此不宣称完整 MCP/Skills 兼容。

## 规范基线

实现固定以下官方稳定基线，并在每次依赖升级时重新执行规范差异评审：

- [MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28)：无连接级会话的最新稳定核心协议。
- [Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)：单一 POST 端点、逐请求元数据、标准镜像头、JSON/SSE 响应与 Origin 校验。
- [Authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)：HTTP 资源服务器的 OAuth 2.1 发现、资源指示和受众约束。
- [server/discover](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)：协议版本与能力发现。
- [Skills 扩展](https://modelcontextprotocol.io/extensions/skills/overview)，扩展标识 `io.modelcontextprotocol/skills`；格式遵循 [Agent Skills](https://agentskills.io/specification)。
- [官方 Rust SDK](https://github.com/modelcontextprotocol/rust-sdk) `rmcp 3.4.0` 与 [官方一致性套件](https://github.com/modelcontextprotocol/conformance)。

上游发现、OAuth 和标准头已在 M6.2b2a 固定采用当前发布版 `rmcp 3.4.0`，不在 Brokerrouter 内复制协议状态机。下游 server 与 MRTR 继续复用同一 SDK。Skills 扩展在官方 SDK 提供同等能力前由本仓库实现扩展类型和安全门；升级 SDK 时必须检查其 Skills 支持并删除重复代码。

## 现状结论

评审前的 Brokerrouter 是 OpenAI 兼容模型网关和治理服务，不是 MCP client、host、server 或 intermediary。已有 turn、节点、护栏、真人审批、人民币账本和审计事实可以复用，但不存在协议线和 Skill 信任边界。因此“支持智能体治理”不等于“兼容 MCP”。

| MCP 2026-07-28 要求 | 评审前状态 | M6.2a 状态 |
| --- | --- | --- |
| `server/discover`、逐请求 `_meta`、`resultType` | 未实现 | 未实现 |
| 无状态 Streamable HTTP `/mcp`、Origin/Accept/标准头校验 | 未实现 | b2b2a 已实现目录入口 |
| OAuth 2.1 资源服务器/客户端、issuer/resource/audience 绑定 | 未实现 | 下游 JWT Protected Resource 与上游 client credentials 已实现；真实 IdP 认证待 LIVE-22 |
| MRTR `input_required` 暂停与恢复 | 只有 Brokerrouter 自有审批协议 | 未实现协议映射 |
| 上游 MCP server 注册、宿主分配身份和能力快照 | 未实现 | M6.2b1 已持久化；实际发现连接待 b2 |
| `skills/list`、`skills/get`、`resources/read` | 未实现 | `skills/list` 分页发现及直接 `skills/get` 元数据查询已实现；内容读取未实现 |
| 完整 manifest、512 文件/16 MiB、逐文件 size/SHA-256 | 未实现 | 内部校验已实现 |
| `(host server id, URI)` 身份与名称冲突隔离 | 未实现 | 内部身份/冲突检测已实现 |
| 内容按需读取、未列文件拒绝、frontmatter 逐字段核对 | 未实现 | 内部读取校验已实现 |
| 完整资源集合绑定审批，增删改后撤销 | 未实现 | 逐轮次真人审批事实与当前 manifest 失效检查已实现；内容使用门待 M6.2c2c |
| 动态 Skill、`allowed-tools`、嵌套和跨源读取安全门 | 未实现 | 动态 Skill 默认拒绝；工具授权被标记，执行门待 M6.2c |
| 官方 core/client/server/extension 一致性测试 | 未执行 | 待真实端点完成后执行 |

当前准确表述是：**Skills 安全内核、元数据发现、受管上游发现、下游目录和持久治理的单次工具代理已实现；Skill 激活、MRTR、Runtime 恢复与官方认证未完成，因此尚不是完整 MCP 网关。**

## 双向网关边界

Brokerrouter 同时承担两个独立角色，不能把两侧身份或授权直接透传：

1. 面向 Hermes/OpenClaw 等 Runtime，是受 OAuth 保护的 MCP server/intermediary；每次请求重新验证真人、Runtime、turn 和 client capability。
2. 面向外部 MCP 服务，是 MCP host/client；每个上游由 Brokerrouter 分配不可变 `server_id`，不使用对方自报的 `serverInfo.name` 做安全决定。
3. 模型/工具/资源/Skill 的调用都进入既有逐节点治理。批准只解除指定 turn、指定来源、指定内容摘要和指定动作的治理门，不代表工具已经执行成功。
4. 上游凭据仅由受限服务身份解密；Runtime、模型和控制台永远看不到上游 bearer token。下游用户 token 不转发给上游 MCP 服务。

## Skills 安全规则

M6.2a 的 `src/mcp.rs` 固定以下不变量：

- 只接受完整静态 manifest；动态 Skill 在首期生产策略中拒绝，因为其内容无法绑定持久审批。
- 接受 `skill://` 及满足结构约束的领域 URI，但不因 URI scheme 自行认定资源是 Skill。
- Skill 身份始终为 Brokerrouter 分配的 `server_id` 加完整 URI；名字仅用于展示和冲突提示。
- manifest 必须非空、包含根 `SKILL.md`、资源 URI 唯一且位于根目录内，最多 512 个文件、总计最多 16 MiB。
- SHA-256 必须为 64 位小写十六进制。资源在使用时按原始字节长度和摘要验证，不在发现或审批时批量预取。
- `SKILL.md` 的 YAML frontmatter 与发现结果逐字段相等；名称、描述和 Agent Skills 可选字段按规范校验，未知字段原样保留。发现对象和 YAML 头另设 128 KiB 解析上限，避免不可信元数据消耗无界内存。
- 审批指纹绑定服务器身份、Skill URI 和完整排序后的 `{resource URI, digest}` 集合。任何来源、增加、删除或替换都会产生新指纹。
- 未列资源、目录读取中新出现的资源、路径逃逸或跨源资源一律拒绝；刷新 entry 后必须重新审批。
- `allowed-tools` 只表示远端请求权限，不能自动扩大工具、文件系统或代码执行权限。
- 嵌套 Skill 作为普通支持文件读取；若要激活，必须创建独立审批。跨源读取必须逐次列出源与目标并由真人批准。

## 后续开发包

### M6.2b：MCP 连接与能力目录

- b1 已完成租户连接注册、HTTPS/stdio profile、宿主身份、加密 credential reference、OAuth 三元绑定、revision 和不可变能力快照；详见 [连接目录契约](m6-mcp-connection-catalog.md)。
- b2a 已固定 `rmcp 3.4.0` 实现 2026-07-28 Streamable HTTP 上游 `server/discover`、Secret/OAuth 与出站边界；详见 [上游发现契约](m6-mcp-http-discovery.md)。
- b2b1 已实现受管 stdio launcher 和真实 Discover lifecycle；详见 [stdio 发现契约](m6-mcp-stdio-discovery.md)。
- b2b2a 已暴露单一 `/mcp` POST，完成 Host/Origin/标准头、虚拟 Key、描述摘要授权和租户工具目录；执行在治理恢复凭据完成前失败关闭。
- b2b2b 已接入 OAuth JWT 资源认证，不把下游 token 转发到上游。
- b2c 已实现逐调用 turn/callId、输入/输出护栏、持久租约/回执及上游单次执行；MRTR/task 响应只持久封存，尚未提供协议恢复。
- 客户端和服务器 core 一致性测试必须全部通过；版本升级必须重新跑一致性与安全回归。

### M6.2c：Skill 注册、真人审批和隔离缓存

- M6.2c1 已完成 `skills/list` 客户端分页、TTL/cacheScope、静态 manifest 校验和不可变持久快照；M6.2c2a 已完成 `skills/get` 直接查询；M6.2c2b 已完成逐轮次真人审批，`resources/read` 仍待后续包。
- entry、资源验证事实和审批决定写入 PostgreSQL；manifest 变化在同一事务内撤销旧批准。
- 内容按需读入只写主机专用不可变缓存，路径编码 server identity，排除所有本地 Skill 扫描目录。
- 接入现有审批工作台：展示来源、URI、完整资源变化、`allowed-tools`、代码执行/跨源请求；Agent 不能代批。

### M6.2d：MRTR、Runtime 原生恢复与正式认证

- 将 `input_required` 映射到现有持久暂停点；响应丢失、进程终止和多副本恢复不得重复产生外部副作用。
- Hermes/OpenClaw 使用同一治理事实，不为不同 Runtime 复制批准语义。
- 执行官方 core server/client 套件、Skills 扩展用例、OAuth/SSRF/DNS rebinding、名称碰撞、提示注入、缓存篡改、嵌套/跨源读取和撤权竞态测试。
- 只有官方套件、预生产 OAuth/HTTPS、权限矩阵、备份恢复、压测和 Runtime 故障演练全部通过后，才可标记 MCP 2026-07-28/Skills 可用。

## 当前验证

M6.2a 新增 7 项单元测试，覆盖静态 manifest、内容和 frontmatter 篡改、动态 Skill 拒绝、来源/资源集合变更后的审批失效、Agent Skills 结构与扩展上限、跨来源同名冲突、领域 URI 与路径逃逸，以及不可信 `_meta` 不得授权工具或改变审批身份。

本地验证完成 `cargo fmt --check`、Clippy `-D warnings`、release 构建；隔离 PostgreSQL 全量回归与最后的 MCP/Skills 复验合计覆盖 204 项唯一常规用例，全部通过，0 失败。4 项既有 MinIO/固定 ClamAV/FFmpeg 专项按仓库规则由 CI 显式执行。以上验证安全内核和历史回归，不替代 wire 或官方一致性认证。
