# current-turn-output-v1：产物策略、真人审阅与受控交付

本包在隔离转存和不可变扫描证据之上接通实际字节交付，不把扫描完成、用户批准、文件读取或 Agent 执行完成混为一个状态。当前支持经过既有 MP4 档位技术检查的文生视频产物；图片、3D、专用云内容审核、控制台与 Runtime 原生弹窗不在本包完成范围。

## 授权链

```text
私有输出 + 当前配置的完整扫描
  → 技术 clean / 精确字节绑定 / Key、模型、Runtime、用户、turn 有效
  → 当前 media_output 策略
  → 内容 pass，或真人批准指定内容护栏（仅当前 turn）
  → 每个分块读取前后重新校验 → 审计提交 → 交付字节
```

技术 unsafe / unsupported / error、报告缺失、哈希/长度不一致、资产过期、作业取消、权限撤销、关闭或过期的 turn 都不允许人工作为内容例外放行。现有 ClamAV/FFprobe/FFmpeg 检测器的 `content` 恒为 `unknown`，因此首次匹配内容护栏必须等待真人决定；这不是已接入完整语义合规的声明。

每个产物必须从创建时就绑定受信 turn。旧的无 turn 作业不能在交付时由 Agent 临时追加上下文；本档位不会交付这种作业。当前 turn 最长一小时，产物审批有效期取 turn、资产、扫描有效期的最小值，不续期、不跨轮。视频播放/下载与后续用途必须在关闭 turn 前完成；历史资产导出、永久下载和跨轮重新授权不是本接口能力。

## 规则与策略

沿用 `guardrail-publish` 和 `policy-put` 运维入口，新增服务端专用阶段 `media_output`。例子中的 ID 是占位值，必须替换为本租户该模型已经发布且有效的扫描 profile。

```json
{
  "id": "video-content-review",
  "version": 1,
  "name": "视频内容审阅",
  "stage": "media_output",
  "rule": {"kind": "asset_scan", "profile_id": "00000000-0000-0000-0000-000000000001"}
}
```

该护栏的策略需要明确匹配对应 `model_id`，也可增加 Key / Runtime 条件。每次交付必须匹配至少一个 `asset_scan`，且所有匹配的扫描规则必须绑定当前 profile；缺少策略时封闭，不因删除最后一条护栏而自动开放。可以叠加 `always` 要求额外人工批准，不接受文本字面规则或文本 `model_judge` 代替二进制审核。`asset_scan` 不允许发布到其他阶段。

内容结果：`pass` 通过该内容护栏，`risk` 要求真人意见，`unknown` 明确展示未完成语义判断并要求真人意见。没有 `fail_open` 选项。检测 profile 升级后，需要发布引用新 profile 的新护栏版本；旧版本 grant 不覆盖新版本。更新配置的过渡窗口保持封闭。

普通 `/v1/governance/checks`、工具 claim、Agent JSON 内容不能创建/执行 `media_output` 操作。专用服务根据数据库事实创建操作，`media_output_deliveries` 不可变绑定 scan、job、operation；操作摘要再绑定资产 ID、SHA-256、长度、扫描报告摘要、profile 与 turn。数据库触发器拒绝不相容绑定。

## API 和恢复

新进程：`brokerrouter serve-output-delivery --bind 127.0.0.1:8085`。对外由 TLS 入口按以下路径转发到独立服务，不与模型执行或真人决定进程合并。

| 请求 | 身份 | 语义 |
| --- | --- | --- |
| `POST /v1/media/jobs/{job}/output/check`，正文 `{}` | 原 Key | 显式消费本地扫描证据，幂等创建/重检输出操作与审批，不调用收费模型或重发生成任务 |
| `GET /v1/media/jobs/{job}/output` | 原 Key | 当前可交付清单；不启动检测或创建审批 |
| `GET /v1/media/jobs/{job}/output/chunks/{n}` | 原 Key | 当前已获授权的精确分块，不接受 Range |
| `POST /v1/approvals/{approval}/output/review`，正文 `{}` | 请求绑定的真人证明 | 待决定审批的专用审阅清单，不是 grant |
| `POST /v1/approvals/{approval}/output/review/chunks/{n}`，正文 `{}` | 请求绑定的真人证明 | 技术检查通过、审批仍待决定时的私有审阅分块 |
| `POST /v1/approvals/{approval}/resolve` | 独立控制面的真人证明 | 沿用 M3 的明确批准、拒绝、反馈；交付服务没有此路由或数据库决定权限 |

`check` 返回 `ready` + `output`，或 `waiting_approval` + 原 M3 approval 对象。审批摘要含精确产物/报告元数据及风险原因；不含供应商 URL、对象存储路径、访问密钥或可分享授权。Runtime 持久化 operation/approval ID，经已实现的治理事件恢复等待；真人批准后可重新 `check` 或直接读取，GET 始终重新判定权限。

“继续本轮”只对用户选择的护栏版本写入 turn grant。随后本轮其他产物可以跳过同一版本的内容护栏，但每件产物的技术安全、扫描配置与当前权限仍必须有效。部分批准会产生剩余护栏的待决定审批；拒绝禁止该作业在同一或更新扫描配置下重新交付；反馈取消旧 turn，不能重新执行原任务。原币/汇率快照、人民币消费、预留和供应商生成次数均不因批准/拒绝/下载而改变。

清单给出整文件 SHA-256、字节数、4 MiB 分块大小及分块数，最后一块可较短。消费者必须按序拼接并校验整文件哈希后使用。断开后可安全重取同一编号，重复读取会单独记审计，不将 GET 次数当模型调用数。不生成永久 URL，也不将文件转为公开资产；所有旧 `/v1/assets` 输出读取路径仍禁止访问。

`operation_in_progress` 表示需要调用显式 check / 处理审批；`output_policy_missing` 表示未配置所需资产策略；`policy_changed` 要求重检并由运维更新对应规则；`output_unsafe` 是不可豁免技术失败；`result_unavailable` 可能是未完成转存/扫描、无受信 turn、过期、取消或不可访问。通过原媒体状态判断流程阶段，不自动生成替代任务。

媒体作业仍为 `output_pending`（生成终态），新增 `output_delivery` 提供操作状态和接口路径。操作状态是历史事实，不是缓存授权：`waiting_approval` 也可能已有有效 grant，`ready` 也可能因后续撤销不能读取。最终以交付服务当前授权为准。读完文件不把工具 operation 改为 `completed`，更不伪造 Agent 执行回执。

## 真人审阅和部署隔离

沿用 M3 的 RS256 `br-human+jwt`：可信 BFF 私钥不在 Agent 环境，证明绑定用户、租户、POST、完整路径和精确正文 SHA-256，每条证明短期有效。审阅第 0 块的证明不能读取第 1 块、另一审批或提交批准。每次审阅要求审批仍 pending、当前策略快照一致、用户拥有对应 turn；决定后审阅入口关闭。人类身份与 Key 不可相互代替。

返回分块使用 `application/octet-stream`、attachment、明确 Content-Length、no-store、nosniff、`default-src 'none'; sandbox`、no-referrer，不设置 CORS 或 cookie。BFF 不得在主控制台源直接解释未经审阅的主动内容；需要独立无凭据预览源、受限播放器/下载器、CSRF 与明确风险提示。此预览 UI 和真实 Hermes/OpenClaw 通道还需 M5/M6 实现和 LIVE-13 认证，本包只完成其可用服务端协议。

使用新增 NOLOGIN 权限组 `brokerrouter_output_delivery` 的独立 TLS 登录，应用迁移后执行 `deploy/permissions.sql`。该组只读输出资产/分块与扫描事实，可创建服务端操作、审批和审计；不能决定审批、写 grant、修改资产/扫描、更新账本/预算或切换到其他用途身份。真人控制组增加只读产物元数据以验证审批，不获得对象存储凭据或分块表权限。所有服务登录均不能是数据库 owner/superuser，不得附加其他组成员关系。

交付服务只需要 `BROKERROUTER_ASSET_*` 的固定 S3 配置和 **GetObject-only** IAM、结果加密 keyring，以及 `BROKERROUTER_HUMAN_*` 的验证公钥。它不需要扫描 token、供应商密钥、BFF 私钥、S3 写/删权限。旧输出 worker 继续负责资产取消/到期清理，人类控制服务继续清理到期签名记录；不能因没有运行清理器而延长授权。

读取以数据库时钟和 tenant/turn 锁序列化授权检查，在 S3 I/O 前提交锁事务，读取并校验密文后再次检查，再原子提交读取审计才返回字节。I/O 内发生撤销会拦住本块；最终授权提交后已交付的字节无法收回，撤销阻止之后的块。`output.chunk_served` / `output.review_chunk_served` 是服务端准备交付的审计，客户端断开时不表示已收到全部字节或执行成功。并发池在响应体消费/断开前保留许可；入口还需配置连接、空闲/总时长、慢客户端和速率限制，实际容量由 LIVE-07/09/13 验证。

升级先停止/排空旧服务、应用 0010 与权限脚本，再启动匹配版本；旧二进制 readiness 会因 schema 漂移拒绝就绪。回滚不删除审批、绑定、扫描或账本事实，回滚应用必须兼容新增 schema；没有经过演练的降级不允许带流量执行。

## 验证范围

新增 PostgreSQL/HTTP 用例覆盖并发幂等、人工审阅、相同决定重放、读取恢复、同轮与跨轮、技术门禁、profile/策略升级、部分批准/拒绝/反馈、撤销和期限、事务故障、真实受限数据库角色、请求绑定证明、旧接口/伪造阶段、读取中撤销及审计失败不交付。现有真实 S3 与 ClamAV/FFmpeg 用例扩展为审批后读取并校验精确字节。

本地合成风险回执是协议测试；实际技术检测用的是无害测试特征库，不代表生产检出率。专用多模态语义供应商、真实人机界面、组织权限/预览隔离、生产 S3/TLS/IAM 与容量仍是发布闸门。详见验收待办 LIVE-12/13。

本包同时修复资产分块幂等恢复：数据库已确认 stored 的分块重传只读取、解密、校验现存对象，不再重复 PUT；尚未确认写入的条件 PUT 遇到失败/不确定响应时，只在现存精确对象校验通过后恢复成功。缺失、损坏、不可读对象继续失败，不因数据库标记而伪造成功或重建对象。条件写仍使用 `PutMode::Create`，不会覆盖已有对象；其避免覆盖的 S3 语义参见 [AWS 条件写说明](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)。此恢复不改变任何收费模型 POST 的不确定处理。
