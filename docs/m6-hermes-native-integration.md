# Hermes 原生治理接入：已核验边界与实施顺序

状态：2026-10-11，R1.1 正在实现工具最终文本的加密隔离、服务端当前策略释放及原生宿主恢复，契约见 [工具文本结果](m6-hermes-tool-results.md)。此前 M6.1a/M6.1b 已实现工具输入门、受信 turn 绑定、原生审批通知及完整性校验。**未安装到用户运行实例；二进制产物隔离、会话发布去重和完整生产恢复认证仍未完成**。目标上游为用户指定的 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)，实现位于 `adapters/hermes`。

## 固定目标

先前会话曾只读核验 `/opt/hermes` 包版本 `0.21.1`、容器 `d5dd6c405e54`、镜像内容 ID `sha256:59e5509d41aee6a1e86724e990c376463dba01968d53127e8cd20df74952f8b0` 和上游提交 `67764dc0863349a384c16425e73ee8571f3a94b7`。它们不是当前 Mac Docker 中已部署的实例。当前验证使用固定上游源码及新建隔离测试环境，未修改或热补丁用户实例。

| 宿主文件 | 固定源码 SHA-256 |
| --- | --- |
| `agent/tool_executor.py` | `1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9` |
| `gateway/run_turn.py` | `dd203228bdfb7f69f1442adf4e24cc7e3196fb348b096ab6e7c2cb2bd2af5214` |
| `gateway/run_turn_runner.py` | `9bfa045616264e944b417ce3bfe0e302168c7b6d0218ceeb253b2dad2629e4ba` |
| `model_tools.py` | `875512e422e3e0d1502f7f7e127d47288bf1f1b350b4442ad5fc210e526b3f67` |
| `tools/registry.py` | `ca02644ea36dfa08ab7e12893d283a3b12549a5ffd33ea72c29c632a94e7b201` |
| `gateway/platforms/base.py` | `14f2bcd915e97ecb2d14cae290e1dc07d36c3247ac9b7599f38c3175670ec52a` |
| `gateway/run_voice.py` | `236853144929c0165f766c9c44927c8acb0c2e575776b68b8a5f6dd0f9e8651f` |

overlay 安装器仅接受这七个完整摘要和全部唯一锚点，支持从精确的中断安装态继续，任何其他混合状态拒绝。R1 覆盖实际串/并行派发中早于最终放行的 post/transform hook、原始异常日志及平台附件发送、自动/显式语音入口。当前原生产物没有字节审批和发布管线，治理模式明确拒绝附件及语音发送，不能将路径文字的放行当作文件授权。构建清单包含宿主补丁与适配器全文件摘要；治理模式在 Gateway 模块导入时再校验，漂移则进程拒绝接受 turn。

## M6.1a + M6.1b 已实现

- 独立受信控制 API 已能按 `key_id,session_id,message_id,run_id,generation` 创建 turn；原生通道 ingress 到该 API 的身份映射及签名接线仍待实现，现有 BFF 不提供这条聊天入口。Hermes 接收注入的 `MessageEvent.metadata.brokerrouter_turn` 中 `turn_id,run_id`，从宿主现场取得 `session_id,inbound_message_id,run_generation`，在历史、模型和工具调用前请求 `POST /v1/governance/turns/{id}/bind`。服务端在锁内重验 Key、Runtime 绑定、用户、turn 活性/到期和全部五个身份字段；不匹配返回 not found。
- 受管模式禁止 Hermes Gateway proxy，因为远程代理尚无法证明本地 turn 身份。Agent、模型、普通插件、工具参数和全局 turn 环境变量都不是受信来源。
- `tool_input` 强制门位于 Relay/执行 middleware、普通插件参数改写、scope 与 Hermes 本地 guardrail 之后，`_begin_tool_execution` 和真实 `execute` 之前。普通 hook fail-open 不会绕过此入口。
- `checking` / `waiting_approval` 保持同一 turn、operation key 和参数摘要，原工具 worker 使用 Hermes `human_wait_window` 持久等待。审批协议必须明示 `pending,current_turn_only,resume_requires_recheck=true` 和固定三个决定选项，否则失败关闭。
- 首次待审批由 Hermes 原生 adapter 通道发出审批 ID 和 `#approvals` 安全审阅链接。通知不携带证明、claim token、风险原文或决定能力；聊天里回复“同意”不会批准。通知在 SQLite 中以租约去重，明确失败重试，响应超时按不确定已发送处理，避免重复消息。真人决定仍只由独立审批 BFF 提交。
- 本地 SQLite 检查点强制 WAL、`synchronous=FULL`、`0600` 和 5 秒锁等待。早期 v2 保存身份/参数摘要、operation、状态、加密 claim token 与通知租约；R1 追加最终结果密文、内容摘要、模型绑定及原生调用事实，原地升级既有数据库，不保存明文工具参数或结果。
- `ready` 后先持久化 256-bit claim token，只有首个 `dispatch_authorized=true` 进入工具。claim 响应丢失、授权后重启、工具异常转 unknown；工具已返回但回执失败只补回执，不重派发，也不伪造丢失结果。

固定 Hermes 0.21.1 无网络临时容器的 26 项宿主/状态机/安装合约通过；Rust 的 turn 绑定 PostgreSQL 事务与真实 HTTP 路由合约通过。测试未调用模型、未产生供应商费用、未执行真实外部副作用。

## 不可直接套用的机制

1. `agent/tool_executor.py::_pre_tool_block` 捕获插件异常后返回允许路径；普通 pre-tool 插件不是强制治理入口。
2. Hermes 本地 `request_tool_approval` 的 approved 不是 Brokerrouter 的请求绑定真人证明，不能 mint grant，不能继承跨轮 always-allow。
3. 其他插件可改写参数。网关检查、真人所见摘要、claim 与真实最终参数必须一致，治理门之后禁止第二次改写。
4. `_flush_session_db_after_tool_progress` 在工具返回后持久化。执行副作用到结果落盘之间仍有崩溃窗口，不能把没有结果当成没执行并重跑。
5. `gateway/hooks.py` 通知异常不中断宿主，只可观测，不可当安全门禁。

## 下一开发包（顺序不可交换）

1. 收口最终文本隔离与当前策略释放，再完成真实二进制产物和会话发布边界。文本输出不使用新的工具执行 claim；未批准结果不得先进入模型历史、普通消息或可下载产物，`pending` 不伪装成普通失败 tool message。
2. 用持久事件驱动原检查重放，重验用户、Key、绑定、turn/代次、工具/模型硬权限和最新策略。反馈关闭旧代次并新建受信 turn，不原地修改待执行参数。
3. 在固定新镜像中补齐串行、并行、委派、MCP、内联和异步工具零费用矩阵，对暂停点写盘前后、决定后、claim 发送前/响应后、副作用后/回执前逐点终止进程。
4. 完成受管镜像 digest、出口限制、滚动/回滚、多副本租约、SLO/RPO/RTO 认证；再安装到非生产 Hermes 实例。
5. OpenClaw 仍需明确项目与版本；不把 Hermes 适配自动宣称为所有 Claw 的兼容实现。

真实 IdP/签名授权、受管部署、生产多副本和人工读屏验收继续列于 LIVE-05/06/21。上述剩余开发优先使用确定性无外部副作用工具，不需要供应商 Key 或新增付费预算。
