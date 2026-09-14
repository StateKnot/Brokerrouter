# M4.2：持久媒体作业与版本化供应商协议

契约：`media-jobs-v1` / `dashscope-wan27-v1`。本包实现文生视频的准入、输入审批、持久提交/查询、受理不确定恢复和人民币结算。**供应商成功后停在 `output_pending`，没有视频下载接口**；M4.3 的隔离转存、二进制审核和审批交付尚未完成。不要把账本结算成功当作已向用户交付。

## 支持范围与配置

首个连接器依据 [阿里云百炼 Wan2.7 文生视频官方 HTTP 协议](https://help.aliyun.com/zh/model-studio/text-to-video-api-reference) 实现（核对日期 2026-09-13）。不使用 Wan2.6 的 size 协议，不假设其他供应商仅替换 base URL 就能兼容。协议是显式版本枚举；未知协议/参数拒绝，不执行配置里的脚本、模板代码或任意 HTTP 方法。

当前只支持 `video.generate`：prompt、negative_prompt、720P/1080P、五种画幅、2–15 秒整数，具体组合必须在端点 profiles 中明确发布。固定发送 `watermark=true`、`prompt_extend=false`。图片/音视频输入、自定义音频、图像生成、3D、回调和供应商取消 API 均未在本包开放，不能当作已实现的待测功能。

使用可信 `brokerrouter_operator` 数据库身份创建模型后，通过 `media-endpoint-add --file endpoint.json` 注册。下面的价格、区域和模型版本均为配置示例；价格 **不是报价或实时价格**，必须按实际合同配置并完成 LIVE-10 认证。

```json
{
  "id": "00000000-0000-4000-8000-000000000001",
  "tenant_id": "00000000-0000-4000-8000-000000000002",
  "model_id": "video/wan",
  "name": "百炼视频端点",
  "protocol": "dashscope-wan27-v1",
  "upstream_model": "wan2.7-t2v-2026-06-12",
  "origin": "https://dashscope.aliyuncs.com",
  "credential_env": "BROKERROUTER_DASHSCOPE_VIDEO_KEY",
  "allow_private": false,
  "currency": "CNY",
  "priority": 10,
  "maximum_output_bytes": 134217728,
  "output_origins": ["https://provider-output.example"],
  "profiles": [
    {"resolution":"720P","aspect_ratio":"16:9","durations":[5,10],"price_per_second":"0.10"}
  ]
}
```

每逻辑模型最多 100 个活跃媒体端点。按 priority 升序、UUID 稳定顺序选择首个完整满足参数组合的端点；弱端点不收窄其他端点的能力。整个作业固定端点、价格和 FX，不因重启或配置变化重新路由。版本不可修改/删除；更新需新增版本，并用 `media-endpoint-disable --tenant ... --id ...` 停用旧版本。已提交作业继续使用原版本查询。

M4.3a 起，新准入同时要求输出容量和 origin 合同，上面的输出域名是待替换占位值；缺少输出合同的旧端点不参与新路由，旧已提交任务仍可核对费用。私有转存与专用 worker、数据库 RLS、升级顺序见 [输出隔离合同](m4-output-quarantine.md)。

凭据只保存环境变量名称，不写入端点配置、事件或响应。新增凭据名称/轮换 Secret 后需滚动重启 worker，查询期限内保留原账号的查询能力。公开生产端点禁止 `allow_private=true`；该开关仅供经过审核的私网部署或隔离 fixture。请求逐次解析并固定 DNS IP，拒绝未允许私网、代理、重定向和客户端隐式重试。

## 进程与接口

先用迁移身份应用 0007，再重新应用 [数据库权限](../deploy/permissions.sql)。API 与 worker 使用受限 `brokerrouter_runtime`，不授予真人批准权限；沿用 [结果加密密钥配置](m2-text-gateway.md)。`serve-control` 仍是独立真人控制面，不能用 Agent key 替代。

```sh
cargo run -- serve-media --bind 127.0.0.1:8083
# 独立进程；停止 API 不会丢失已准入任务。
cargo run -- media-worker --concurrency 4
```

API 默认 32 并发、32 KiB 请求体；worker 并发 1–16，默认 4。部署方仍须实现 TLS、入口连接/速率限制和运行监控；loopback 监听不是公网生产部署方案。

| 方法与路径 | 语义 |
| --- | --- |
| GET `/health/live`、`/health/ready` | 存活、数据库迁移版本/checksum 就绪 |
| GET `/v1/media/capabilities?model=video/wan` | 当前 Key 获授权的活跃端点、参数组合、原币价格；不返回 origin/凭据引用 |
| POST `/v1/media/jobs` | 202；幂等准入并预留费用，不同步等待视频 |
| GET `/v1/media/jobs/{id}` | 当前 Key 的状态、金额、可处理审批和事件游标；不调用供应商 |
| GET `/v1/media/jobs/{id}/events?after=0` | 持久单调序号，最多 100 条；只读，不启动收费工作 |
| POST `/v1/media/jobs/{id}/cancel` | 提交前取消并释放；提交后只记录取消意愿 |

统一文本服务 `/v1/models` 已包含获授权且具有活跃媒体端点的模型。业务接口要求 `Authorization: Bearer <VIRTUAL_KEY>`；创建还要求 `Idempotency-Key`，受管调用要求服务端 turn UUID 的 `X-Brokerrouter-Turn-Id`。当前轮定义和真人审批遵循 [governance-v1](m3-governance.md)，不是永久豁免。

```json
{
  "model": "video/wan",
  "task": "video.generate",
  "input": {"prompt": "傍晚湖面的缓慢镜头", "negative_prompt": "模糊"},
  "output": {"resolution": "720P", "aspect_ratio": "16:9", "duration_seconds": 5}
}
```

同 Key 相同幂等键、正文和 turn 返回同一作业；正文/turn 不同返回冲突。媒体幂等命名空间与文本/内部护栏调用隔离。普通 SDK 不会自动出现审批弹窗，仍需 Hermes/OpenClaw 受信通道展示和恢复。

## 状态与资金边界

| 状态 | 后续处理与资金 |
| --- | --- |
| `queued` | 作业、事件、价格/FX 快照及预留同一事务提交；尚未 POST |
| `waiting_approval` | 输入检测要求真人决定；未提交生成，持有预留 |
| `submitting` | 不可逆发送边界已持久化；进程丢失时不能证明没发过 |
| `submission_unknown` | 超时、非 2xx、无任务 ID、任务 ID 写库失败等；保留预留，禁止自动 POST/切端点 |
| `running` | 已持有不可替换的上游 task ID，只重试 GET |
| `output_pending` | 用量校验、人民币结算和隔离元数据同一事务提交，仍不可下载 |
| `needs_reconciliation` | 上游失败/取消/未知状态，或查询窗口耗尽；费用证据不足，不自动退款 |
| `cancelled` | 已确定在首次生成 POST 前停止，预留已释放 |
| `reconciled` | 独立核对身份按证据闭合费用，不代表有产物，不授权重执行 |

输入以绑定 tenant/job/密钥版本的密文保存。护栏沿用模型/Key/Runtime 策略，发送前再次锁定租户、turn 和策略检查。真人批准产生当前 turn、指定护栏版本的 grant；拒绝或旧 turn 失效不会被解释为批准。检测费用仍独立入统一账本；生成取消不撤销已发生的检测费用。

网络调用不持有数据库锁。claim 使用持久 UUID 和到期时间，写回核对 owner；旧结果不能抢先结算。提交前 claim 最长 900 秒（含语义检测），查询 claim 180 秒；生产 HTTP 上限 30 秒，查询间隔 15 秒，单项处理上限 840 秒。SIGTERM 后停止领新任务、等待当前工作；强杀后由数据库租约恢复。已进入 `submitting` 的过期 owner 一律转不确定，不重发。

队列/审批准入上限为创建后 1 小时；查询截止创建后 23 小时，为官方任务有效期留余量。输入终结时清除，不确定输入在查询截止后 1 小时清除。成功 URL 仅加密保存 1 小时，API 不返回该地址；它尚未经过下载时网络校验或二进制审核。后台按 100 条批次清理密文。M4.3 必须扩展状态机并及时安全转存，不能靠暴露供应商 URL 实现交付。

每租户最多 256、每 Key 最多 32 个未闭合作业（含不确定/待核对），这是持久准入上限，不是 TPS 承诺。资金和作业使用统一锁顺序；媒体任务从文本短时维护路径排除，避免长视频被错误退款。

按 `video_second_720p`/`video_second_1080p` 计价，严格核对上游 duration、output_video_duration、input_video_duration、分辨率、比例、video_count，不猜缺失或异常用量。账本保留校验过的供应商用量、task ID、协议和价格/FX 快照，遵循 `units-v1` 的 CNY 整数微元和精确原币计算。配置价格核算不代表供应商发票已对账。

生成前取消释放预留；生成 POST 后返回 `cancel_requested=true`，不伪装上游已取消，不释放未知费用，仍查询并结算成功用量。M4.3 交付必须再次检查取消、身份和审批。Key 撤销后不得读取或新调用，但已发生费用仍需结算。

## 人工恢复

仅由 `brokerrouter_reconciler` 身份运行，无普通 Agent HTTP 入口。恢复与审计同一事务，runtime 缺少审计写权限会整体回滚。证据参数为脱敏票据/核查记录引用，不能填密钥或原始用户正文。

确认供应商原任务确实对应此作业后补录（不是重新提交）：

```sh
cargo run -- media-attach-task --tenant <TENANT_UUID> --id <JOB_UUID> \
  --provider-task-id <ORIGINAL_TASK_ID> --evidence <EVIDENCE_REFERENCE>
```

无可恢复任务或确认最终收费后，按明确账单证据核对。`usage.json` 是数组，如 `[{"unit":"video_second_720p","quantity":"0"}]`。零只用于确有未收费证据的情况；所有价格维度必须出现，超预算用量拒绝，不静默截断。

```sh
cargo run -- media-reconcile --tenant <TENANT_UUID> --id <JOB_UUID> \
  --usage-file usage.json --evidence <EVIDENCE_REFERENCE>
```

相同完整用量可重放，不同用量发生冲突；这不是余额调整接口。账单与配置价格差额需要明确的后续调账契约，不能私改历史账本。

## 验证与未完成闸门

本包 13 项本地测试覆盖 HTTP 闭环、动态多端点、原子预留/幂等、真人输入审批/同轮豁免、受理超时与 task ID 写库失败、只重试查询、结算故障恢复、租约抢占、取消资金、发送前停用、撤销后结算、数据库权限和严格解析。PostgreSQL 测试实际应用完整迁移，不用 mock 数据库替代。

未调用真实供应商。LIVE-10 跟踪区域/固定版本、用量/账单、长任务/超时、有效期、凭据轮换和取消语义；LIVE-05/06 跟踪真人渠道和 Runtime。按用户要求延期真实联调，不索取凭据，不把端点标为 certified。

上包 CI 的 MinIO 启动期 TCP reset 发生在常规测试通过之后。本包健康检查增加有界 `--retry-all-errors`，覆盖 refused/reset，仍在耗尽重试后失败退出，不忽略存储启动错误。
