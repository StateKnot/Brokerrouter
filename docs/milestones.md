# 开发里程碑

更新日期：2026-09-23。主语言 Rust。里程碑是生产方案的开发顺序，每个发布能力必须完成对应验收，不能把本地 fixture 当作供应商认证。

| 里程碑 | 范围 | 验收闸门 | 状态 |
| --- | --- | --- | --- |
| M0 工程与生产契约 | Rust 锁定工具链、构建、迁移、CI、运行说明、v0.3 决策 | fmt/clippy/test、真实 PostgreSQL 迁移、健康检查 | 本地与远端 CI 验收通过 |
| M1 访问、模型与金额基础 | 租户、虚拟密钥生成/撤销、逻辑模型多端点、预算、价格、人民币/美元汇率配置 | 跨租户不可见、撤销即时生效、预算并发、汇率快照与低价精度 | 基础模块本地验收通过 |
| M2 统一文本调用闭环 | Chat/Embedding、能力过滤、端点路由、请求/attempt、精确计费、日志 | 本地协议/故障集成测试；真实供应商调用认证另行执行 | 非流式核心、能力受控 SSE、工具扩展字段规范化、个人初始化及本地故障回归已实现；[阿里云 4 模型真实冒烟](live-aliyun-2026-09-14.md)仅部分通过，当前修复的真实复验、SSE/SDK/账单与容量认证待完成 |
| M3 护栏与用户审批 | 用户身份与 Agent 分离、turn 权威生命周期、持久审批、输出隔离、当前 turn 豁免、恢复协议 | 代批失败、重复决定原子、迟到批准失效、重启不重复副作用 | 核心及文本语义连接器已实现；多模态/专用审核适配、检测质量与真实 BFF/Runtime 认证待完成 |
| M4 多模态作业与资产 | 同步图像/异步视频/3D、连接器配置、持久 worker、提交不确定、私有资产、回调 | 接收后断线不重提、转存恢复、取消竞态、跨租户资产访问拒绝 | 资产/计费、文生视频作业、隔离转存、扫描与技术检测、资产策略及真人审批交付、隔离 MP4 预览已实现；专用内容语义、其他模态预览、图片/3D/其他协议待开发，生产认证待完成 |
| M5 控制台与治理 | TypeScript 控制台、OIDC、模型/端点、策略/护栏、汇率、人民币统计、任务审计、经验/评测样本 | 页面使用真实 API；各维度账本一致；权限矩阵与审批审计 | 报表、OIDC BFF、配置管理、护栏监控、本人 JSON/隔离 MP4 审阅审批、逐轮次节点/调用/事件治理与 HTTPS edge 已实现；其他模态预览、经验候选/评测样本和其余统计维度待开发，真实 IdP/整链路/部署待认证 |
| M6 Runtime、MCP 与生产认证 | Hermes Python / OpenClaw TypeScript Adapter、MCP 2026-07-28/Skills、通道认证、压测、故障演练、部署与回滚 | 指定版本完整调用/审批/恢复；MCP 官方 client/server 一致性；容量、RPO/RTO、凭据与真实费用边界达标 | Hermes 0.21.1 M6.1a/b 已实现工具输入强制门、受信 turn 绑定与原生审批通知；M6.2a–b 完成安全内核、连接目录、HTTP/stdio 发现、下游目录/OAuth 与持久治理的单次工具调用；MRTR/持久 Skill 审批/官方认证、Hermes/Claw MCP 原生恢复、输出/产物发布仍待完成 |

## v0.3 已决定的实施契约

1. PostgreSQL 是配置、预算、账本、作业和审批事实来源；Redis 在配额/吞吐需要时加入，不承担资金正确性。
2. 初始控制面通过可信运维 CLI 管理 PostgreSQL；Web 控制面接入 OIDC 后复用相同事务操作。不公开万能管理员 token 接口。
3. 价格保存币种和每百万 token 等计价基数；计算过程 Decimal，账本最终使用人民币微元整数并向上舍入非负费用，原币精确金额单独保留。
4. 汇率使用租户配置 `1 USD = X CNY`，必须大于零，版本追加、生效时间明确；CNY 成本无需 FX，USD 缺少有效 FX 拒绝准入。每次调用固定价格与汇率快照。
5. 对有成本上界的请求做数据库原子预留；不确定上游结果保留预留，待对账。不自动重复发送结果未知的收费请求。
6. 逻辑请求和上游 attempt 分开；只对能力匹配的端点路由，新增弱端点不收窄原模型已发布能力。
7. 需要输出护栏的请求默认完整检查后释放；未配置输出护栏的流式请求可以实时传递。分段审核必须通过对应能力认证后启用。
8. 一轮定义为一个受信用户消息驱动的任务。重新生成、会话分叉创建新 turn；反馈改向默认撤销旧待执行动作，其他已批准护栏是否继续生效由新版本契约明确实现。
9. Agent key 无审批决定权限。服务端 grant 只绑定真人、租户、Runtime、session、turn、护栏版本；Native 工具硬权限不被网关 grant 覆盖。
10. API 明确支持字段及语义；未完成能力返回明确 unsupported，不模拟成功。未通过联调的供应商端点不标 certified。

## 评审追踪

| 评审项 | 落地里程碑 |
| --- | --- |
| F01 提交不确定、F07 作业状态 | M2/M4 |
| F02 流式与输出审批 | M2/M3 |
| F03 真人身份/turn、F04 恢复 | M3/M6 |
| F05 预算/账本、汇率配置新增需求 | M1/M2 |
| F06 SDK 重试与审批协议 | M2/M3/M6 |
| F08 Runtime 治理范围 | M3/M6 |
| F09 版本与撤销 | M1/M3 |
| F10 资产、F13 护栏自身调用 | M3/M4 |
| F11 能力匹配、F12 兼容清单 | M1/M2/M4 |
| F14 配额/指标、F15 运维 | M2/M5/M6 |

## 需要真实联调时再获取

未完成项统一追踪于 [验收待办](acceptance-backlog.md)。2026-09-11 用户决定先记录未测项、继续 M3；上述认证闸门不会因此关闭。

供应商凭据引用、区域、模型版本和人民币测试上限；Runtime 测试部署与通道。联调前由用户提供，不在仓库保存真实密钥。平台尚未完成全部闸门前，不宣称整个平台生产就绪。

## 首批实施记录（2026-09-11）

已实现 Rust CLI、授权模型/汇率只读 API、两次 schema 迁移，以及预算预留/结算基础库。当前 CLI 是受限数据库运维身份的管理入口；Web 控制台属于 M5，不以公开管理员密钥代替。

本地验证：`cargo fmt --check`、`cargo clippy --all-targets --locked -- -D warnings`、`cargo test --locked`、`cargo build --release --locked`。测试为 1 项金额单元测试和 6 项真实 PostgreSQL 集成测试：

1. 租户隔离、虚拟密钥撤销、汇率历史与未来生效。
2. 并发预算、请求幂等、提交不确定不得释放、重复结算只入账一次。
3. 人民币主账本、美元原币金额、历史汇率快照、跨租户 FX 版本拒绝、撤销后结算。
4. 数据库迁移 checksum 漂移导致不就绪、网关生成请求 ID。
5. Key 预算不足回滚租户预留、模型停用后禁止发送、未发送释放幂等。
6. 用实际受限数据库角色完成准入和结算；禁止修改汇率、预算、迁移记录、禁用账本触发器和截断事实表。

代码提交 `e063f07` 的 [GitHub Actions 验证](https://github.com/StateKnot/Brokerrouter/actions/runs/34576422617) 已通过 fmt、clippy、全部测试和 release build。另以发布二进制在本地隔离数据库应用迁移并启动服务，真实 HTTP `/health/ready` 返回 204；检查完成后服务已关闭。

使用的是独立的本地 PostgreSQL 测试库，没有调用收费供应商、没有将任何 provider 标为 certified。M1 的模型/端点为配置基础，不代表当时已经可以转发模型请求。

## M2 实施记录（2026-09-11）

已实现当时的 `text-nonstream-v1`，后续增量已将当前 [M2 契约](m2-text-gateway.md) 提升为 `text-chat-v2`：Chat/工具调用协议、Embedding、最多 3 个候选端点的安全回退、逻辑请求与 attempt、当前 Key 的请求日志、人民币结算、加密结果重放与有效期、正常停机排空、持久不确定状态维护、独立核对身份及证据审计。沿用 M1 的预算/账本事务，没有另建资金事实库。

当前测试集：3 项纯单元测试 + 19 项 PostgreSQL/HTTP 集成测试，共 22 项（含 M1 回归测试）。新增覆盖数据库等待上限、真实 HTTP 客户端→网关→供应商测试服务闭环、连接拒绝后按实际端点价格结算、上游接收后超时不回退、无效用量/重定向/429 不伪装未收费、工具能力/向量维度/SSRF、并发幂等、密文过期与跨租户访问、客户端断开与排空、重启状态恢复、数据库结算失败不重发，以及运行身份不能代核对人员闭合未知费用。

当时未完成闸门为真实供应商和 SDK 认证；后续虽已实现能力受控 SSE，仍需固定供应商/区域/模型 ID、凭据引用和人民币测试上限完成真实复验。Responses 和多模态 Chat content 仍明确不支持；不据此宣称整个平台可上线。

## M3 实施记录（2026-09-11）

用户要求先记录未测项并推进下一里程碑，已建立 [验收待办](acceptance-backlog.md)，M2 的真实认证仍保持未完成。

本阶段实现 [governance-v1](m3-governance.md)：请求级 RSA 真人证明、独立控制面与数据库权限、用户/Runtime/Agent 绑定、不可复开的 turn、版本化确定性护栏与条件策略、模型输入/输出隔离审批、部分批准/拒绝/反馈、当前 turn grant、持久事件与游标确认、执行 claim/receipt/不确定人工核对、加密内容与签名记录过期清理。反馈采用“取消旧 turn，BFF 创建新代次”契约。

当前验证集 36 项：3 项纯单元测试、33 项真实 PostgreSQL/HTTP 集成测试（含 M0–M2 回归）。新增输入与输出两次人工审批只产生一次供应商调用和结算、输出拒绝不退款、RSA 签名与正文/路径/用户绑定、部分批准、并发冲突、策略升级、Key 撤销、当前 turn 生命周期、执行竞争、人工核对、审批事务故障注入、过期清理与数据库权限隔离。

该批规则仅开放 `always`、`contains_any`，不把字面规则标为完整内容合规能力。后续语义检测实现见下面增量记录。Hermes/OpenClaw 原生交互、检查点恢复和真实登录渠道属于 LIVE-05/LIVE-06 验收，未将 fixture 标为已认证 Runtime。

## M3 语义护栏增量（2026-09-12）

按用户“继续开发、后期统一测试”的安排完成 [model-judge-text-json-v1](m3-semantic-guardrails.md)：版本绑定的一次性检测身份、服务端内部调用用途、检测失败转审批、同轮豁免、独立检测并发池、真实请求/attempt/人民币账本复用、费用上限与 FX 快照、无锁网络调用、过期事件恢复及禁止 GET 触发收费检测。

本地验证：fmt、clippy（`-D warnings`）、release build 通过；debug/release 各 49 项全部通过（4 项纯单元、45 项 PostgreSQL/HTTP 集成，含历史回归）。本批新增 12 项集成和 1 项严格结果解析单元测试：人民币/美元费用快照、同轮豁免、检测递归隔离、非法分类正文、超时与费用保留、并发单次提交、准入失败、执行者崩溃、结算故障、实际受限数据库角色、执行中策略升级、HTTP 断开及 GET 不启动收费检查。未调用真实供应商，未消耗真实测试费用。

真实供应商分类质量、渠道及 Runtime 联调继续延期，新增 LIVE-08，不因本地测试关闭发布闸门。下一开发主线是 M4 多模态作业/私有资产，再衔接资产感知护栏；不将文本 JSON 分类器标为多模态内容审核。

## M4.1 私有输入资产与单位计费（2026-09-12）

实现 [private-input-assets-v1 / units-v1](m4-assets-pricing.md)：独立资产服务与最小数据库权限、租户/Key 双层字节配额、不可变分块和 SHA-256 清单、S3 条件写与 AES-GCM 分块加密、故障恢复、流式分块鉴权、逻辑删除/过期和可恢复 GC。输入资产完整性与内容安全状态分开，不开放供应商输出的绕过审核入口。

按张/秒/次的多维价格与用量保留精确原币/FX/计价版本，复用现有 requests 与人民币账本；拒绝缺失/越界用量及精度丢失，重复结算核对完整用量证据，不将非 Token 用量伪装成 Token。

M4 分包顺序：M4.1 输入资产与计费基础 → M4.2 持久生成作业、受理不确定和供应商协议 → M4.3 输出隔离转存、资产感知护栏、审批交付与取消竞态。M4.1 当时未开放生成任务 API；M4.2 增量见下，本地输入资产可用不等于整个多模态网关完成。

本包未测的生产条件统一登记 LIVE-09，不提前索取真实凭据。生产 S3、供应商及 Runtime 的实际部署认证仍为发布闸门。

本地验证：fmt、clippy（`-D warnings`）及 release build 通过。debug 常规 62 项通过，S3 专项单独通过；release 使用 `--include-ignored` 显式纳入 S3，63 项全部通过（6 项纯单元、57 项 PostgreSQL/HTTP/S3 集成，包含全部历史回归）。新增 14 项覆盖配额原子回滚、幂等、跨租户/Key、密文损坏、全文件哈希、写对象后数据库故障恢复、真实 S3 条件写、流式中途撤销、过期清理、数据库最小权限、单位金额/精度上限、跨租户 FX 与撤销后只结算一次。没有调用真实模型供应商，未产生真实供应商测试费用。

## M4.2 持久文生视频作业（2026-09-13）

实现 [media-jobs-v1 / dashscope-wan27-v1](m4-media-jobs.md)：独立媒体 API/worker、按逻辑模型发布参数/价格档位、多端点能力匹配、不可变配置、作业与人民币预留原子准入、输入真人审批与当前 turn 豁免、发送前权限再检查、持久租约/过期 owner 隔离、上游原 task ID 查询、取消意愿及未知收费核对。提交不确定不自动重发，GET 不启动收费工作，Key 撤销不抹掉已发生费用。

成功用量、人民币结算及加密输出元数据同一事务提交，但停在 `output_pending`，不提供供应商 URL 或下载。下一包 M4.3 完成隔离转存、二进制护栏、真人输出审批和交付；其他图片/3D/供应商连接器仍需实现。真实视频协议/用量/价格/恢复认证登记 LIVE-10，按用户要求后期联调。

本地 fmt、clippy（`-D warnings`）、release build 通过；debug/release 均显式 `--include-ignored` 纳入本地 S3，76 项全部通过（7 项纯单元、69 项 PostgreSQL/HTTP/S3 集成，包含全部历史回归）。新增 13 项覆盖 HTTP 调用与能力查询、预算/作业原子回滚、幂等、多端点匹配、输入审批与同轮豁免、受理后超时/写库失败、查询错误、结算失败恢复、过期租约竞争、取消与停用、实际运行角色权限和严格协议解析。没有调用付费模型供应商。

同时修复上包 CI 的 MinIO 启动期 TCP reset 健康检查：增加有界全错误重试，不隐藏最终失败；本地启动、健康检查和实际 S3 合约通过。远端 CI 结果以本包 PR checks 为准，不将上包失败记录改写为通过。

## M4.3a 持久输出隔离转存（2026-09-13）

实现 [private-output-quarantine-v1](m4-output-quarantine.md)：生成准入时原子预留存储容量、固定输出来源合同、无供应商鉴权头的 DNS 固定下载、强 ETag/长度约束、私有 S3 不可变加密分块、全文件 SHA-256、持久租约与恢复、取消/权限撤销清理，以及独立输出 worker。完整资产在最后事务失败时可直接恢复隔离状态，不重复下载、更不重新付费生成；存储清理不会撤销已经发生的人民币模型费用。

输入/输出资产通过数据库 RLS 和互不继承的用途角色隔离，旧输入服务也不能从旧路由读取输出。增加受限 session 身份测试，验证双向角色切换被拒绝，而输出转存和输入上传各自仍可执行。不能使用迁移账户、表拥有者或超级用户运行这些服务。

M4.3 划分为当前隔离存储包和 M4.3b 内容审核/审批交付包；本包 `quarantined` 只代表完整性验证，不代表内容安全或真人批准。媒体作业继续停在 `output_pending`，不发布供应商 URL、资产下载或自动放行。真实来源/S3/区域/恢复窗口认证列为 LIVE-11，M4.3b 仍是待开发项，不能当成仅待测试项。

最终代码本地 fmt、clippy（`-D warnings`）、release build 通过；debug/release 均以 `RUST_TEST_THREADS=2 --include-ignored` 显式纳入两个本地 S3 用例，88 项全部通过（7 项纯单元、81 项 PostgreSQL/HTTP/S3 集成，含全部历史回归）。新增 12 项包括原子容量准入、全部旧资产访问路径、来源变更、对象写后数据库故障、最终隔离事务失败、取消/租约竞争、权限隔离和 S3 实际清理。限制的是不同测试用例的调度并行数，未移除用例内并发、改变生产超时或掩盖此前高负载下旧语义 fixture 的失败记录。未调用付费供应商、未产生真实模型测试费用。

前序 M4.2 提交 `f052602` 的两次远端验证均已通过：[分支 CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34744504579)、[PR CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34744563913)。本包远端结果以独立 PR checks 为准，不沿用前序 CI 作为当前提交的证据。

## M4.3b 第一包：扫描作业与证据链（2026-09-13）

实现 [output-scan-evidence-v1](m4-output-scan-evidence.md)：独立扫描 API/数据库角色、模型级不可变检测清单摘要、租户/profile 专属随机凭据、机密分块读取、持久幂等 claim、有限续期与三次恢复上限、旧执行者隔离、严格回执、配置/凭据/Key/turn 撤销及维护清理。扫描回执和审计同事务提交，原有模型人民币成本不变。

本包是 M4.3b 的证据接入部分，不是实际检测引擎或审批交付的完成标记。技术安全与内容风险使用独立结果；Agent 凭据不能接入扫描通道，扫描凭据没有模型执行或真人 grant 权限。媒体状态中的 complete 只表示回执已记录，产物仍然不向客户端发布。真实引擎适配、资产感知策略、真人审阅与同轮批准后的受控交付继续开发，LIVE-12 记录真实引擎/沙箱/检测质量和部署认证。

本地 fmt、clippy（`-D warnings`）、release build 通过；debug/release 均以 `RUST_TEST_THREADS=2 --include-ignored` 显式纳入两个本地 S3 用例，97 项全部通过（7 项纯单元、90 项 PostgreSQL/HTTP/S3 集成，包含历史回归）。新增 9 项测试验证完整领取/读取/回执、并发幂等、旧租约隔离及重试上限、严格摘要/版本/结果、凭据轮换/配置升级、撤销、回执事务失败恢复、真实受限数据库角色和 HTTP 越权；原 S3 用例扩展了扫描通道读取与回执验证。首次集成测试发现并修复租约创建/截止的数据库时钟基准差异，没有放宽数据库约束。测试使用合成媒体字节和检测回执，不代表真实引擎检出率已验证；没有调用付费模型或审核供应商。

前序 M4.3a 提交 `175ce2b` 的两次远端验证已通过：[分支 CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34764980560)、[PR CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34765023688)。当前包的远端验证仍以本包 PR checks 为准。

## M4.3b 第二包：实际隔离技术检测器（2026-09-14）

实现 [clamav-ffmpeg-mp4-v1](m4-isolated-detector.md)：独立 Rust `brokerrouter-scanner`、固定镜像/worker 摘要与有界有效期、拉取式机密分块、全文件校验、实际 ClamAV/FFprobe/FFmpeg 完整解码、无网络只读容器与资源上限、内外 watchdog、独立空 Docker 配置、持久 claim/证据/receipt、续期/撤销和重启幂等回报。扫描进程不持有数据库、S3、供应商或真人审批凭据，失败不改模型人民币费用。

最终本地 fmt、clippy（`-D warnings`）、release build 通过；debug/release 各 **104 项全部通过**，均使用 `RUST_TEST_THREADS=2 --include-ignored` 显式纳入两项真实检测引擎用例和两项 S3 用例。新增 7 项覆盖清单/媒体档位、进程超时和输出上限、数据库回执失败/重启不重扫、manifest 漂移/撤销/私有目录/锁，以及真实有效 MP4 的扫描闭环、无害 AV 标记命中和非 MP4/截断媒体拒绝。

本地检测镜像是 Linux arm64、ClamAV 包 `1.4.3+dfsg-1~deb12u2` 和 FFmpeg 包 `7:5.1.9-0+deb12u1`，镜像 ID `sha256:6b313c2cd95f075f29d93cddfc1fcab5461d48606c730edc04ccc2aa0fa75672`。使用单条无害测试特征库，**不能作为生产镜像或检测覆盖认证**。CI 在 amd64 测试平台重新解析并固定官方 Debian 基础摘要、构建测试镜像后，按其不可变 ID 执行真实检测专项。

首轮测试修复了 macOS fixture 的 Unix socket 路径长度问题；验证期间出现过一次旧 S3 用例的 Storage 失败，随后健康/桶检查与原断言复测通过；另一次手写测试 keyring 长度错误改为准确生成。没有放宽生产超时或测试断言，最终完整 debug/release 均通过，早期失败不改写成通过。

技术 clean 不代表语义合规或人工批准：本包所有内容结果仍为 unknown，客户端仍不能下载产物。下一开发包继续资产策略消费、内容审核、真人审阅与当前 turn 批准后的受控交付；真实特征库/恶意样本/沙箱/宿主终止恢复/归档告警等保留 LIVE-12，付费供应商与 BFF/Runtime 联调仍延期。没有调用付费模型或云审核供应商。

前序扫描证据提交 `1ae60a5` 的两次远端验证已通过：[分支 CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34766731322)、[PR CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34766758813)。当前包远端状态以本包 PR checks 为准。

## M4.3b 第三包：产物策略、真人审阅与当前轮交付（2026-09-14）

实现 [current-turn-output-v1](m4-output-delivery.md)：新增 `media_output` 及绑定精确扫描 profile 的 `asset_scan`、不可变产物/扫描/操作绑定、独立交付 API 与数据库角色、请求绑定的真人私有审阅、部分批准/拒绝/反馈、当前 turn grant 和授权分块读取。技术失败、无匹配策略、配置升级、Key/模型/Runtime/用户撤销、取消、关轮和有效期始终再校验；普通 Agent 检查/claim、旧输入资产接口都不能绕过。读取审计不伪造 Agent 执行完成，也不改变已经产生的人民币费用。

最终本地 fmt、clippy（`-D warnings`）、release build 通过；debug/release 各 **116 项全部通过**，均以 `RUST_TEST_THREADS=2 --include-ignored` 显式纳入两项真实 S3 和两项真实技术检测用例。新增 11 项产物交付 PostgreSQL/HTTP 用例及 1 项存储故障注入；原真实 S3 / ClamAV/FFmpeg 用例扩展为真人批准后读取精确字节。覆盖并发幂等、受限 session 角色、证明路径/用户绑定、同轮/跨轮、技术不可豁免、策略版本变化、部分批准、审批事务失败、读取期间撤销和审计失败不交付。

首轮交付测试出现夹具连续四次生成超出其既有存储配额，修正为显式配置该测试所需容量，没有放宽生产配额。首轮 release 复现旧 S3 用例第二次条件 PUT 的 Storage 失败，与前序提交 `a0c40b4` 的 [失败 CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34769139708) 位置相同；该前序提交另一条 [CI 通过](https://github.com/StateKnot/Brokerrouter/actions/runs/34769219790)，不将两者合并为全绿。独立重复 12 次未再复现，尚未确定存储端内部失败原因。本包修复了恢复路径对重复 PUT 和特定响应错误类别的依赖：已 stored 的分块只读回校验；不确定条件写只能以精确现存对象的解密/哈希验证恢复。新增测试证明未写入、密文损坏、对象缺失不能误报成功或自动重写。临时诊断已删除，最终完整 debug/release 均通过；早期失败记录保留。

新产物审批最多覆盖其原始 turn（最长一小时），不能用新 turn 追加授权到无 turn 旧作业，也不提供永久历史下载。专用多模态语义审核、独立预览 UI、真实 BFF/Runtime 弹窗与恢复仍需继续开发，新增 LIVE-13；图片/3D/其他协议不标已实现。生产特征库、实际供应商、TLS/IAM/S3、容量和部署认证保持发布闸门，本包未调用付费供应商，也未提前索取密钥。

## M5.1 第一包：独立控制台读取与人民币报表（2026-09-14）

实现 [console-reporting-v1](m5-console-reporting.md)：独立 `serve-console` 与只读数据库身份、区别于审批证明的 `br-console+jwt`、显式租户用户 `usage_read/audit_read` 权限及原子变更审计、人民币汇总/每日曲线、模型/虚拟 Key 维度、请求日志和审计元数据。字段隔离视图不暴露原文、密文、Key 哈希或凭据引用；查询没有供应商访问或批准/执行/改预算权限。

请求/费用/预留分开统计，未知费用保留 null，不确定提交不并入失败；缺失 Token 证据通过覆盖请求数和 null 表达。金额来自 finalize 账本，计数/金额/Token 使用字符串保留精度，按原请求创建日归属，UTC/北京时间日界线和空日补零明确。单次报表为一致快照，跨页采用稳定游标；查询前后检查撤销/失效，另有 31 天窗、100 行分页、8 并发、16 KiB Body、5 秒 SQL/1 秒锁等待和 20 秒 HTTP 处理时限。

本地 fmt、clippy（`-D warnings`）及 release build 通过；最终 debug/release **各 127 项全部通过**，均以 `RUST_TEST_THREADS=2 --include-ignored` 纳入两项真实 S3、两项真实 ClamAV/FFmpeg 专项和全部历史回归。新增 10 项 PostgreSQL/HTTP 集成及 1 项停滞请求超时测试，覆盖签名类型/路径/正文、显式权限/撤销竞态、跨租户/注入式筛选、账本与未知状态、大整数精度、日界线/分页、审计故障回滚和真实受限 session 身份。首轮 126 项通过后新增总 deadline，再完整重跑到最终 127 项；没有放宽旧断言。

在上轮隔离真实联调库应用新迁移并以 console 受限身份核对账本视图，结果仍为 6 请求、¥0.000408、0 预留、351 输入/48 输出 Token；没有改写历史账本或新增付费调用。前次实际调用兼容缺陷仍追踪于 LIVE-01，本包只推进下一阶段，不把问题改记为已修复。

下一包推进真实 OIDC/BFF/会话与 TypeScript 页面接入；控制台配置写接口、团队/供应商/端点/Agent 维度、媒体用量、护栏监控完整口径与任务/评测治理仍待开发。新增 LIVE-14 跟踪 BFF、UI、最小权限部署、生产容量及迁移窗口认证，不以测试签名器代替真实登录系统。前序 `5dbb017` 的两次 CI 已确认通过：[分支检查](https://github.com/StateKnot/Brokerrouter/actions/runs/34798458071)、[PR 检查](https://github.com/StateKnot/Brokerrouter/actions/runs/34798338678)；本包远端状态以独立 PR 为准。

## M5.2a：OIDC 控制台 BFF 与持久浏览器会话（2026-09-14）

实现 [oidc-console-bff-v1](m5-oidc-bff.md)：Rust 标准授权码/PKCE、一次性加密登录事务、预登记机构/不可变用户/租户绑定、随机 Cookie 与服务端会话、精确 Origin/CSRF、固定报表代理、30 秒用途隔离证明、查询后撤销再检查、退出原子审计与周期清理。绑定 generation 及用户/租户 epoch 防止重新启用复活旧 Cookie；BFF 没有模型凭据、原文解密、预算管理、审批或自行授予权限的数据库能力。

本地 fmt、clippy（`-D warnings`）、最终 release build 通过。debug 全量 **139 项通过**；补充弱 RSA 配置拒绝断言后，最终 12 项 BFF 专项复核通过。release 全量 **139 项通过**，均显式 `--include-ignored` 纳入两项真实 S3、两项 ClamAV/FFmpeg 和全部历史回归。新增 12 项覆盖真实本地 HTTP/OIDC 协议到人民币报表、JWT/身份/CSRF/重放/失效/并发退出、审计失败原子回滚、受限 PostgreSQL session 角色及清理。没有调用真实模型，人民币账本实现未改，新增真实供应商费用 ¥0。

初轮测试发现库默认不验证 `azp`，已在应用合同中显式校验并保留负面用例；受限角色测试发现过期登录清理的行锁要求额外 UPDATE 权限，改为不需要该权限的有界删除，没有扩大 BFF 的身份管理权限。未放宽断言或改写首次失败记录。

本包为登录/服务端接入包，TypeScript 登录与人民币用量页面继续下一包；不把本地免交互 IdP 夹具当真实用户系统认证，不将控制台登录等同于用户批准智能体动作。真实 IdP/浏览器/TLS/MFA/机构停用同步/多副本轮换与生产容量保留 LIVE-14，审批 BFF/原文界面及 Hermes/OpenClaw 保留 LIVE-05/06/13。前序 `36860e5` 的 [PR #10](https://github.com/StateKnot/Brokerrouter/pull/10) 两项 CI 已确认通过，本包远端结果以自己的 PR 为准。

## M5.2b：TypeScript 人民币工作台与 HTTPS edge（2026-09-14）

已实现 [console-ui-v1](m5-console-ui.md)：React/TypeScript 组织登录、人民币用量/每日精确表与柱图、模型/虚拟 Key 分组、请求日志和 20 行微秒游标分页；费用/Token/计数均使用受校验字符串，未知费用/用量保留独立状态。运行包无演示数据、万能 Token、供应商凭据或浏览器持久存储。只接真实 Rust BFF，明确区分租户范围读取与智能体审批。

会话到期、隐藏/恢复、跨窗口退出、权限撤销和迟到响应清理已接入；退出失败不谎报撤销。新增 Rust 浏览器回调固定错误落点及过长 URI 安全头测试。交付同源 Nginx HTTPS 静态/代理白名单、限额/超时、无缓存/重试、严格 CSP 与不含 URI/凭据的日志配置，以及独立前端 CI。

本地 Rust fmt/clippy/release build 通过，debug 常规 136 项与四项 S3/隔离检测专项全部通过，共 140 项；前端构建/格式和 12 项单元测试通过，Chromium/WebKit 38 项通过。实际 Nginx TLS/资源/代理头/Host/日志测试通过。浏览器覆核了 320/375/414/768/1440px、登录 1280×800、31 天窄屏图表、最小 44px 控件与 axe/对比度；320–1920px 另按 32px 步长检查未裁剪布局。实现中发现的 WebKit Origin、下拉框高度、垂直 meter 不绘制、图表宽度、语义和焦点对比度问题均修复，不以语义检查通过代替视觉复核。

浏览器 API 与 edge 上游仍使用隔离的测试夹具，不等于真实 IdP→BFF→报表整链路认证；真实 MFA/组织停用/证书/LB/WAF/容量/浏览器矩阵保留 LIVE-14。下一包推进审计/模型端点与配置管理的权限和审计闭环；审批 BFF、风险原文/独立预览、Hermes/OpenClaw 恢复仍按 LIVE-05/06/13 单独开发验收。本包没有付费调用，未索取或复用供应商密钥。

前序 `87abb9c` 的 [PR #11](https://github.com/StateKnot/Brokerrouter/pull/11) 两项 Rust CI 已确认通过：[检查一](https://github.com/StateKnot/Brokerrouter/actions/runs/34813079197)、[检查二](https://github.com/StateKnot/Brokerrouter/actions/runs/34812938209)。本包独立 Draft PR 的远端检查以实际结果为准，不自动合并堆叠分支。

## M5.3a：模型/端点配置与审计工作台（2026-09-14）

实现 [catalog-management-v1](m5-catalog-management.md)：独立 Rust 配置服务/DB 身份、`br-catalog+jwt` 与独立 BFF key/audience、显式 `catalog_read`/`catalog_write`、运维按租户预登记连接、逻辑模型和文本/向量端点新建/启停/价格/优先级管理。新增默认停用，能力/供应商地址不可原地替换，新增不自动授予 Key 调用权限。模型/端点更新包含 CLI 的数据库版本推进，旧版本拒绝覆盖；配置/前后审计/真人与理由/幂等回执同事务提交。

TypeScript 页实现模型搜索、多供应商端点、只读/可写状态、变更摘要后二次提交、失败复用相同操作号、版本冲突重新核对、精确人民币/美元价格，以及独立审计页面的 UTC/北京时间过滤、微秒游标、真人/数据库执行身份/理由/操作号。页面使用现有样式和原生可访问表单，按需加载管理模块，未增加依赖；不出现密钥/任意 URL 输入。准备/提交期间禁止并行表单和切换列表；关闭页面后须先核对记录，不自动重新创建。

本地 Rust fmt/clippy（`-D warnings`）/release build 通过；全量 **148 项通过**，包含全部 PostgreSQL/HTTP、两项 S3 和两项实际 ClamAV/FFmpeg 专项。新增 7 项 catalog 测试与 1 项真实本地 OIDC/BFF→catalog HTTP 测试，覆盖跨租户、用途/读写分离、预登记连接、重复提交竞争、精确价格、旧版本、审计失败回滚、锁等待撤权/证明过期、旧路由价格快照与实际部署角色。最后恢复原 CLI 审计字段后另复核 catalog/core；没有修改账本事实或执行付费调用。

前端格式/构建、12 项单元、Chromium/WebKit **46 项**和真实 Nginx HTTPS edge 通过；扩展禁止切换/未裁剪 320–1440px/44px 断言后追加复核 8 项配置与审计专项。两页 axe 与桌面视觉复核通过，仍不等同于完整人工无障碍认证。初轮失败为选择器对原生 select 的 label 文本匹配及 WebKit 刷新时多个 status 元素，改为语义角色/具体状态选择器，未降低页面安全规则。

新增 LIVE-15 保留真实 IdP/MFA/浏览器整链路、跨副本撤权与轮换、供应商登记与出站、0013 维护窗口/恢复/规模/人工无障碍认证。尚未开发：Key/护栏/策略/汇率 Web 管理、护栏监控/其他统计维度、任务追踪/经验/评测治理、审批 BFF 与独立预览、Hermes/OpenClaw 适配、图片/3D/其他多模态连接器和 SSE/Responses。已发现的阿里云兼容问题仍属于 LIVE-01，没有改记为已修复。

本包从已合并 `main` 新建独立分支；前序主分支 [Rust CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34825161814) 与 [控制台 CI](https://github.com/StateKnot/Brokerrouter/actions/runs/34825161767) 已确认通过。按用户新授权，本包提交正式 PR，经 CI 与变更复核后合并；不执行生产部署。远端最终结果以对应 PR 检查为准。

## M5.3b：虚拟 Key 与人民币汇率工作台（2026-09-15）

实现 [key-fx-management-v1](m5-key-fx-management.md)：独立 `serve-settings`、最小数据库权限与 `br-settings+jwt`；Key/FX 独立读写权限、精确人民币预算、明确模型授权、有效期、永久撤销；USD/CNY 当前/计划/历史版本和即时/定时追加发布。浏览器安全随机生成 Key，只提交摘要，先保存再登记，服务端回执和审计均无密钥明文。跨副本幂等由数据库事务保证，配置/授权/前后审计/真人/理由/回执原子提交。

Key 配置版本不会因消费变化抖动，但更新仍锁定实时预留/消费并拒绝过低预算。CLI 授权增删/撤销使旧版本失效；CLI FX 与 Web 发布共用 tenant 锁。汇率页使用同一语句时点返回当前版本和观察时间，避免计划边界标签矛盾。新 Key 和有期限 Key 必须保留有效期；过大历史授权只读并明确提示，不截断覆盖。原有资金、审批和 Agent 权限边界不变。

本地验证：Rust fmt/clippy（`-D warnings`）与 release 构建通过；**152 项常规测试 + 4 项专项通过，共 156 项**。新增 7 项 PostgreSQL/HTTP settings 合约和 1 项本地真实 OIDC/BFF→settings 合约，覆盖用途/租户/读写隔离、重复发放/撤销、预算与 CLI 版本并发、精确汇率/计划分页/过期证明、审计故障原子回滚和实际 DB 角色。4 项专项为真实 MinIO 与固定测试签名 ClamAV/FFmpeg，不是生产检测认证。最终统一汇率查询时点后另复核 settings/编译。

前端 16 项单元、Chromium/WebKit **58 项**、格式/构建及真实 Nginx HTTPS edge 通过。页面沿用 ui-styling 的原生可访问表单、键盘与明确错误反馈，覆盖只读控件、一次性明文不出浏览器、同编号失联重试、撤权清理、预算/汇率字符串精度、320–1440px 控件边界、200% 字体和 axe；视觉复核不代替人工读屏验收。未新增依赖，未进行付费调用，也未更改生产数据。

新增 LIVE-16：实际 IdP/MFA、受控终端密钥保存、真实 TLS/权限/签名轮换、跨副本撤权与资金竞争、0014 维护窗口/备份恢复、容量及人工无障碍。仍未开发：护栏/策略配置与监控、任务/调用/经验/评测治理 UI、审批 BFF 与隔离预览、Hermes/OpenClaw 原生适配、图片/3D/其他多模态协议、SSE/Responses，以及已知阿里云工具回传/Embedding 上限修复。FX 计划版本取消、周期预算及自动密钥轮换也不在本包开放范围。

## M5.3c：护栏版本、策略配置与匹配预览

2026-09-16，完成 [governance-settings-v1](m5-governance-settings.md)。复用 `serve-settings` 和定向 OIDC BFF，新增 `governance_read/write`；配置角色不能取得审批、执行或运行内容权限。页面使用真实接口，不产生付费调用。

交付内容：

- 四种既有规则的结构化表单：always、contains_any、text_json 模型判定、asset_scan；阶段/字节/数量/人民币上限及同租户引用检查。
- 护栏不可变历史与单调版本发布，首次停用，启停也发布新版本；当前版本延迟外键和 CLI 配置修订保护。旧版本同轮豁免不能用于新版本。
- 模型、虚拟 Key、Runtime、工具精确选择器，AND/OR 去重，默认停用；扫描模型必须匹配，引用退役仍可安全停用。
- 当前匹配预览与引用查询，复用运行时规则选择但不检测内容/不批准/不执行；只返回有界摘要，规则内容分页最多 20 条。
- 同事务审计 before/after/真人/理由和不可变幂等回执，陈旧版本 409；完整规则二次确认、失败同号重试、权限失效清屏与键盘焦点管理。

审批语义明确：策略始终引用护栏当前版本；选择器编辑后仍命中相同规则版本，则原本有效的同轮豁免保持。实际选择集合改变时待审批快照复核失败。配置发布不批准旧申请，也不撤回已经执行的外部副作用。

本地验证：Rust fmt/clippy（`-D warnings`）和 release 构建通过；**160 项常规 + 4 项专项通过，共 164 项**。新增 8 项治理配置 PostgreSQL/HTTP 合约，扩展真实本地 OIDC/BFF 定向路径合约；最终有界摘要/引用角色再次通过 settings 15 项复核。4 项专项为实际 MinIO 与固定测试签名 ClamAV/FFmpeg，不代表生产检测认证。前端 **19 项单元、70 项 Chromium/WebKit 合约**、格式/构建及真实 Nginx edge 通过；ui-styling 指导完整规则确认、键盘焦点、320–1440px 与 200% 字体检查。未进行真实供应商收费调用或生产部署。

新增 LIVE-17：实际 IdP/MFA→BFF→settings、配置分权/轮换/撤权、跨副本配置与审批竞争、0015 DDL/旧运维脚本迁移、备份恢复/审计留存/容量和人工读屏。下一包继续护栏监控与治理统计；审批 BFF/隔离预览、任务/调用/经验/评测 UI、Hermes/OpenClaw 原生适配、专用多模态审核、图片/3D/其他协议和 SSE/Responses 等仍待开发，不混作仅“待测”。

基于 `main` 的 `375b149956a68677438ec9bd42bf3f0066014db0` 独立开发，按授权直接提交非 Draft PR，CI 通过后正常合并；不会绕过检查或自动部署生产。

## M5.3d：护栏监控与治理统计

2026-09-16，完成 [guardrail-monitoring-v1](m5-guardrail-monitoring.md)。独立 `monitoring_read`，复用只读 serve-console 与定向 OIDC BFF；不增加审批或执行权限。

- 0016 规范化 `(操作,护栏,版本)` 观测，重复轮询不增加分母；原始风险和失败不可被后续用户批准覆盖，批准跳过单列。
- 通过/风险/检测失败/待检测/检测中/首次即跳过互斥分类，明确通过率分母；关联操作状态、过期审批、已关闭轮次与执行结果不确定分别展示。
- 每日序列、时间/业务模型/Key/护栏/阶段 AND 筛选、护栏历史版本分页；即时检查耗时与语义检测生命周期分开，样本缺失返回 null。
- 语义检测请求唯一关联人民币账本，保留未结算和预留金额；历史事件回填去重，不补造缺失即时耗时，跨采集边界明确标注历史覆盖不完整。
- 安全视图排除规则、原始事件、审批内容与媒体地址；实时读权限复核，查询切换同步清除旧内容。UI 沿用 ui-styling 可访问性原则与现有视觉系统，表格独立横向滚动，原生键盘和 200% 字号可用。

本地验证：Rust 常规回归 166 项，加最终补充的微秒/多条件筛选 1 项，共 **167 项常规通过**；**4 项真实本地 MinIO/固定测试签名 ClamAV/FFmpeg 专项通过，共 171 项**。fmt/clippy（`-D warnings`）和 release 构建通过。新增 6 项 PostgreSQL 监控合约与 1 项 BFF HTTP 路由权限测试；前端 **21 项单元、80 项 Chromium/WebKit 合约**、格式/构建与实际 Nginx edge 通过。窄屏测试发现并修复了表格最小宽度撑开外层筛选区域的问题。无新增付费模型调用，无生产部署。

新增 LIVE-18：真实 OIDC/组织权限/撤权与角色部署、0016 历史体量回填和停写升级窗口、多副本观测/批准竞争、生产规模查询计划/SLO/留存、人民币对账及人工读屏。上述项未认证，不能标为整体生产可用。下一包优先推进真人审批 BFF 与安全审阅边界；逐节点任务/调用/经验/评测 UI、Hermes/OpenClaw 原生弹窗和恢复、专用多模态审核、图片/3D/其他协议及 SSE/Responses 仍未开发。

人工复查又发现 200% 字号下固定四列会把金额拆行，已将本页指标网格改为随可用宽度/字号自动换列，并增加单行指标断言；补修后 10 项双浏览器复验、格式和构建通过，重新核看放大截图通过。PR 按最终提交重新运行完整检查，不沿用旧提交的 CI 结论。

本包从 `fb6bb32f3ccb5ecfdd1dacb9459cf7c6808e3108` 创建独立开发分支，正式 PR / CI / 合并按既有授权管理；不绕过检查或部署生产。

## M5.4a：本人审批 BFF 与安全 JSON 审阅

2026-09-16，实现 [human-approval-workbench-v1](m5-human-approval-workbench.md)。独立 Rust `serve-approvals`、`brokerrouter_approvals` DB 角色、`br-approvals+jwt` 与独立 BFF 密钥/audience。只开放本人 query/review/decide，不提供 Agent 自批、代批、创建 turn、claim 或核对外部执行接口。

- 本人列表分页只读元数据；点击审阅才解密 128 KiB 内完整 JSON，不截断、不执行 HTML/链接、不自动下载。原文按字符串传输保留大整数，核对运行时内容与上下文摘要。
- 不可变审阅凭据绑定本人/审批/操作/护栏快照，最长 5 分钟；新决定同事务重新校验有效期、Key/Runtime/模型/当前规则和所选护栏。审阅/决定审计不含正文与反馈；审计视图开放本人和操作元数据。
- 明确选择护栏与意见、范围确认、冻结内容后二次提交；失联只手动同决定重试，冲突停止；批准≠执行，部分批准仍可能等待剩余护栏。反馈关闭原 turn，不自动生成或重发任务。
- 页面关闭/隐藏/到期/撤权清理敏感原文；审批凭据不能替代真实用户读完的证明或 IdP/MFA。已接受的在途决定不因关闭/退出自动撤销，须重新登录查原审批。
- 文本 Chat 与工具 JSON 可审阅；artifact 不允许凭描述放行，media_output 决定在本页和服务端关闭，专用媒体审阅与原生 Runtime 交互仍需下一包开发。

新增 LIVE-19，保留真实 IdP/MFA、浏览器整链路、跨副本撤权/在途决定、签名/解密键轮换、0017 升级/备份/恢复、审计留存/规模和人工读屏认证。按 ui-styling 指导使用原生语义表单、键盘焦点和窄屏/200% 字体检查；不新增前端依赖，不调用付费供应商。

本地常规 Rust **178 项通过**，实际 MinIO 与固定测试签名 ClamAV/FFmpeg **4 项专项通过，共 182 项**。新增 9 项审批 PostgreSQL/HTTP/真实角色合约、1 项真实本地 OIDC→BFF→审批服务合约及文本结构边界单元测试。最终审计投影、维护角色及文本/工具混合消息边界另做定向复验。fmt/clippy（`-D warnings`）与 release 构建通过；前端 **23 项单元、94 项 Chromium/WebKit**、格式/构建与实际 Nginx HTTPS edge 通过，最终审批页追加 14 项复验。320/768/1440px 与 200% 字体、精确原文、无外部资产请求、axe/焦点与清屏均验证；自动化和截图不替代人工读屏与真实 IdP 认证。

首次全量回归遇到既有 catalog 过期测试的 1 秒证明在准入前已过期，返回另一种合法拒绝码；将夹具改为 3 秒准入、4 秒审计延迟，明确在事务内过期，保留 401 与无写入断言，没有修改业务权限检查。第二次全量常规回归通过。

本包从 `4762ec08804908138ceee3f8f6397feef3878f0b` 的干净 main 创建独立分支，正式 PR 按最终提交重新运行 CI，经检查通过后正常合并；没有生产部署。下一包继续媒体隔离预览/审批边界，然后逐节点治理与 Hermes/OpenClaw 原生暂停/恢复；专用多模态审核、图片/3D/其他协议和 SSE/Responses 仍为未开发项。

## M5.4b：隔离 MP4 审阅与媒体审批边界

2026-09-16，实现 [isolated-mp4-review-v1](m5-isolated-media-review.md)。从干净 main `20171fcca4bae79db3bc5ca7b34b4d445c5c897e` 创建独立分支；Rust `serve-media-review`、专用 `br-media-review+jwt` / BFF 密钥/audience、只读媒体角色，与决定服务分离。

- 0018 不可变审阅会话、已交付块和完成事实；本人/审批/产物/护栏快照精确绑定，5 分钟且不超过当前授权期限，每用户最多 4 个有效会话。每块读前后重检权限/技术扫描/策略，审计与截止时间检查成功才返回。
- 完整块集合、总长度、整文件 SHA-256 和用户明确审阅确认后才产生可放行媒体的 review；JSON 或部分字节不能放行。本人可直接拒绝/反馈，不强迫观看风险内容、不发豁免。完成审阅不发 grant，不执行任务；决定服务再验证凭据与当前事实，使用原二次确认/同内容重试协议。
- 独立无登录权限静态站点，opaque iframe sandbox、无网络/存储、固定 MessagePort、真实 MP4 解码；最多 64 MiB/300 秒/8,294,400 像素，无外链/下载降级。控制台只中转字节，技术 unsafe/unsupported/error 不可豁免。
- UI 按 ui-styling 的原生语义/焦点/窄屏/200% 字体要求；不引入新依赖。独立 Nginx 预览配置只发布三份静态文件，不代理 BFF，精确 CSP/父源/Host；现有主源仅增加固定预览 frame-src。

新增 LIVE-20；真实 IdP/MFA、独立域名/浏览器站点隔离、解码器/受管终端、生产 S3 GetObject-only、跨副本撤权/失联、0018 维护窗口/恢复、审计容量/人工无障碍与 Runtime 整链路仍未认证。用户声明和块交付不能证明人看完；受信客户端/发布链/浏览器被攻破的风险不由 sandbox 自动解决。没有付费调用或生产部署。

本地 Rust **188 项常规测试分批通过**：先全量 187 项，再为“不必观看即可拒绝/反馈”补充 1 项，并复验媒体 9 项和原审批 9 项；**2 项真实 ClamAV/FFmpeg 专项通过**，fmt/clippy（`-D warnings`）与最终 release 构建通过。新媒体合约覆盖分块完整性、本人/摘要/策略绑定、存储读取期间撤权、审计失败回滚、事务内过期、并发幂等、真实 DB 分权和 HTTP 用途隔离；BFF 另测二进制边界、CSRF、退出后响应隔离。MinIO 本地初始化遇到自动权限审核连续超时，本轮未执行两项本地 S3 专项，不记为通过；正式 PR 的隔离 MinIO CI 必须补验成功才合并。

前端 **25 项单元、108 项 Chromium/WebKit 用例分批通过**；最终全量先通过 100 项，8 项原生 option 禁用断言被测试工具误判，改为验证原生 disabled 属性后，28 项审批/媒体双浏览器复验全部通过。格式/构建、主控制台和独立预览站点的实际 Nginx HTTPS 边界通过；无障碍/320–1440px/200% 字体和截图检查通过，不等于完整人工读屏认证。更早的字号夹具曾通过注入 style 标签触发严格 CSP，已改为仅测试环境 CSSOM 调整字体，没有降低生产 CSP。Vite 仍提示主包略高于 500 kB 的非阻断体积警告。

下一开发包：逐节点任务/调用治理，再推进固定 Hermes/OpenClaw 原生暂停/恢复及经验候选/评测样本；图片/3D/音频预览、专用多模态语义审核、其余生成协议和 SSE/Responses 仍未开发。正式 PR 按最终提交 CI 全绿后正常合并，不绕过检查。

## M5.5：逐节点任务与调用治理

2026-09-16，从干净 main `7b82ac23898d264af7397edf04391a4dcc0948da` 开始，实现 [execution-traces-v1](m5-node-call-governance.md)。独立 `traces_read` 组织审计权限，复用只读 console/OIDC BFF、固定签名路径与响应后撤权检查，不赋予原文审阅、审批、claim 或事件 ack 权限。

- 0019 安全投影和轮次/节点/请求/审批索引；轮次 31 天内精确 AND 筛选与微秒键集分页，节点下钻关联调用/护栏/审批。模型调用、模型护栏调用、端点尝试、媒体状态、当前身份绑定和原始节点状态分别展示，不补造运行时 DAG。
- 金额按唯一请求汇总，pre/post/attempt 不重复记账；未知/预留与已知 CNY 分开，业务与护栏调用分开。请求归属视图用两个互斥 UNION ALL 分支，让轮次条件可下推至对应索引。
- 原始风险不被后续同意覆盖；实际 grant 版本与审批事实分离展示；过期执行租约只读显示 unknown。持久事件固定已提交上界，模型护栏完成事件由不可变 evaluation 找节点，不显示原始事件 JSON，不推进 Runtime 消费游标。
- UI 沿用 ui-styling 的可访问表单、焦点、独立横向表格与大字号规则；下钻后焦点回到类别选择，返回列表保留日期；无新依赖、没有模拟后端替代真实 API。

本地常规 Rust 全量 **195 项通过**，最终索引友好投影再做 **6 项 PostgreSQL 治理合约复验通过**；新增共 6 项数据库/真实受限角色测试和 1 项真实本地 OIDC→BFF 路由测试。fmt 与 Clippy（`-D warnings`）通过。前端 **28 项单元、124 项 Chromium/WebKit 全量通过**；最终焦点/日期保持补丁另作治理页复验。实际 Nginx 主站与隔离预览站合约通过，首次 Docker 启动回收超时后单独复验成功。未新增付费调用，没有修改真实用户 Hermes 容器或生产部署。本地本包不重跑 4 项 MinIO/ClamAV/FFmpeg ignored 专项，正式 PR 必须由 CI 补验成功后才合并；最终 release/远端状态以本包 PR 记录为准。

新增 LIVE-21 保留真实部署/IdP/分权、多副本与在途撤权、0019 大体量维护窗口、查询计划/SLO/留存、供应商对账和人工读屏验收。已只读核验用户 Docker 中 Hermes `0.21.1` 及文件摘要，发现普通工具插件失败放行、本地批准不等于网关真人证明等边界，形成 [M6.1 原生接入实施约束](m6-hermes-native-integration.md)。本节交付时 Hermes/Claw 原生暂停恢复仍未实现，后续进展见 M6.1a。

## M6.1a：Hermes 工具输入原生暂停与一次性派发

2026-09-17，从干净 main `153c6bc5c482b4e8fa759547a86e6c8ce2619705` 开始，针对用户指定的 NousResearch/hermes-agent 固定提交 `67764dc0863349a384c16425e73ee8571f3a94b7` 实现第一包宿主适配。网关 Rust/PostgreSQL 治理协议不变；只有贴近 Hermes 执行器的适配按宿主使用 Python。

- 严格 overlay 安装器验证 `tool_executor.py` 完整 SHA-256 和唯一锚点，在新镜像构建根内原子安装、保留固定基线并生成摘要清单；不热补丁用户运行容器。
- 强制门位于所有参数改写和 Hermes 本地授权之后、真实 dispatch 之前。受信 Brokerrouter turn 缺失、配置/存储异常、网关不可用与异常响应均形成结构化 hard stop，不降级执行。
- `checking`/`waiting_approval` 重放同一 operation；批准后先持久化 AES-256-GCM 加密的随机 claim token，仅 `dispatch_authorized=true` 执行。原始最终参数不写检查点数据库。
- claim 响应丢失、授权后重启、工具异常、回执丢失分别保留 `claim_unknown`、`execution_unknown` 或 `effect_recorded`；恢复只补安全回执，不自动再次调用外部工具，也不把没有结果说成未执行。

在当前 Hermes 0.21.1 镜像的无网络临时容器中 **15 项适配器/安装器/真实宿主派发合约通过**，补丁后宿主与适配器均通过 Python 编译；没有模型调用、供应商费用、真实工具副作用或对运行容器的修改。CI 新增独立 Hermes 固定源码摘要/overlay/状态机检查。

未完成边界：实际 Gateway 受信 turn 注入与审批卡片、`tool_output`/`artifact` 发布检查和会话结果落盘后的回执、真实进程逐点终止/多实例/串并行/委派/MCP 工具矩阵、受管镜像发布及 OpenClaw 适配。以上继续 M6.1b 和 LIVE-06/21，当前包不宣称完整输入输出治理或生产部署认证完成。

## M6.1b：Hermes 受信 turn 与原生审批通知

2026-09-17，从合并 M6.1a 的干净 main `db50843` 开始，在不修改用户运行容器的前提下完成第二包：

- Rust Runtime 新增受虚拟 Key 保护的 `POST /v1/governance/turns/{id}/bind`，在事务锁内核对当前绑定用户、turn 活性及 `session_id,message_id,run_id,generation`。缺失或任一不一致不泄露记录。
- Hermes Gateway 仅从事件 metadata 接收 BFF 创建的 `turn_id,run_id`，其余身份取自宿主上下文；在历史读取、首次模型/工具调用前绑定，网络不确定也失败关闭。受管 Gateway proxy 明确禁用。
- 待审批时通过 Hermes adapter 原生通道发出审批 ID 和本人审阅页链接；聊天回复不能签署决定。SQLite v2 租约去重明确失败，对不确定发送不重复轰炸。决定后重放原检查并重验全部绑定与最新策略。
- overlay 扩展为三个完整 SHA-256 宿主文件，可从精确中断状态恢复；治理模式在 Gateway 导入时校验宿主补丁、适配器全文件和清单。

固定 Hermes 0.21.1 自动删除临时容器 **26 项全部通过**；新增 PostgreSQL 直接事务和真实 HTTP Router 合约通过。未调用模型、未产生供应商费用或真实工具副作用。当前通知是通道内安全审阅链接，不是聊天内直接批准卡片；真人决定仍在已实现的独立审批 BFF 完成。

Rust 常规全量 **197 项通过**，4 项需 MinIO/固定 ClamAV/FFmpeg 环境的专项本地按既有闸门 ignored；`cargo fmt` 与 Clippy `-D warnings` 通过。首次全量命令因上一个被界面超时截断但仍留在后台的测试进程，与第二次 SQLx 同名临时库冲突；残留进程结束后单独复跑受影响的 outputs 42 项及后续 settings/pricing 19 项全部通过，失败中没有代码断言错误。

下一包必须完成 `tool_output`/`artifact` 与 Hermes 会话持久化之间的隔离两阶段发布，不用路径 metadata 或“工具已返回”伪装安全发布。受管镜像部署、真实进程逐点终止/多实例/工具矩阵和 OpenClaw 仍由 LIVE-06/21 阻断。

## M6.2a：MCP 2026-07-28 与 Skills 安全基础

2026-09-18 对照最新稳定核心协议、Streamable HTTP、Authorization、`server/discover`、官方 Skills 扩展和 Agent Skills 格式完成差异评审。结论是现有 OpenAI 兼容模型网关不符合 MCP：没有 MCP client/server、逐请求元数据、无状态 HTTP、OAuth、MRTR、发现和 Skills wire 方法；已有逐节点治理、真人审批和审计只能作为上层机制复用。

本包新增 [MCP/Skills 接入契约](m6-mcp-2026-skills.md) 和 Rust 安全内核：静态完整 manifest、512 文件/16 MiB、领域 URI、来源绑定身份、按需 size/SHA-256/frontmatter 校验、跨源同名冲突、动态 Skill 默认拒绝以及绑定完整资源集合的审批指纹。核心协议后续固定复用当前官方 `rmcp 3.4.0`，不自研另一套状态机。

未完成边界：本包不开放 `/mcp`，不注册上游 MCP server，不读取远端 Skill，不持久化批准，也未运行官方一致性套件。M6.2b–d 将依次完成连接/能力目录、Skill 注册和审批/隔离缓存、MRTR 与 Runtime 恢复认证；以上完成前不能宣传 MCP 或 Skills 兼容。

本地新增 7 项 MCP/Skills 单元测试通过；隔离 PostgreSQL 全量回归与最后的 MCP/Skills 复验合计覆盖 204 项唯一常规用例，全部通过、0 失败；4 项既有 MinIO/固定 ClamAV/FFmpeg 专项按 CI 规则保持 ignored。fmt、Clippy `-D warnings` 和 release 构建通过。没有连接外部 MCP server、调用模型或产生供应商费用；正式 PR 仍须由 CI 执行全部固定专项。

## M6.2b1：MCP 连接与能力持久目录

2026-09-18，在 M6.2a 的身份与资源安全内核上完成 [连接目录契约](m6-mcp-connection-catalog.md) 和 0020 迁移。每个上游由 Brokerrouter 按租户分配不可变 UUID；Streamable HTTP 强制 HTTPS，stdio 只能引用部署 launcher profile，控制面不能注入命令。环境变量/Secret Manager credential reference 使用独立 MCP AES-256-GCM 密钥环加密并绑定 tenant/server，不保存实际 Secret。

OAuth client credentials 配置固定 issuer/resource/audience 且 audience 必须等于规范化 resource。服务器先以 draft 注册，再通过乐观 revision 明确启用；更新与发现通过 server advisory lock 串行。`server/discover` 持久边界只接受 `2026-07-28`、`resultType=complete`、包含当前版本的能力结果，限制 JSON 大小/深度和 TTL；private cache 同时绑定实际解析认证上下文指纹，使引用不变时的 Token/Secret 轮换也失效，远端 serverInfo 仅保留为不可信展示字段。配置、发现、幂等回执和审计原子提交，旧 revision 快照不会用于新配置。

部署新增互不继承的 `brokerrouter_mcp_catalog` 与 `brokerrouter_mcp_client` 角色：前者不能写发现或读取业务密钥/审批，后者不能更新连接或审批。CLI 增加 `mcp-server-register`/`mcp-server-update` 严格 JSON 入口，专用密钥环境变量已加入样例。

新增 5 项 PostgreSQL 合约全部通过，覆盖并发幂等/冲突、跨租户、密文与 AAD、draft/启用/revision、OAuth 绑定、实际认证上下文变化、错误协议/resultType/传输/超限拒绝、cacheScope 分区、旧快照失效、stdio 路径逃逸、租户停用、审计故障回滚和真实受限角色正反权限。全量常规 Rust **209 项通过、0 失败**；4 项既有 MinIO/固定 ClamAV/FFmpeg 专项按本地闸门 ignored，必须由正式 CI 执行。fmt、Clippy `-D warnings` 和最终 release 全目标构建通过；正式 PR 状态以下方提交记录为准。

未完成边界：本包没有增加未使用的 SDK 依赖，不进行网络发现，也不开放 `/mcp`。固定 `rmcp 3.4.0`、Secret resolver、OAuth metadata/token 校验、DNS/IP 固定、Streamable HTTP/stdio wire、Origin/标准头/SSE 和官方 client/server conformance 属于 M6.2b2；M6.2c–d 继续 Skill 持久审批、MRTR 与 Runtime 恢复认证。以上完成前仍不能宣传 MCP 兼容。

## M6.2b2a：MCP Streamable HTTP 上游发现

2026-09-20，在 M6.2b1 不可变连接目录上完成 [Streamable HTTP 发现执行边界](m6-mcp-http-discovery.md)。固定官方 `rmcp 3.4.0`，只采用 MCP `2026-07-28` Discover lifecycle；新增 `mcp-server-discover`，由 `brokerrouter_mcp_client` 读取 active revision、解密引用、经 DNS/IP 固定和受管私有 CA 发起真实 `server/discover`，再原子记录完整 snapshot/audit/receipt。

凭据支持 none、Bearer 和 OAuth client credentials。Secret Manager URI 只通过显式 URI→注入环境变量映射解析；OAuth 禁止 legacy metadata fallback，精确核对 issuer 和 endpoint/resource/audience，并对 metadata/token 每个 URL 重做出站策略。JSON/SSE 发现响应限制 512 KiB，OAuth 响应限制 1 MiB；同源 metadata 重定向最多 5 次，Token 不重定向。private cache 指纹绑定实际凭据/token，operation 重试先读持久 receipt，已完成时不重复联网。

本地真实 TCP wire 回归验证两次 discover 均携带标准头和 self-contained metadata、从不发送 initialize、Bearer 只到固定上游、超大响应在解析前失败；Secret/指纹和 receipt/server 绑定也有回归。未完成边界：stdio launcher、下游 `/mcp`/Origin/认证/工具路由、官方 client/server conformance、真实 HTTPS/OAuth/私有 CA 预生产认证仍属 M6.2b2b/LIVE-22；M6.2c–d 继续 Skill 持久审批、MRTR 与 Runtime 恢复。

本地 fmt、Clippy（`-D warnings`）通过；最终 debug `--all-targets` 为 **214 项通过、0 失败、4 项按环境闸门 ignored**。release 全目标先以 213 项通过，最终 MIME 精确匹配加固后又以 release 定向复验 5 项 MCP client 测试。4 项 ignored 是既有隔离 S3 与固定 ClamAV/FFmpeg 专项，必须由正式 PR CI 补验；release 全目标包含真实 PostgreSQL 和本地 TCP wire 回归。没有连接付费模型供应商、调用模型或产生供应商费用。

## M6.2b2b1：MCP 受管 stdio 上游发现

2026-09-20，完成 [受管 stdio 发现执行边界](m6-mcp-stdio-discovery.md)。部署侧使用版本化只读 launcher registry；租户仅能引用 profile 名称。connector 对 registry/program 绝对规范路径、Unix 权限、参数/环境/数量/超时做严格校验，以无 shell、清空环境、显式环境映射和丢弃 stderr 的方式启动官方 `rmcp 3.4.0` child transport。stdio 禁止配置 HTTP Bearer/OAuth，private cache 绑定完整 launcher manifest 与注入环境值。

真实 stdio wire fixture 验证两次 2026-07-28 discover、版本选择、可执行文件摘要、私有缓存指纹与关闭路径通过；新增依赖固定进 lockfile。常规 Rust **216 项通过、0 失败**；首次全量运行有 1 个既有媒体并发用例在 1 秒等待窗超时，隔离复跑通过；4 个既有 MinIO/固定 ClamAV/FFmpeg 专项按 CI 闸门 ignored。fmt、Clippy `-D warnings` 和 release 锁定依赖构建通过。未完成边界：统一下游 `/mcp`、虚拟密钥/OAuth 资源认证、Host/Origin/标准头、租户工具目录与治理后转发仍属 M6.2b2b2；Skill 持久审批、MRTR、Hermes/OpenClaw 恢复与官方冻结 conformance/预生产认证继续由 M6.2c–d/LIVE-22 阻断。

## M6.2b2b2a：下游 MCP 目录与摘要授权

2026-09-20，完成 [下游 MCP 目录边界](m6-mcp-downstream-catalog.md)。新增单一无状态 `POST /mcp`，固定官方 `rmcp 3.4.0`/协议 `2026-07-28`，强制 Host、Origin、Accept、协议版本、逐请求元数据和标准头，拒绝 initialize、旧 session/Last-Event-ID 与 GET。当前使用虚拟 Key 鉴权，独立 `brokerrouter_mcp_server` 角色不能读取上游 credential、审批或修改 grant。

上游发现现在在同一认证生命周期内完整分页采集 `tools/list`，以 snapshot/revision/TTL/cacheScope 保存不可变目录。Key 工具 grant 绑定完整 descriptor SHA-256；上游更改描述、schema、annotations、icons 或 `_meta` 后旧授权自动失效。`tools/list` 只返回当前 Key 的明确授权；`tools/call` 重新鉴权并写参数摘要审计后返回 `governance_execution_unavailable`，不会直连上游或产生副作用。

未完成边界：OAuth Protected Resource/JWT、治理后的真实上游调用、MRTR input_required、执行租约/回执、Hermes/OpenClaw 恢复和冻结官方 server conformance 仍属 M6.2b2b/M6.2d；因此本包是可部署的只读目录服务，不是完整 MCP 工具代理。

## M6.2b2b2b：下游 MCP OAuth 资源认证

2026-09-20，完成 [OAuth Protected Resource 边界](m6-mcp-oauth-resource.md)。新增 path-specific RFC 9728 metadata；401/403 challenge 携带 `resource_metadata` 和最小 scope。下游 JWT 只允许预配 issuer、非对称 `at+jwt`、严格单一 canonical resource audience、有界生命周期和标准 scope；拒绝 token 指定远程 key。远端 JWKS 经 DNS/IP 固定、无代理/重定向、大小/key 数/TTL/未知 kid 刷新限制获取，也支持部署固定公钥 JWKS。

0022 迁移增加 OAuth issuer/subject/tenant 到虚拟 Key 的显式映射。签名 token 的自报租户不能直接授权；映射、Key、租户、scope 和工具 grant 每次请求都重新校验。配置通过幂等 operation、事务审计与专用 catalog/server 数据库权限发布。虚拟 Key 路径继续兼容，下游 token 从不转发到上游。

本地 OAuth 单元测试 **3 项通过**；隔离 PostgreSQL 17 的 MCP catalog **6 项**和 server **2 项**通过，全仓常规回归 **224 项通过、0 失败、4 项环境专项 ignored**；fmt、Clippy `-D warnings` 通过。ignored 项由正式 CI 的 S3/固定 ClamAV/FFmpeg fixture 显式执行。真实 IdP/JWKS TLS/轮换、opaque introspection、容量和官方 conformance 仍在 LIVE-22。治理后的真实上游调用、MRTR `input_required`、执行 claim/receipt、Hermes/OpenClaw 恢复仍属后续包，因此 `tools/call` 继续审计后失败关闭。

## M2 JiaClaw 消费者闭环增量（2026-09-20）

针对 JiaClaw 必须统一走 Brokerrouter 的个人 Agent 约束，完成四个相互依赖的生产边界：

1. 新增事务化 `init-personal`，在一个数据库事务中创建有预算租户、Chat 模型、上游端点、有期限虚拟 Key、模型 grant 和审计事实；同名 profile 不重发密钥。用于 JiaClaw 的单用户 TOML 只在创建成功时输出一次。
2. Chat 端点新增默认 false 的 `supports_streaming`。`stream:true` 只在满足能力的端点中路由，严格校验 OpenAI 兼容 SSE；客户端断开后仍完成账本与加密结果持久化，且只在这些事实成功后发 `[DONE]`。输出护栏命中时出网前失败关闭。
3. 供应商非标准 `tool_calls[].index` 在响应边界规范化，同一 assistant message 原样回传时也会验证后移除扩展字段。无工具或 SSE 能力时返回 `unsupported_model_capability`，不静默删除请求字段。
4. 新增 [JiaClaw 消费者指南](jiaclaw-consumer-guide.md)、[工具往返认证矩阵](tool-roundtrip-certification.md)和可重复执行的 `brokerrouter-certify-tools`，用虚拟 Key 验证两轮 tool 闭环、幂等重放、单 attempt 及人民币预留归零，不输出凭据或正文。

本地回归覆盖个人初始化原子回滚、工具原样两轮往返、SSE 成功结算/幂等重放/客户端断开/上游中断/缺失 usage、能力路由与输出护栏。常规 Rust 全量 **232 项通过、0 失败、4 项环境专项 ignored**；最终组合能力加固后 Gateway **34 项通过**，fmt、Clippy `-D warnings` 和 release 构建通过。Console 合约 **28 项**、Chromium/WebKit 真实浏览器 **124 项**全部通过。另以一次性 PostgreSQL 17 和真实 `init-personal`/`serve` 进程完成非流式、SSE、请求详情及认证 CLI 端到端冒烟，幂等重放未增加上游请求。

阿里云北京的当前修复复验、真实 SSE、固定 SDK、账单核对和容量仍由 LIVE-01/03/04 阻断；在真实两轮证据入库前，认证矩阵不将任何真实供应商标为 JiaClaw 生产默认。

## M6.2b2c：治理后的 MCP 单次工具调用（2026-09-23）

完成 [持久工具调用契约](m6-mcp-governed-tool-calls.md)：`tools/call` 绑定受信 turn 与不变的 `callId`，输入/输出两侧复用现有策略和真人审批；独立 `mcp-worker` 重新核对授权、连接 revision、工具完整描述，再对 HTTP/受管 stdio 上游做一次调用。密文结果与执行回执原子提交，重复请求只读取已有事实，断线或结果不确定不自动重发。

0024 迁移增加逐调用状态机、审批后结果不可变约束和有界租约维护；`brokerrouter_mcp_server` 仅能入队/释放，`brokerrouter_mcp_worker` 才能访问上游连接与凭据引用，两者都不能审批或改 grant。真实 PostgreSQL/受限角色、stdio fixture 与下游 HTTP 覆盖输入审批、输出审批、撤权、幂等冲突、丢失结果、单次副作用、租约失效审计及结果篡改拒绝。

本地最终 `cargo fmt --check`、Clippy `-D warnings`、release 构建和 Rust 常规全量 **241 项通过、0 失败**；4 项既有隔离 S3/ClamAV/FFmpeg 专项按本地闸门 ignored，正式 CI 必须单独执行。没有连接付费供应商或产生模型费用。

当前只在上游返回完整结果时释放；`input_required` 和 task 响应保留加密事实但**不伪装成可恢复的成功调用**。M6.2d 仍需 MRTR/task 持久恢复、Hermes/OpenClaw MCP 原生暂停恢复、真实 HTTPS/OAuth 与官方 conformance。未在既有生产站点启用 MCP 工具执行，也未产生付费供应商调用。
