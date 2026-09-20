# Agent 工具调用闭环认证矩阵

工具能力按“供应商 + 地域/端点 + 固定模型版本 + Brokerrouter 提交”认证，不能仅凭端点声明 `supports_tools=true`。运行时无已认证工具端点时返回 `unsupported_model_capability`，不会静默删除 `tools`、`tool_choice`、`parallel_tool_calls` 或 tool 消息。

## 消费者检查清单

每一行必须用同一个虚拟 Key 完成以下证据，且所有请求经过 Brokerrouter：

1. 第一次响应返回合法 `assistant.tool_calls`；网关规范化供应商流式/非流式扩展字段。
2. 客户端不修改该 assistant message，追加与 `id` 对应的 `role=tool` 消息。
3. 第二次响应返回普通 assistant 内容；没有重复工具派发。
4. 两个逻辑请求各只有一次成功 attempt，usage 完整，人民币 finalize 与租户/Key 消费一致，预留归零。
5. 相同幂等键重放不产生新 attempt/费用；变更正文返回冲突。

## 当前矩阵

| 供应商 / 区域 | 上游模型 | Brokerrouter 逻辑模型 | 非流式工具闭环 | SSE 工具闭环 | JiaClaw 默认级别 | 证据 |
| --- | --- | --- | --- | --- | --- | --- |
| 阿里云百炼 workspace / 北京 | `qwen3-30b-a3b-instruct-2507` | `aliyun/qwen3-30b-a3b-instruct-2507` | 待当前修复提交真实复验 | 待真实复验 | experimental；不能作为工具默认 | [2026-09-14 联调](live-aliyun-2026-09-14.md)已证明供应商能生成工具调用并在规范字段下继续，但当时网关未规范化 `index` |
| 本地受控 OpenAI-compatible fixture | `fixture-tool-model` | `logical/model` | 已认证（协议/计费/幂等故障测试） | 文本 SSE 已认证；工具 SSE 待补 | 仅测试，不是生产供应商默认 | `provider_tool_index_is_normalized_for_exact_agent_roundtrip`、`sse_stream_settles_before_done_and_replays_without_provider_call` |

当前没有可诚实标记为 JiaClaw **生产默认** 的真实供应商工具路径。阿里云北京行只有在新提交上用原样 assistant message 完成两轮调用、复核人民币账本并记录脱敏证据后才能从 experimental 改为 certified；真实复验前不得为了关闭 Issue 改写结论。

重复认证使用仓库内工具；虚拟 Key 只从环境读取，输出不含 Key、prompt 或 tool result：

```sh
export BROKERROUTER_CERT_VIRTUAL_KEY='<短期、限预算、仅授权目标模型的虚拟 Key>'
cargo run --locked --bin brokerrouter-certify-tools -- \
  --base-url http://127.0.0.1:8080 \
  --model aliyun/qwen3-30b-a3b-instruct-2507
```

## 端点配置规则

- `supports_tools` 和 `supports_streaming` 默认均为 false；配置页面新建端点后仍默认停用。
- 两个能力只能依据同一固定端点的实际结果开启。工具非流式通过不自动证明 SSE 工具增量格式可用。
- 同一逻辑模型有多个供应商时，工具/流式请求只在满足所需能力的端点集合内排序；不会路由到弱端点再删字段。
- 供应商扩展的非流式 `message.tool_calls[].index` 在响应边界删除；客户端回传的历史 `index` 也在验证后删除再发送上游。标准 `id/type/function` 不改变。
