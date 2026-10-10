# Hermes 0.21.1 原生治理适配器

目标宿主固定为 NousResearch/hermes-agent 提交
`67764dc0863349a384c16425e73ee8571f3a94b7`。受管模式在原生工具入口绑定调用身份，
最终参数改写、scope 与本地 guardrail 后检查输入，在真实 dispatch 前再核对参数摘要。
可自行发布原文的可选 Relay/请求/执行 middleware 暂无认证白名单，受管模式跳过这些入口。

当前开发包含以下强制边界，完整通道、二进制产物及生产认证状态另行记录：

- Gateway 在历史读取、首次模型和工具调用之前，用真实 Hermes `session_id` / inbound
  `message_id` / `run_generation` 与受信 ingress 注入的 `turn_id` / `run_id` 向 Brokerrouter 做精确绑定校验。
  缺失、过期、Key/成员/绑定撤销、任一身份不一致或网络不确定均在 Agent 运行前失败关闭。
- `tool_input` 门为同一 Hermes tool call 形成稳定 operation key，最终参数先送 Brokerrouter 检查；
  `checking`/`waiting_approval` 时工作线程持久暂停并重放同一检查。首次等待会通过 Hermes 原生通道
  发出安全审阅链接与审批 ID；聊天中回复“同意”不会产生 grant。`ready` 后先持久化随机 claim
  token，只有首次 `dispatch_authorized=true` 执行。
- 工具最终文本在核心 observation 与提示加工后加密冻结；普通完成日志、spill、模型历史和会话
  写入前，调用服务端当前输出策略释放。恢复只重检冻结文本，不再次派发或加工正文。具体状态及线协议
  见 [结果契约](../../docs/m6-hermes-tool-results.md)。
- 原生附件发送、自动及显式 TTS 目前明确返回不支持，且不读取文件、下载远端资源或调用 TTS 供应商。
  文件字节隔离、扫描、本人审阅及发布管线完成后，才按格式逐项开放；路径或 URL 的文本审批不能授权附件。
- 原生最终文本发送复用 Hermes 持久发送台账：正文摘要及目标不可变，同一发送只由一个实例领取。
  发送失败、取消或确认丢失保留不确定状态，不自动重发；明确成功的回执用于抑制重复发送。
  该台账约束发送恢复，最终助手文本的服务端治理授权及会话写入边界继续收口。

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
`gateway/run_turn_runner.py`、`model_tools.py`、`tools/registry.py`、`gateway/platforms/base.py`、`gateway/run_voice.py` 、`gateway/delivery_ledger.py`、`agent/agent_runtime_helpers.py` 和 `agent/turn_api_call.py` 的固定源码摘要；`--apply` 原子写入 overlay、保存固定源码备份并
生成安装清单。安装中断只允许从精确的“原始/已审计补丁”混合态恢复；任何其他漂移都拒绝。治理模式在
Gateway 模块导入时重新校验十个宿主文件、适配器全文件集与清单，不匹配则进程无法接受 turn。镜像发布必须
固定内容 digest，不使用浮动 tag。

治理模式必须由受管部署设置：

```text
BROKERROUTER_HERMES_GOVERNANCE=required
BROKERROUTER_BASE_URL=https://gateway.internal.example
BROKERROUTER_HERMES_APPROVALS_URL=https://brokerrouter-console.example
BROKERROUTER_VIRTUAL_KEY=<专属虚拟 Key 的 Secret 引用注入值>
BROKERROUTER_HERMES_STATE_DB=/opt/data/brokerrouter/governance.db
BROKERROUTER_HERMES_STATE_KEY=<独立 32 字节随机密钥的 64 位 hex 注入值>
BROKERROUTER_HERMES_GATEWAY_CA_FILE=/run/secrets/gateway-ca.pem
```

`BROKERROUTER_HERMES_STATE_DB` 的父目录必须为非组/全局可写目录；数据库强制 `0600`、WAL、
`synchronous=FULL`。等待复用 Hermes 原生 session-scoped `human_wait_window`，
`BROKERROUTER_HERMES_APPROVAL_WAIT_SECONDS` 不得大于 Hermes `approvals.timeout`（默认均为 300 秒）。
仅本机测试可显式设置 `BROKERROUTER_HERMES_ALLOW_LOOPBACK_HTTP=true`。
`BROKERROUTER_HERMES_GATEWAY_CA_FILE` 可选，只为此 HTTP client 追加私有 CA，未配置则使用 HTTPX
默认公开信任根。文件须为绝对路径、非空普通文件、最多 1 MiB/128 张 CERTIFICATE PEM；非法配置阻止
启动。保持证书链与主机名校验，不读取环境代理或全局 SSL CA 覆盖，不跟随重定向、不自动重试。

受信入口必须先通过独立控制 API 创建 turn，再在对应 `MessageEvent.metadata` 注入；现有 BFF 尚未
提供原生聊天 ingress，身份映射及签名接线属于 R1 部署工作：

```json
{"brokerrouter_turn":{"turn_id":"<uuid>","run_id":"<BFF run id>"}}
```

`session_id`、`message_id` 和 `generation` 必须由 Hermes 宿主现场取值，不接受 metadata 传入。Agent、模型、普通插件和
工具参数均不能设置这些字段。受管模式禁止 Hermes Gateway proxy，因为远程进程无法证明本地 turn 身份。

## 尚未宣称完成的边界

本包不会热修改用户当前容器，也不声明生产部署已认证。当前通知是 Hermes 原生通道中的安全链接，不是在聊天
消息内直接签署的批准按钮；决定仍只能由现有本人审批 BFF 提交。工具最终文本已有服务端当前策略释放；
助手最终文本、真实二进制产物和 Hermes 会话结果的完整隔离发布、逐点终止/多实例矩阵、
受管镜像滚动与 OpenClaw 适配继续开发。在完成前，不能宣称完整输入输出治理闭环。
