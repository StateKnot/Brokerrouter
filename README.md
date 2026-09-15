# Brokerrouter

Rust 主导的国内多供应商 AI 网关：统一模型调用、多模态任务、人民币账本和智能体审批治理。

项目正在按 [开发里程碑](docs/milestones.md) 实施。当前能力及验证结果以里程碑状态为准。

技术路径：Rust 1.98.1、Tokio、Axum、SQLx、PostgreSQL；控制台 TypeScript，Runtime Adapter 按宿主语言实现。

生产原则：租户隔离；虚拟密钥只保存哈希；价格与人民币/美元汇率版本化；上游结果不确定时禁止盲目重提；真人审批与 Agent 身份分离；数据库事务保证预算与执行状态正确性。

## 本地运行与验证

```sh
docker compose -f compose.test.yaml up -d --wait
export DATABASE_URL=postgres://brokerrouter:brokerrouter-local-only@127.0.0.1:55432/brokerrouter
cargo run -- migrate
cargo test --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
# Inject result-encryption keys before starting the data plane; see docs/m2-text-gateway.md.
cargo run -- serve
```

`compose.test.yaml` 仅供本地测试，数据库在 tmpfs 中，删除容器会丢失其数据。它不连接现有 AgentOS 服务，也不是生产数据库部署方案。数据库服务角色由运维创建；生产迁移账户与运行账户须分离，运行账户不得具有创建/删除表、禁用触发器权限。

初始控制面是可信运维 CLI，运行端口不暴露管理员万能密钥。`cargo run -- --help` 查看命令。创建顺序为 tenant-create → model-put → endpoint-add → key-create。密钥仅创建时输出一次，命令输出应当按 Secret 管理。

## 人民币与美元

平台预算、预留、消费和默认展示以人民币为主。支持按租户发布 USD/CNY 配置：

```sh
# 7.20 仅为配置示例，不是自动获取的实时汇率。
cargo run -- fx-set --tenant <TENANT_UUID> --usd-cny 7.20 --source manual
cargo run -- fx-show --tenant <TENANT_UUID>
cargo run -- fx-history --tenant <TENANT_UUID> --limit 20
```

可用 `--effective-at` 指定 RFC3339 生效时间；历史列表按版本倒序，通过 `--before <VERSION>` 翻页。历史配置不可覆盖，已有请求固定价格和汇率快照。USD 端点缺少已生效汇率时拒绝准入；CNY 端点直接以人民币计算。金额使用整数微元，价格与汇率在最终舍入前保留精确十进制；API 汇率、价格和原币金额为十进制字符串。

结算事实保留原币金额、换算后尚未取整的人民币金额及最终人民币微元。它们是配置价格计算结果，不等于已完成供应商发票对账。数据库权限组与完整口径见 [运行说明](docs/operations.md)。

## 当前 HTTP 接口

- `GET /health/live`：进程存活。
- `GET /health/ready`：数据库可连接，且迁移版本和 checksum 与运行二进制一致。
- `GET /v1/models`：只返回当前虚拟密钥被授权的活跃模型。
- `GET /v1/exchange-rates`：当前租户汇率版本与人民币主币种。
- `POST /v1/chat/completions`：非流式文本/工具调用协议。
- `POST /v1/embeddings`：文本向量，固定向量空间维度。
- `GET /v1/requests`：当前虚拟密钥的请求日志及人民币金额。
- `GET /v1/requests/{id}` 与 `/result`：查询逻辑请求/attempt 状态与原结果。
- `POST /v1/requests/{id}/check-result`：显式驱动策略升级后所需的输出检测；GET 结果读取不启动收费检测。

业务接口通过 `Authorization: Bearer <VIRTUAL_KEY>` 鉴权。所有响应包含网关生成的 `x-request-id`；日志不记录鉴权头和请求正文。

收费 POST 还需 `Idempotency-Key`。完整支持字段、明确拒绝能力、恢复方式、费用核对与密钥配置见 [M2 调用契约](docs/m2-text-gateway.md)。

M3 已增加确定性/文本语义护栏、模型输入/输出审批、当前 turn 豁免、独立真人控制面与工具执行恢复契约，见 [M3 治理协议](docs/m3-governance.md) 与 [语义护栏及人民币费用契约](docs/m3-semantic-guardrails.md)。受管调用必须携带服务端登记的 `X-Brokerrouter-Turn-Id`。`serve-control` 使用独立数据库角色和用户 BFF 公钥，不能与 Agent 凭据混用。真实 BFF/Runtime、供应商、SDK 与检测质量认证仍在 [验收待办](docs/acceptance-backlog.md)；不宣称普通 SDK 已能弹窗、多模态内容审核已接入或整个平台生产认证完成。

M4.1 已增加 [私有输入资产与非 Token 人民币计费](docs/m4-assets-pricing.md)：`serve-assets` 提供 `/v1/assets` 分块上传、校验、私有下载和过期清理；`asset-quota-set` 显式配置租户/Key 字节配额。S3 兼容存储中的资产分块经应用加密，内容安全状态单独记录；不能把输入上传完成视为审核通过。

M4.2 已增加 [持久媒体作业](docs/m4-media-jobs.md)：`serve-media` 提供动态能力、作业创建/状态/事件/取消，`media-worker` 独立执行。首个连接器为百炼 Wan2.7 文生视频，支持多端点能力匹配、输入真人审批、不确定受理不重发、恢复查询及冻结价格/汇率的人民币结算。生成成功停在隔离的 `output_pending`，只有经过下述 M4.3 审批交付链才可读取文件。真实供应商认证尚未执行。

M4.3a 增加 [输出隔离转存](docs/m4-output-quarantine.md)：生成前原子预留存储容量、独立 `output-worker`、精确来源/强 ETag 校验、持久加密分块与取消清理，并用数据库 RLS 阻断旧输入资产接口读取输出。转存成功不等于审核或交付，后续门禁见下。

M4.3b 第一包增加 [扫描作业与证据链](docs/m4-output-scan-evidence.md)：独立 `serve-output-scans`、版本化检测清单摘要、租户/profile 专属扫描凭据、分块读取、持久租约与不可变回执。完成回执不是人工批准，不直接开放产物下载。

M4.3b 第二包接入 [实际隔离技术检测器](docs/m4-isolated-detector.md)：独立 Rust `brokerrouter-scanner` 通过无网络、只读、有限资源的固定摘要镜像执行 ClamAV、FFprobe 与 FFmpeg 全流解码，持久化证据并支持失败恢复。**技术检测不替代语义合规**：`content` 始终为 unknown，不能据此自动交付。

M4.3b 第三包增加 [资产策略、真人审阅与当前轮交付](docs/m4-output-delivery.md)：独立 `serve-output-delivery`、绑定扫描配置的 `media_output` 护栏、受信真人审阅、部分批准/拒绝/反馈与原 Key 受控分块读取。技术失败不可豁免，内容风险/未知由真人决定；同轮批准不跨轮，撤销和配置变化在读取前后重新校验。没有永久链接或 Agent 代批权限；专用多模态语义审核、控制台预览/原生弹窗及生产认证仍待后续完成。

M5.1 增加 [控制台只读报表](docs/m5-console-reporting.md)：独立 `serve-console`、用途隔离的 BFF 请求证明与用户读权限、人民币汇总/每日曲线、模型/Key 维度、请求日志和审计元数据。金额来自实际账本，未知费用/预留/用量缺失单独标识；查询不能批准动作或修改预算。

M5.2a 增加 [OIDC 控制台 BFF](docs/m5-oidc-bff.md)：标准授权码/PKCE、预登记机构/用户/租户绑定、加密一次性登录、持久随机 Cookie 会话、CSRF、退出/停用代次、固定只读报表代理与最小数据库权限。不把协议测试 IdP 标为生产登录渠道。

M5.2b 增加 [TypeScript 只读控制台](docs/m5-console-ui.md)：组织登录、人民币用量与每日精确金额、模型/Key 分组、请求日志、会话到期/跨窗口退出和同源 HTTPS 部署配置。页面只接真实 BFF，金额/Token/游标不损失精度；已完成本地双浏览器与实际 Nginx 合约检查。审计页面、审批 BFF、配置写入及其他治理页面尚需开发，真实 IdP/MFA/生产证书/整链路与容量认证仍是发布闸门。

M5.3a 增加 [模型/端点配置与审计工作台](docs/m5-catalog-management.md)：独立 `serve-catalog`、用途隔离签名与显式读写权限、运维预登记供应商连接、默认停用的逻辑模型/Chat/Embedding 端点、精确价格、版本冲突/重复提交保护与事务审计。审计页面区分真人与数据库执行身份；配置不等于批准智能体动作。

M5.3b 增加 [虚拟 Key 与人民币汇率管理](docs/m5-key-fx-management.md)：独立 `serve-settings`/签名/权限，浏览器一次性 Key 生成与摘要登记、预算/有效期/模型授权/永久撤销；人民币本位的 USD/CNY 精确配置、当前/计划/历史版本和幂等发布。剩余护栏/策略 Web 配置、监控、审批/治理 UI 与 Runtime 适配仍按里程碑推进，真实部署认证另行验收。

2026-09-14 已完成 [阿里云北京 4 模型真实冒烟](docs/live-aliyun-2026-09-14.md)，并记录工具原样回传、Embedding 端点限制和账单核对待办；不据此宣称全部供应商模型或完整生产认证通过。
