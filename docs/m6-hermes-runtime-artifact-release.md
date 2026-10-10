# 原生工具文件：扫描、独立审阅与单文件交付

状态：2026-10-11，schema32 子能力同源全仓本地检查通过，固定核心镜像的原生文本恢复组合验证通过。已封存输入阶段见 [私有接收契约](r1-runtime-artifact-intake-2026-10-11.md)。本包不开放 Hermes 附件发送，也不关闭 R1。

## 服务与事实

复用原 `runtime_artifacts`、私有资产、扫描队列、隔离播放器和本人审批。`output_scans` 以严格互斥的 `job_id` 或 `runtime_asset_id` 区分媒体作业和原生文件；不创建虚假的媒体作业或生成任务。Worker 从原始已完成 tool claim/receipt、批次、文件字节和当前模型扫描 profile 取得任务，按原有租户、token、profile 并发上限领取。实际 ClamAV、FFprobe 和 FFmpeg 读取同一原始 MP4 字节；技术检查通过仍可产生 `content=unknown`，需现行策略与明确真人决定。

`runtime_artifact_deliveries` 固定文件、scan 和保留的 artifact operation。`runtime_artifact_grants` 仅授权该 operation、payload、完整现行 policy 与规则版本；不借用原 `guardrail_grants` 的同轮授权。原媒体 `current-turn-output-v1` 合同保持原样。`producer_revision` 与原始声明摘要是不可变审计声明，不能充当独立生产器执行认证。

## 独立读取入口

使用 `serve-output-delivery --runtime-output` 启动原生文件 reader，并配置独立用途数据库身份、HTTPS 和对象存储访问。writer 的 `serve-runtime-artifacts` 只接收私有字节，不提供下载入口。

| 请求 | 行为 |
| --- | --- |
| `POST /v1/runtime-artifacts/{asset_id}/release`，严格 `{}` | 检查原来源、现行 scan/profile/policy；只返回 pending 元数据，或 ready 的完整文件证据 |
| `GET /v1/runtime-artifacts/{asset_id}/manifest` | 当前单文件授权有效时返回 `runtime-artifact-mp4-v1` 证据 |
| `GET /v1/runtime-artifacts/{asset_id}/chunks/{ordinal}` | 逐块读取同一已授权文件，并在对象读取与审计之后重新检查现行授权和期限 |

请求使用对应原 Key。拒绝 query、Range、未知请求字段和非 identity 编码；没有任意路径、URL、预签名或公开文件接口。请求/字节缓冲有界，响应体保留占用额度直到消费或丢弃；客户端断开不能取消已经开始的持久 release 事务。ready 的数据库检查最多三秒。

## 本人完整审阅

原生文件证据明确 `source=runtime_artifact`、`scope=current_artifact_in_original_turn`，绑定原 turn/model/tool/input operation/batch、文件 ordinal/hash/bytes/revision、当前 scan/profile 与完整 report。审批摘要、播放器 manifest 和 complete proof 必须匹配整份证据；阶段、来源或范围混用不能降级成同轮批准。

本人经独立 media-review 用途读取全部 chunk、核对完整字节 SHA 与长度，在独立预览源播放 MP4，再确认完整审阅并以单文件范围执行两步决定。摘要或扫描 JSON 不能替代字节审阅。decision 用途不能读取原始资产块，review 用途不能决定或插入 grant。Agent、scanner 和 storage 用途均不能代人批准。

技术 unsafe、错误或不支持的媒体类型不能人工豁免。更新扫描 profile、规则、原身份权限或到期会阻止旧授权；同一文件的拒绝/反馈在 profile 升级后仍有效。PostgreSQL 时钟在最终审计之后再次检查，越过截止时间不提交读取审计，也不返回字节。

## 已执行及剩余验证

最终 135 个质量输入固定：348 项常规 Rust 测试、6 项实际检测器/S3 专项、fmt、Clippy `-D warnings` 与 release 全部通过，见 [同源安全证据](evidence/runtime-artifact-b-2026-10-11.json)。原生文件的两个真实 MP4 专项均实际执行，不能把常规运行中的 ignored 当作通过。子测试保留全部断言并放入独立 module 目录，原私有接收角色测试同步区分扫描元数据读取与禁止写入/发授权；首次失败日志单独留存。

固定核心镜像 `sha256:f91fefbc1b5dec597983bc26cd5ad6fdc2493fda737075a7bffcedfce719efdc` 的 103 个编译输入与本次源码一致。原生文本链路在同 schema32 下另通过 17 组实际验证：真实 SDK 流式来源、登记 ACK 丢失、本人完整审阅与 V2 再审批、实际核心及宿主重启；模型调用 1 次、历史 2 行、本地接收器发送 1 次。该验证使用合成 purpose-signed 本人身份及本地通道，不代替原生文件组合联调、外部 IdP/通道认证，见 [原生恢复记录](r1-native-publication-recovery.md)。

本地 outputs 整组 64 项通过、5 个环境专项单独保留；固定真实 detector 的两个原生 MP4 专项通过，覆盖三引擎扫描、完整字节 Human 审阅、独立 per-file grant 与同字节读取。真实 HTTP 回归证明保留命名空间不能转为普通 checks、tool claim 或 receipt，伪造 scan metadata 返回 400，且原执行事实保持不变。

控制台同源通过 34 个单元测试、类型检查、构建及 174 个 Chromium/WebKit 浏览器用例；原有 132 个用例保留，新文件合同 42 个。浏览器中的 API/report 为明确的测试夹具，不能冒称实际后端扫描或组织身份认证。

后续必须把原工具文字及全部文件集合在服务端原子绑定、对当前工具/文件/最终文字授权共同重检，并完成独立固定生产器、capture broker 和 opaque resolver 的真实隔离与恢复。文件 A 的批准不能释放文件 B、工具文字或最终助手正文。完整 IdP/通道、同源组合部署、费用与容量验收仍分别保留；在这些条件通过前，Hermes 附件与 TTS 继续关闭。
