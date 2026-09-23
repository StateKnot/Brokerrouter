# M6.2b2c：MCP 工具调用的持久治理边界

状态：2026-09-23 已实现受控 `tools/call` 单次派发；**尚未实现 MCP MRTR 恢复、上游 task 轮询、Hermes/OpenClaw MCP 原生恢复或官方 conformance**。生产开放工具执行仍以 LIVE-22 的协议与故障认证为闸门。

## 调用契约

- 下游 `POST /mcp` 使用已有虚拟 Key 或 OAuth `mcp:tools:call` scope，以及精确工具描述摘要 grant。每个 `tools/call` 的逐请求 `_meta` 必须含 UUID 字符串 `io.brokerrouter/turnId` 和 `io.brokerrouter/callId`。Runtime 必须从受信 BFF 创建并绑定的当前 turn 获取前者；后者每个逻辑工具动作生成一次，重试原样复用。
- 相同 `callId` 只允许相同 tenant、Key、turn、服务器 revision、发现快照、上下游工具名、描述 SHA-256 和规范化参数 SHA-256。任何变化返回冲突；绝不作为新动作再次派发。
- `tool_input` 在排队前执行现有护栏检查。命中审批时返回 `approval_required`，由独立真人工作台决定，Agent/MCP 客户端不能自行批准。批准后重放原请求进入持久队列；Worker 在派发前重新核对 Key、grant、连接 revision、描述摘要与治理 claim。
- 只有隔离的 `mcp-worker` 角色解析上游凭据并发出工具调用。它在相同认证生命周期内重新发现工具并核对完整描述；每个已 claim 的动作最多调用一次上游 `tools/call`，不自动重试结果未知的副作用。
- 完整上游响应作为密文与 SHA-256 保存，并与治理执行回执同一事务提交。`tool_output` 再次通过策略和真人审批后才解密释放给下游。审核拒绝、过期、Key/turn 失效或不可用时不返回正文。

## 状态与恢复

`queued → authorizing → executing → output_pending → completed` 是完整结果的主线。输入审批发生在排队前；输出审批发生在 `output_pending`。上游 `input_required` 和 task 响应会被加密记录为独立终态，但目前**不恢复、不转发为成功的完整工具结果**；下游会收到待处理错误，待 M6.2d 提供持久 MRTR/task 映射。

发送前确定失败进入 `failed`；发送后断线、超时、协议异常或 worker 租约失效进入 `execution_unknown`。这两种状态都不会自动重派发，其中不确定状态必须人工核对上游效果。租约维护以最多 100 行的 `SKIP LOCKED` 批次运行。turn 过期后待发调用失败、已存结果密文擦除；审计仅存 ID、状态、摘要，不存工具参数或结果明文。

当前 MCP JSON-RPC 待审批/排队/不确定错误的 `data.code` 分别为 `approval_required`、`tool_call_pending`、`execution_unknown`。客户端必须保存原 `callId`，在同一有效 turn 内重试读取；不得因为连接中断生成新 `callId`。当前协议映射不是官方 `input_required` 恢复实现，不能据此宣传完整 MCP 兼容。

## 部署与权限

先应用 0024 迁移及最新 `deploy/permissions.sql`。`serve-mcp` 使用 `brokerrouter_mcp_server` 登录，只持有结果密钥环和目录/治理/入队权限；`mcp-worker --concurrency <N>` 使用 `brokerrouter_mcp_worker` 登录，持有**不同** MCP 配置密钥环、结果密钥环、Secret 注入映射和固定的 stdio launcher registry。两个进程不能使用表拥有者或超级用户登录。结果密钥环轮换必须保留旧 key，直至所有未过期密文退出保留期。

Worker 只允许出站访问已登记的 HTTPS MCP 服务及明确授权的 Secret/OAuth 地址；受管 stdio profile 必须由部署方固定程序 SHA-256、工作目录和环境。边界代理必须使用精确 Host/Origin、TLS、无客户端转发头透传。不要把用户模型供应商密钥、上游 MCP 凭据或审批私钥传入 MCP edge。

验证覆盖真实 PostgreSQL 与实际受限角色、stdio 工具一次执行、输入和输出审批、grant 撤销、幂等冲突及结果丢失不重发。真实 HTTPS/OAuth、并发压测、进程逐点终止、上游 task/MRTR、官方 conformance、Hermes/OpenClaw 恢复和生产回滚仍列为 LIVE-22；这些闸门未关闭前，不在既有 `stknot.com` 生产入口启用 MCP 工具执行。
