# MCP Streamable HTTP 上游发现（M6.2b2a）

状态：2026-09-20 完成可独立部署的 M6.2b2a。Brokerrouter 现在可以由隔离的 MCP client 进程读取 M6.2b1 active 连接，解析凭据引用，经官方 `rmcp 3.4.0` 执行 MCP `2026-07-28` `server/discover`，并把完整结果写入不可变能力快照。**本包不是完整 MCP 网关**：stdio、下游 `/mcp`、工具调用/MRTR、Skill 持久审批和官方 client/server conformance 仍未完成。

## 固定协议与网络边界

- 依赖固定为官方 `rmcp = 3.4.0`，只协商 `2026-07-28` 的 Discover lifecycle，不回退 `initialize` 或旧版 HTTP+SSE。协议状态机、每请求 `_meta`、标准 HTTP headers 和 SSE 解析由 SDK 提供。
- Streamable HTTP 端点仍只接受 HTTPS。连接前在 3 秒内解析 DNS，拒绝未授权的 loopback、私网、链路本地、元数据、CGNAT、文档和隧道地址，然后把已验证地址固定到无代理、无重定向、无重试的 reqwest client，防止 DNS rebinding。
- 单次 lifecycle deadline 20 秒；并发固定为 1。JSON 和用于发现的 SSE 响应在原始字节层限制为 512 KiB，超过上限会在反序列化前失败。OAuth metadata/token 响应限制为 1 MiB。
- `BROKERROUTER_MCP_CA_BUNDLE` 可引用最多 1 MiB、128 张证书的 PEM bundle；只追加可信根，不关闭主机名或证书验证。用于私有 PKI 时，Streamable HTTP、OAuth metadata 和 token endpoint 共用同一受管根集合。
- SDK lifecycle 首次 discover 用于协商版本；第二次幂等 discover 取得 SDK peer view 未保留的 `supportedVersions`、`ttlMs` 和 `cacheScope`。两个请求都必须成功，且都携带标准头和 self-contained client metadata。

## 凭据与 OAuth

- `authentication=none` 不读取 Secret；private cache 仍绑定显式的无认证上下文指纹。
- Bearer 环境引用的变量值是原始 Token。值只在隔离 connector 内存中存在，不写数据库、审计或 CLI 输出。
- OAuth client credentials 的 Secret 是严格 JSON：

```json
{
  "client_id": "brokerrouter-production",
  "client_secret": "injected-secret",
  "scopes": ["mcp:read"]
}
```

  不接受未知字段、空值、重复/非法 scope 或超过 64 KiB 的凭据。客户端先按 RFC 9728/RFC 8414 发现 metadata；禁止 SDK 的 legacy endpoint fallback，metadata `issuer` 必须精确等于目录固定值，且 Streamable HTTP 的 `resource`/`audience` 必须精确等于端点。Token 请求始终携带固定 `resource`。OAuth 每个 URL 都重新执行 DNS/IP 固定；仅 GET/HEAD metadata 可在最多 5 跳内同源重定向，Token 请求不跟随重定向。
- private discovery cache 的输入指纹采用带长度域分隔的 SHA-256。OAuth issuer/resource/audience、client id、client secret、实际 access token 和 TLS trust bundle 均参与；凭据、Token 或私有 CA 轮换后不会复用旧 private snapshot。真实材料不落库。

Secret Manager URI 通过生产 Secret 注入器绑定到进程环境变量，映射本身不含 Secret：

```text
BROKERROUTER_MCP_SECRET_ENV_BINDINGS=[
  {"reference":"vault://production/mcp/internal-tools","environment":"MCP_INTERNAL_TOOLS_OAUTH"}
]
```

这适用于 Vault Agent、Secrets Store CSI、External Secrets 及云厂商等价注入器。URI 未绑定、环境变量缺失或内容非法时失败关闭；Brokerrouter 不执行调用方命令，也不把 URI 当作网络地址自行抓取。

## 运维入口与恢复

进程使用 `brokerrouter_mcp_client` 数据库登录、MCP 配置解密密钥和它所需的上游 Secret，不得复用 catalog、console、Runtime 或模型网关进程身份：

```bash
brokerrouter --database-url "$DATABASE_URL" mcp-server-discover \
  --tenant 00000000-0000-0000-0000-000000000000 \
  --server 00000000-0000-0000-0000-000000000001 \
  --operation 00000000-0000-0000-0000-000000000002 \
  --actor mcp-connector:production
```

`operation` 是持久幂等身份。命令先读取已有 discovery receipt；若上次已经落库，直接返回同一回执而不联网。无回执时才执行发现，快照写入仍检查服务器 revision、租户/服务器 active 状态和原子审计。网络成功但事务失败时可以用相同 operation 重试；`server/discover` 是只读幂等请求。

## 已验证与剩余闸门

本包测试覆盖严格 Secret 绑定/凭据格式、无歧义认证指纹、真实本机 TCP Streamable HTTP discover、两次请求均无 `initialize`、标准头、self-contained metadata、Bearer 注入、超大 JSON 提前拒绝，以及 PostgreSQL receipt/server 绑定。全仓 fmt、clippy、debug/release 测试继续作为 PR 闸门。

LIVE-22 仍阻断发布声明：需要预生产 HTTPS/OAuth/私有 CA 真实服务认证、官方 conformance 固定版本；M6.2b2b 还需实现预注册 stdio launcher 与统一下游 `/mcp` server（Origin、认证、工具路由），随后 M6.2c–d 完成 Skill 持久审批、MRTR 和 Hermes/OpenClaw 恢复。
