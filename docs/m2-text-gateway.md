# M2：统一文本调用契约

实现档位：`text-chat-v2`。这份清单是当前代码的支持边界，不等于完整 OpenAI API 兼容或真实供应商认证。主币种仍为 CNY。

## 接口与字段

所有业务接口使用虚拟密钥 Bearer 鉴权。收费 POST 必须传 `Idempotency-Key`（1–200 个可见 ASCII 字符）；Python/JS SDK 可通过 `extra_headers` 配置，并将客户端自动重试设为 0。`x-request-id` 标识本次 HTTP 请求；`x-brokerrouter-request-id` 标识持久逻辑请求，后续用它查状态和原结果。

| 接口 | 当前支持 | 明确拒绝 |
| --- | --- | --- |
| `POST /v1/chat/completions` | 文本 system/user/assistant/tool 消息；`max_tokens`；temperature/top_p/stop；单个 choice；function tools/tool_choice/parallel_tool_calls；user；`stream:false`；经端点能力门禁的 `stream:true` OpenAI 兼容 SSE | 多模态 content、`n>1`、Responses、response_format、strict tools、max_completion_tokens 及其他未列字段 |
| `POST /v1/embeddings` | 文本或 1–32 条非空文本数组；float 向量；已配置维度；user | token ID 数组、base64、未配置维度、音视频输入 |
| `GET /v1/requests` | 当前 Key 的请求日志、人民币费用、游标分页；`before=<request UUID>`、`limit=1..100` | 跨 Key、跨租户日志；原文搜索 |
| `GET /v1/requests/{id}` | 请求状态、各 attempt、端点 ID、HTTP 状态、错误类别、人民币费用、结果有效期 | 未授权请求、原始密钥、出网地址、请求正文 |
| `GET /v1/requests/{id}/result` | 读取同一请求已加密保存且未过期的结果 | 重新生成结果、对已关闭权限的模型释放结果 |

Chat 未传 `max_tokens` 时使用 1024；端点能力不足时拒绝，不静默截小。Embedding 的逻辑模型必须具有相同向量空间语义与维度；数据库强制操作类型和维度一致，模型版本/向量语义等价仍须供应商认证，不可仅凭相同营销名称分组。

文本/工具定义在请求前做类型和字段验证，不支持字段不会被静默丢弃。工具调用这里只是模型协议数据，不执行工具，也不表示已实现 M3 的护栏或真人审批。

## 路由、幂等与费用

1. 按 Key 授权模型，筛选操作类型、工具/SSE 能力、输出上限、Embedding 维度和可用凭据引用。端点的 `supports_tools` / `supports_streaming` 默认均为 false；不满足时返回 `unsupported_model_capability`，不删字段降级转发。
2. 按 `priority` 升序、endpoint ID 稳定排序，固定最多 3 个候选端点及其价格/汇率快照。后续配置变化不改已准入请求的价格。
3. 预算预留使用候选中最大的可界定成本上限：Chat 按端点完整输入上下文上限 + 输出上限，Embedding 按每条输入上下文上限 × 批大小。这个保守上限不是实际用量，可能高于短请求最终费用。端点上下文上限必须与真实供应商一致。
4. 每次 attempt 在发送前持久化，并重查 Key/租户/模型权限与端点启用状态。数据库条件更新保证多个实例和相同幂等键只有一个获得发送权。
5. DNS/请求构建等发送前失败，或 HTTP 客户端确认的连接阶段失败，记录 `not_sent` 后可以尝试下一候选。自动重定向、系统代理和 HTTP 客户端隐式重试均关闭。
6. 已发送后的超时、断线、非 200 响应（包括 429）、无效 JSON、响应过大、用量缺失/越界进入 `submission_unknown`；不切端点、不把费用记为零、不释放预留。此通用档位不假定某个错误码代表未收费。
7. 用供应商返回且校验通过的 token 量，按实际成功 attempt 的价格/汇率结算；原币金额、人民币精确金额与人民币微元写同一账本事务。请求的 `price_snapshot` 是最大预留上限所依据的快照，实际计费快照以 finalize 流水中的 `price_snapshot` 为准，候选全量快照保存在 `routing_snapshot`。
8. 成功结果、attempt 成功和账本结算同事务提交。相同幂等键与相同请求重放仅读取结果；变更请求体则冲突。即使结果过期，也不能重新使用旧幂等键生成。

### SSE 结算与失败语义

`stream:true` 时网关强制上游返回最终 usage，校验每个事件并重建完整结果。总响应上限 4 MiB、最多 100000 个事件。下游断开后，已提交请求在受管后台中继续读取、验证、结算和加密持久化；不因客户端断开释放费用。

在成功结果、attempt 和人民币账本已持久化之前，网关不发送 `[DONE]`。缺少或非法 JSON、usage、finish reason 或 `[DONE]`、上游断流与越界都是 `submission_unknown`；流已开始时用 SSE error 事件结束且不发 `[DONE]`。相同幂等键重放已成功请求时，从加密结果合成等价 SSE，不重调供应商。由于已发送 token 无法追回，命中 `post` 输出护栏的流式请求在出网前以 `streaming_requires_unbuffered_output` 失败关闭，必须改用非流式完整审查。

当前费用是配置端点单价的计算值：输入 token 按统一 input 价，缓存折扣、阶梯价、供应商发票、补贴与客户加价未混为“真实成本”。原始 usage 明细另存，供应商对账后由有权人员核对。超过已准入上界的用量不会强行结算或伪装成免费成功。

日志中的 `maximum_cny` 为原始准入上限，`reserved_cny` 为当前仍占用额度（终态为零），`cost_cny` 为已入账费用（未知时为 null，不能显示成零）。

## 结果、断线与恢复

结果使用 AES-256-GCM 加密，AAD 绑定租户、逻辑请求和密钥版本；无明文正文请求日志。为了幂等恢复会保存加密响应，这与“不记录明文 prompt 日志”不同，不能宣传为从不保存内容。

- 默认结果有效期 24 小时，可配置 60 秒至 7 天；读取实时检查有效期，后台每 30 秒分批清理过期密文。账本和请求幂等记录不跟随密文删除。
- 客户端断开不取消已准入执行；应用跟踪这些任务，在正常 SIGTERM 排空时等待其完成。进程被强制终止时，后台将超过 deadline 的 submitting 标成不确定，不重发。
- 在发送前崩溃的 reserved 请求，重放可以重新竞争发送权；deadline 后释放。已经记录提交意图、但无法证明是否发送的请求保守保留额度。
- 供应商响应已回到内存、但进程在落库前被杀，存在不可消除的不确定窗口；通过供应商核对闭合，不承诺跨外部系统 exactly-once。
- 默认调用总 deadline 120 秒（可设 1–600 秒），DNS 上限 3 秒，连接阶段上限 5 秒，响应体上限 4 MiB。API 请求体上限 2 MiB，单进程最多 64 个调用（可设 1–1024）。混合负载和多实例公平限流另有 M6 验收。

## 人工费用核对（不是护栏审批）

该入口只允许单独的数据库 `brokerrouter_reconciler` 身份，不开放为 Agent/虚拟密钥 HTTP 接口。它不能批准工具动作，也不能让模型自动再执行。

```sh
# 已从供应商确认没有收费；evidence 是工单/账单等证据引用，不要粘贴敏感正文。
brokerrouter request-reconcile --tenant <TENANT_UUID> --id <REQUEST_UUID> \
  --no-charge --evidence <EVIDENCE_REFERENCE>

# 已核实用量但无法恢复原结果；依旧使用请求固定的原价格和汇率。
brokerrouter request-reconcile --tenant <TENANT_UUID> --id <REQUEST_UUID> \
  --input-tokens 123 --output-tokens 45 --evidence <EVIDENCE_REFERENCE>
```

核对将费用闭合为 `reconciled`，释放未消费预留，保留操作人和证据引用，但不把缺失结果伪装成成功。重复相同金额不重复入账，冲突金额拒绝；后续账单调整属于独立审计流程，不能改写原事实。

## 运行配置与密钥轮换

服务启动必须提供 `BROKERROUTER_ACTIVE_RESULT_KEY` 和 `BROKERROUTER_RESULT_KEYS`，后者是 `{"key-id":"64位十六进制AES密钥"}` 的 JSON；禁止使用文档示例/固定测试密钥作为生产密钥。生产通过 Secret 注入，所有副本配置一致。先加入新版本，再切 active，保留旧版本直到该版本密文与所需恢复备份全部过期。按调用量轮换密钥，不允许同一随机 nonce 密钥无限期使用；每个版本总加密次数应控制在 `2^32` 以下。

供应商 `credential_env` 仅引用环境变量；不写数据库明文。启动读取已启用端点引用的变量。模型、端点和价格读取数据库动态生效；增加新的凭据引用或轮换环境 Secret 后要滚动重启加载，不能误以为修改外部环境变量会更新已有进程。

环境配置：

- `BROKERROUTER_MAX_CONCURRENCY`：默认 64。
- `BROKERROUTER_REQUEST_TIMEOUT_SECONDS`：默认 120。
- `BROKERROUTER_RESULT_TTL_SECONDS`：默认 86400。
- 生产使用经证书验证的 HTTPS；显式 `--allow-private` 才能访问内网/HTTP 测试端点。公共端点 DNS 解析后校验地址并固定连接地址，避免第二次解析绕过校验。

## 验证与真实认证

本地测试使用真实 PostgreSQL 与真实 localhost HTTP 连接；覆盖成功、连接失败、已接收后超时、响应损坏、未知费用、并发幂等、客户端断开、进程状态恢复、数据库结算失败和受限数据库角色。它们证明网关协议/故障行为，不证明任意供应商模型可用。

真实认证前须固定供应商/区域/模型版本、端点、单价和凭据引用，确认人民币测试上限，分别验证普通对话、工具往返、SSE 分块/中断/用量、Embedding、账单口径和模型等价性。本地回归证明网关失败语义，不自动把某个真实端点标成已认证。Responses、多模态 Chat content 及分段输出审核不在当前档位内。

参考接口来源：[Chat API](https://developers.openai.com/api/reference/resources/chat)、[Embedding API](https://developers.openai.com/api/reference/resources/embeddings/methods/create)。出网禁用重试/跳转等依据 [reqwest 0.12.28](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html)，加密采用 [RustCrypto AES-GCM](https://docs.rs/aes-gcm/0.10.3/aes_gcm/)；本项目只承诺上表列出的子集。
