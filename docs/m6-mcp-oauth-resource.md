# M6.2b2b2b：下游 MCP OAuth 资源认证

本包把 `serve-mcp` 提升为 MCP 2026-07-28 OAuth 2.1 Protected Resource，同时保留既有 `sk_br_` 虚拟 Key 供受管一方客户端使用。OAuth 只负责证明调用主体和 scope；工具可见性、租户隔离、预算与撤权仍由映射后的虚拟 Key 决定。`tools/call` 继续治理失败关闭，本包不会把下游 access token 转发给任何上游。

## 协议端点与 challenge

- canonical resource 必须是服务的精确 HTTPS MCP URL，例如 `https://gateway.example.com/mcp`，且 authority 必须在 `BROKERROUTER_MCP_ALLOWED_HOSTS` 中。
- Protected Resource Metadata 发布在 path-specific RFC 9728 地址：`GET /.well-known/oauth-protected-resource/mcp`。文档声明 `resource`、`resource_name`、`authorization_servers`、最小 scopes 和 Bearer header；access-token 验签算法是服务端策略，不误用“资源响应签名算法”元数据字段。
- 缺少凭据或 token 无效返回 401；challenge 包含 `resource_metadata` 和当前请求所需 scope。已认证但 scope 不足返回 403 `insufficient_scope`，一次给出当前操作所需 scope。
- `mcp:tools:read` 用于发现和目录读取，`mcp:tools:call` 用于调用。现有虚拟 Key 维持原权限语义；OAuth token 必须显式带 scope。

## JWT 与 JWKS 边界

只接受由明确配置的 authorization server 签发的 JWT access token：

- JOSE `typ` 必须为 `at+jwt`，必须有有界 `kid`；仅允许 `RS256`、`PS256`、`ES256`、`EdDSA`。
- 拒绝对称 key、算法不匹配、重复 `kid`、`jku`、内嵌 `jwk`、`x5u`、`x5c`、未知 critical/header 扩展和压缩/加密 JWT。
- 强制 `iss`、`sub`、`aud`、`tenant_id`、`scope`、`iat`、`nbf`、`exp`；`aud` 必须是且只能是当前 canonical resource，token 生命周期不得超过部署上限。
- 未验证 payload 只能选择预配置 issuer，不能提供新 JWKS 地址。签名通过后，`issuer + subject + tenant_id` 还必须命中 `mcp_oauth_principals` 的显式映射；映射目标 Key 或租户一旦撤销/停用，token 立即失效。
- 远端 JWKS 经无代理、无重定向、DNS/IP 校验并固定地址的 HTTPS client 获取；响应上限 512 KiB、最多 32 个 key，遵守有上限的 `max-age`。未知 `kid` 的刷新有 single-flight 和频率限制，防止随机 `kid` 制造出站洪泛。企业私有 PKI 复用有界 `BROKERROUTER_MCP_CA_BUNDLE`，不会关闭证书或主机名验证。
- 隔离环境可内联部署固定的**公钥** JWKS；私钥不得进入网关。固定 JWKS 的轮换通过原子配置发布和进程滚动完成。

当前包实现 JWT access token profile，不支持 opaque token/introspection。接入的 authorization server 必须签发满足上述契约的 access token，不能把 ID token 当作 access token。

## 配置和主体映射

远端 JWKS 示例：

```sh
export BROKERROUTER_MCP_ALLOWED_HOSTS='["gateway.example.com"]'
export BROKERROUTER_MCP_ALLOWED_ORIGINS='["https://console.example.com"]'
export BROKERROUTER_MCP_RESOURCE='https://gateway.example.com/mcp'
export BROKERROUTER_MCP_OAUTH_ISSUERS='[{"issuer":"https://identity.example/tenant","jwks_uri":"https://identity.example/tenant/jwks","allow_private":false}]'
export BROKERROUTER_MCP_OAUTH_MAX_TOKEN_SECONDS=3600
cargo run --locked -- serve-mcp --bind 127.0.0.1:8092
```

运维审核后，用幂等 operation 把 subject 映射到已有虚拟 Key：

```json
{
  "tenant_id": "<TENANT_UUID>",
  "key_id": "<VIRTUAL_KEY_UUID>",
  "operation_id": "<NEW_UUID>",
  "actor": "operator:alice",
  "reason": "approved Hermes production runtime",
  "issuer": "https://identity.example/tenant",
  "subject": "hermes-runtime-42",
  "active": true
}
```

```sh
cargo run --locked -- mcp-oauth-principal-set --file principal.json
```

同一 operation/body 重放返回原回执；同 operation 改正文冲突。配置、审计、回执在同一事务提交。`brokerrouter_mcp_catalog` 可以管理映射但不能验证 token 或调用上游；`brokerrouter_mcp_server` 只能读取映射和现行工具授权，不能修改它们。

## 本包验证与剩余边界

专项回归覆盖 metadata/challenge、正确 token、错误 audience、scope 403、远程 key header 拒绝、Key 撤销、主体映射幂等/跨租户拒绝和数据库角色权限。隔离 PostgreSQL 17 全仓常规回归为 224 项通过、0 失败；4 项固定 S3/ClamAV/FFmpeg 契约由 CI 的独立 fixture 执行。真实 IdP/JWKS TLS、key 轮换、Authorization Code + PKCE 客户端、opaque token/introspection 与容量测试仍属于预生产认证。

后续必须把 `tools/call` 绑定到当前 Runtime/turn、护栏决策、MRTR `input_required`、执行 claim/receipt 和不确定结果对账，再接 Hermes/OpenClaw 原生恢复。完成前 Brokerrouter 仍不能宣称是完整 MCP 工具代理。
