# JiaClaw 接入 Brokerrouter

本页是 JiaClaw 类单用户 Agent 的消费者契约。生产部署仍使用独立数据库角色、Secret 管理、TLS、备份和发布闸门；`init-personal` 只减少本地开发步骤，不是生产认证捷径。

## 本地个人配置

1. 复制并核对 [`examples/jiaclaw-personal.aliyun.json`](../examples/jiaclaw-personal.aliyun.json)。必须填写实际 workspace URL，并按供应商当前合同/区域重新确认模型 ID、上下文、输出上限和人民币单价。只有完成对应协议测试后才能把 `supports_tools` / `supports_streaming` 设为 `true`。
2. 启动仓库固定的 PostgreSQL 17：

   ```sh
   docker compose -f compose.test.yaml up -d postgres
   export DATABASE_URL=postgres://brokerrouter:brokerrouter-local-only@127.0.0.1:55432/brokerrouter
   export BROKERROUTER_PERSONAL_PROVIDER_KEY='<从本地 Secret 工具注入，不写入文件或 shell 历史>'
   cargo run --locked -- init-personal \
     --file examples/jiaclaw-personal.aliyun.json \
     --gateway-base-url http://127.0.0.1:8080
   ```

   命令自动应用当前 migration，并在一个数据库事务中创建一个租户、逻辑模型、端点和有期限虚拟 Key。任何一步或审计写入失败都会全部回滚。同名 profile 不会悄悄再发 Key；遗失输出时须显式撤销/重建，不能从数据库恢复明文。

3. 保存命令输出的 JiaClaw 配置；虚拟 Key 只显示一次：

   ```toml
   [provider]
   provider_type = "brokerrouter"
   base_url = "http://127.0.0.1:8080"
   api_key = "<one-time-virtual-key>"
   model = "<logical-model-id>"
   ```

4. 为结果恢复生成仅本机使用的独立随机 keyring，再启动网关。以下变量必须来自 Secret 管理；不要复制固定示例值：

   ```sh
   export BROKERROUTER_ACTIVE_RESULT_KEY=local-$(date +%Y%m%d)
   # VALUE 必须是密码学安全随机生成的 32 字节十六进制值。
   export BROKERROUTER_RESULT_KEYS='{"local-YYYYMMDD":"<64_HEX_CHARACTERS>"}'
   cargo run --locked -- serve --bind 127.0.0.1:8080
   ```

本地 PostgreSQL 密码、HTTP、单进程和个人 Key 都不满足生产部署条件。个人 profile 默认 Key 有期限（模板为 168 小时），预算和到期时间仍由数据库强制执行。

### 已验证的本地流程

2026-09-20 以一次性 PostgreSQL 17 数据库、真实 `cargo run -- init-personal`、真实 `cargo run -- serve` 和 localhost OpenAI-compatible TCP fixture 执行本节流程。非流式 Chat、SSE Chat 及两轮工具闭环都只产生一个成功 attempt，人民币费用结算后预留归零；幂等重放没有新增上游请求。此证据验证个人开发闭环，不代替真实供应商、TLS、Secret 和容量认证。

## HTTP 请求合同

每个收费请求必须携带：

- `Authorization: Bearer <VIRTUAL_KEY>`
- 唯一且可持久恢复的 `Idempotency-Key`；同一业务操作重试必须复用，同一键不得改正文
- 受治理 Runtime 还必须携带服务端登记的 `X-Brokerrouter-Turn-Id`

响应的 `x-request-id` 是一次 HTTP 请求；`x-brokerrouter-request-id` 是持久逻辑调用，可用于 `/v1/requests/{id}` 和 `/v1/requests/{id}/result` 恢复。JiaClaw 必须关闭 SDK 自动重试；只有 `not_submitted` 才表示网关证明没有发给供应商，`submission_unknown` 必须进入核对流程。

| Chat 字段 | 支持 | 行为 |
| --- | --- | --- |
| `model`, 文本 `messages` | 是 | system/user/assistant/tool；多模态 content 明确拒绝 |
| `max_tokens`, `temperature`, `top_p`, `stop`, `user` | 是 | 严格类型和上限校验 |
| `tools`, `tool_choice`, `parallel_tool_calls` | 端点能力控制 | 无认证端点返回 `unsupported_model_capability`，不静默删除 |
| `stream:true` | 端点能力控制 | OpenAI 兼容 SSE；存在匹配的 post 输出护栏时在出网前拒绝 |
| `n` | 仅 1 | `n>1` 拒绝 |
| Responses、`response_format`、strict tools、`max_completion_tokens` | 否 | `invalid_request`，不会转发未知字段 |

## 非流式示例

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer ${BROKERROUTER_VIRTUAL_KEY}" \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: jiaclaw-$(uuidgen)" \
  -d '{"model":"aliyun/qwen3-30b-a3b-instruct-2507","messages":[{"role":"user","content":"只回复 pong"}],"max_tokens":16}'
```

Rust `reqwest` 示例：

```rust
let response = reqwest::Client::builder()
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .build()?
    .post("http://127.0.0.1:8080/v1/chat/completions")
    .bearer_auth(std::env::var("BROKERROUTER_VIRTUAL_KEY")?)
    .header("Idempotency-Key", operation_id.to_string())
    .json(&serde_json::json!({
        "model": "aliyun/qwen3-30b-a3b-instruct-2507",
        "messages": [{"role":"user", "content":"只回复 pong"}],
        "max_tokens": 16
    }))
    .send()
    .await?;
```

端点完成 SSE 认证并显式开启 `supports_streaming` 后，JiaClaw 可使用：

```sh
curl --no-buffer --fail-with-body http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer ${BROKERROUTER_VIRTUAL_KEY}" \
  -H 'Content-Type: application/json' \
  -H "Idempotency-Key: jiaclaw-stream-$(uuidgen)" \
  -d '{"model":"aliyun/qwen3-30b-a3b-instruct-2507","messages":[{"role":"user","content":"只回复 pong"}],"max_tokens":16,"stream":true}'
```

消费者必须按 SSE 事件读取数据，保存响应头 `x-brokerrouter-request-id`，且只把最终 `[DONE]` 当作网关已完成持久结算的确认。应用不得在断流后用新幂等键自动重发。

## SSE、结算与恢复

`stream:true` 只路由到显式 `supports_streaming=true` 的端点。网关固定向上游请求最终 usage，逐事件校验并把供应商模型名改回逻辑模型名。总流量上限 4 MiB、最多 100000 个事件；缺少 role、finish reason、usage 或 `[DONE]`，非法 JSON、断流和越界均进入 `submission_unknown`。

流式请求支持 `stream_options` 为 `null` 或仅含布尔 `include_usage` 的对象。`include_usage:true` 在实时响应和成功重放中返回一个 `choices:[]` 的最终 usage 事件；`false` 或空对象不向客户端发送该事件。省略该字段或传 `null` 保留网关原有的返回 usage 行为。无论客户端是否请求 usage，网关都向上游请求并验证 usage，再持久结算；隐藏客户端 usage 不会免除账本校验。非流式请求携带该字段、错误类型或未支持的选项均在出网与预留前返回 400。usage 事件形状参照 [OpenAI Chat Completions 契约](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)。

网关不会因下游断开而取消已经提交的供应商调用：后台继续读取、验证、加密保存并按实际 usage 结算人民币。只有结果与账本在同一事务持久化成功后才向在线客户端发送 `[DONE]`。相同幂等键重放成功流时读取加密结果并合成标准 SSE，不再次调用供应商。流开始后的错误以 SSE `error` 数据结束且不发送 `[DONE]`；消费者应使用 `x-brokerrouter-request-id` 查询状态。

流式 token 不能在发送后追回，所以匹配 `post` 输出护栏的请求返回 `streaming_requires_unbuffered_output`，要求改用非流式，让完整输出先经过护栏和真人审批。`pre` 护栏仍可在任何上游数据发送前暂停。

## 错误映射

| code | JiaClaw 行为 |
| --- | --- |
| `invalid_virtual_key` | 停止调用并要求换 Key，不重试 |
| `budget_exceeded` | 展示人民币预算不足，不降级直连供应商 |
| `unsupported_model_capability` | 更换已认证逻辑模型/端点，不删除 tools 或 stream 字段重试 |
| `approval_required` | 使用原 turn 等待真人处理；批准后以原业务操作恢复 |
| `not_submitted` | 可按同一幂等键重试；网关已证明未提交上游 |
| `submission_unknown` | 不自动重发；查询请求并走供应商/账单核对 |
| `operation_in_progress` | 轮询原逻辑请求，不生成新幂等键 |
| `idempotency_conflict` | 客户端错误：同一键正文发生变化 |
| `result_unavailable` | 结果已过期；不得用旧幂等键重新生成 |

完整资金、失败恢复和人工核对边界见 [M2 文本调用契约](m2-text-gateway.md)。
