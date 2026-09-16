# Hermes 原生治理接入：已核验边界与实施顺序

状态：2026-09-16 完成用户本地 Docker 源码只读核查；**适配器尚未实现/安装，未恢复或执行真实用户任务**。目标上游为用户指定的 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)。这是一份依据实际宿主源码制定的实施门禁，不是已完成的认证声明。

## 固定目标

本地源码 `/opt/hermes` 的包元数据版本 `0.21.1`，镜像内容 ID `sha256:01fbb06434eedb07908e14218c27a4e6147b6a84af2b2191025c8aa18b557cfa`。镜像没有 `.git`，不能把版本号当成上游 commit；上线前需取得对应镜像构建来源/提交、可重建锁文件及变更列表，或用正式固定构建替换候选。未读取环境变量、数据挂载内容、密钥和用户会话，也未修改/重启容器。

实际读取的关键代码摘要（SHA-256）：

| 文件 | 摘要 |
| --- | --- |
| `agent/tool_executor.py` | `1610a0893588cbc067aaf84b3d019a683cd83ba0fdb92d69baf4f7b9e6d121b9` |
| `agent/turn_tool_round.py` | `3e6cf5c94125dd75e2fb95cda6fe218a456187d9385a2a8005aa2f3276442d57` |
| `gateway/hooks.py` | `1baa2219a5c233891066be2149c5e89d98ef2b2a38bd006b3b53db7a6d53ad0b` |

## 不可直接套用的机制

1. `agent/tool_executor.py::_pre_tool_block` 捕获插件异常后返回允许路径；文档亦明确 hook failures never block。普通 pre_tool_call 插件不是强制治理入口。插件缺失、导入失败、网关超时都必须禁止新派发，而非靠插件自行“尽量返回 block”。
2. `hermes_cli/plugins.py::_resolve_block_from_details` 的 approve 使用 Hermes 本地 `request_tool_approval`；虽然该分支失败默认阻断，但本地 approved 不是 Brokerrouter 的请求绑定真人证明，不能直接 mint grant，也不能继承跨轮的 always allow。
3. 其他插件可 modify 参数。网关检查、真人所见摘要、执行 claim 与真实最终参数必须一致；治理入口需在所有参数改写之后、实际 dispatch 之前，执行时禁止第二次改写。
4. `_dispatch_authorized_once` 是实际 dispatch 边界；`_flush_session_db_after_tool_progress` 在工具返回后持久化，再发送完成回调。执行副作用到结果落盘之间仍有崩溃窗口，不能把没有结果当成没执行并重跑。
5. `gateway/hooks.py` 生命周期通知异常不会中断宿主；可用于观察，不能当作安全门禁。文件 checkpoint 与工具副作用检查点不是同一概念。

## M6.1 实施包（顺序不可交换）

1. 固定宿主构建与强制执行门：对已核对源文件打最小可审计宿主补丁/受信 middleware；启动检查版本、摘要、适配器注册完整性。覆盖串行/并行、委派、MCP、内联工具和异步工具路径；缺任何强制路径即不以治理模式启动。不注入线上运行容器，先独立零成本测试镜像。
2. 以 Python 实现 Hermes 宿主适配（保持网关 Rust）：复用其持久会话库，独立迁移保存 turn、generation、native session/run/tool_call ID、最终参数摘要、加密检查点、pending approval、claim token、receipt/outbox。checkpoint 提交先于 UI 暂停提示与网关 ack；禁止原始参数进入通用日志。
3. 将 pending/checking 作为可恢复的暂停状态，不伪造成普通失败 tool message 后继续规划。停止后续依赖节点；并行已派发节点继续独立记账/回执，不假装撤销。原生界面只展示批准/拒绝/意见意图，由受信真人通道验证本人身份、短期审阅和精确快照；Agent/插件不持有审批签名密钥。
4. 用户决定后通过持久事件驱动原检查重放，重验用户、Key、绑定、turn/代次、模型硬权限和最新策略；同一轮只跳过选中的护栏版本。反馈关闭旧代次并新建受信轮次，不原地修改待执行参数。
5. claim token 先落盘；仅首次 `dispatch_authorized=true` 可派发。claim 响应丢失、进程崩溃或副作用回执缺失均进入 unknown，要求外部证据/幂等查询或人工核对；重复 claim 返回 false 不重派发。工具输出与产物另外检查，批准输入不等于批准输出。
6. 原生模型调用必须保留请求幂等键/turn，409/checking/approval_required 不可落入 SDK 自动模型回退或换 ID 重试。非流式能力先闭环；当前网关尚未实现 SSE/Responses，不对客户端谎称兼容。

## 不得跳过的零费用故障验收

- 正常、批准选中/未选中、同轮/跨轮/版本升级、拒绝/反馈、审批过期、Key/成员/绑定撤销；本人界面需实际看到审阅和决定。
- 暂停点写盘前后、审批决定后、claim 发送前/响应后、工具副作用后/回执前逐点终止宿主；重启后无重复外部副作用、无未批准输出释放。
- 并行两个工具、父子委派、插件改写参数、插件缺失/抛错、网络超时/429/5xx、断线重连、重复事件与多实例租约竞争。
- 原生 UI、BFF、网关、持久会话事实及 M5.5 节点/调用/CNY 报表一致；fixture 永不证明生产端点安全或生产恢复能力。

真实 IdP/签名授权、受管容器发布/出口限制、可用性 SLO/RPO/RTO 与生产多副本演练继续列 LIVE-05/06/21。此阶段先用确定性无外部副作用工具，不需要供应商 Key 或新增付费预算。Claw 仍需用户指定具体项目/版本，不能把 Hermes 适配自动宣称为兼容所有 Claw。
