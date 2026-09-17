# M5.5：逐节点任务与调用治理

档位 `execution-traces-v1`，2026-09-16。此包为真实网关治理事实的只读工作台；不替代 Hermes 原生检查点、执行器或真人审批，也不声称已有运行时 DAG。

## 权限与部署

复用 `serve-console` / `brokerrouter_console`、既有 OIDC BFF 和 `br-console+jwt` 请求绑定证明；新增显式 `traces_read`。此权限面向组织审计人员，允许本组织的任务元数据，不自动赋予仅本人风险原文访问、决定、执行、ack、预算修改权限。Agent Key、`br-human+jwt`、配置签名不可调用这些报表端点。

在备份与维护窗口内停止旧版服务，应用 `0019_execution_traces.sql`，用受信迁移身份执行 `deploy/permissions.sql`，同版升级 console/BFF/前端及其他要求迁移 checksum 一致的服务。DDL 包含常规索引建立，需先在生产等价数据上测维护时间；不能在线偷偷改已应用迁移。回退优先回退发布流量，不删除治理事实、不重写已应用 checksum；恢复数据库必须按已演练备份方案进行。

```sh
brokerrouter console-permission-set --tenant <TENANT_UUID> --user <USER_UUID> \
  --permission traces_read --enabled true --reason '批准组织任务审计只读访问'
```

此命令是受信运维入口，不给 Agent 运维凭据。用户/组织/读权限于读取前及事务提交后重新校验。每次报表为 repeatable-read、read-only 事务，继承 5 秒语句/1 秒锁等待上限；超时不返回部分报告。

## API

浏览器 `POST /bff/v1/traces/turns/{query|detail|items}`；内部固定映射到 `/console/v1/traces/turns/...`。CSRF/Origin/会话检查、上游响应 2 MiB 限制及响应后会话复核沿用 BFF。错误和成功均 no-store。

| 端点 | 请求 | 返回 |
| --- | --- | --- |
| `query` | `filter:{start,end,user_id?,key_id?,turn_id?,runtime_id?,agent_id?,session_id?,run_id?,state?}`, `before?:{created_at,id}`, `limit:1..50` | 轮次元数据、观测时间、`next_before` |
| `detail` | `turn_id` | 权威轮次、节点/审批分类计数、业务/护栏费用与未知费用/预留 |
| `items` | `turn_id,section,operation_id?,after?,limit` | `nodes/requests/approvals/events/checks` 之一、同类型行、下一游标 |

`query` 最多 31 天、半开区间，按轮次创建时间；所有条件 AND 精确匹配，不接受客户端 tenant。详情不受列表时间过滤裁剪，显示整个轮次的迟到入账。时间游标必须原样传递（含微秒），按 created_at/id 倒序。`checks` 必须限定节点，`nodes` 禁止节点过滤；其余类别可按同租户同轮节点精确筛选。非法游标/未知字段拒绝。

`items` 一般最多 50，审批最多 5 张；每张最多 256 个实际 grant。最多 3 个文本 attempt / request。子记录批量一次查询，无逐行 N+1 查询。JSON 只投影元数据，不包含原始请求、operation_key、反馈、回执内容、规则/策略快照、供应商地址、资产 URL、凭据或任意事件 data。

事件第一页记录已提交 `event_clock`，后续 `after:{kind:"event",sequence,through}` 固定这个上界；新事件只有刷新后可见。不把未来序号、负数或非规范整数字符串作为合法游标。事件类型/核对结果白名单，未知历史类型只显示 `unrecognized`；关联 ID 使用同租户/同轮 JOIN，不把任意 JSON 字符串强制转 UUID。`guardrail.finished` 通过不可变 evaluation 归属找到节点。只读视图不推进 Runtime ack。

## 状态与人民币口径

- 轮次 `active/completed/cancelled/failed`；active 到期派生 `expired`，保留 stored state。身份绑定/用户/Key 当前有效性另列，不意味着仍有模型授权或执行权。
- 节点保留 recorded state；执行租约过期显示 `execution_unknown`，不写回或自动重发工具。节点 ready 只表示可继续校验，不表示已执行。
- 模型 `succeeded` 是调用/记账结果，不等于 post/media_output 放行。审批 `approved` 是历史本人决定，不等于可执行；grant 精确显示审批实际新增的护栏版本，同轮且仍受过期/撤权/当前策略约束。
- 请求是唯一计费单位。按 immutable request.turn 或 judge evaluation→operation→turn 归属分组；业务模型和护栏检测分开，pre/post、重试 attempt、轮询和审批不会再加钱。金额是配置价格账本的 CNY 十进制字符串，不是供应商已核对实付发票；unknown cost/tokens 不伪装为零。预留与已知费用分开。
- 节点和调用的关联使用服务器既有精确契约：text pre=同 Key/turn/model 的 idempotency key，post=request UUID；media pre=`media:<job UUID>`，media output=delivery FK，judge=evaluation FK。关键字段仅在安全视图内部使用，不对浏览器泄漏；这些是网关关联，不是运行时父子步骤证据。
- Runtime 事件消费确认不证明副作用完成。没有观测到节点、attempt 或 token 时直接显示缺失，不补造成功/延迟。

## 页面

入口 `#traces`，筛选轮次→详情→节点/调用/审批/事件，节点可下钻护栏与关联调用；仅所属真人显示“前往本人审批”链接，实际审批页仍独立授权。切换查询/类别、撤权、退出或隐藏页面沿用立即清理旧数据规则，不缓存到 localStorage。无自动付费检测、审批或恢复按钮。

沿用现有控制台组件和 ui-styling 可访问性规则：语义表格、原生表单/焦点、局部横向滚动、320/768/1440px、200% 字号和 axe。自动化/截图不代替人工读屏。

## 验收边界

本包本地 PostgreSQL/HTTP/受限角色/浏览器证据汇总于 milestones；正式 PR 还必须通过全量 CI。LIVE-21 保留真实 IdP/TLS/组织权限/多副本撤权、0019 大数据索引窗口/恢复、查询计划/规模/SLO/留存、人民币供应商对账与人工读屏认证。Hermes M6.1a/b 已增加受信 turn 与工具输入暂停/审批通知，但输出/产物隔离发布、生产恢复认证和 OpenClaw 仍属 M6 未完成能力，不能把报表或本地合约视为整体原生恢复已完成。

下一接入的已读源码约束见 [Hermes 0.21.1 接入边界](m6-hermes-native-integration.md)。
