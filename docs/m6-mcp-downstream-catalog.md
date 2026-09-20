# M6.2b2b2a：下游 MCP 目录边界

本包开放第一个生产可部署的下游 MCP 入口，但只提供经过虚拟 Key 显式授权的工具目录。工具执行继续失败关闭；在 M6.2d 将调用绑定到持久 turn、审批事实、执行租约和 Runtime 恢复凭据以前，不能把本包描述为完整 MCP 工具代理。

## 协议与 HTTP 边界

- `serve-mcp` 只暴露 `POST /mcp`，使用官方 `rmcp 3.4.0` 的无状态 Streamable HTTP server 和 `NeverSessionManager`。
- 只声明 `2026-07-28`；拒绝 `initialize`、`Mcp-Session-Id`、`Last-Event-ID`、独立 GET/SSE 会话和旧协议降级。
- SDK 强制 `Accept`、`Content-Type`、`MCP-Protocol-Version`、逐请求 `_meta`、`Mcp-Method`/`Mcp-Name`/`Mcp-Param-*` 一致性；请求体上限 1 MiB。
- Host 必须命中 `BROKERROUTER_MCP_ALLOWED_HOSTS`。存在 Origin 时必须命中 `BROKERROUTER_MCP_ALLOWED_ORIGINS`；空 Origin allowlist 表示拒绝所有携带 Origin 的请求，而不是关闭校验。
- 当前资源认证只接受已有 `sk_br_` 虚拟 Key，重复或格式异常的 Authorization 失败关闭，并返回 Bearer challenge。OAuth 2.1 Protected Resource Metadata/JWT 资源令牌属于 b2b2b，尚未实现。

示例部署配置：

```sh
export BROKERROUTER_MCP_ALLOWED_HOSTS='["gateway.example.com"]'
export BROKERROUTER_MCP_ALLOWED_ORIGINS='["https://console.example.com"]'
cargo run --locked -- serve-mcp --bind 127.0.0.1:8092
```

TLS 必须由同机反向代理或 service mesh 终止，且代理必须覆盖而不是透传客户端提供的 Host/转发头。生产数据库连接使用独立 `brokerrouter_mcp_server` 登录角色。

## 工具目录与授权

发现 active 上游时，如果 `server/discover` 声明 tools，connector 会在同一个认证生命周期内完整分页调用 `tools/list`。最多 128 页、1024 个工具、单描述 512 KiB、目录合计 4 MiB；重复名称、游标循环、缺少 2026 缓存元数据、非完整结果或非法 schema 都使整次发现失败，不保留半份目录。

每个观察到的工具写入与 discovery snapshot 绑定的不可变事实。下游名称由 Brokerrouter 的不可变 `server_id`、上游名称摘要和可读名称生成，避免不同供应商同名覆盖。工具可见性还要求：

1. 服务器 active，snapshot 与当前 config revision 一致且未过期；
2. 工具自身 TTL 未过期；
3. 虚拟 Key 未撤销、未过期；
4. 存在该 Key、server、server config revision、upstream name 与**完整描述 SHA-256**完全一致的 active grant。

因此上游修改 description、input/output schema、annotations、icons 或 `_meta` 后，旧授权不会静默继承。运维必须重新查看并授权新摘要：

```sh
cargo run --locked -- mcp-tools-list --tenant <TENANT_UUID> --server <SERVER_UUID>
cargo run --locked -- mcp-tool-grant-set --file /secure/reviewed-tool-grant.json
```

授权文件示例：

```json
{
  "tenant_id": "00000000-0000-0000-0000-000000000000",
  "key_id": "00000000-0000-0000-0000-000000000000",
  "server_id": "00000000-0000-0000-0000-000000000000",
  "operation_id": "00000000-0000-0000-0000-000000000000",
  "actor": "operator:alice",
  "reason": "reviewed exact input/output contract",
  "downstream_name": "br.<server>.<digest>.<name>",
  "descriptor_sha256": "<64 lowercase hex characters>",
  "active": true
}
```

mutation 使用持久 operation id 幂等，写入 revision 与审计。`brokerrouter_mcp_client` 只能写发现和工具事实；`brokerrouter_mcp_catalog` 只能审阅/配置 grant，不能调用上游；`brokerrouter_mcp_server` 只能验证 Key、读取当前目录并追加阻断审计，不能读取上游 credential reference、批准记录或修改授权。

## 执行失败关闭

`tools/list` 返回 Key 专属的 private-cache 目录。`tools/call` 会再次验证 Key、服务器、最新 snapshot、TTL 和描述摘要授权，并只持久化参数摘要；随后返回 `governance_execution_unavailable`，不会连接上游、不会产生外部副作用，也不会消费模型费用。

后续 b2b2b/M6.2d 必须补齐 OAuth 资源认证、逐调用 turn/runtime 绑定、护栏检查、MRTR `input_required`、执行 claim/receipt、不确定结果对账以及 Hermes/OpenClaw 原生恢复。完成前不运行或宣称通过完整 server tools/call conformance。
