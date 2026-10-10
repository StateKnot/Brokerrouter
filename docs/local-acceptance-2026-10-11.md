# R0 本地发布验收（2026-10-11）

用户已授权 GitHub 无额度时采用本地部署验证。本次 R0 开发退出检查通过，可以继续 R1 开发；AUD-05、目标环境容量、真实身份/Runtime/供应商及完整产品生产验收仍未关闭。

发布代码固定为 `a633eeff1da55fb1cd5ffa88060e723570f0915d`，基线 `e01ecb9`；[草稿 PR #40](https://github.com/StateKnot/Brokerrouter/pull/40)。镜像从该提交的 `git archive` 纯源码快照构建，未复制 target、node_modules 或虚拟环境，未包含同时开发的 Hermes R1 代码。实际镜像 ID 为 `sha256:874000faca6075f1d9d5ed1b2b28fa8f714921116afc7c1a86c1695979891d81`，标签 `brokerrouter:a633eef`。

## 检查结果

| 检查 | 固定环境与结果 |
| --- | --- |
| Rust | 1.98.1、锁文件；fmt、Clippy 全目标 `-D warnings`、全部 release 目标构建通过 |
| 常规 Rust 测试 | 真实隔离 PostgreSQL：269 通过、0 失败、4 环境专项 ignored |
| 环境专项 | 4 个 ignored 显式执行：2 个真实 ClamAV/FFmpeg、S3 条件写和 S3 输出转存全部通过 |
| Console | Node 24.19.0、Playwright 1.63.0：格式、构建、28 单元、132 浏览器场景通过（Chromium 66、WebKit 66），真实 Nginx 主站/预览站契约通过；npm audit 0 漏洞 |
| Hermes R0 | Python 3.12.10、httpx 0.28.1、cryptography 50.0.0：26 用例中 24 通过，2 个完整宿主导入契约按原条件跳过；固定三源码 SHA、安装前检查/应用/复查、编译和 required 模式 manifest 检查通过 |
| 本地部署 | 生产镜像、TLS PostgreSQL、UID 10001 和受限 runtime 登录、独立 MCP judge worker；20 轮慢客户端及真实 180 秒连接期限通过 |

Hermes 的两个跳过项没有记为通过；三文件 overlay 验证不能代替 R1 的完整原生会话/工具/发布与恢复验证。浏览器仍使用 API fixture，不能代替真实 OIDC。此次没有调用付费供应商，供应商费用为零。

## 部署资源与账本证据

项目 `brokerrouter-local-acceptance-20261010` 使用独立合成凭据和数据卷。网关配置为 HTTP/1.1、TCP 上限 64、SSE 下游上限 4、上游并发 16、请求期限 120 秒、连接期限 180 秒、内存上限 512 MiB、停机宽限 6 分钟。压测 RSS 阈值在运行前固定为 384 MiB。

20 轮每轮增加 4 个不读取响应的客户端（接收缓冲 1024 字节，每次响应 3 MiB），包含上游已结束仍滞留的连接：

- 最多 64 个建立的 TCP 连接；64 次慢连接进入，16 次超额连接被拒绝。
- 进程峰值 RSS/HWM `142831616` 字节（约 136.2 MiB），观测到的 cgroup 峰值 `119468032` 字节；两者是不同采样指标，不能相加。
- 67 次成功请求对应 67 次供应商夹具调用，每次恰好一条 finalize；单价独立断言为每次 7 人民币微元，总计 469 微元，预留余额归零。
- 断连/慢消费未阻断后台结算，最长观测等待 1.563 秒；重放和服务重启没有增加供应商执行次数。
- HTTP/2 preface/SETTINGS 被拒绝，没有增加请求或费用。空闲和未完整 HTTP 请求在 180.004 秒关闭，连接容量恢复，未产生费用。

原 Axum 自动协议路径在同一部署配置上接受 HTTP/2，每个 TCP 可声明 200 个并行 stream；因此只限制 TCP 无法解释原内存上界。旧部署探针和永久回归都能失败，新显式 HTTP/1.1 发布物通过。HTTPS 边缘可终止客户端 HTTP/2，并以 HTTP/1.1、关闭 SSE 缓冲转发到网关。

## 证据与重建

可移植的非机密计数、版本和日志摘要见 [证据索引](evidence/r0-local-2026-10-11.json)。原始日志和报告保存于 `/private/tmp/brokerrouter-acceptance-20261010/evidence`，位于构建缓存之外；重建命令见 [本地生产部署验证](../deploy/verification/README.md)。合成数据仍保留供恢复检查，不删除其他 Docker 项目或用户数据。

本机确认旧 dev 缓存可重建且无活跃构建后进行了清理，`df` 可用空间由 280 GiB 增至 347 GiB，实际增加 67 GiB；没有将目录逻辑大小当作释放量。当前仓库约 5.3 GB 构建缓存继续用于 R1（不复制），Console 约 140 MB 和 Hermes 约 21 MB 依赖用于重复验证；后续按容量/年龄管理。其他项目的活跃缓存、数据库卷与共享 Docker 缓存保留。

AUD-01–04 的本地修复/回归与本次核心部署证据已具备；这份报告不关闭真实环境/完整产品条目。GitHub job 无执行步骤的账户额度故障与私有仓库保护 403 仍待环境条件恢复，不记作代码失败，也不改变仓库可见性绕过保护。
