# Hermes 0.21.1 原生治理适配器

目标宿主固定为 NousResearch/hermes-agent 提交
`67764dc0863349a384c16425e73ee8571f3a94b7`。适配器位于所有 Relay/执行 middleware、
普通插件参数改写、scope 与 Hermes 本地 guardrail **之后**，真实工具 dispatch **之前**。

当前包完成两个相互依赖的强制边界：

- Gateway 在历史读取、首次模型和工具调用之前，用真实 Hermes `session_id` / inbound
  `message_id` / `run_generation` 与受信 BFF 注入的 `turn_id` / `run_id` 向 Brokerrouter 做精确绑定校验。
  缺失、过期、Key/成员/绑定撤销、任一身份不一致或网络不确定均在 Agent 运行前失败关闭。
- `tool_input` 门为同一 Hermes tool call 形成稳定 operation key，最终参数先送 Brokerrouter 检查；
  `checking`/`waiting_approval` 时工作线程持久暂停并重放同一检查。首次等待会通过 Hermes 原生通道
  发出安全审阅链接与审批 ID；聊天中回复“同意”不会产生 grant。`ready` 后先持久化随机 claim
  token，只有首次 `dispatch_authorized=true` 执行。

claim 响应丢失、授权后重启、工具异常和回执失败都不自动重派发。原始参数不写适配器数据库，claim
token 使用 AES-256-GCM 加密。审批通知有独立持久租约：明确失败可重试，超时的不确定发送按已发送处理，
避免重启后重复打扰用户。

## 构建镜像

不要在运行容器中热补丁。构建新镜像时复制 `adapters/hermes`，然后：

```bash
python /build/brokerrouter-hermes/install.py --source /opt/hermes
python /build/brokerrouter-hermes/install.py --source /opt/hermes --apply
PYTHONPATH=/opt/hermes python -m unittest discover -s /build/brokerrouter-hermes/tests -v
```

首次命令同时核验 `agent/tool_executor.py`、`gateway/run_turn.py` 和
`gateway/run_turn_runner.py` 的固定源码摘要；`--apply` 原子写入 overlay、保存固定源码备份并
生成安装清单。安装中断只允许从精确的“原始/已审计补丁”混合态恢复；任何其他漂移都拒绝。治理模式在
Gateway 模块导入时重新校验三个宿主文件、适配器全文件集与清单，不匹配则进程无法接受 turn。镜像发布必须
固定内容 digest，不使用浮动 tag。

治理模式必须由受管部署设置：

```text
BROKERROUTER_HERMES_GOVERNANCE=required
BROKERROUTER_BASE_URL=https://gateway.internal.example
BROKERROUTER_HERMES_APPROVALS_URL=https://brokerrouter-console.example
BROKERROUTER_VIRTUAL_KEY=<专属虚拟 Key 的 Secret 引用注入值>
BROKERROUTER_HERMES_STATE_DB=/opt/data/brokerrouter/governance.db
BROKERROUTER_HERMES_STATE_KEY=<独立 32 字节随机密钥的 64 位 hex 注入值>
```

`BROKERROUTER_HERMES_STATE_DB` 的父目录必须为非组/全局可写目录；数据库强制 `0600`、WAL、
`synchronous=FULL`。等待复用 Hermes 原生 session-scoped `human_wait_window`，
`BROKERROUTER_HERMES_APPROVAL_WAIT_SECONDS` 不得大于 Hermes `approvals.timeout`（默认均为 300 秒）。
仅本机测试可显式设置 `BROKERROUTER_HERMES_ALLOW_LOOPBACK_HTTP=true`。

受信入口必须先由 BFF 创建 turn，再在对应 `MessageEvent.metadata` 注入：

```json
{"brokerrouter_turn":{"turn_id":"<uuid>","run_id":"<BFF run id>"}}
```

`session_id`、`message_id` 和 `generation` 必须由 Hermes 宿主现场取值，不接受 metadata 传入。Agent、模型、普通插件和
工具参数均不能设置这些字段。受管模式禁止 Hermes Gateway proxy，因为远程进程无法证明本地 turn 身份。

## 尚未宣称完成的边界

本包不会热修改用户当前容器，也不声明生产部署已认证。当前通知是 Hermes 原生通道中的安全链接，不是在聊天
消息内直接签署的批准按钮；决定仍只能由现有本人审批 BFF 提交。`tool_output`/`artifact` 与 Hermes 会话结果的隔离发布事务、
真实进程逐点终止/多实例/工具矩阵、受管镜像滚动与 OpenClaw 适配仍属后续包。在完成前，不能宣称完整输入输出治理闭环。
