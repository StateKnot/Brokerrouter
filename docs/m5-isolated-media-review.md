# M5.4b：隔离 MP4 审阅与审批边界

契约 `isolated-mp4-review-v1` / `isolated-mp4-viewer-v1`，2026-09-16。在已有当前轮视频交付链上增加独立 Rust 媒体审阅服务、定向 BFF 二进制代理、无登录权限的静态沙箱播放器，以及绑定完整审阅记录的媒体决定。不是图片/3D/任意文件查看器，也不是 Hermes/OpenClaw 原生适配。生产认证仍由 LIVE-20 阻断。

## 权限与信任边界

| 部件 | 所需权限 | 明确没有 |
| --- | --- | --- |
| `serve-media-review`（默认 127.0.0.1:8091） | `brokerrouter_media_review` 登录、结果解密 keyring、私有输出 GetObject-only、媒体审阅验证公钥 | 决定/豁免/执行/生成/修改资产/账本/策略、BFF 私钥 |
| `serve-approvals` | 原审批权限，加输出扫描及审阅元数据读取 | 资产分块表、S3 凭据、媒体字节读取 |
| OIDC BFF | 独立媒体用途签名、持久会话、固定三个路径 | 模型凭据、S3/结果解密密钥、任意 URL 代理 |
| 控制台主页面 | 本人登录、逐块中转 ArrayBuffer、明确审阅和决定交互 | 在主源解码视频、生成分享 URL、自动批准 |
| 独立静态预览站点 | 固定 HTML/JS/CSS 和沙箱内存 Blob | 登录 Cookie、同源权限、storage、联网 fetch、下载、顶层导航、审批接口 |

媒体在主页面 JS 中仅作为有界二进制中转，并逐块转移给 iframe；不要把这描述成“字节完全不经过控制台”。实际解码只在 `sandbox="allow-scripts"` 的不透明源中进行，不开放 `allow-same-origin`。预览源无 BFF/API 代理，CSP 限制 `connect-src 'none'`、`media-src blob:`、指定控制台 `frame-ancestors`、禁止表单/基址，并由响应头再次施加 sandbox。主页面仅允许配置的预览源作为 frame-src，仍禁止同源媒体解码。

独立域名必须只服务本项目审定的三份静态文件，不承载用户 HTML、重定向、其他应用或登录系统；生产使用独立可注册域名并验收浏览器站点隔离，不能只靠不同端口。BFF 与浏览器至少拒绝相同 hostname、非规范 origin 和非 HTTPS 配置；公共后缀/站点隔离及 DNS/TLS 所有权属于部署验收。主源 XSS、受信静态脚本/发布链被攻破、浏览器或解码器漏洞不因 iframe 消失，需受管浏览器、补丁和严格静态发布控制。

父页面仅向固定 iframe WindowProxy 传递一次 MessagePort，不给子页面 Cookie、用户签名或浏览器会话。因为子页面是 opaque origin，初始化使用 `targetOrigin='*'`；随后内容仅经该私有 port 传递。子页面检查父 Window、部署配置的精确父 origin、协议、大小/块数/期限，拒绝第二次初始化。iframe 重载/导航、消息乱序、未知消息、摘要不符或解码失败停止审阅。该选择依据 [MDN iframe sandbox](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe) 与 [postMessage 安全说明](https://developer.mozilla.org/en-US/docs/Web/API/Window/postMessage)，不是通过放宽同源权限获得播放。

## 服务协议和事实

浏览器 POST `/bff/v1/media-review/{start,chunk,complete}`，BFF 只转发到对应 `/media-review/v1/*`，30 秒 `br-media-review+jwt`，独立 RSA key/audience。每次请求检查同源/CSRF，签名绑定租户、本人、方法、路径与精确正文；每次响应交付前再次检查 BFF 会话。无自动重试、重定向、Range、查询串、匿名链接或 cookie 跨域转发。

| 路径 | 正文 | 返回 |
| --- | --- | --- |
| start | `approval_id`, `session_id`（浏览器随机 UUID，重试复用） | 当前完整扫描清单与短期会话；不读取媒体字节、不发放豁免 |
| chunk | `session_id`, `ordinal`（0 起） | ≤4 MiB 的 octet-stream/attachment/no-store；逐块受控读取 |
| complete | `session_id`, `sha256`, `reviewed:true` | 与该会话同 ID 的不可变 approval review，媒体元数据；不作审批决定 |

新 `0018` 建立不可变的 session、已交付块、completion 元数据。session 绑定本人/审批、产物和护栏快照摘要、完整内容 SHA-256/长度/块数。最长 5 分钟，不超过审批、turn、资产和扫描期限，每用户最多 4 个仍有效会话；新 ID 不用于自动续期。每块 I/O 前后重新校验本人、Runtime、Key、模型、turn、作业未取消、当前 profile 和护栏快照、技术 clean 以及资产/扫描期限。存储读取不持有 DB 锁，解密后的分块 hash/长度仍须匹配。

块记录与读取审计同事务提交才返回；审计后再次核对证明和审阅截止时间。网络丢失可人工重新读取同块，但不创建新模型作业。complete 要求完整唯一块集合和总长度、浏览器提供的完整 SHA-256 与存储事实一致、明确 `reviewed:true`；同事务创建原审批审阅凭据与 completion、写审计。失败不留下半个可用凭据，重试不会发放 grant。

播放器接收完毕后先计算整文件 SHA-256，再创建 video/mp4 Blob，实际 `loadeddata` 成功才提示用户确认；限制 64 MiB、≤300 秒、≤8,294,400 像素。无法解码、过大/超时、缺帧或受管设备不支持时保持不可放行，不回退成外链或下载。原技术扫描规则仍在前置层强制执行；其 unsafe/unsupported/error 永远不是可人工豁免的内容风险。

“我已实际审阅视频”是用户声明，服务端块交付及客户端完整 hash 校验并不能证明一个人看完或理解了内容；恶意真人可构造请求，不把 UI 点击当密码学证明。业务高风险操作依然需要 MFA、受控终端、组织制度与人工责任。无障碍用户若不能感知原视频，应拒绝继续或交由具备权限且能够审阅内容的原任务本人处理，不能用空字幕伪造已审阅。

随后仍使用原 `/approvals/v1/decide`：媒体**放行**必须有匹配的 completion 与未过期 review，JSON 占位审阅不能代替；决定在同一事务重新检查当前媒体/技术/权限/策略。本人可基于有效的节点元数据审阅直接拒绝或反馈，不要求被迫观看风险内容，也不发放任何豁免。明确选择护栏、意见、范围后二次提交，批准仅是当前 turn 所选版本豁免，不执行、不扩张工具/模型授权或人民币预算。审批后原 Key 当前轮交付协议保持不变。原 `br-human+jwt` 控制通道仍是独立受信集成协议，不能暴露给浏览器或 Agent；本包没有把旧原生通道认证为此新浏览器审阅契约。

## 清理、失联和审计

到期/切页/隐藏/退出/撤权清除 iframe、abort 请求并关闭 MessagePort；播放器也在自己的到期和 pagehide 清理 Blob。不能撤回已经交付的字节，关闭页面不撤销已接受审批。chunk 或 complete 失联只影响审阅，不自动提交意见；用户重新打开同一审批并实际重新审阅。决定失联仍沿用原冻结正文、同内容重试和查询原回执协议，不盲目重跑。

`media_review.started`、`media_review.chunk_served`、`approval.reviewed` 保存 ID、本人、摘要、序号/长度等元数据，无原始字节、供应商 URL、反馈或 Cookie。已交付块记录表示服务端准备交付，客户端断线不保证收齐；completion 额外记录完整校验与用户声明，仍不是执行完成。审阅事实不可 UPDATE/DELETE/TRUNCATE，生产须规划容量和受控归档；维护任务只清理过期证明记录，不通过删除事实重新打开审批。

## 部署与回滚

1. 停写排空，备份并核对未知执行/待审批清单；迁移账号应用 `0018`，DBA 执行权限脚本。为媒体服务创建仅属 `brokerrouter_media_review` 的 TLS 登录，不附加 runtime/approvals/资产写入组，非 DB owner/superuser。审批服务新权限仅限受控输出元数据，不给分块表。
2. 部署 `serve-media-review`，注入 `BROKERROUTER_MEDIA_REVIEW_ISSUER`、`BROKERROUTER_MEDIA_REVIEW_AUDIENCE`、`BROKERROUTER_MEDIA_REVIEW_PUBLIC_KEYS`，以及既有 ASSET S3 固定地址/桶/区域、GetObject-only IAM 和结果解密 keyring。阻断供应商出站，不注入私钥、扫描 token 或 S3 写删权；复用输出 worker 负责原资产到期清理。
3. BFF 必须先配置独立 approvals，再一次性配置 `BROKERROUTER_BFF_MEDIA_REVIEW_ORIGIN`、`BROKERROUTER_MEDIA_REVIEW_AUDIENCE`、`BROKERROUTER_BFF_MEDIA_REVIEW_SIGNING_KID`、`BROKERROUTER_BFF_MEDIA_REVIEW_SIGNING_PRIVATE_KEY`、`BROKERROUTER_BFF_MEDIA_PREVIEW_ORIGIN` 五项。媒体 RSA 模数/audience 不能与 console/catalog/settings/approvals 相同。该配置进入会话指纹，旧会话需重新登录。
4. `preview/` 只读发布到独立站点，使用 `deploy/preview/nginx.conf`。原子替换其中预览 DNS/证书路径/精确父 origin、`preview/review.html` 的父 origin、控制台 edge 的 frame-src、BFF preview origin；禁止通配符、第三方脚本、同域凭据或静态目录混挂仓库。新旧版本协同发布，readiness 必须通过。
5. 预生产先用本地合成视频和零费用测试任务完成浏览器→实际 IdP/MFA→BFF→两种服务→私有存储→审批→Runtime 重检/下载/关轮整链路。测试 Chrome/WebKit/受管浏览器、codec、低内存设备、键盘/读屏、60–64 MiB 峰值、乱序/断线/失联、跨副本撤权、证明/解密轮换、profile 升级、审计容量及恢复；此步不需要供应商 Key。
6. 轮换先加验证公钥再换 BFF kid/private key，保留 30 秒证明和滚动窗口；媒体/审批公钥不得安装成通用 HUMAN 执行控制信任。
7. 回滚先停用媒体入口并核对已接受的 review/approval/grant。不可仅撤掉播放器就宣称撤销批准；优先向前修复，库/程序恢复须同恢复点并核对之后的成本与外部副作用，禁止删除审阅事实重开操作。

本地测试通过不代表生产 IdP、S3 IAM、技术检测质量或真实 Runtime 已认证。LIVE-20 与 LIVE-06/09/12/13/19 仍是发布闸门；图片/3D/音频预览、专用多模态语义审查、原生 Hermes/OpenClaw 和逐节点治理待后续开发。本包未调用付费供应商、未生产部署。
