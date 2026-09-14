# M5.3a：模型/端点配置与审计工作台

`catalog-management-v1` 交付逻辑模型、文本 Chat / Embedding 多供应商端点管理和审计页面。页面使用真实可写接口，不是演示表单；不调用收费模型，不把新建端点标为已认证。媒体连接器、Key/护栏/策略/汇率的 Web 配置与智能体审批 UI 仍独立追踪。

## 权限与调用边界

浏览器继续使用同源 OIDC Cookie + CSRF，BFF 只代理以下固定路径：

| 浏览器 POST | 独立配置服务 POST | 权限 |
|---|---|---|
| `/bff/v1/catalog/models/query` | `/catalog/v1/models/query` | `catalog_read` |
| `/bff/v1/catalog/endpoints/query` | `/catalog/v1/endpoints/query` | `catalog_read` |
| `/bff/v1/catalog/apply` | `/catalog/v1/apply` | `catalog_read` 与 `catalog_write` |

报表/审计仍走 `serve-console` / `br-console+jwt`。配置使用 `serve-catalog` / `br-catalog+jwt`，独立 audience、RSA 签名私钥和 DB 登录组 `brokerrouter_catalog`。BFF 启动拒绝配置与报表复用 RSA 模数或 audience；配置公钥绝不能添加到 HUMAN/审批信任集合。Agent Key、报表证明、审批证明不能代替配置证明。BFF 证明最多 30 秒，绑定真人/租户/POST/精确 URI/正文；浏览器没有签名私钥或服务证明。

两项配置权限通过运维 CLI 显式授予，Web 无自授权入口：

```sh
brokerrouter console-permission-set --tenant <租户UUID> --user <治理用户UUID> --permission catalog_read --enabled true --reason '允许查询模型配置'
brokerrouter console-permission-set --tenant <租户UUID> --user <治理用户UUID> --permission catalog_write --enabled true --reason '允许管理已批准供应商端点'
```

`audit_read` 仍独立授权。SQL 行锁将配置事务与租户/用户停用、读/写权限撤销排序，提交前再检查证明时效。BFF 在转发前、收到响应后检查会话。已发出的短期证明不是分布式取消令牌：退出/断网可能让用户看不到已提交结果，不能据此推断回滚或自动另起操作。

## 运维预登记连接

网页不接收上游 URL、环境变量名或密钥。运维向配置服务注入 `BROKERROUTER_CATALOG_CONNECTIONS` JSON 数组，以下均为占位值：

```json
[
  {
    "tenant_id": "11111111-1111-4111-8111-111111111111",
    "id": "approved-provider-beijing",
    "name": "已批准供应商 · 北京",
    "base_url": "https://provider.example/compatible-mode/v1",
    "credential_env": "BROKERROUTER_PROVIDER_BEIJING_KEY",
    "allow_private": false
  }
]
```

配置服务只有**引用名**；真正 Key 只注入模型运行服务，不注入 BFF/报表/配置服务。运维必须核实区域、HTTPS 地址、供应商与该引用专用于此供应商，不能引用数据库/OIDC/签名等秘密。登记按租户隔离，租户内 ID 唯一；最多每租户 100 个、总共 2000 个、JSON 1 MiB（还受部署系统环境变量限制）。启动验证全部条目；`[]` 可管理既有配置，但不能新增供应商端点。私网/HTTP 只能运维显式批准，仍须配合出站网络与数据面 SSRF 防护。

页面仅收到连接 ID、名称、内容摘要；创建必须带该摘要，部署期间登记变更返回 `configuration_conflict`。端点保存 URL/凭据引用快照，不可变关联保存登记 ID/名称/版本。修改白名单只影响后续创建，不改写已有端点；撤销已有供应商必须显式停用端点/模型并轮换运行服务凭据。历史 CLI 端点显示“无登记快照”，不推断供应商归属。

## 写入合同、精度与恢复

写入正文为 `operation_id`（UUID）、非空且最多 500 个 UTF-8 字节的 `reason` 和封闭的 `change`，拒绝未知字段；名称/模型文本最多 200 个 UTF-8 字节、不能有控制字符：

- `model_create`：ID、名称，默认停用。
- `model_update`：ID、预期版本、名称、启停；ID 不可原地替换。
- `endpoint_create`：模型、登记连接/摘要、名称、上游模型、Chat/Embedding 合同、上下文/输出上限、优先级、CNY/USD 和输入/输出每百万 Token 价格。默认停用；不会替 Key 授权。
- `endpoint_update`：UUID、预期版本、名称、启停、优先级、币种与价格。连接、上游模型、能力/向量维度不可原地替换；新建替代端点后停用旧端点。

模型/端点每次更新（含 CLI）由数据库推进 `config_revision`。旧版本 409，不自动覆盖。配置、审计前后值/真人/理由/操作号、幂等回执同事务提交，任一步失败全部回滚。同租户/用户/操作号的相同内容返回原回执，不重复创建/审计；不同内容 409。回执保持不可变，未来归档也必须保持去重墓碑和审计完整性，不能通过清理让旧操作重新可执行。

页面先显示变更摘要，再明确“确认提交配置”；准备/提交期间禁止并行配置表单和切换模型列表。失败不自动重试，手动重试复用原内容与操作号，冲突必须重新核对。草稿/待确认操作不进入浏览器持久存储；关闭/刷新/后退后先按对象查询配置及审计，不能因没有回执重复新建端点。移动端可能不触发离开提示，该提示不是正确性保证。

金额用十进制字符串、服务端 Decimal，价格 0–10 亿、最多 12 位小数，人民币为账本本位币。USD 必须有生效汇率，否则数据面拒绝准入；配置价不是供应商实付账单认证。停用/改价影响之后重新读取配置的准入，已准入请求的价格/汇率/路由快照不变，不取消在途收费或副作用。

复用原 `store::insert_endpoint` 事务校验同逻辑模型 operation 与向量维度。维度相同不证明向量空间相容，真实工具/上下文/计量能力保留 LIVE-01/02。没有隐式供应商探测/收费调用按钮。

## 查询与审计

模型搜索与端点 keyset 分页每页最多 100；页面分别 20/30 行。审计页为已有只读 API，31 天以内 UTC/北京时间窗、动作/对象精确过滤、30 行微秒游标。审计 ID、版本、游标均为字符串。审计视图仅增加四种配置动作的真人 UUID、操作 UUID、理由、版本，不开放任意 `details`、正文、密文或凭据引用。`database_actor` 明确是数据库执行身份，不冒充最终操作人。

配置 HTTP 10 秒、8 并发、16 KiB 正文、SQL 5 秒/锁等待 1 秒，无网络访问处于配置事务中。no-store、CSP、固定路径、静态错误映射保持，管理页按需加载，无新增依赖。

## 发布与运维

1. 维护窗口暂停/排空入口，迁移身份应用 `0013`，DBA 应用 `deploy/permissions.sql`。精确 schema 检查使旧二进制在新 schema 上不就绪，不直接混跑/降级事实。
2. 专用 `brokerrouter_catalog` 登录运行 `serve-catalog --bind 127.0.0.1:8088`，不附加 runtime/operator/human/console 组。该组能管理模型/端点和读取引用，不能读 Key 哈希/请求密文、改预算、批准或自授权。配置服务是受信应用边界，DB 凭据不能分发给网页管理员。
3. 配置服务环境：`DATABASE_URL`（验证服务器 TLS）、`BROKERROUTER_CATALOG_ISSUER`（BFF 公开 origin）、`BROKERROUTER_CATALOG_AUDIENCE`、`BROKERROUTER_CATALOG_PUBLIC_KEYS`（kid→RSA 公钥 PEM）、`BROKERROUTER_CATALOG_CONNECTIONS`。
4. BFF 新增：`BROKERROUTER_BFF_CATALOG_ORIGIN`（固定 HTTPS 内网入口）、`BROKERROUTER_CATALOG_AUDIENCE`、`BROKERROUTER_BFF_CATALOG_SIGNING_KID`、`BROKERROUTER_BFF_CATALOG_SIGNING_PRIVATE_KEY`。全未设则关闭代理，部分设置拒绝启动。origin 不接受路径/查询/账号密码，不跟随重定向、不使用代理或自动重试。8088 HTTP 仅为 TLS 终止后的内侧监听，不裸露公网。
5. 配置目标加入 BFF 指纹，启用/更改 origin、audience、kid 会重新登录；本版扩展指纹也使旧会话失效。签名 keyring/kid 独立轮换。匹配发布 UI/BFF/报表/配置版本，保留旧静态资源/备份；回滚前验证迁移兼容，不撤销已生效配置或审计事实。
6. 监控静态错误码、409/权限拒绝、锁等待、审计/幂等表增长、TLS/签名轮换、跨副本一致性。网络只允许 BFF 访问配置服务，配置服务只访问 DB，BFF 无供应商出站权限。共享 NAT/容量限制必须按预生产实测调整。

本地 DB/HTTP、浏览器/edge 协议夹具不是生产验收。真实 IdP/MFA、浏览器→BFF→配置服务、成员撤权、跨副本/轮换、角色/TLS/网络、迁移恢复、数据规模与人工无障碍由 LIVE-15 阻断发布，无需提前索取供应商 Key。
