# R1.3 用途隔离的出站 HTTPS 私有 CA

三个现有 Rust 客户端及 Hermes 治理 HTTP client 支持启动时加载部署指定的私有 CA，继续使用各自默认信任根，并保留证书链与主机名校验。此包仅交付出站 TLS 配置；它不部署 IdP、S3、scanner 或完整审批链路，也不将本地 HTTPS 测试夹具记为真实身份认证或正式存储认证。

| 用途 | 可选环境变量 | 生效位置 |
| --- | --- | --- |
| Console BFF 到允许的 IdP 与固定控制面目标 | `BROKERROUTER_BFF_CA_FILE` | `serve-bff` 构造其唯一出站 HTTP client |
| 私有 S3，包括输入资产、输出转存、扫描与审阅的 S3 读取 | `BROKERROUTER_ASSET_S3_CA_FILE` | `Assets::from_env` 构造专用 S3 transport |
| 独立安全 worker 到固定扫描 API | `BROKERROUTER_SCANNER_CA_FILE` | scanner `Worker::new` 构造专用 HTTP client |
| Hermes 到治理网关 | `BROKERROUTER_HERMES_GATEWAY_CA_FILE` | `GovernanceGate` 构造治理 `HttpTransport` |

每个变量只读取本用途文件；未设置则维持现有默认信任行为。不会回退到 MCP/PG 的 CA 变量或另一个用途变量。当前锁定依赖通过 feature 合并启用了 WebPKI 与 native roots；系统库原本会读取 `SSL_CERT_FILE`/`SSL_CERT_DIR`，本包保留这一行为。用途隔离指显式新增 CA 配置，不表示隔离或移除现有系统信任根；部署需要用途隔离时，不应将本用途私有 CA 放入所有 workload 的通用系统 CA 覆盖变量。BFF 中的同一 CA bundle 会增加对已允许 IdP 与控制面 HTTPS 目标的信任，部署时只放入该用途确实需要的 CA。

通过现有 Secret 注入机制设置容器内只读文件路径，例如 `BROKERROUTER_BFF_CA_FILE=/run/secrets/bff_ca.pem`。CA 文件只允许 CERTIFICATE PEM 与 ASCII 空白，不接受注释、其他文字、私钥、公钥块或混合 PEM。文件必须是非空普通文件，最多 1 MiB、128 张证书；UTF-8/PEM/Base64/证书 DER 无效、路径为空、文件缺失或超限都阻止客户端构造，不会降级成忽略错误继续启动。真实凭据、CA 私钥和测试生成私钥不得进入镜像或仓库。挂载文件及其父目录由受信部署身份管理，Agent 与普通应用用户不得写入。

客户端构造时读取并解析一次。运维更新 CA 后应按不可变配置发布、排空和重启；修改或删除文件不会改变已运行客户端。轮换时可在同一用途 bundle 中同时保留旧、新 CA，完成服务证书迁移后再移除旧 CA 并重启。证书有效期、撤销策略、正式 DNS/TLS 责任仍需按目标环境落实，本功能不代替这些操作。

增加 CA 不扩大任何目标集合，不修改 BFF 的精确 HTTPS origin/OIDC origin allowlist，也不改变 S3 的部署固定 endpoint 或 scanner 的固定 origin。所有三类 transport 继续禁用环境代理和自动重定向，保留各自超时、重试及响应大小限制；MCP 的 DNS/IP pin 与 PostgreSQL TLS 配置不在本包改动范围。没有关闭证书、主机名校验的选项。

本地验证使用 OpenSSL 临时私有 CA 与 Python 3 标准库 HTTPS server，Rust 测试通过隔离子进程注入每个用途配置，避免并行测试期间修改进程全局环境。测试检查每个实际 client builder 的正确 CA/多证书 CA 成功，未配置、跨用途配置、错误 CA、错误主机名拒绝，非法文件启动失败，以及代理零命中、重定向目标零命中和启动后删文件仍可握手。执行入口：

```sh
df -h . /private/tmp
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --locked --lib tls_roots::tests::purpose_scoped_private_ca_https_clients -- --exact --nocapture
```

测试需要 `openssl`、带标准库 `ssl` 的 `python3` 和数值 loopback 监听权限，不调用真实供应商或数据库。临时证书、HTTPS/proxy 子进程仅属于该测试，正常结束自动清理。全仓格式、Clippy 与原有测试由整体 R1 验收另行执行。

Hermes 使用 HTTPX 0.28.1、`trust_env=False`，显式追加 CA 到 HTTPX 的默认公开根，不读取 `SSL_CERT_FILE`/`SSL_CERT_DIR` 或 Rust 用途变量。其私有 CA 变量须为绝对文件路径。安装器将适配器配置、transport 与全部其他文件摘要纳入受管镜像完整性清单。八项真实本地 HTTPS 测试使用锁定 Python 3.12 依赖，覆盖信任、主机名、多根轮换、代理/重定向拒绝、非法启动配置和响应大小/格式限制：

```sh
PYTHONPATH=adapters/hermes python -m unittest discover \
  -s adapters/hermes/tests -p test_transport_tls.py -v
```
