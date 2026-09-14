# M4.3b 第一包：隔离扫描作业与证据链

契约 `output-scan-evidence-v1`，2026-09-13。本包实现独立安全工作负载的拉取式扫描接入、机密字节通道、租约恢复和不可变回执。没有内置“默认安全”检测器，不把服务端接收回执等同于网关实际运行了杀毒/格式解码/内容分类。真实检测引擎适配与认证、真人审阅和审批交付仍需后续完成；当前客户端不能下载产物。

后续实际 ClamAV/FFmpeg 技术检测工作负载已在 [第二包](m4-isolated-detector.md) 接入；本文件以下保留第一包协议边界。内容语义审核、真人审阅和审批交付仍未开放。

## 信任与权限

检测引擎必须在 Agent/Hermes/OpenClaw 容器之外运行；它会处理潜在恶意文件，必须使用无特权沙箱、只读镜像、CPU/内存/时间/解压与帧数限制、受限网络和隔离临时目录。不得在网关进程里直接解码不可信媒体。该接口不执行 shell，也不接受命令、任意下载 URL 或 Agent 自报的审核结果。

`serve-output-scans` 使用独立数据库登录，仅加入 `brokerrouter_output_scan`。该角色不继承模型、输入资产、输出转存或真人控制角色。它只能读取 output 资产并维护扫描事实/审计，没有资产写入/删除、人民币账本、预算、容量、配置发布和审批 grant 权限。RLS 隔离的是服务用途；租户/凭据/任务精确鉴权由接口逐次执行。服务登录不得是超级用户、表拥有者或混合用途角色。

S3 只授予 GetObject；不得为扫描服务开放 Put/Delete/List、公开 ACL 或匿名访问。扫描 API 与转存 worker 共用资产加密 keyring，但扫描工作负载只持有扫描凭据，不持有数据库、S3 或内容密钥。HTTP API 默认绑定 `127.0.0.1:8084`，生产必须置于工作负载专用 TLS 入口、网络允许列表和速率/连接/慢客户端限制之后，不能暴露给浏览器、用户 BFF 或 Agent 网络。

扫描凭据是随机 256 位、仅保存 SHA-256 的 `sc_br_` token；和模型 `sk_br_` Key 不通用。每枚凭据精确绑定 tenant + profile ID，必须设置不超过 90 天的到期时间。每个租约还绑定具体 token ID，同 profile 的其他有效凭据也不能直接读取或回报该执行者的任务。轮换时先创建新凭据，再撤销旧凭据；已撤销执行者即时失去访问，新凭据可以重新领取该扫描任务的下一次尝试。

这是受信扫描服务的鉴权证据，不是对检测引擎未被入侵的密码学证明。凭据不能共享给不受信插件；扫描服务、检测质量、凭据隔离及网络控制必须完成 LIVE-12 认证。

## 发布与升级

先暂停扫描接入，应用迁移 0009，重新执行 `deploy/permissions.sql` 并验证专用登录权限，再部署新版本扫描 API/媒体服务。未配置扫描 profile 的模型保持原先隔离状态，没有默认通过路径。

运维发布 profile 文件，字段如下：

```json
{
  "tenant_id": "10000000-0000-4000-8000-000000000001",
  "id": "20000000-0000-4000-8000-000000000001",
  "model_id": "your-video-model",
  "version": 1,
  "engine_revision": "0000000000000000000000000000000000000000000000000000000000000000"
}
```

UUID 和全零摘要仅为格式示例。`engine_revision` 必须替换为实际不可变检测清单的 SHA-256（小写 64 hex）；清单应涵盖格式解码器构建、杀毒引擎/特征库版本、内容分类模型/阈值/覆盖方式以及资源限制。网关校验声明摘要是否匹配已发布 profile；部署认证负责证明真实引擎确实执行该清单，而不是只回显摘要。

```sh
brokerrouter output-scan-profile-publish --file <profile.json>
brokerrouter output-scanner-token-create --tenant <tenant> --profile <profile-id> --name <workload-name> --expires-at <RFC3339>
brokerrouter serve-output-scans --bind 127.0.0.1:8084
brokerrouter output-scanner-token-revoke --tenant <tenant> --id <token-id>
brokerrouter output-scan-profile-disable --tenant <tenant> --id <profile-id>
```

数据库 URL 由 Secret 注入。凭据创建命令只在 stdout 显示一次 token，直接安全存入该扫描工作负载的 Secret Store，不复制到仓库、日志、聊天或 Agent。每个模型同一时刻只允许一个 active profile；发布更大的 version 时在同一事务停用旧 profile，旧版本不能复开、修改或继续读取资产。新 profile 必须重新配置凭据，旧回执保留为历史事实，不迁移或伪装为新版本审核。

## 工作负载协议

所有接口使用 `Authorization: Bearer <SCANNER_TOKEN>`。JSON 拒绝未知字段，最大 16 KiB。

1. 工作负载先持久化随机 UUID `claim_id`，然后 `POST /v1/output-scans/claim {"claim_id":"..."}`。网关只分配该 profile 的模型、已成功隔离的输出；扫描记录绑定 job、asset、原 Key、完整文件 SHA-256 和字节数。无任务返回 `{"state":"no_assignment"}`，此时没有占用该 claim ID。
2. 有任务时返回 `attempt_id,scan_id,job_id,asset_id,sha256,byte_length,chunk_count,profile_version,engine_revision,lease_until,deadline`。原 claim 重试返回同一次尝试；已经失效或被接替的 claim 不会悄悄领取别的文件。每个 token 最多 2 个活动租约、每 profile 8 个、每租户 16 个。
3. 通过 `GET /v1/output-scans/attempts/{id}/chunks/{ordinal}` 读取固定 4 MiB 分块，最后一块可更短。不能指定资产 ID/任意 URL，不支持 Range；负序号、越界、其他 token/tenant、失效租约均拒绝。响应仅为附件 `application/octet-stream`，并设置 no-store、nosniff、sandbox。工作负载必须按序组合并独立计算完整 SHA-256/长度，再执行检测；不能只检查文件名或网关声明的 MIME。
4. 初始租约最多 5 分钟。`POST .../{id}/renew {}` 可在有效期内续期，但单次尝试永远不超过原创建后 30 分钟，且不超过资产/turn 的期限。失效租约不能复活。执行者崩溃后新 claim 可重领原 scan 的新 attempt，最多 3 次；每次都重新绑定执行者。超过上限关闭扫描，不无限消耗资源。
5. 持久化实际扫描证据后，向 `POST .../{id}/report` 提交下述回执。只有当前尝试的原凭据可以提交；迟到执行者不能覆盖新结果。回执和审计原子提交；响应丢失重发完全相同回执幂等，冲突回执拒绝。完成后不能继续读块或续期。

```json
{
  "sha256": "<exact-file-sha256>",
  "byte_length": 123456,
  "profile_version": 1,
  "engine_revision": "<published-manifest-sha256>",
  "technical": "clean",
  "content": "risk",
  "media_type": "video/mp4",
  "reason_codes": ["policy_risk"],
  "evidence_sha256": "<persisted-scanner-evidence-sha256>",
  "scanned_at": "2026-09-13T12:00:00Z"
}
```

v1 clean 档位仅接受已实际识别的 `video/mp4`。`technical` 为 clean/unsafe/unsupported/error：clean 必须代表格式检查与恶意文件检查均完成，不允许超时后标 clean；非 clean 必须 `content=unknown,media_type=null`。`content` 为 pass/risk/unknown；risk/unknown 不是格式/杀毒失败。非全通过结果必须给出固定 reason code；最多 32 项，每项 1–64 位小写字母、数字或下划线，不能放原文、URL、错误堆栈或模型指令。结果时间必须在本次尝试开始之后，最多允许未来 5 秒偏差。

`evidence_sha256` 是扫描服务保存的证据包摘要，不是公开地址。网关不接收任意证据正文，不能据此从浏览器下载未审资产。技术不安全/不支持状态不得在后续交付阶段被“本轮跳过内容护栏”覆盖；内容风险与技术硬安全边界分开。

## 撤销、恢复与成本

读取分块前后都重查凭据、配置、当前租约、原 tenant/Key、模型权限、生成取消、资产与 turn 生命周期；存储 I/O 不持数据库事务锁。已发出的字节不能收回，但下一个授权边界禁止继续。数据库不可用时失败关闭，不能把数据库错误当成审核通过。

待完成扫描由独立维护循环每 30 秒清理，先选真正失效的候选，再在租户锁下重查，避免活动任务占满批次或旧扫描误关闭新尝试。已取消/过期资产由输出 worker 沿用现有容量/GC 清理；扫描服务不改容量或模型费用。回执及执行审计为不可变事实，后续归档需要独立保留策略。

本档位用于自管、非按次付费的安全工作负载；领取重试是重复本地检测，**不是**模型生成或付费审核调用。扫描工作负载不得持有付费供应商凭据或绕过网关调用模型。CPU/GPU/存储流量是运维成本，不伪装成 0 元的供应商账单。云内容审核服务及其非 Token 计费连接器仍需开发，届时必须复用人民币预算/账本并处理受理不确定，不能直接套用本协议的三次重领规则。

## 当前交付边界与验收

媒体 API 增加 `output_scan` 状态、profile version 和技术/内容结果概要。它不包含文件、供应商 URL、扫描凭据或证据包位置。`complete` 只表示可信通道接收了回执；`delivery=not_released,approval=not_granted` 始终明确返回，作业仍停在 output_pending。现有 Agent/用户资产路由继续拒绝输出文件。

本包 9 项新增 PostgreSQL/HTTP 测试覆盖完整拉取/回执、并发 claim、旧租约隔离/三次上限、严格版本与摘要、凭据轮换/配置升级、取消/Key/turn 撤销、提交故障恢复、独立数据库角色及 HTTP 越权；已有实际 S3 用例扩展验证扫描通道读取解密分块。fixture 是合成字节及合成回执，不代表 MP4 有效性、杀毒检出率或多模态语义质量已验证。首次运行发现并修复租约创建与截止时间使用不同数据库时钟基准的问题，未放宽 30 分钟约束。

LIVE-12 登记真实检测引擎/沙箱/清单摘要、样本覆盖/误报漏报、资源上限、工作负载凭据/TLS/IAM、证据保存和崩溃恢复认证。M4.3b 后续包仍须实现实际引擎适配、资产感知策略消费、真人独立审阅、同轮批准及逐块交付授权，不能把这些未开发项标成“仅待联调”。
