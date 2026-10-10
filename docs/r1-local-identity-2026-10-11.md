# 本地身份与本人审批验收：2026-10-11

全新隔离部署实际通过 **19 组断言，退出码 0**。使用官方 Keycloak 26.8.0 的 HTTPS、优化生产启动模式及独立 TLS/SCRAM PostgreSQL，实际运行 gateway、control、Console、approvals、BFF 和控制台 TLS 入口。浏览器执行真实 OIDC 登录、服务端 code 交换及本人审阅和两步确认审批。

后端固定为 release `73aae4b8b060f381a79a19b67987677fdc38b08b`，镜像 ID `sha256:897f5483b13a648b2a5053a4166e66c05ba5a070079ec0c3fd5ba148e0278000`。迁移和 canonical permissions 来自匹配的 **schema 29 源码快照**；未对该实例应用当前开发中的 0030/0031。控制台 TLS 模板沿用已验证的 Docker DNS 修复。官方与交付镜像 ID、源码与模板 SHA256、日志摘要及实际版本见[安全验收记录](evidence/r1-local-identity-2026-10-11.json)。

实际检查覆盖：

- PKCE S256、state/nonce、重新认证、固定回调、不可变用户绑定；未绑定和跨租户登录拒绝，其他用户无法查询、审阅或决定本人的审批。
- Secure HttpOnly 不透明会话、浏览器无上游 token 存储；缺少 CSRF、错误 Origin、未授权 Console 查询及消费后的回调重放均拒绝。
- 实际控制台显示风险原文、选择护栏版本、确认作用域并两步提交；原 Agent 重查得到 ready，注销和绑定撤销立即失效。
- 使用正确 HUMAN kid/type/audience、但由独立 approvals 私钥签名的断言返回 401；真实 Agent Key 也无法认证审批控制面。
- 登记 19 个用途 LOGIN；实际五个工作负载均独立用途、池上限 2、PG 连接全部 TLS。IdP 进程 UID1000，独立非超级用户 LOGIN、池 2–4、连接限制 10。仅一条本人决定及一条 grant，供应商/工具请求和账本条目均为零。
- 新构建的浏览器工具实际运行 Node 24.19.0、Playwright 1.63.0、Chromium 153.0.8010.12。系统 CA 包由官方 Node 公共根证书通过校验 HTTPS 引导，保留 Debian 签名及证书/主机名检查。服务日志没有生成凭据、HUMAN 断言、虚拟 Key 或 OAuth code/token。

三个用户均为明确的**合成测试身份**。HUMAN 私钥仅由受信宿主夹具持有，不进入 Agent、BFF 或浏览器。本轮使用随机独立项目和 tmpfs 数据库；结束后本包容器、网络、生成 Secret 目录均为零，未操作其他项目或数据卷。保留不可变镜像、源码和只读 npm 缓存供复验；清理后实际可用空间约 329 GiB。

此结果只验收身份和工具输入审批链路，**不能称完整 R1**，也不代表真实外部组织 IdP 已接入。当前 0030/0031、最终文本写入历史与原生通道发布、OIDC 到 Hermes 的真实通道 ingress、媒体/S3/scanner/delivery 和容量负载均未由本轮验证。最终 publication 的工作台格式缺陷已另行反馈并暂停其候选验收，本轮旧版本的成功不会覆盖该缺口。

复验入口和依赖见[独立身份验收说明](../deploy/verification/identity/README.md)。安全 JSON 只保存公共元数据和日志 hash；原始私密状态及生成凭据已删除，未进入仓库。
