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

业务接口通过 `Authorization: Bearer <VIRTUAL_KEY>` 鉴权。所有响应包含网关生成的 `x-request-id`；日志不记录鉴权头和请求正文。

收费 POST 还需 `Idempotency-Key`。完整支持字段、明确拒绝能力、恢复方式、费用核对与密钥配置见 [M2 调用契约](docs/m2-text-gateway.md)。此阶段不宣称流式、Responses、护栏审批或任何供应商已完成认证。
