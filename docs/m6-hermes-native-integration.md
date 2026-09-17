# Hermes 原生治理接入：已核验边界与实施顺序

状态：2026-09-17 完成 M6.1a 工具输入强制门、加密持久检查点与一次性 claim 适配器；**未安装到用户运行实例，工具输出/产物发布闭环和生产恢复认证仍未完成**。目标上游为用户指定的 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)。实现位于 `adapters/hermes`，当前结论只覆盖下文明确范围，不是整套原生治理已通过生产认证的声明。

## 固定目标

当前本地源码 `/opt/hermes` 的包元数据版本为 `0.21.1`，容器 `d5dd6c405e54` 的镜像内容 ID 为 `sha256:59e5509d41aee6a1e86724e990c376463dba01968d53127e8cd20df74952f8b0`，OCI revision 为 `67764dc0863349a384c16425e73ee8571f3a94b7`。该提交可在上游仓库解析，三个关键文件与上游固定提交逐字节 SHA-256 一致。未读取环境变量、数据挂载内容、密钥和用户会话，也未修改、重启或热补丁当前容器。

实际读取的关键代码摘要（SHA-256）：

| 文件 | 摘要 |
| --- | --- |
| `agent/tool_executor.py` | `1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9` |
| `agent/turn_tool_round.py` | `3e6cf5c94125dd75e2fb95cda6fe218a456187d9385a2a8005aa2f3276442d57` |
| `gateway/hooks.py` | `1baa2219a5c233891066be2149c5e89d98ef2b2a38bd006b3b53db7a6d53ad0b` |

## M6.1a 已实现范围

- 严格 overlay 安装器只接受上述 `tool_executor.py` 摘要和唯一派发锚点，原子生成备份、补丁和文件摘要清单；源码漂移、锚点变化或安装后改写均拒绝。要求构建新镜像，不支持对运行容器热修补。
- 强制门位于 Relay/执行 middleware、普通插件参数改写、scope 与 Hermes 本地 guardrail 之后，`_begin_tool_execution` 和真实 `execute` 之前。普通 hook fail-open 不会绕过此入口；治理模式缺配置、适配器导入/数据库错误转为 Hermes 结构化 `halt`，不会落到真实派发。
- 最终参数通过 HTTPS 发送到现有 `/v1/governance/checks`。`checking`/`waiting_approval` 保持同一个 turn、operation key 和参数摘要，线程持久等待并重查；用户批准后仍由网关重新检查 Key、绑定、turn、策略和 grant。
- 本地 SQLite 检查点启用 WAL、`synchronous=FULL`、`0600` 和 5 秒锁等待；只保存不可逆身份/参数摘要、网关 operation ID、状态与 AES-256-GCM 加密 claim token，不保存原始工具参数或结果。目录必须禁止组/全局写入。
- `ready` 后先提交随机 256-bit claim token，只有首个 `dispatch_authorized=true` 进入工具。claim 响应丢失、授权后重启、工具异常转 `execution_unknown`；工具已返回但回执失败保留 `effect_recorded`，重启只补回执且不重派发，也不伪造丢失结果。
- 本地无网络临时 Hermes 镜像完成 15 项合约：固定源码安装/漂移拒绝、最终改写参数强制门、结构化停止、批准恢复、加密落盘、4xx 立即停止、claim 丢响应、授权后重启、工具异常、回执丢失及不重复派发。没有模型调用或付费请求。

M6.1a 暂停发生在原工具 worker 内，因此同一存活进程可在批准后继续原调用；重启后相同 tool call 可恢复检查，但任何已取得执行权的崩溃窗口一律进入 unknown。当前尚未把 `tool_output`/`artifact` 的准入与 Hermes 会话结果落盘做成同一发布回执，也未把受信 Brokerrouter turn 注入和审批卡片接到实际 Gateway 通道。这些属于 M6.1b，不得用本包测试宣称输入输出全闭环。

## 不可直接套用的机制

1. `agent/tool_executor.py::_pre_tool_block` 捕获插件异常后返回允许路径；文档亦明确 hook failures never block。普通 pre_tool_call 插件不是强制治理入口。插件缺失、导入失败、网关超时都必须禁止新派发，而非靠插件自行“尽量返回 block”。
2. `hermes_cli/plugins.py::_resolve_block_from_details` 的 approve 使用 Hermes 本地 `request_tool_approval`；虽然该分支失败默认阻断，但本地 approved 不是 Brokerrouter 的请求绑定真人证明，不能直接 mint grant，也不能继承跨轮的 always allow。
3. 其他插件可 modify 参数。网关检查、真人所见摘要、执行 claim 与真实最终参数必须一致；治理入口需在所有参数改写之后、实际 dispatch 之前，执行时禁止第二次改写。
4. `_dispatch_authorized_once` 是实际 dispatch 边界；`_flush_session_db_after_tool_progress` 在工具返回后持久化，再发送完成回调。执行副作用到结果落盘之间仍有崩溃窗口，不能把没有结果当成没执行并重跑。
5. `gateway/hooks.py` 生命周期通知异常不会中断宿主；可用于观察，不能当作安全门禁。文件 checkpoint 与工具副作用检查点不是同一概念。

## M6.1 后续实施顺序（顺序不可交换）

1. 将 M6.1a overlay 固定到可重建 Hermes 镜像 digest，并在进程启动时校验安装清单；补齐串行、并行、委派、MCP、内联工具和异步工具的实际零费用工具矩阵。缺任何强制路径即不以治理模式启动。
2. 由 Hermes Gateway 受信入口绑定 `agent._brokerrouter_turn_id`，不能从模型、普通插件、工具参数或全局环境变量接受 turn；接入本人审批卡片和安全审阅链接，Agent/插件不持有审批签名密钥。
3. 将 `tool_output`/`artifact` 检查、claim 与 Hermes 会话结果持久化组成发布事务边界。pending/checking 不伪造成普通失败 tool message；并行已派发节点独立记账/回执，不假装撤销。
4. 用户决定后通过持久事件驱动原检查重放，重验用户、Key、绑定、turn/代次、模型硬权限和最新策略；同一轮只跳过选中的护栏版本。反馈关闭旧代次并新建受信轮次，不原地修改待执行参数。
5. claim token 先落盘；仅首次 `dispatch_authorized=true` 可派发。claim 响应丢失、进程崩溃或副作用回执缺失均进入 unknown，要求外部证据/幂等查询或人工核对；重复 claim 返回 false 不重派发。工具输出与产物另外检查，批准输入不等于批准输出。
6. 原生模型调用必须保留请求幂等键/turn，409/checking/approval_required 不可落入 SDK 自动模型回退或换 ID 重试。非流式能力先闭环；当前网关尚未实现 SSE/Responses，不对客户端谎称兼容。

## 不得跳过的零费用故障验收

- 正常、批准选中/未选中、同轮/跨轮/版本升级、拒绝/反馈、审批过期、Key/成员/绑定撤销；本人界面需实际看到审阅和决定。
- 暂停点写盘前后、审批决定后、claim 发送前/响应后、工具副作用后/回执前逐点终止宿主；重启后无重复外部副作用、无未批准输出释放。
- 并行两个工具、父子委派、插件改写参数、插件缺失/抛错、网络超时/429/5xx、断线重连、重复事件与多实例租约竞争。
- 原生 UI、BFF、网关、持久会话事实及 M5.5 节点/调用/CNY 报表一致；fixture 永不证明生产端点安全或生产恢复能力。

真实 IdP/签名授权、受管容器发布/出口限制、可用性 SLO/RPO/RTO 与生产多副本演练继续列 LIVE-05/06/21。此阶段先用确定性无外部副作用工具，不需要供应商 Key 或新增付费预算。Claw 仍需用户指定具体项目/版本，不能把 Hermes 适配自动宣称为兼容所有 Claw。
