# M4.1 私有输入资产与非 Token 人民币计费

更新：2026-09-12。此交付包实现 `private-input-assets-v1` 与内部 `units-v1` 账本契约。多模态生成作业、供应商输出转存、二进制审核与审批后交付属于后续 M4.2/M4.3，不在本包伪装成已支持。

## 服务边界

`brokerrouter serve-assets --bind 127.0.0.1:8082` 使用独立数据库登录身份，权限组为 `brokerrouter_assets`。统一网关域名的反向代理把 `/v1/assets` 及其子路径转给该服务，其余模型调用仍转发到 `serve`。不把服务端口直接暴露公网；外层终止 TLS、限制连接数和上传/下载空闲时间、限制请求体至 4 MiB，禁止日志记录鉴权头和二进制正文。完成校验可能持续 300 秒，代理的该路由响应超时需与之匹配。

本服务使用虚拟密钥，不是后台管理员或真人审批接口。资产绑定创建者的 `tenant_id + key_id`；同租户另一个 Key 也不能读取。当前不支持资产跨 Key 分享。撤销/过期 Key、停用租户立即禁止新操作；已发送到客户端的字节无法收回，后续下载分块重新鉴权。

只有用户上传的私有 **input**。无 `kind=output` 开关、无任意 URL 导入、无供应商临时 URL 透传、无公开/预签名下载链接。`declared_media_type` 仅是调用方声明；`content_safety=not_evaluated` 明确表示未执行内容审核、格式探测或恶意文件扫描。完整性校验通过不等于安全通过。下载总是 `application/octet-stream`、`attachment`、`nosniff`、`Cache-Control: private, no-store`，不内联执行或预览内容。

## API

所有业务请求带 `Authorization: Bearer <VIRTUAL_KEY>`。所有返回资产 ID 的接口均不返回对象存储路径或凭据。

| 方法与路径 | 契约 |
| --- | --- |
| `POST /v1/assets` | 创建不可变上传清单；必须带 `Idempotency-Key` |
| `PUT /v1/assets/{id}/chunks/{ordinal}` | 二进制分块上传，序号从 0 开始 |
| `GET /v1/assets/{id}` | 查看状态、期限、声明类型、哈希与分块进度 |
| `POST /v1/assets/{id}/complete` | 重新读取并解密全部分块，核对整个文件长度与 SHA-256 |
| `GET /v1/assets/{id}/content` | 顺序下载完整内容；不支持 HTTP Range |
| `GET /v1/assets/{id}/chunks/{ordinal}` | 下载单块；供断点下载使用 |
| `DELETE /v1/assets/{id}` | 立即禁止逻辑访问，异步清理存储，重复删除不重复释放配额 |

创建正文（示例哈希对应 `abc`，不代表有效图片）：

```json
{
  "byte_length": 3,
  "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  "media_type": "application/octet-stream",
  "ttl_seconds": 3600
}
```

单资产 1 字节至 512 MiB；固定 4 MiB 分块，最后一块必须精确等于清单剩余长度；最多 128 块。TTL 为 60 秒至 30 天，上传必须在创建后 `min(TTL, 1 小时)` 内完成。所有期限从创建时起算，重试不续期。允许声明 PNG/JPEG/WebP/GIF、MP4/WebM、WAV/MPEG/OGG、GLB 或通用二进制；类型不在列表、未知 JSON 字段、非法 SHA-256/长度/期限均拒绝。

同一幂等键与相同清单返回同一资产，包括已经删除/过期的资产；清单改变返回冲突。分块一经登记不可换内容：相同序号和内容可在 uploading 阶段重试，不同内容冲突。完成后重复 complete 幂等返回原状态；已完成后的 PUT 不再接受。整个哈希不符不会发布为 available，需删除并用新幂等键重新上传，不能修改原分块“修补”。

上传或完成 HTTP 响应丢失时，先 GET 状态，再恢复同一不可变操作；不要创建新 ID 猜测重传。上传/完成工作由服务端跟踪，即使客户端断开也继续执行；进程崩溃后，清单与分块状态仍在 PostgreSQL。服务正常关闭时排空跟踪任务。S3 超时/故障返回安全错误，不把原始错误中的 URL、签名或凭据发给客户端。

## 一致性、配额与清理

准入在同一事务中锁租户再锁 Key，两层都必须有足够资产配额；一层不足整笔回滚。默认配额为 0，需要运维显式启用：

```sh
brokerrouter asset-quota-set --tenant <TENANT_UUID> --bytes 1073741824
brokerrouter asset-quota-set --tenant <TENANT_UUID> --key <KEY_UUID> --bytes 536870912
```

配额是已准入的逻辑明文字节数（包括尚未完成的上传），不是对象存储账单；删除即释放逻辑配额，物理密文、加密开销、版本与备份仍可能占存储并产生费用。不能把资产字节配额计入模型人民币消费。物理容量、长期 tombstone 增长及服务容量需要 LIVE-07/LIVE-09 压测和告警；不要无限开大配额。

每块先登记不可变 UUID、长度和 SHA-256，再用 AES-256-GCM 加密，租户和块 UUID 绑定于认证数据；对象路径由服务端构造。S3 必须正确支持 `If-None-Match: *` 条件写。数据库确认失败但对象已写入时，重试读取旧密文、解密并核对哈希，不覆盖已有对象；若对象损坏或无法解密，则失败关闭。

删除/到期留下不可复开的 tombstone。维护任务每 30 秒处理最多 100 个到期资产；逻辑删除后至少等 2 分钟再清理。每轮最多扫描 16 个清理资产，存储删除有 10 秒总 I/O 时间窗，逐块持久化游标，失败后延迟 5 分钟再试。成功后每 24 小时复核已知对象键，处理存储端迟到的 PUT。进程中断、重复删除、Key 撤销不导致二次减配额；网络调用不持有数据库行锁。此机制不承诺对象版本/供应商备份立即销毁。

数据库只保留清单、配额、哈希、状态及审计，不保存文件字节。`asset.chunk_read` 是读取授权尝试审计，不保证客户端最终完整收到文件。资产元数据保留策略与监管要求需在部署时确认；当前不能通过应用角色删除/截断这些事实表。

## 存储与密钥配置

通过部署 Secret 注入以下环境变量，不能写入仓库、镜像或真实 CLI 参数：

| 变量 | 说明 |
| --- | --- |
| `BROKERROUTER_ASSET_S3_ENDPOINT` | 运维配置的可信 HTTPS 源站，只允许 scheme/host/port；不可来自租户输入 |
| `BROKERROUTER_ASSET_S3_BUCKET` | 专用私有桶 |
| `BROKERROUTER_ASSET_S3_REGION` | 固定区域 |
| `BROKERROUTER_ASSET_S3_ACCESS_KEY_ID` / `BROKERROUTER_ASSET_S3_SECRET_ACCESS_KEY` | 最小权限的存储服务凭据 |
| `BROKERROUTER_ASSET_S3_SESSION_TOKEN` | 可选临时会话凭据；轮换后按部署流程重启实例 |
| `BROKERROUTER_ASSET_CONCURRENCY` | 1–64，默认 8；限制上传/校验/下载工作并发 |
| `BROKERROUTER_ACTIVE_RESULT_KEY` / `BROKERROUTER_RESULT_KEYS` | 本服务独立的应用加密 key ID 与 keyring，格式复用 M2 加密契约 |

只为本服务授予桶中 `assets-v1/*` 的对象 Get/Put/Delete，不授予建桶、公开 ACL、桶策略或租户选择任意对象路径的能力。桶必须禁止匿名读取和公共 ACL；备份同样限制访问。客户端采用 path-style 签名、显式凭据、禁用代理继承、重定向和隐式重试。底层 S3 客户端使用固定 [object_store 0.13.2](https://docs.rs/object_store/0.13.2/object_store/aws/struct.AmazonS3Builder.html)。特定国内 S3 兼容服务的签名/区域/条件写支持不能由 MinIO 测试替代认证。

默认不允许 HTTP；`BROKERROUTER_ASSET_ALLOW_HTTP=true` 仅用于隔离本机 fixture，生产不得启用。部署负责网络出口白名单；私有 S3 源站由运维信任，不使用用户可配置 URL。无默认 AWS 环境凭据发现或匿名降级。

应用加密密钥必须与对象及 PostgreSQL 备份共同纳入恢复策略。新写入使用 active key，读取按 envelope 中 key ID 选择；轮换时保留旧 key，直到所有可能恢复的旧资产与备份生命周期结束。不要依据 HTTP 结果缓存 TTL 提前删除资产加密 key。丢失 key 会导致资产不可恢复，不能以禁用校验绕过。

`/health/ready` 当前只验证数据库与 migration checksum，不证明桶 IAM、可写性、存储容量或备份健康。生产须有独立 S3 canary、错误率/延迟、GC 积压、过期未清理量和容量告警，并纳入 LIVE-09 验收。

## units-v1 人民币主账本

内部 Rust `UnitPriceSnapshot` 支持最多 16 个明确计价维度，如 `image_1024`、`video_second_720p`、`generation_3d`。维度由受信供应商适配器/配置确定，不能根据名称猜测，也不能让调用者提交自定价格或 usage。当前未公开价格管理/生成 API；本库将在 M4.2 作业事务中使用，不能据此宣称已能发起付费生成。

快照包含 `schema=units-v1`、非空 `price_version` UUID、CNY/USD 原币、FX 版本、精确汇率、每维度单价与最大数量。所有价格/数量都是十进制字符串；只接受非负、单值不大于 1e9、最多 12 位小数。单位名只允许小写字母、数字、`_-.`，长度至 64；最大数量必须大于 0。超出精确算术表示范围直接拒绝，不隐式降低精度。

准入要求预留人民币微元恰好等于报价上限，经原有租户/Key 预算事务和授权模型检查。USD 版本必须是当前租户真实存在、已生效且金额匹配的版本。原币各维度乘积精确相加，再乘被冻结的汇率，最后整笔向上取整至人民币微元；不会逐图片/逐秒提前取整，更不会换算成 Token。

结算必须提供每一个已报价维度，零用量也需显式给出。缺失/未知/重复维度、负数、超出最大数量、无法精确表示的金额均拒绝并保留现状供核对。幂等结算同时比较归一化的完整用量证据与费用，不仅比较四舍五入后金额。已发送后 Key 撤销不抹去真实费用；重复结算只有一笔 finalize 和一次剩余预留释放。结果未知不能自动释放预留或重提收费请求。

单位费用与 Token 费用共享 requests/ledger_entries，不建立第二份资金事实源。账本保留原币金额、精确人民币金额、最终人民币微元、计价版本与各维度数量。后续任务成功/部分成功/取消时，供应商是否收费以及真实用量凭证仍需具体连接器实现和认证。

## 本地验证与生产发布闸门

常规 `cargo test --locked` 包含资产、单位计价及历史回归。S3 测试被显式标记 ignored，必须单独运行；CI 已添加该步骤，不能把默认测试集通过当作 S3 验证通过。

```sh
docker compose -f compose.test.yaml --profile assets up -d minio
curl --fail --silent --show-error --retry 15 --retry-connrefused --retry-delay 1 --max-time 30 http://127.0.0.1:59000/minio/health/live
curl --fail --silent --show-error --max-time 10 --aws-sigv4 'aws:amz:us-east-1:s3' --user 'brokerrouter-test-only:brokerrouter-local-only-not-production' -X PUT http://127.0.0.1:59000/brokerrouter-test-assets
# Use the fake fixture-only environment values from .github/workflows/ci.yml.
cargo test --locked --test assets s3_conditional_write -- --ignored
docker compose -f compose.test.yaml --profile assets rm -sf minio
```

上述 MinIO 只绑定本机 59000/59001，tmpfs 数据随容器删除而丢失；测试假凭据不得复用到生产。运行测试需要同 README 中的专用 PostgreSQL。桶创建重复执行可能返回已存在，应确认目标仍是测试容器，不能改成操作真实桶。

生产 S3 的私有策略、密钥轮换、地区网络、物理清理/版本保留、备份恢复和完整容量测试仍未完成，见 LIVE-09。M4.2 下一步为持久生成作业、供应商提交/查询与不确定状态；M4.3 接输出转存、资产感知护栏、真人审批后交付与取消竞态。尚不索取真实供应商凭据。
