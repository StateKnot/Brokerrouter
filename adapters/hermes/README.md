# Hermes 0.21.1 原生治理适配器

目标宿主固定为 NousResearch/hermes-agent 提交
`67764dc0863349a384c16425e73ee8571f3a94b7`。适配器位于所有 Relay/执行 middleware、
普通插件参数改写、scope 与 Hermes 本地 guardrail **之后**，真实工具 dispatch **之前**。

当前包完成 `tool_input` 原生强制门：同一个 Hermes tool call 形成稳定 operation key，最终参数先送
Brokerrouter 检查；`checking`/`waiting_approval` 时工作线程持久暂停并重放同一检查；`ready` 后先持久化
随机 claim token，再 claim，只有首次 `dispatch_authorized=true` 执行。claim 响应丢失、授权后重启、
工具异常和回执失败都不自动重派发。原始参数不写适配器数据库，claim token 使用 AES-256-GCM 加密。

## 构建镜像

不要在运行容器中热补丁。构建新镜像时复制 `adapters/hermes`，然后：

```bash
python /build/brokerrouter-hermes/install.py --source /opt/hermes
python /build/brokerrouter-hermes/install.py --source /opt/hermes --apply
PYTHONPATH=/opt/hermes python -m unittest discover -s /build/brokerrouter-hermes/tests -v
```

首次命令只核验固定源码摘要；`--apply` 原子写入 overlay、保存固定源码备份并生成安装清单。源码漂移、
锚点变化或二次修改均拒绝启动构建。镜像发布必须固定内容 digest，不使用浮动 tag。

治理模式必须由受管部署设置：

```text
BROKERROUTER_HERMES_GOVERNANCE=required
BROKERROUTER_BASE_URL=https://gateway.internal.example
BROKERROUTER_VIRTUAL_KEY=<专属虚拟 Key 的 Secret 引用注入值>
BROKERROUTER_HERMES_STATE_DB=/opt/data/brokerrouter/governance.db
BROKERROUTER_HERMES_STATE_KEY=<独立 32 字节随机密钥的 64 位 hex 注入值>
```

`BROKERROUTER_HERMES_STATE_DB` 的父目录必须为非组/全局可写目录；数据库强制 `0600`、WAL、
`synchronous=FULL`。等待复用 Hermes 原生 session-scoped `human_wait_window`，
`BROKERROUTER_HERMES_APPROVAL_WAIT_SECONDS` 不得大于 Hermes `approvals.timeout`（默认均为 300 秒）。
仅本机测试可显式设置 `BROKERROUTER_HERMES_ALLOW_LOOPBACK_HTTP=true`。

每个受管 Hermes turn 必须由受信入口设置 `agent._brokerrouter_turn_id`，值为 BFF 创建的当前
Brokerrouter turn UUID。缺失或格式错误直接停止工具，不允许 Agent、模型、普通插件或工具参数自行提供。

## 尚未宣称完成的边界

本包不会热修改用户当前容器，也不声明生产部署已认证。`tool_output`/`artifact` 在 Hermes 结果写入会话库
之后的发布回执、原生 Gateway 审批卡片、容器滚动升级和真实进程终止恢复演练属于后续包；在完成前，
受管环境只能把本包认定为工具输入与外部副作用派发门，不能宣称完整输入输出治理闭环。
