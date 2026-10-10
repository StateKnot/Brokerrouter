# Hermes 最终助手文本发布契约

状态：R1 开发中。该服务端契约已有真实 PostgreSQL、HTTP 和受限角色回归；固定 Hermes 模型请求、最终文本冻结、会话落盘及原生通道仍在接线验证，不能据此关闭完整发布验收。

原始模型响应、工具最终 observation 与最终助手文本是不同内容。Hermes 的最终 hook、压缩与格式加工后，必须冻结实际要写入会话和发送的正文，再按服务端当前 `post` 策略释放；原模型请求成功或工具输出获准不会自动批准加工后的正文。

| 请求 | 输入 | 结果 |
| --- | --- | --- |
| `POST /v1/governance/final-publications` | `turn_id`、`model_id`、`source_request_ids`、`text` | 返回不可变 publication ID、摘要、字节数、原 source ID 和期限，无正文 |
| `POST /v1/governance/final-publications/{id}/release` | 严格 `{}` | `checking` / `waiting_approval` 只有治理元数据；`ready` 才包含原 UTF-8 正文 |

来源 ID 只能使用实际模型 HTTP 响应的 `x-brokerrouter-request-id` UUID。Provider JSON `id`、诊断 `x-request-id` 和 Hermes 本地 API 计数都不能替代。1–64 个来源不得重复；服务端排序后逐项核对同租户、Key、turn、model 的实际成功 Chat 请求，拒绝 judge/Embedding、其他轮次、受理不确定、缺失或已清理的响应。服务器记录原 request hash、响应密文 SHA 和原期限，数据库插入触发器重复核对这些事实。

同 Key、turn 只能有一份最终 publication。turn 表保存受信创建的 session/message/run/generation；客户端没有另一个身份覆盖入口。正文和来源集合重放返回同一 ID、摘要和期限，任何变更均冲突，不能靠新 Idempotency-Key 替换已经冻结的文本。数据库不可覆盖正文、续期、删除事实或更换来源；仅到期维护可清理密文。

正文 JSON 字符串最大 512 KiB，包含转义开销；超限拒绝，不截断。期限由 PostgreSQL 时钟及服务端结果 TTL 决定，最迟不超过任一来源原期限。相同已登记请求的晚到 ACK 只返回元数据，不重新生成、调用供应商或延长期限。

释放使用保留的 `runtime-final-output:{publication_id}` operation 和独立 `post` payload，包含最终正文与受信 turn 身份。POST 可驱动既有语义检测和账本；断开 HTTP 不取消已启动任务。最后在租户锁内重验当前模型权限、成员/绑定/Key/turn、全部原来源、密文/正文摘要、现行规则/版本及 grant 截止时间，再返回正文。审批事件/监测写入跨过到期时刻则失败关闭；旧 grant 不能跳过升级后的版本。没有最终工具 execution claim，普通 checks/claim/receipt 不得使用这个命名空间。

本接口当前只处理助手文本。二进制附件、MEDIA 路径、任意 URL 和 channel metadata 不能借文本批准获得下载或发送权；产物必须使用独立字节扫描、审阅与交付授权。服务端释放也不等于原生通道已确认发送，发送台账仍须保留一次领取和未知状态，会话恢复须比较原 message/run/generation，不能清除新轮次的恢复标记。

人工审批使用真实 `/approvals/v1/review` → `/approvals/v1/decide` 工作台。保留 operation 的 JSON 必须逐字段匹配登记的 publication、原来源、turn 身份、正文摘要和长度；任意 `{text}` 或借 Chat choices 伪造的正文不具备审阅资格。审阅和决策前后都会核对原来源仍可用，审批日志只保存摘要和身份。审批数据库角色仅能查询无密文的事实视图；不能直接读取、覆盖或清理独立 publication/provider 密文。工作台现有 128 KiB 完整 JSON 审阅限制保持不变，超过限制不能获得人工继续授权，不截断内容。

所有经过工作台的决定在审计写入之后、事务提交之前，重新按 PostgreSQL 当前时钟校验原审阅凭证的期限和摘要。审计等待跨过凭证到期时刻会回滚决定与授权，用户须重新审阅；不能沿用事务开始时仍有效的凭证。

拒绝或反馈会立即清理 operation 审阅正文并阻止 release；独立 publication 密文按原 TTL 清理，不能宣称人工拒绝会立即删除该密文。最终文本和来源审计事实持续保留，不能借拒绝、重新登记或重启替换正文。
