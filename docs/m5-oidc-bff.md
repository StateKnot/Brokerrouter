# M5.2a：真实 OIDC 协议与控制台 BFF

本包交付 `oidc-console-bff-v1`：Rust BFF、持久登录事务/会话、预登记身份、报表代理和退出撤销。它对接标准 OIDC，不提供测试密码或“管理员万能 Token”生产入口。后续 [M5.2b](m5-console-ui.md) 已接入 TypeScript 登录/只读页面和同源 HTTPS edge；本地协议测试 IdP **不等于真实用户系统或整链路浏览器认证**。当前不能宣称整个控制台已上线。

## 身份链路与授权边界

1. 同源页面 `POST /bff/v1/login`，正文 `{"tenant_id":"UUID"}`。指定租户只选择登录上下文，不授予成员身份。
2. BFF 返回 `authorization_url` 和 HttpOnly 登录 Cookie。浏览器执行顶层导航，IdP 进行真实登录。
3. IdP 回到固定 `/bff/v1/callback`。BFF 校验独立 Cookie + state，原子消费一次性事务，再交换授权码、校验 ID Token，匹配 `(租户, OIDC issuer, OIDC sub)` 的预登记绑定。
4. 登录成功轮换随机会话 Cookie，固定 303 到 `/`。页面读取 `/bff/v1/session` 获取当前租户、内部用户、到期时间与 `CNY`；没有上游 Token、角色或签名私钥。
5. 页面调用固定报表路由，BFF 验证会话和 CSRF，向独立 `serve-console` 签发最多 30 秒的 `br-console+jwt`，绑定确切路径、方法和原始正文。报表返回前再次查库验证会话，正在执行的退出/撤销不会依靠旧快照放行。

外部 OIDC `sub` 与内部治理用户 `subject` **不是同一个命名空间**。绑定通过已有治理用户 UUID 显式建立；不能用邮箱、姓名、浏览器的 `tenant_id/roles` 自动认领，也不会登录即创建管理员。内部治理用户的 issuer 必须等于本 BFF 的公开 origin；不匹配则拒绝登录。

本包仅能代理 `usage/query`、`usage/groups/query`、`requests/query`、`audit/query` 四条只读路由。BFF 不提供通用 URL 代理或“生成任意证明”API，不返回内部证明，不转发浏览器 Authorization、Cookie 或其他自定义头到后端。审批/创建 turn/资产预览不在本包开放。**BFF 签名公钥只能配置到 console 信任集合，禁止同时信任为 HUMAN/审批公钥**；未来审批 BFF 必须有独立用途与明确用户操作。

## OIDC 合同

- 固定 `openidconnect = 4.0.1`，授权码流、S256 PKCE、随机 state/nonce、`max_age=0`；不申请 `offline_access`，不保存任何 access/refresh/ID Token，不提供密码流。
- Discovery/JWKS 在登录发起和回调各读取一次，支持密钥轮换。只访问管理员配置的 HTTPS origins；禁用代理、自动重定向和请求重试。浏览器、JWT 的 URL 头不能新增目标。IdP 必须是受信任的管理员配置；网络策略仍须限制服务出站，配置不是任意用户输入的 SSRF 过滤器。
- 校验签名、issuer、audience、nonce、exp，且显式要求 `azp`（存在时等于 client ID，多 audience 时必需）、`auth_time`、近期 iat。只支持 RS256 + `client_secret_basic` 这一认证档位，不声称支持所有 IdP 算法和认证方式。
- `auth_time` 不能早于本次登录事务开始时间 5 秒以上、不能晚于当前时间 5 秒以上；要求 IdP 对 `max_age=0` 返回实际认证时间。认证最大年龄/iat 限制 300 秒，登录事务 5 分钟。
- 发起与回调的 authorization/token/JWKS 端点必须一致；端点变更时重新发起登录，不把旧 code 发到新端点。响应 `iss` 存在时必须匹配配置机构。
- 回调事务先删除再交换：网络中断、进程终止、兑换成功后数据库失败都要求重新登录，禁止重放原 code。失败不会提交新会话或伪造成功审计。

标准依据：[OpenID Connect Core 1.0](https://openid.net/specs/openid-connect-core-1_0.html)；库接口与不跟随重定向的要求见 [openidconnect 4.0.1](https://docs.rs/openidconnect/4.0.1/openidconnect/)。库有意不默认校验 `azp`，本项目在应用层补充了上述固定 client 合同，并以错误 azp 用例验证。

M5.2b 补充浏览器错误落点：回调 `Accept: text/html` 的失败统一 303 到 `/?login=failed`，包括查询解析错误，并清除登录 Cookie；不把上游错误、code/state 放进重定向地址。非 HTML 请求仍保留 JSON 错误合同。登录安全校验与会话事务不变，新增 PostgreSQL/HTTP 回归覆盖此边界。

## Cookie、会话与恢复

| Cookie | JavaScript 可读 | 内容 | 属性 |
|---|---|---|---|
| `__Host-br-login` | 否 | 256 位随机值 | Secure、HttpOnly、SameSite=Lax、Path=/、最长 300 秒、无 Domain |
| `__Host-br-session` | 否 | 256 位随机值 | Secure、HttpOnly、SameSite=Lax、Path=/、无 Domain |
| `__Host-br-csrf` | 是 | 独立 256 位随机值 | Secure、SameSite=Lax、Path=/、与会话同到期、无 Domain |

所有 POST 必须有精确 `Origin`、`application/json`；若有 `Sec-Fetch-Site` 必须为 `same-origin`，不信任 Referer/Host/X-Forwarded-* 推断来源。已登录 POST 还必须传 `X-BR-CSRF`，与 CSRF Cookie 匹配并通过服务器存储摘要校验。缺少、重复、超长或格式错误的 Cookie/CSRF 拒绝；不开放 CORS。公开源必须独立承载受信任控制台，不能与用户上传 HTML 共源。

数据库只存会话/CSRF/state/Cookie 的 SHA-256 摘要；登录 nonce/PKCE 由独立 BFF AES-GCM keyring 加密，AAD 绑定租户和登录事务 UUID。原始会话令牌只在 Set-Cookie 中发送一次。

会话有效期为 ID Token exp 与认证时间 + 1 小时的较早者，无滑动续期或后台刷新。成功重新登录原子撤销该浏览器旧会话；同一绑定最多保留 10 个未撤销会话，超限撤销最旧。BFF 多副本共享 PostgreSQL，不依赖粘性会话。配置指纹变更（公开源、issuer/client、允许的 origins、console 目标/audience、签名 kid）使旧会话和未完成登录失效。

绑定停用/重新启用改变 generation；租户或治理用户 `active` 变化由数据库触发器增加 epoch。旧会话即使后来重新启用也不会复活。每次读前均查库，报表读后再查一次；不能收回此前已传输的数据字节，不能将这套读取授权宣称为全局分布式撤销事务。

`POST /bff/v1/logout` 正文 `{}`，需要 CSRF；撤销和审计同事务提交后才清除 Cookie。审计失败返回错误且事务回滚，不谎报已退出。退出只终止 Brokerrouter 本地会话，不终止 IdP 的其他应用会话；下一次登录仍要求 `max_age=0`。IdP 侧停用账号/全局登出没有实时后门回调，本地最多一小时到期；紧急退出须同步禁用本地绑定/治理用户，不能依赖 IdP 单点注销自动传播。

## 运行配置与权限

使用独立 TLS 数据库登录，只授予 `brokerrouter_bff`；应用新迁移后执行 [permissions.sql](../deploy/permissions.sql)。BFF 仅能读身份元数据，写/清理自己的登录和会话表、追加审计；不能读取模型凭据、账本、请求正文、报表视图、资产或审批表，也不能修改身份/权限/预算。报表使用另一个仅有 `brokerrouter_console` 的数据库登录。

环境变量（以下仅为变量说明，真实值从部署 Secret 系统注入，禁止进仓库）：

| 变量 | 要求 |
|---|---|
| `DATABASE_URL` | BFF 专用最小权限 TLS 登录，非迁移账户 |
| `BROKERROUTER_BFF_ORIGIN` | 如 `https://console.example.com`，精确 HTTPS origin，无尾部 `/` 或路径 |
| `BROKERROUTER_OIDC_ISSUER` | 真实机构的 issuer，允许 realm 路径，不允许查询串/凭据/fragment |
| `BROKERROUTER_OIDC_ALLOWED_ORIGINS` | 可选 JSON 字符串数组，最多 8 个精确 HTTPS origin；默认仅 issuer origin；必须包含 issuer origin |
| `BROKERROUTER_OIDC_CLIENT_ID` / `BROKERROUTER_OIDC_CLIENT_SECRET` | 机密 OIDC 客户端；只登记固定 `{BFF_ORIGIN}/bff/v1/callback`，无通配 redirect |
| `BROKERROUTER_BFF_CONSOLE_ORIGIN` | 独立报表后端的精确 HTTPS origin，禁止暴露给公网；配置 egress/ingress 限制 |
| `BROKERROUTER_CONSOLE_AUDIENCE` | 与 serve-console 完全一致的独立用途 audience |
| `BROKERROUTER_BFF_SIGNING_KID` / `BROKERROUTER_BFF_SIGNING_PRIVATE_KEY` | 专用 RSA 至少 2048 位 PEM；对应公钥登记到 console，绝不登记到 HUMAN |
| `BROKERROUTER_BFF_ACTIVE_KEY` / `BROKERROUTER_BFF_KEYS` | 独立 AES-256 keyring，JSON `{kid:64位hex}`；不能重用模型/资产解密密钥 |

console 服务：`BROKERROUTER_CONSOLE_ISSUER={BFF_ORIGIN}`、相同 audience、`BROKERROUTER_CONSOLE_PUBLIC_KEYS` 为 `{kid:公钥PEM}`。先在 console 增加新公钥，再切 BFF 私钥/kid（用户需重新登录），至少等待旧证明 30 秒和滚动发布窗口后移除旧公钥。BFF 加密 keyring 轮换保留旧键直到所有旧登录事务过期并清理完成；不会需要保留上游 Token。

Operator CLI：

```text
brokerrouter governance-user-add --tenant <uuid> --issuer <BFF_ORIGIN> --subject <内部不可变ID>
brokerrouter bff-identity-bind --tenant <uuid> --user <治理用户uuid> --issuer <OIDC_ISSUER> --subject <IdP不可变sub>
brokerrouter console-permission-set --tenant <uuid> --user <治理用户uuid> --permission usage_read --enabled true --reason <原因>
brokerrouter bff-identity-set --tenant <uuid> --id <绑定uuid> --active false --reason <停用原因>
brokerrouter serve-bff --bind 127.0.0.1:8087
brokerrouter bff-maintain
```

绑定不可重新指定 user/issuer/sub；迁移身份应先停用旧绑定再登记新身份，保留审计。无权限用户能登录但报表返回 403，不能靠前端隐藏列替代授权。`audit_read` 需单独授予；不授予会话自己的权限管理能力。

## 部署约束与容量

- 当前 Rust listener 在受信网络/loopback 后承载 HTTP，公网入口必须终止 TLS，固定单一 Host/origin，仅转发 `/bff/v1/*` 到 BFF；健康检查仅内部开放，其他路径由后续静态控制台承载。不可直接把该 HTTP listener 对公网发布。服务响应设置 no-store、nosniff、no-referrer、frame/CSP 与 HSTS，未提供前端页面。
- 公网代理必须限制 URI/header/body、慢客户端、连接数及登录频率（每 IP/租户起始建议低频、经容量验证调整），禁止缓存登录/回调/报表响应。不得记录回调 query、Cookie、Authorization、token 请求体或 Secret；访问日志用路径而非完整请求 URI。
- 当前出站 TLS 使用 rustls/WebPKI 信任根，要求可验证的服务证书；私有 CA 注入和 mTLS 尚未开放，不能通过关闭证书校验接入自签名服务。实际 IdP/console 证书链须在部署认证中验证。
- IdP 每个响应最多 256 KiB，报表响应最多 2 MiB；TCP connect 3 秒、单次 HTTP 12 秒、整体请求 40 秒、每副本 8 并发、请求体 16 KiB/URI 8 KiB；数据库沿用有界 statement/lock/acquire timeout。
- 全库最多 1000 个未过期登录事务，通过事务级 advisory lock 准入；只在短 DB 事务持锁，不包围网络调用。`serve-bff` 每分钟批量清理最多各 1000 个过期登录/过期或已撤销会话；手动 `bff-maintain` 可加速清理。清理失败保留数据并记录静态错误码，应监控积压，不增大有效期。
- readiness 只检查数据库 schema，不把外部 IdP 的短时故障变成进程 liveness 失败。IdP/console 可达性和端到端登录应另设无凭据泄露的监控。
- `0012` 修改租户/用户并增加 epoch 触发器，需维护窗口、备份与全部实例版本切换；旧二进制 schema 检查会拒绝新版本，不承诺混合版本或零停机。回滚须保留新增状态并遵循数据库变更方案，不能删列/恢复旧 epoch 来“找回”会话。

## 本地验证与上线闸门

12 项新增专项覆盖实际本地 HTTP 授权码/PKCE/JWKS→报表→退出、错误 nonce/issuer/audience/azp/多 audience/签名/过期/认证时间/at_hash、响应过大/重定向、未知用户/跨租户、重复回调竞争、加密登录事务、CSRF/Cookie 重复、用途路由白名单、配置漂移、绑定/用户/租户重新启用、查询中退出、身份/登录/退出审计失败原子回滚、真实受限 PostgreSQL session 角色与清理。测试 IdP 故意免交互授权，仅作为协议故障夹具；生产构造路径不允许该 HTTP 配置。

本包最终 fmt/clippy/release build 通过；debug 全量 139 项通过，追加弱 RSA 拒绝断言后最终 BFF 12 项再通过；release 全量 139 项通过（包含所有 ignored S3/实际技术检测用例）。首轮 azp/清理权限失败与修复记录见 [里程碑](milestones.md)。

真实 IdP 客户端/域名、浏览器 Secure/SameSite/并发标签页、IdP MFA/组织条件访问、机构停用同步策略、真实 key rotation/多副本恢复、反向代理限制与日志脱敏、备份恢复和容量测试继续列入 LIVE-14。审批用途的 BFF/风险原文 UI/Hermes/OpenClaw 认证仍在 LIVE-05/06/13，不能用本包“已登录”代替用户对某个动作的明确同意。本轮不调用任何付费模型，不复用此前贴出的供应商密钥。
