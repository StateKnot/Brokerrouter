# MCP 连接与能力目录（M6.2b1）

状态：2026-09-18 完成 M6.2b 的第一个可独立上线切片。该切片建立连接配置和 `server/discover` 结果的生产级持久边界，**尚未开放 `/mcp`，也尚未发送 MCP 网络请求**。M6.2b2 完成 `rmcp 3.4.0` 双向 wire、OAuth 获取和一致性测试之前，不能宣称 Brokerrouter 已兼容 MCP。

## 已实现的不变量

- `mcp_servers.id` 是 Brokerrouter 生成、租户内不可变的 UUID。远端 `_meta.io.modelcontextprotocol/serverInfo` 只作为不可信展示元数据保存，不能改变服务器身份、路由、权限、缓存或审批。
- Streamable HTTP 仅接受无 userinfo/query/fragment 的 HTTPS URL。私网开关只影响后续固定 DNS/IP 的出站策略，不能把 HTTP 降级为可接受传输。
- stdio 只能引用部署侧预注册的 `launcher_profile`；目录不保存命令、参数、工作目录或环境变量，控制面因此不能注入任意进程。
- 认证模式是 `none`、`bearer` 或 `oauth_client_credentials`。OAuth issuer/resource/audience 在配置版本内固定；audience 必须等于规范化后的 resource，禁止把下游用户 Token 转发给上游。
- 输入只能包含环境变量名，或 `vault`/`aws-sm`/`gcp-sm`/`aliyun-kms` Secret Manager URI。URI 禁止 userinfo、query 和 fragment。引用本身由独立 MCP 配置密钥 AES-256-GCM 加密，并绑定 tenant/server UUID；真实密钥值不进入数据库、审计、CLI 文件、Runtime 或模型上下文。
- 新服务器始终是 draft（`active=false`），必须以当前 revision 做第二次更新后才能被连接器读取。每次更新自动推进 revision；旧发现快照不会用于新配置。
- 配置和发现写入均有 operation UUID、请求摘要、并发幂等回执、短事务、租户活性锁和原子审计；同 operation 不同正文返回冲突。服务器更新和发现按 server advisory lock 串行，避免把旧请求结果绑定到新配置。
- 发现快照只接受 MCP `2026-07-28`、`resultType=complete`、包含该版本的非重复 `supportedVersions`、对象型且最大 256 KiB/32 层的 capabilities、最大 16 KiB serverInfo 和 0–24 小时 TTL。
- `cacheScope=private` 要求连接器提交实际已解析认证上下文的 domain-separated SHA-256（Bearer Token 或 OAuth issuer/client/token/Secret 版本参与计算，原值不落库），再绑定租户、服务器、配置 revision 和认证引用密文生成内部 hash；读取时必须提供同一指纹。Secret 或 Token 在引用不变时轮换也不会复用旧 private cache。`public` 不接受认证指纹，也不会跨租户共享。快照和幂等回执不可更新/删除。
- 租户停用、服务器停用或配置 revision 变化后，连接器读取和当前能力读取失败关闭。

## 密钥与数据库角色

MCP 使用与模型结果不同的加密密钥环：

```text
BROKERROUTER_ACTIVE_MCP_CONFIG_KEY=<KEY_ID>
BROKERROUTER_MCP_CONFIG_KEYS={"<KEY_ID>":"<64_HEX_CHARACTERS>"}
```

生产中两个进程使用不同 PostgreSQL 登录：

- `brokerrouter_mcp_catalog`：注册/更新连接配置和审计；不能写发现、调用工具、读取虚拟 Key/模型结果或审批。
- `brokerrouter_mcp_client`：读取 active 配置、解密引用、写发现快照和审计；不能更新连接配置、审批或读取模型结果。

两个角色都没有角色切换能力。MCP 配置密钥只进入这两个受限进程，不进入 console、Runtime、模型网关或 Agent 容器。Secret Manager 引用解析和实际 Secret 获取仅属于 M6.2b2 client 进程。

## 运维入口

`mcp-server-register --file` 接受严格 JSON，注册后返回 host `server_id` 和 revision 1：

```json
{
  "tenant_id": "00000000-0000-0000-0000-000000000000",
  "operation_id": "00000000-0000-0000-0000-000000000001",
  "actor": "operator:alice",
  "reason": "approved production MCP connection",
  "display_name": "Internal tools",
  "transport": {
    "type": "streamable_http",
    "endpoint_url": "https://mcp.example.com/mcp",
    "allow_private": false
  },
  "authentication": {
    "type": "oauth_client_credentials",
    "credential_reference": {
      "type": "secret_manager",
      "uri": "vault://production/mcp/internal-tools"
    },
    "issuer": "https://identity.example.com/tenant",
    "resource": "https://mcp.example.com/mcp",
    "audience": "https://mcp.example.com/mcp"
  }
}
```

`mcp-server-update --file` 使用返回的 `server_id`、`expected_revision` 和新的 operation UUID 提交完整替换配置；启用必须显式传 `active=true`。响应丢失时只能重试同一个 JSON/operation UUID。

## 本包验证与未完成边界

新增 PostgreSQL 合约覆盖：并发幂等与冲突、宿主身份、跨租户、密文/AAD、draft/启用/revision、OAuth 三元绑定、非 HTTPS/超大元数据/错误协议/resultType 拒绝、private/public 缓存分区、配置变化后旧快照失效、审计故障原子回滚，以及两个真实受限数据库角色的正反权限。

仍未完成：`rmcp 3.4.0` 固定依赖、Streamable HTTP/stdio 实际连接、Secret resolver、OAuth metadata/token 获取与 issuer/resource/audience 响应验证、DNS/IP 固定、`server/discover` 网络调用、单一 `/mcp` server、Origin/标准头、SSE、官方 client/server conformance。它们属于 M6.2b2，不能用手工写入的发现 fixture 替代。
