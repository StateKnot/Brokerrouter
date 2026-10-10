# Hermes 原生治理接入：已核验边界与实施顺序

状态：2026-10-11，工具与最终助手文本服务端本地验收已通过；十九源码原生最终发布包通过固定完整宿主的 212 项回归，实际分离服务及原生通道部署正在验收。契约见 [工具文本结果](m6-hermes-tool-results.md) 和 [最终助手文本](m6-hermes-final-publications.md)。此前 M6.1a/M6.1b 已实现工具输入门、受信 turn 绑定、原生审批通知及完整性校验。**未安装到用户运行实例；二进制产物与文本的耦合发布、独立生产器和完整生产恢复认证仍未完成**。目标上游为用户指定的 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)，实现位于 `adapters/hermes`。

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
| `gateway/delivery_ledger.py` | `b2656cd6429fb848f8df405fd7bb5e50f2ea6cb0620141c59ccccc3f7c314ab7` |
| `agent/agent_runtime_helpers.py` | `328f3f8ee5e6956ae530007ec0c22457e3bb681b6125c918cd43ea7124b1767e` |
| `agent/turn_api_call.py` | `5dead01b498131df2ee987e431ded73ca21313ef1a52633e9aa802efc2aa00b5` |
| `agent/turn_final_response.py` | `ff414924aa9ffc30fb64b91a602f4621307f22a9f6e5aa3f88e916eabbadbce1` |
| `agent/turn_finalizer.py` | `4b79fa4eeb325095b4664df6085df54385d0f38cb6e610c6145c91680f46badd` |
| `agent/session_persistence.py` | `83ee5c9f48a7790deb3f771e45990180e1fd714c7b1fabc24303944f17773a1b` |
| `agent/stream_delivery.py` | `38d3fda9b9ab09048c14154588e30fa695db89db20efb06fdeb6ffdbf00721c3` |
| `gateway/session_lifecycle.py` | `fc8a17d5dd5cbc40fd51abf2b4c75b6c5dd5c1bf1cecbd5f7f4deea8e1f58071` |
| `agent/auxiliary_client.py` | `ca46e494e1e518ca793c65c3b4707ae2f3f38c27ee142b2076ff050978d1065b` |
| `agent/title_generator.py` | `1eb801e5f020bb9f32373ed52afb68ab64e384618649260fdf82b12370c13d4b` |
| `agent/chat_completion_helpers.py` | `2e5308b231ac50cb52b90439f17d7a5e05618d46dd23cffcf45cafb5d77e3073` |
| `gateway/run_inbound.py` | `d20049510d55674bb60d20cb48f09b71f829880850467b18a9c55a8188e2f9f5` |

overlay 安装器仅接受这十九个完整摘要和全部唯一锚点，支持从精确的中断安装态继续，任何其他混合状态拒绝。R1 覆盖实际串/并行派发中早于最终放行的 post/transform hook、原始异常日志及平台附件发送、自动/显式语音入口。当前原生产物没有完成文本与文件的耦合授权和独立生产器管线，治理模式明确拒绝附件及语音发送，不能将路径文字的放行当作文件授权。构建清单包含宿主补丁与适配器全文件摘要；治理模式在 Gateway 模块导入时再校验，漂移则进程拒绝接受 turn。

2026-10-11 的八源码完整宿主验证为 169 项通过、0 跳过。新增发送台账复用 Hermes `state.db`，固定正文摘要与真实目标、原子领取和发送回执；发送后确认丢失保留 `delivery_unknown`，禁止自动重发。包括真实进程退出、并发发送、不可变事实及 `/steer /hello` 被原生入口转成 Agent turn 的回归：命令前缀不能推断为免治理的控制消息。最终助手文本服务端授权、会话落盘与新代次 CAS 恢复仍未关闭；原有七源码 137 项结果保留为历史证据。

此前十源码包在同一固定完整宿主上通过 188 项测试、0 跳过，45.378 秒；十个真实模块导入也通过。模型请求使用已绑定 turn 与持久化逻辑请求身份，仅认实际 `x-brokerrouter-request-id`，重试和重启不分配新身份重复付费。只有 SDK 已解析完成的 `stop` / `tool_calls` 响应可以登记来源；裸响应头、截断 SSE、`length`、过滤和旧 function finish 均不能成为已完成来源。四项真实红证据及原/补丁/适配器摘要见 [十源码证据](evidence/hermes-native-ten-2026-10-11.json)。此包认证固定宿主与本地零费用 TLS 工具链；来源事实不能代替独立最终文本发布授权，完整工作台审批、最终正文落盘和原生通道恢复仍在后续包验收。

## 十九源码最终发布包

固定完整宿主的 212 项回归通过，0 跳过，69.482 秒；十九个真实模块导入和安装器 inspect → apply → inspect 均通过。原/补丁字节、适配器、测试与红绿日志摘要见 [十九源码证据](evidence/hermes-native-nineteen-2026-10-11.json)。镜像只保证登记的关键源码匹配固定上游提交，不把整个基础镜像冒称为该官方提交。

最终文本在原生转换完成后冻结于加密候选；仅保留本次轮次的 working messages，不重写旧历史。恢复先核对原 turn 和宿主所有者代次，再取得当前服务端发布权限。普通 user/final 历史与候选历史游标在同一 SessionDB 事务提交；发送继续复用原生不可变目标账本。流式 delta、推理、中间文字、进度正文、自动标题、辅助模型和轨迹不能提前落入普通历史或通道。附件与 TTS 仍保持关闭。

真实 `/refine` 回归曾把待审批缓存传给后台评审，`/save` 曾在拒绝附件前写文件；现已在真实 handler 前拒绝这些尚未接治理的入口。治理 required 模式关闭 `review/refine/bg/btw/compress/retry/goal/subgoal/heartbeat/loop/moa/save/debug/diff/memory/skills` 及对应别名，覆盖空闲与忙碌入口。`status/context/help` 只返回已验证的安全状态；`steer/queue` 继续进入模型治理。off 模式保留原生行为。命令路径将来只有完成同等来源、隔离和发布认证才可开放。

上述宿主包不代表实际服务、外部 IdP、消息通道或文件生产器认证。真实部署必须使用该摘要及对应新核心镜像另行验收，R1 尚未关闭。

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
