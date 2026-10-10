# R1 本地基础闭环验证（2026-10-11）

工具最终文本的隔离、当前策略审批和重启恢复已通过本地部署验证。R1 继续开发：真实模型请求身份、最终助手文本与会话发布、二进制产物扫描/审阅/下载、真实 OIDC 和原生通道完整验收仍需收口。用户已授权本地验证；GitHub 额度和强制检查保护条件继续阻断正式远端发布。

## 固定版本与检查

运行服务从 `73aae4b8b060f381a79a19b67987677fdc38b08b` 的 `git archive` 纯源码构建，镜像 ID 为 `sha256:897f5483b13a648b2a5053a4166e66c05ba5a070079ec0c3fd5ba148e0278000`。原快照没有纳入仅供测试的 `src/tls_roots/https_fixture.py`；该文件已在 `116fe8d` 补入 Git，完整可重建测试源码为 `a0e0254`。最终测试涉及的 123 个 Cargo、Rust、迁移、权限及夹具文件摘要已逐一核对；运行 Rust 和 0029 及之前迁移没有改变。报告不将原快照宣称为完整测试源码。

| 检查 | 实际结果 |
| --- | --- |
| Rust 常规 | 隔离 PostgreSQL 17、锁文件：297 通过、0 失败、4 ignored；fmt、Clippy 全目标 `-D warnings`、release 构建通过 |
| 四项环境专项 | 固定 ClamAV/FFmpeg 两项、真实 S3 资产及输出两项显式执行，全部通过 |
| 私有 CA | 三个 Rust 生产 client builder 共 48 个真实本地 TLS 场景；Hermes HTTPX 8 个真实 HTTPS 场景通过 |
| 完整 Hermes 宿主 | 固定上游 `67764dc`，Python 3.13.5、httpx 0.28.1、cryptography 50.0.0：八源码及适配器 169 通过、0 跳过，代码包 `a0c8958` |
| 独立用途登录 | 19 个实际 TLS/SCRAM LOGIN，33 个部署场景通过；固定运维镜像的新装/升级/冲突及明文连接反向验证通过 |
| 控制面部署 | 14 个用途配置、严格无 eval 入口、Compose、发布预检、真实主站/预览 TLS 契约通过；实际后端 IP 替换后两个存活 edge 无重启切到新地址 |

Rust 使用 `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`，复用当前仓库缓存。测试日志和交付证据保存在构建目录外。验证前 `df` 可用空间约 333 GiB、缓存约 5.4 GB，未复制或清理其他项目缓存。

## 进程分离的 HTTPS 审批恢复

独立项目 `brokerrouter-local-hermes-20261011` 使用 0029 schema、固定服务镜像、TLS PostgreSQL、最小用途登录和两个独立私有 HTTPS CA。Agent 容器只持有虚拟 Key、加密检查点密钥和 runtime CA；没有审批私钥、controller Secret、control CA 或 Docker socket。独立 controller 使用本地合成人类签名夹具驱动审批；这验证签名用途与进程边界，尚不代替真实 OIDC 用户验收。

实际顺序及最终事实：

1. 执行零费用工具一次，服务端完成文本登记后故意丢掉成功响应；本地恢复只补登记，不重新派发。
2. 输出等待审批时没有返回正文；独立控制身份批准版本 1 后，恢复返回原 UTF-8 文本。
3. 操作员发布版本 2，原 grant 不能释放同一结果，产生新审批；新的当前版本决定才允许继续。
4. 实际重启 gateway 容器并等待 HTTPS 就绪，再恢复原调用，返回相同字节。

最终数据库为 1 个 input claim、1 个 completed receipt、1 份加密结果、1 个 output operation、2 个版本的审批和 2 个 grant。工具执行标记始终为 1，模型 requests 为 0，费用为 0，runtime 不能 UPDATE 审批或 INSERT grant。没有调用外部付费供应商。

这条部署使用真实 Rust HTTP/数据库和 HTTPX TLS；宿主串/并行工具、hook、异常和发送台账由固定完整 Hermes 导入测试覆盖。它们尚未组成真实模型、登录用户、通道和二进制发布的全链路认证。

## 本包修复的部署与到期竞争

共享治理 gate、工具文本释放、MCP 已完成结果、claim 和输出下载均重新核对实际审批截止时间，防止锁内审计/事件写入跨过到期时刻。输出下载回归用真实对象和到期触发器证明最后审计确实运行，过期响应不带原文或字节。

真实 Compose 启动复现了 inline tmpfs 列表将 `mode=1777` 拆成第二个挂载的错误；已将整个选项作为一个字符串，旧配置失败、新配置启动通过。真实后端替换复现 Nginx 固定启动 DNS 地址持续发送到旧 IP；固定用途目标现使用 Docker resolver，实际请求保留原路径、查询、JSON 正文和签名/Cookie/CSRF/Origin，无客户端指定目标和自动重试。

## 保留的验收条件

八源码新增发送台账固定内容摘要、路由和一次领取；平台确认丢失或进程退出保留发送未知，不能盲目重发。加密产物 spool 的 15 个场景包括真实 SIGKILL、原字节恢复和不可变事实。两者尚不赋予最终助手正文或二进制内容发布权：最终助手文本必须绑定实际模型 request UUID 并重检服务端 post 策略；二进制必须完成真实字节隔离、扫描、本人审阅及当前授权下载。

安全的版本、计数和日志 SHA 见 [证据索引](evidence/r1-foundation-2026-10-11.json) 与 [八源码完整性](evidence/hermes-native-eight-2026-10-11.json)。原始日志位于 `/private/tmp/brokerrouter-acceptance-20261010/evidence`，合成凭据与数据库卷保留在 Git 之外。R1.1–R1.4 仍为开发/联调中，R2–R5 继续保留原范围，不将本报告记为完整生产认证。
