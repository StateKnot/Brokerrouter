# M5.2b：TypeScript 人民币只读控制台

后续 [M5.3a](m5-catalog-management.md) 已增加审计页和模型/文本向量端点配置页；下文只读范围与 38 项浏览器测试是 M5.2b 历史记录，当前范围和验证以里程碑最新增量为准。

本包交付 `console-ui-v1`：组织 OIDC 登录入口、人民币用量与每日费用、模型/虚拟 Key 分组、请求日志、会话失效/退出，以及同源 HTTPS 静态部署配置。运行代码只调用真实 BFF，不包含演示账号、模拟数据开关、万能管理 Token 或浏览器直连供应商的路径。身份与权限仍由 [Rust OIDC BFF](m5-oidc-bff.md) 和 [只读报表服务](m5-console-reporting.md)执行。

当前是完整控制台中的只读功能包，不是全部 M5 完成：审计日志页面、模型/端点/Key/护栏/策略/汇率的配置写入、审批原文与独立资产预览、任务追踪、经验候选与评测样本仍需开发。真实 IdP、实际浏览器→BFF→报表联合部署及 MFA/生产证书/容量认证未完成，保留 LIVE-14 发布闸门。

## 页面与金额口径

| 页面 | 实际功能 | 明确边界 |
| --- | --- | --- |
| 组织登录 | 输入组织 UUID，交给 BFF 发起 OIDC 顶层导航；显示组织/内部用户/会话期限；安全退出 | 不用邮箱自行认领身份，不在页面保存组织历史，不接收供应商 Key；退出仅撤销本地应用会话 |
| 人民币用量 | 已知入账费用、已准入请求、成功/失败、Token 证据覆盖、已知请求均费、待结算/预留/执行中/提交不确定/已核对；每日柱图与精确表格 | 不是供应商实际账单；准入前拒绝不在请求分母中；缺失费用/Token 不当作零 |
| 维度统计 | 逻辑模型、虚拟 Key；费用、预留、待结算、请求状态和 Token；20 行游标分页 | 不虚构供应商、团队、端点、Agent 或媒体单位维度 |
| 请求日志 | 请求 UUID、精确时间、模型/Key、状态/错误码、用途、人民币费用/预留与输入输出 Token | 不含敏感请求/响应原文；未有证据的耗时、供应商字段不展示；没有“重新执行”按钮 |

金额、均费、计数、Token 和游标由 Zod 校验为有界字符串，不能通过 `Number`、`parseFloat` 或 `Date` 重建账本或游标。金额展示至少两位小数，但保留全部有效精度，例如 `¥0.000408`；均费可保留 12 位。柱图先用 BigInt 精确比较和归一化，再把有限的 0–100 比例交给带无障碍名称的 SVG，图表不是结算来源。每个日期都有可展开的精确数据表。

USD 来源费用使用后端请求冻结的 USD/CNY 汇率快照，前端不重新换算、也不展示假实时汇率；当前主要币种固定 CNY。预留与入账分别展示，`null` 费用显示“待结算”，`null` Token 显示“无证据”。均费只以费用已知请求为分母。媒体的图片张数/视频秒数尚未加入此报表，不能用 Token 合计代替。

日期默认最近 7 天，支持北京时间/UTC。UI 的结束日期包含当日，使用 Temporal 将日期窗口转换为后端半开区间；最多 31 个日历日，后端还有独立时间跨度校验。字段修改后必须明确点击“查询 / 刷新”，已生效时区单独标注；无背景轮询或自动重试。模型/Key/请求 ID 为精确匹配，过滤后重置游标。请求 `created_at` 游标原字符串（含微秒）原样传回。跨页是实时 keyset 查询，不是固定快照导出，迟到结算可能改变下一次查询结果。

## 浏览器安全与失效行为

- React 文本节点渲染服务端标签，不使用 `dangerouslySetInnerHTML`。不读取或持有 BFF 私钥、供应商密钥、OIDC Token、内部服务证明；无 LocalStorage、SessionStorage、IndexedDB、Service Worker 或第三方分析脚本。
- 网络路径限定为同源 `/bff/v1/` 的登录、会话、退出和三条报表查询；`credentials/mode=same-origin`，拒绝跳转、禁止缓存。已登录 POST 必须从唯一合法 `__Host-br-csrf` Cookie 取得 `X-BR-CSRF`，缺失时不发请求。前端校验不替代服务端 CSRF/租户权限。
- 文档使用 `Referrer-Policy: no-referrer`；仅同源 Fetch 使用 `strict-origin`，最多发送 origin，绝不发送页面路径/query。这保留 WebKit 非 CORS POST 的精确 Origin，而不是让 BFF 接受 `Origin: null`。行为依据：[Fetch 的 Origin 追加规则](https://fetch.spec.whatwg.org/#append-a-request-origin-header)。
- JSON 响应按流读取，最多 2 MiB；验证 Content-Type、严格 UTF-8、JSON schema，45 秒超时。异常不显示原始错误正文，不自动重试。报表新请求清空旧结果，generation/AbortController 阻止迟到响应重新填充已失效页面。
- 页面隐藏时清除报表并取消请求，重新可见或 BFCache 恢复时重新查询会话；到期清除；成功退出通过 BroadcastChannel 通知同源其他窗口。窗口消息只是客户端清理通知，不是服务端授权依据；其他设备已交付数据无法收回。
- 任一报表权限被拒绝时清除当前工作台报表。退出开始即隐藏数据，但只有服务端返回 204 才显示退出成功；网络/审计失败明确提示“会话可能仍有效”，允许显式重试，不谎报撤销成功。
- 登录回调失败：浏览器请求 `Accept: text/html` 时固定 303 到 `/?login=failed`，清除登录事务 Cookie；绝不反射 code/state/上游错误内容。非 HTML 客户端保持 JSON 错误状态。过长 URI 的早期拒绝也保留安全响应头。
- BFF 的 `usage_read` 是当前租户范围的读取权限，不是仅个人/团队范围。授予前应确认该用户可查看租户全量费用与 Key 元数据。本 UI 不新增授权能力，也不允许利用已登录状态批准智能体动作。

## 构建与依赖

Rust 仍承载全部业务规则。新增 `console/` 使用 React/TypeScript、Vite、Tailwind、Zod、Temporal polyfill；直接依赖固定版本，锁定 `package-lock.json`，不复制网关金额/授权实现。字体从锁定依赖自托管（Inter/Space Grotesk），中文使用系统字体，不依赖公共 CDN。

```sh
cd console
npm ci --ignore-scripts --no-fund
npm run format:check
npm run build
npm test
npm exec -- playwright install chromium webkit
npm run test:browser
```

CI 使用 Node 24.19.0；声明支持的构建 Node 范围为 `>=24.19.0 <27`。产物为 `console/dist/`，没有任何 `VITE_*` 运行 Secret。Vite dev/preview 仅作本地前端工具，不提供认证绕过或生产 TLS；没有 BFF 的开发地址会明确显示连接失败，不能以 HTTP 下缺少 Secure Cookie 为理由降低生产 Cookie 属性。

Vite 编译目标 Chrome 120 / Firefox 128 / Safari 17.4 不是最低版本兼容认证。当前只验证锁定 Playwright 版本自带的 Chromium/WebKit；Firefox、企业浏览器策略、真实 Safari/iOS/Android、读屏器人工体验和系统日期控件需按组织支持矩阵补验。

## 同源 HTTPS 部署

仓库提供完整 [Nginx edge 配置](../deploy/console/nginx.conf)。它是**同主机或同 Pod 网络命名空间**部署合同：Nginx TLS listener 在 8443，Rust BFF 仅绑定同一网络空间 `127.0.0.1:8087`，BFF 再通过受限 HTTPS 连接独立 console 服务。不能把 loopback 地址简单替换成跨机器明文 HTTP。

1. 先完成 BFF 文档里的身份预登记、专用数据库登录、证明签名用途隔离与 TLS console 地址；本包无新 schema 迁移。生产数据库仍使用既有迁移/维护窗口规程。
2. 将配置中的两处 `console.example.invalid` 换为唯一生产 DNS；将可信证书链/私钥通过 Secret 系统只读挂载到指定路径。公网 443 可通过四层端口映射到 8443，或由运维调整 listener；BFF origin 和 IdP 固定 callback 必须与用户看到的 HTTPS origin 完全一致。此配置不开放 HTTP 入口。
3. 只发布构建后的 `dist` 到 `/srv/brokerrouter/console`，绝不挂载仓库、`.env`、请求正文、用户资产或源代码。以非 root 服务身份读取；进程目录/临时目录受限，主机/容器另配置 CPU、内存、连接和磁盘上限。
4. 在部署环境执行 `nginx -t`，验证证书链、预期 SNI/Host、Cookie 属性、CSP、正文/URI/连接限额与实际 IdP 回调。未知 Host 返回 421，未知 SNI 拒绝 TLS；只有根页面、哈希 JS/CSS/woff2 与 `/bff/v1/` 白名单可达，其他路径 404，不把错误路径都重写成登录页面。
5. UI、BFF、报表必须使用匹配的发布版本。静态发布使用版本化目录和原子切换；旧哈希资源至少保留最长会话/回滚窗口，不能在仍有打开页面时立即删旧字体/JS/CSS。发布前后用对应构建产物执行浏览器/edge 检查；回滚只切匹配应用版本，不回退会话 epoch 或恢复已撤销会话。

该 edge 禁止缓存、上游重试和响应落盘；保留原始 Origin/CSRF/Cookie/Authorization，让 BFF 验证或拒绝，不能用代理合成可信 Origin。CSP 无外部脚本/字体、`unsafe-inline` 或 `unsafe-eval`，并设置 HSTS/nosniff/DENY/Permissions-Policy。默认每 IP 16 个连接，BFF 路由 10 次/秒、burst 20，以及 16 KiB body、慢客户端/上游超时；这些是有界保护而非已验证容量，机构共用 NAT 时应按实测调整，登录反滥用还须结合组织入口限制。

访问日志只含随机 request ID、method、status、耗时；不包含 URI/query/referrer、任何 Cookie/鉴权头或正文。BFF location 禁用 Nginx error log（其错误可能携带完整 callback URI）；用元数据 4xx/5xx/429 比例、连接/资源、BFF 静态错误码与受控健康探针监控，不为排障重新开启原始请求日志。外层 LB/WAF/APM 也必须遵守此约束，仓库配置不能证明它们已脱敏。

## 验证证据与未完成项

2026-09-14 本地结果：

- Rust debug 回归 140 项通过：常规 136 项 + 4 项显式 S3/实际隔离检测器测试；fmt、clippy `-D warnings`、release build 通过。新增 1 项 PostgreSQL/HTTP 测试覆盖浏览器回调安全落点及过长 URI 的安全头；没有数据库迁移改动。
- 前端格式、TypeScript 与 production build、12 项单元测试通过；锁文件安装时 npm audit 为 0 个漏洞。浏览器最终 38 项（Chromium/WebKit 各 19）通过，覆盖微额/大整数、微秒游标、31 天/UTC、CSRF/Origin、模式错误、权限撤销、请求迟到、退出失败/重试、跨窗口和失效清理。
- 320/375/414/768/1440px 响应式与两页 axe WCAG A/AA 检查通过，登录键盘焦点和控件边界实测对比度至少 3:1；含 320px 的 31 天图表/展开表格。自动 axe 无违规不等于完整 WCAG 或读屏器认证。
- 实际 Nginx HTTPS 静态入口/资源/404/固定 Host/同源代理头/日志脱敏测试通过。测试使用官方镜像 `nginx@sha256:dc5069ad14f19660b141b21236140b91656bf89bbc3e2417c70ae650cd66104c`、非 root/只读/no-new-privileges；自签证书仅用于隔离本地测试，不安装信任根，不复用生产私钥。

浏览器 API 由 `tests/` 合成协议夹具提供，Nginx 上游也是仅用于验证代理的临时夹具；它们从不进入 `src/` 或 production bundle。Rust 的真实本地 DB/HTTP 协议测试与浏览器合约测试分开运行，**尚未完成真实浏览器→真实 IdP→BFF→报表的整链路认证**。BFCache 测试是模拟 `pageshow.persisted` 事件，不是所有浏览器缓存/后退场景认证。

测试与视觉复核发现并修复了 `<dl>` 语义结构、窄屏图表内在宽度、WebKit 的 `Origin: null`、时区控件名称、原生 select 高度、垂直 meter 在 WebKit 不绘制以及按钮焦点环对比度；图表改为无内联样式的原生 SVG，新增可见几何断言。Nginx 检查发现并修复非 root tmpfs 权限及意外 Host 的状态码。没有降低安全校验或放宽无障碍阈值来通过测试；页面布局检查会临时取消防御性裁剪，另以 32px 步长扫描 320–1920px，避免靠裁剪隐藏溢出。

CI 为 [.github/workflows/console.yml](../.github/workflows/console.yml)，执行锁定安装、审计、格式/构建/单元、双浏览器和真实 Nginx edge；镜像在每次 CI 拉取后固定到实际 digest，日志保留证据。复测本地 edge：

```sh
cd console
BROKERROUTER_TEST_NGINX_IMAGE=nginx@sha256:<已核验的64位摘要> node tests/edge.mjs
```

LIVE-14 继续阻断发布：真实 IdP/用户权限/MFA/停用同步、Secure/SameSite 的整链路多窗口与后退、实际 LB/WAF/证书链和日志、生产查询规模与 SLO、跨副本轮换/撤销/恢复、浏览器支持矩阵与人工无障碍验收。审批用途 BFF/风险原文界面及 Hermes/OpenClaw 仍归 LIVE-05/06/13，不把控制台登录当作“同意继续执行”。本轮无付费模型调用，也未复用或索取供应商凭据。
