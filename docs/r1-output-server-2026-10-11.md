# R1 原生输出服务端验收记录

状态：本批服务端本地验收通过，R1 尚未退出。本文覆盖 schema 30 的私有产物接收和 schema 31 的最终助手文本；Hermes 原生最终发布、二进制扫描/审阅/下载、独立 producer 与完整用户链路另行验收。

## 已实现的边界

- 私有产物接收先绑定原工具 execution claim，按确切清单预留容量；原 completed receipt、文件摘要和实际完整字节共同确定隔离事实。独立 `runtime_assets` 身份只能接收，不能读取、批准或发放下载授权。现有输入资产及媒体路径不能借此读取未扫描产物。
- 最终助手文本单独登记不可变正文与真实成功 Chat 请求来源，按原 Key/turn/model、原响应密文摘要与期限核对。重新登记只返回原事实，不能替换正文、续期或重新执行模型。
- 释放驱动已有语义检测；待检测/审批响应只有元数据。返回正文前再次核对当前身份、模型、来源、策略和实际授权期限。最终 operation 不产生工具 claim，普通 checks/claim/receipt 拒绝保留命名空间。
- 真人工作台审阅确切的最终文本 payload。受限审批角色只读取无密文事实视图，不能读取独立 publication/provider 密文。审阅凭证、本人证明及原来源在决策审计后再次核对，过期会回滚决定和授权。

## 实际发现与回归

| 复现 | 修复后的行为 | 证据范围 |
| --- | --- | --- |
| 最终文本已经进入真实工作台，但旧 Chat choices 判断使 `can_continue=false` | 仅完整匹配保留 publication payload 才可审阅和继续 | 真实 HTTP review→decide→release、实际受限审批角色、伪造 shape、被清理来源 |
| 原模型来源在审批审计阻塞期间过期 | 返回 410，决定和 grant 回滚 | PostgreSQL `pg_sleep` 与不随事务回滚的 sequence 证明到达审计 |
| 有效审阅凭证在审批审计期间过期，修前返回 200 | 提交前按 PostgreSQL 时钟复验，返回 409，决定和 grant 回滚 | 同一数据库时刻构造不可变五分钟凭证；原工具审批工作台的真实 HTTP 请求 |

最新针对性回归为工作台 10 项与最终文本 15 项全部通过。0030 接收的 11 项回归包含配额/原回执事务、完整字节与摘要、用途角色和请求断开后的容量边界。修后冻结源码的全仓常规 324 项通过，4 个 ignored 的真实 S3/扫描专项显式通过，fmt、全目标 Clippy `-D warnings` 和本机 release 构建通过；release 用时 579.048 秒。130 个源码与验证输入摘要及全部日志摘要见 [本批质量证据](evidence/r1-output-server-2026-10-11.json)。

对应固定 Docker 镜像的最终文本分离部署已通过 16 组，见 [安全证据](evidence/r1-final-publications-deployment-2026-10-11.json) 与 [可重跑验证包](../deploy/verification/final-publications/README.md)。镜像 ID 为 `sha256:5401aaad310f24218d1d853206c96d0a1b76c7c46a1304aa1c10d287eebd6e3c`，100 个编译输入与当前冻结源码一致。它当时没有 Git revision label，不能按 label 推断来源。

该部署从零建立 schema 31、20 个用途登录、4 套私有 CA 和 runtime/control/approvals 三个独立进程。Agent 容器仅挂 runtime CA，没有 Human/BFF 签名密钥、数据库凭据或 Docker socket。真实模型 HTTP 请求产生原 gateway source UUID；丢弃登记响应后，重放仍是一份正文。实际工作台完成 review→decide，过期 review 返回 409 且没有 grant；策略 V2 重新审批后，实际重启 gateway，恢复相同 publication/output operation。

最终为 1 次 provider fixture 调用、1 个 request、1 份 publication、2 份版本审批、2 份 grant、1 条 finalize 账本记录。端点价格为零，没有真实供应商费用。controller 的合成人证明用于本包用途隔离验证；它没有接入 IdP/BFF，也没有执行 Hermes 普通历史或原生通道，不能把这 16 组视为完整 R1 用户验收。初次验证夹具误把正确的就绪 204 期待为 200，保留失败日志后修正并从零重跑。

## 部署与恢复

按顺序应用新增迁移，再按当前 `deploy/permissions.sql` 更新用途权限。产物接收需要显式 19→20 用途升级和独立 `runtime_assets` 登录；旧 57 份 Secret 不轮换。运维 helper 对现存文件和悬空符号链接均故障关闭，固定修复与实际 TLS/PG 验证见 [清单证据](evidence/r1-purpose-symlink-2026-10-11.json)。

`serve-assets --runtime-output` 是独立接收入口；它不能作为下载服务。助手发布使用治理 API。未完成扫描、字节审阅、具体文件授权及最终文本与附件集合绑定前，Hermes 附件入口保持拒绝。

升级失败应隔离新入口并保留数据库、原 receipt、密文和审计事实，不降级已应用迁移。重新启动和重放必须沿用原 publication、工具 claim 和来源，不能用旧二进制重新派发来恢复缺失结果。

最终文本工作台沿用 128 KiB 完整 JSON 审阅上限；较大正文不能截断后批准。拒绝/反馈立即清理 operation 审阅正文，独立 publication 密文按原 TTL 清理。服务端批准不等于原生通道已确认发送，也不证明 Python producer 与 Agent 已形成挂载隔离。
