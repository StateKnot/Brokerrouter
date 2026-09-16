# M5.3d 护栏监控与治理统计

本包交付真实 PostgreSQL 统计 API、受限 BFF 路由和 TypeScript 监控页。只读观测，**不审批、不恢复节点、不调用模型**。不以代码完成替代生产认证。

## 统计契约

`guardrail_observations` 每个 `(tenant_id, operation_id, guardrail_id, version)` 一条。首次结果、来源、时间、即时耗时不可改；`approval_skipped_at` 只允许从空写入一次。重复 check、幂等请求、重复事件不增加分母。语义评估复用既有 `guardrail_evaluations` 的当前状态与不可变终态；不靠客户端再次轮询才看见检测完成。

| 指标 | 口径 |
| --- | --- |
| 去重检查 | 在筛选半开时间窗 `[start,end)` 内首次观测的操作 / 护栏版本数；不是 HTTP 请求数 |
| 通过 / 风险 / 检测失败 / 待检测 / 检测中 / 首次即跳过 | 互斥六类，合计等于检查总数。超时但尚未归档的 running 单列子集，不通过报表篡改运行状态 |
| 明确分类通过率 | passed / (passed + risk)，分母为空返回 null。检测失败、等待、批准跳过不伪装成通过 |
| 曾按批准跳过 | 某次 gate 使用过本轮批准的去重检查数，可与最初 risk/failed 重叠；批准后仍保留原始风险 |
| 即时均耗时 | live 的 always / contains_any / asset_scan 规则检查样本，包含批准查询，不含提交、语义模型等待、资产扫描引擎时间；无证据返回 null |
| 语义生命周期 | 已结束 evaluation 的 created_at → finished_at，含排队；不是纯供应商延迟或整个网关耗时 |
| 费用 | 与评估唯一关联的模型请求连接已有账本投影，分别给出 CNY 已知 / 未结算笔数 / 预留。整数微元聚合、Decimal 转字符串；不套用当前汇率重算，不等于供应商发票 |
| 操作状态 | 筛选命中检查关联的操作去重，展示快照当前状态；executing 超过 deadline 显示 execution_unknown，不重发。已关闭 / 过期轮次作为状态子集另列 |
| 审批单状态 | 上述操作关联的全部审批单当前状态，含政策更新产生的多张单。pending 按是否过期拆分，不能据此宣称有权继续 |
| 当前启用护栏 | 全租户当前配置数量，不跟随窗口/业务模型筛选；不是引擎健康、可用性或覆盖率 |

按业务模型 ID、虚拟 Key ID、护栏 ID、阶段精确匹配，各条件 AND。阶段支持 pre / post / tool_input / tool_output / artifact / media_output。模型 ID 是被治理业务模型，不是语义检测器的模型；runtime/agent 归因报表留到后续，不用可变运行时绑定伪造历史归属。

每日序列以首次观测日归档、状态和迟到结算随刷新更新，并非风险发生次数的历史快照。支持北京时间或 UTC；最多 31 天，跨时区可返回 32 个日期桶，无观测日期补零。概览与每页版本统计各有 observed_at；不同请求不保证同一快照，不展示假的跨页排名。

未成功提交的事务、配置解析失败、未选到任何护栏的操作、绕过网关的调用不在该观测分母中。批准服务、Runtime 接入与强制过网关仍是独立部署边界；不能由该页面推导“全局没有风险”。技术扫描阻断仍沿用原交付通路，本页 asset_scan 指治理规则读取扫描证据，不代表所有扫描失败都有一条 gate 观测。

## 权限与 API

新增独立 `monitoring_read`。`governance_read/write` 配置权限、`usage_read`、Agent Key、审批授权均不隐含该权限。运维 CLI `console-permission --permission` 支持该值，同时补齐之前遗漏在 CLI 枚举中的 governance_read/write。

沿用 serve-console（br-console+jwt，精确 POST/path/body 签名），BFF 固定路由：

| 后端 | 浏览器 | 请求 |
| --- | --- | --- |
| POST /console/v1/governance/monitor/query | /bff/v1/governance/monitor/query | `{filter:{start,end,guardrail_id?,model_id?,key_id?,stage?},timezone?}` |
| POST /console/v1/governance/monitor/rails/query | /bff/v1/governance/monitor/rails/query | `{filter,limit:1..100,after?:{guardrail_id,version}}` |

重复读使用只读 REPEATABLE READ；语句 5 秒、锁 1 秒、请求 20 秒、并发 8，报表发出前以新快照重新核验用户 / 租户 / 权限 / 凭据过期。前端保持 2 MiB 响应限制、显式刷新、无持久数据缓存，改变查询或分页清除旧值；403 清除当前页面，登出 / 失焦 / 会话过期维持已有清理约束。

`brokerrouter_console` 仅获安全视图与 coverage 表 SELECT，无 observations/evaluations 原表、规则正文、原始事件 JSON、审批意见、请求/输出正文、密文或资产 URL 读取权。无执行/审批/权限修改权。runtime 与 output_delivery 仅新增观测 INSERT/SELECT 和单列批准跳过 UPDATE；观测失败随 gate 事务回滚，不默默放行。

## 迁移与部署

0016 增加规范化观测表 / 保护触发器 / 时间与审批操作索引 / 安全视图 / 权限枚举；无 ledger/FX 写入。

1. 暂停所有 gate 写入服务（包括模型 runtime 与 output_delivery），完成正在执行事务；不可混用缺少观测写入的旧进程。
2. 在备份副本预演 0016，检查事件体量、索引/回填时间、磁盘与锁等待；在批准维护窗口运行迁移。该事务会扫描存量事件与语义评估，不承诺无停机。
3. 重跑 `deploy/permissions.sql`，以实际部署的最小权限登录验证运行及只读身份；显式授予选定真人 monitoring_read。
4. 同版重启 console/BFF/runtime/output_delivery；schema readiness 必须通过。做本地无费用的规则命中、批准、重复轮询、版本升级抽样并核对原始表/监控/账本。
5. 发布 UI 及既有 HTTPS edge。生产 IdP/MFA、撤权传播、真实运行时、容量、迁移窗口与恢复演练未验收前不标生产可用。

历史回填优先采用全量语义评估，再从 `guardrails.evaluated.results` / `approval.required.evaluations` 读取已知结构，按最早事件去重并保留后续首次批准跳过时间。历史即时耗时为 null；旧版本在等待语义检查时未发出的即时检查事件无法重建。`recorded_from` 标识新规范化采集边界，查询跨边界显示历史可能不完整，legacy_checks 指实际恢复条数，不把缺失记录猜成通过。

失败迁移整体回滚；成功迁移后不以旧二进制继续写入，也不删表回滚事实。应用问题优先同 schema 向前修复；需灾备回退则按经演练的全库一致时间点恢复流程处理，不单独恢复账本或观测表。不可变事实的留存/分区归档策略需遵循组织要求，不能直接删表清理。

## 验证与未完成项

新增数据库回归覆盖重复轮询、部分批准、跨版本、费用待定与迟到结算、最小权限、原始数据不可读、迁移去重、微秒边界及全部筛选；BFF 真实本机 HTTP 测试覆盖 CSRF/独立权限/固定路由；浏览器用测试专用合成契约，无假 API 随生产构建发货。ui-styling 用于语义表格、键盘可滚动表区、明确空值提示、窄屏与 200% 字号；沿用现有视觉系统与原生控件，没有新增运行时 UI 依赖。

实际执行结果见 milestones / acceptance-backlog。未实现审批 BFF/安全预览、原生 Hermes/OpenClaw 弹窗与恢复、逐节点调用追踪 UI、经验候选/评测样本、扫描引擎健康/P95、告警推送、团队/供应商/端点/Agent 统计维度；不要把这些称作“仅待联调”。
