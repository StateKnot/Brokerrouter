# 运行边界与数据库权限

当前按阶段交付能力，不是完整网关生产发布。具体实现状态和未完成闸门见 [milestones.md](milestones.md)。

## 数据库身份

使用专用 PostgreSQL 数据库，不与 Hermes 或其他产品共用 schema。迁移身份拥有该 schema；运行身份不是 owner，不能属于迁移角色，不能具有 superuser、BYPASSRLS、创建角色、复制或禁用触发器权限。数据库连接使用验证服务器证书的 TLS，凭据经 Secret 注入 `DATABASE_URL`，不放入命令参数、镜像和仓库。

按顺序执行：

1. DBA 创建专用数据库、schema owner 和独立登录身份；迁移身份执行 `brokerrouter migrate`。
2. DBA 在该数据库执行 `deploy/permissions.sql`，建立 `NOLOGIN` 权限组。
3. 给服务专用登录身份授予 `brokerrouter_runtime`，给每位运维人员自己的登录身份授予 `brokerrouter_operator`。不要把两种权限授给同一个服务登录身份；不要共享运维登录身份。
4. 以运维身份运行 tenant/key/model/endpoint/fx 命令；以运行身份运行 `serve`。审计记录实际数据库登录身份 `database_actor`。
5. 每次 schema 变更同时评审权限脚本，并以受限运行角色跑集成测试。

M2 增加独立 `brokerrouter_reconciler` 权限组，只给被授权的人工核对身份；不要给网关服务或 Agent。它继承结算所需运行权限及审计写入权限，用于闭合不确定费用，不是通用数据面身份。执行第三次 migration 后重新应用权限脚本。

M3 的第四次 migration 后再次应用权限脚本；单独创建 `brokerrouter_governance_control` 登录身份运行 `serve-control`。此身份可处理真人审批与工具结果核对，但不能改预算/消费，不能调用供应商。真人控制面仅接收 BFF 请求级签名证明；浏览器登录态、BFF 私钥与 Agent 执行环境隔离。完整配置、TTL、原生恢复义务见 [M3 契约](m3-governance.md)。

M4.1 第六次 migration 后再次应用权限脚本；独立 `brokerrouter_assets` 登录身份运行 `serve-assets`，只可操作资产清单/字节用量/审计，不可修改预算、人民币消费、价格、护栏或审批。资产配额仍由独立 operator 配置。对象存储、独立应用加密 keyring、桶权限、反向代理与未完成部署验收见 [M4.1 契约](m4-assets-pricing.md)。

M4 输出链路迁移至 0010 后重新应用权限脚本。输出转存身份 `brokerrouter_media_outputs`、扫描 API 身份 `brokerrouter_output_scan`、审批交付身份 `brokerrouter_output_delivery` 互不继承，也不能附带 input assets / runtime / human control 组。交付组只能创建待决定审批和读取输出；决定仍由独立控制组执行。S3 转存写/删、扫描只读、交付只读凭据分开配置，不能直接复用全权限桶凭据。参见 [隔离转存](m4-output-quarantine.md)、[技术检测](m4-isolated-detector.md) 与 [审批交付部署契约](m4-output-delivery.md)。

权限脚本用于新建的专用权限组，不会清理 DBA 额外授予的权限；部署时必须检查角色继承链与已有授权。触发器不能防御有权删除触发器的 DBA。备份、恢复与数据库管理员审计须独立配置。

M5.2a 迁移至 `0012` 后新增专用 `brokerrouter_bff` 登录运行 `serve-bff`；不要附加 console/runtime/operator/control 组。标准 OIDC、固定 HTTPS 公开源、外部身份预登记、Cookie/CSRF、独立签名及加密 keyring、反向代理和登出边界见 [BFF 契约](m5-oidc-bff.md)。停用/启用租户或用户会增加登录 epoch；不得人为回退 epoch 以恢复旧会话。BFF 每分钟执行有界过期清理，失败告警码为 `bff_maintenance_failed`。BFF 的 console 公钥绝不能同时加入 HUMAN 信任集合。

## 健康与迁移

M5.3a 迁移 `0013` 后重新应用权限脚本，新增专用 `brokerrouter_catalog` 登录与 `serve-catalog`。BFF 配置代理默认关闭，启用需独立签名 key/audience；配置服务只读运维预登记连接，不持有真实供应商 Key。版本冲突、幂等回执、默认停用与发布窗口见 [配置管理契约](m5-catalog-management.md)。本版 BFF 指纹扩展会要求重新登录，不回退会话或审计事实。

M5.3b 迁移 `0014` 后重新应用权限脚本，新增专用 `brokerrouter_settings` 登录与 `serve-settings`。Key/FX 与 catalog/只读/审批分别配置签名用途及显式权限，BFF settings 代理默认关闭。Key 在浏览器生成、仅提交摘要；密钥丢失只能撤销重建。新增 Key 配置与 grant 版本触发器、幂等回执及 CLI FX 发布锁；所有组件须同步升级并重新登录。上线/备份回滚、角色、预算与计划汇率边界见 [Key 与人民币汇率契约](m5-key-fx-management.md)。

`/health/live` 只证明进程能响应；`/health/ready` 校验全部已应用迁移的版本、成功状态和 checksum 与二进制一致。数据库失联、迁移缺失、checksum 不同或有未知迁移均不就绪。服务启动同样校验，不会在普通运行身份下自行迁移。

数据库连接池获取超时 5 秒；连接未配置非零限时时，应用默认设置 SQL `statement_timeout=15s`、`lock_timeout=3s`。运维通过实际登录角色或连接参数配置的非零上限会保留；涉及大表的迁移要使用独立迁移身份和显式更长的有限超时，不在生产服务连接中取消限时。

当前数据库演进采取显式迁移、显式发布；不承诺任意新旧二进制混跑。滚动升级前必须为具体 migration 验证兼容性，否则维护窗口排空后部署。SIGTERM/SIGINT 停止接受新连接并等待 HTTP 服务退出。

## 人民币口径

控制台报表 `serve-console` 使用独立 `brokerrouter_console` 登录身份、独立 `br-console+jwt` 请求证明及显式用户读权限；不得复用 runtime/operator/control 登录。migration 0011 后重新应用权限脚本，部署与查询口径见 [M5.1 控制台报表](m5-console-reporting.md)。只读服务不持有供应商、S3、内容解密或 BFF 私钥。

- 配置与默认展示：人民币 CNY；预算和消费：整数微元，`1 元 = 1,000,000 微元`。
- 美元价格：先计算原币精确金额，再乘该请求锁定的 `USD/CNY` 版本，最后对整笔非负人民币费用向上取整到微元。不能先把单 token 价格取整。
- FX 不自动联网同步、不内置默认实时值；由运维 CLI 或具备显式 fx_read/fx_write 权限的真人追加有效版本。可预定生效，错误版本用新版本纠正，历史请求不重算；即时版本不会取消已有未来计划。
- 账本同时记录原币金额、未舍入的人民币金额、最终人民币微元；这些是配置价格计算值，不伪称供应商发票实付金额。供应商对账与差额调整在 M2/M6 完成。
- `reserve` 占额度但不是花费；`finalize` 是入账消费；`release` 释放未消费预留。不能将三类流水相加当总花费。
- 预留是租户、Key 两层同时通过的事务。不确定提交保留占用，未发送才能释放；已发送后撤销 Key 不阻止核对和结算，但阻止新准入。

## SSE 与传输容量边界（R0.5，目标环境另行验收）

实时流和密文重放共用 SSE 交付额度。程序默认 `BROKERROUTER_MAX_CONCURRENCY=64` 时，上游并发 U=64、SSE 交付 C=16；生产 Compose 明确设置 U=16、C=4。可用 `BROKERROUTER_MAX_STREAM_CONNECTIONS` 将 C 固定在 1–1024，超额请求在预算预留前返回 `gateway_busy`。每个交付队列固定 4 × 64 KiB，即 Q=256 KiB；实时上游响应 R=4 MiB。SSE 响应建立后最多存活 `BROKERROUTER_REQUEST_TIMEOUT_SECONDS + 60` 秒（默认 180 秒）；建立前仍受上游请求超时约束。实时发送单片等待最多 100 ms，重放单片等待最多 5 秒。慢客户端会被关闭，下游停止交付，后台仍消费上游并尝试持久结算；只有结算成功才向仍连接的客户端发 `[DONE]`。

交付队列之外，数据面还限制全部已接受 TCP 连接，包括空闲、未完成请求头、keepalive 和阻塞发送。`BROKERROUTER_MAX_HTTP_CONNECTIONS` 默认/Compose 为 64（有效范围 1–1024），`BROKERROUTER_HTTP_CONNECTION_TIMEOUT_SECONDS` 默认上游请求时限加 60 秒、Compose 为 180 秒（有效范围 1–900）。期限从接受 socket 起算，独立定时器关闭内核 socket，不依赖请求/响应是否正在 poll IO。许可保留至传输关闭，不能在上游完成或响应体排空时提前释放。超额连接在 HTTP 解析和收费准入前关闭，拒绝累计数以 `http_connection_capacity_full` 告警码按 2 的幂采样记录；连接到期码为 `http_connection_deadline`。

数据面显式使用 Hyper HTTP/1.1。原自动协议协商因传递依赖启用 HTTP/2，允许单 socket 多个滞留流，不能沿用按 socket 的内存预算。HTTPS edge 可对外终止 HTTP/2，但必须以 HTTP/1.1 代理到网关，并关闭 SSE 缓冲。SIGTERM/SIGINT 停止新连接、排空在途 HTTP，再等待独立后台结算；阻塞客户端仍受原绝对连接期限约束。

网关 Compose 的停止宽限为 6 分钟：请求体可能在接近 socket 的 180 秒期限时才读完，并启动受独立 120 秒时限约束的后台收费任务；剩余时间用于结算。不能把宽限只设为 socket 期限。连接关闭、停止交付或进程被强制终止均不代表零费用；受理不确定事实继续核对，不能退款后盲目重提。

程序默认配置中 `C × Q + U × R` 约为 260 MiB；生产 Compose 的 U=16、C=4 时约为 65 MiB。这只是发送队列加实时响应硬限的预算，不包含 SSE 解析/聚合副本、历史重放构造、HTTP/内核 socket 缓冲、连接元数据、数据库池、进程基础内存和反向代理缓冲。生产 Compose 容器内存上限 512 MiB，本地探针在负载前固定 RSS 预算 384 MiB（75%）。目标环境须用相同发布物实测 RSS/容器总内存、连接数、拒绝数和结算延迟后确认容量与告警；本地证据不代替目标主机认证。完整复现步骤见 [本地部署验收](../deploy/verification/README.md)，实测结果以对应版本的验收报告为准。

## 当前尚未开放

当前已实现非流式 Chat/Embedding、按端点能力开放的 Chat SSE、幂等结果读取、人工费用核对、持久审批/文本语义检测、私有资产/单位计费，以及首个文生视频协议、隔离转存、技术检测和当前轮审批交付服务端。SSE 只能为经认证端点显式开启，真实供应商仍须通过 LIVE-01；命中输出护栏时失败关闭为非流式。Responses、图片/3D/其他供应商协议、专用多模态语义审核、其他模态预览、自动供应商对账，以及真实 BFF/Runtime/供应商/生产存储认证尚未完成，不能据此宣称完整网关可上线。
