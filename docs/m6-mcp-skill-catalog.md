# MCP Skills 元数据目录（M6.2c1）

状态：2026-09-23 完成上游 `skills/list` 发现、静态 manifest 校验和不可变审计快照。本包**不激活 Skill、不读取资源、不授予 `allowed-tools`**；真人审批、`skills/get`、`resources/read` 和隔离内容缓存仍是 M6.2c2 的发布闸门。

## 协议与边界

- 仅在 `server/discover` 同时声明 `resources` 和 `io.modelcontextprotocol/skills` 后调用 `skills/list`。`rmcp 3.4.0` 负责连接、版本、标准头和 JSON-RPC；扩展方法以带完整 `_meta` 的自定义请求发送。
- 完整拉取分页，限制 128 页、1024 个 Skill、每响应 512 KiB、每条元数据 512 KiB、目录合计 8 MiB；拒绝循环游标、重复 URI、非 `complete`、非法 TTL/cacheScope。
- 只接受规范静态 manifest；动态 Skill、无根 `SKILL.md`、不完整/越界资源或非法 frontmatter 使整次发现失败。发现时不调用 `skills/get`、`resources/read` 或目录读取。
- 宿主分配的 `server_id` 和完整 URI 构成身份。批准指纹覆盖该身份与完整排序后的资源 URI/摘要集合。`_meta` 和自报 `serverInfo` 不授予任何权限。
- `mcp_discovered_skills` 与能力快照同一事务写入；该表禁止 UPDATE/DELETE。新快照不改写旧事实，当前视图只取 active 服务器当前配置版本的最新未过期快照。Skill 列表 TTL 和 cacheScope 另存，不与工具或总能力 TTL 混同。

## 运维与最小权限

隔离 MCP client 进程执行既有 `mcp-server-discover`，操作员用同租户、同服务器身份复核：

```bash
brokerrouter --database-url "$DATABASE_URL" mcp-skills-list --tenant <TENANT_UUID> --server <SERVER_UUID>
```

命令输出 URI、完整原始 entry、批准指纹、`allowed-tools` 请求标志、同服务器名称碰撞提示和过期时间；`activation` 恒为 `disabled_until_approval_and_read_path`。目录本身不是下游 `/mcp` 的资源，虚拟密钥与智能体不能通过它获得 Skill。部署角色中只有 `brokerrouter_mcp_client` 可以插入发现事实，`brokerrouter_mcp_catalog` 可以审阅；下游 MCP server 不可读该表。

## 后续闸门

M6.2c2 需实现直接 URI `skills/get`、以完整 manifest 和人类身份绑定的持久审批、按需 `resources/read` 字节验证、隔离不可变缓存、撤权/重批准竞态，以及下游 Runtime 的显式激活与暂停恢复。在这些完成并通过官方一致性套件前，不宣称 MCP Skills 可用。
