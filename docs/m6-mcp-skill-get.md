# MCP Skills 直接 URI 查询（M6.2c2a）

状态：2026-09-23 完成 `skills/get` 元数据查询与不可变观察。本阶段**不是 Skill 激活**：没有人工批准、没有 `resources/read`、没有内容缓存，也没有向虚拟密钥或下游智能体暴露 Skill。

## 行为与边界

- 对已注册且激活的上游服务器，复用受管 Streamable HTTP 或固定 stdio launcher、凭据引用、网络固定、OAuth 绑定及 `rmcp` 2026-07-28 发现生命周期。只有同时声明 Resources 和 Skills 扩展才发送 `skills/get`，每个请求携带协议要求的完整 `_meta`。
- 按传入 URI 直接查询，不先调用 `skills/list`，所以未列出的 Skill 也可被观察。URI 仅作为上游 MCP 方法参数，网关不会把它当作网络地址自行请求。限制 URI 4096 字节、响应 512 KiB、TTL 24 小时。返回 URI 必须逐字相等，`resultType` 必须是 `complete`。
- 只接收完整静态 manifest：根 `SKILL.md`、frontmatter、资源 URI/摘要/大小均经过现有严格校验；动态 manifest 默认拒绝。`_meta` 与自报 `serverInfo` 不构成权限。批准指纹包含宿主分配的 server ID 与完整资源集合。
- `0026` 将每次成功观察连同服务器配置版本、能力摘要、TTL、有效缓存范围及私有授权上下文摘要写入追加型表。有效 TTL 是发现与查询 TTL 的较小值；任一层标记 private，则观察整体按 private 隔离。事务同时写审计与幂等 receipt。重试同一 operation ID 直接返回 receipt，不再访问上游；不同服务器或 URI 复用该 ID 会冲突。
- 同时加固既有目录：`server/discover` 为 public、但 `tools/list` 或 `skills/list` 为 private 时，整份新快照按 private 绑定授权上下文；历史上缺失绑定的 private 工具行会在下游目录与调用查询中失效，不会迁移改写不可变事实。
- 审阅只展示同租户、活动服务器、当前配置版本、最新且尚未过期的观察。过期或配置更新后必须重新查询。`brokerrouter_mcp_client` 可写观察，`brokerrouter_mcp_catalog` 可审阅；下游 `brokerrouter_mcp_server` 无权读取。

## 运维命令

使用 client 专用数据库身份执行查询，不在命令行提供上游密钥；凭据仍从部署管理的引用解析：

```bash
brokerrouter --database-url "$MCP_CLIENT_DATABASE_URL" mcp-skill-get \
  --tenant <TENANT_UUID> --server <SERVER_UUID> --operation <UUID> \
  --actor 'mcp-connector:operator' --uri 'skill://hidden/SKILL.md'
```

使用 catalog 审阅身份查看完整 manifest：

```bash
brokerrouter --database-url "$MCP_CATALOG_DATABASE_URL" mcp-skill-get-review \
  --tenant <TENANT_UUID> --server <SERVER_UUID> --uri 'skill://hidden/SKILL.md'
```

输出中的 `activation` 恒为 `disabled_until_approval_and_read_path`。不得将观察结果或批准指纹解释为批准凭据。

## 后续发布闸门

M6.2c2b 的真人持久审批已见 [Skill 审批契约](m6-mcp-skill-approval.md)。按需 `resources/read` 字节级校验与不可变隔离缓存仍待 M6.2c2c；M6.2d 才处理 MRTR 和 Hermes/Claw 原生暂停恢复。上述能力和正式一致性/预生产认证未完成前，不宣称 Skills 可供智能体执行。
