# 控制面用途登录与 Secret 基础

本包提供可新装、可重跑的 19 个专用数据库登录。它不修改既有 `generate-secrets.sh`、`postgres-init.sh`、核心 Compose 或 runtime/operator/migration 凭据，不部署真实 IdP，不生成虚构身份或 Agent 审批钥。新服务还需其用途配置、独立签名/密钥环、真实外部依赖与完整编排验收。

部署宿主须有 Python 3.9+ 标准库、POSIX `flock` 支持及 PostgreSQL 16+ 的 `psql` 客户端；仓库锁定生产 PostgreSQL 为 17.11。无需新增 Rust、Python pip 或系统密码学依赖。验证夹具另需 Docker 与 OpenSSL。

## 小接口

```sh
# 目录必须在仓库之外，absolute/canonical，归当前部署身份且 mode 0700。
deploy/production/create-purpose-secrets.sh /opt/brokerrouter/secrets

# 已完成现有迁移、permissions.sql、TLS/SCRAM-only HBA 后执行。
# PostgreSQL service 文件与 passfile 为部署身份所有，mode 0600。
deploy/production/apply-purpose-logins.sh /opt/brokerrouter/secrets \
  /opt/brokerrouter/ops/admin.service provision
```

service 文件仅包含明确目标与凭据文件引用，不能写 `password=`、未校验 TLS 或任意连接选项。以下是格式说明，不是可直接发布的真实连接配置：

```ini
[provision]
host=postgres
port=5432
dbname=brokerrouter
user=brokerrouter_migration
sslmode=verify-full
sslrootcert=/opt/brokerrouter/secrets/pg-ca.crt
passfile=/opt/brokerrouter/ops/admin.pgpass
connect_timeout=5
```

实际宿主运行 CLI 时，`host` 必须能解析到正式专用数据库，CA 路径属于该 CLI 的文件系统；新应用 `.env` 中的 `postgres` 和 `/run/secrets/pg_ca` 是现生产 Docker 网络与 Secret 挂载契约。不要把应用 DSN、密码、SCRAM verifier 放 CLI 参数、普通日志或 Git；libpq passfile 由 Secret 系统创建并保持 0600。脚本清除继承的 `PG*` 环境，禁用 `.psqlrc` 和密码交互，只使用显式 service 与 passfile，以 `verify-full` 连接；不支持直接执行生产 DB 的隐式默认目标。

## 登录清单

| purpose 文件名 | 独立 LOGIN | 唯一直属用途组 |
| --- | --- | --- |
| control | brokerrouter_control_login | brokerrouter_governance_control |
| console | brokerrouter_console_login | brokerrouter_console |
| catalog | brokerrouter_catalog_login | brokerrouter_catalog |
| settings | brokerrouter_settings_login | brokerrouter_settings |
| approvals | brokerrouter_approvals_login | brokerrouter_approvals |
| media-review | brokerrouter_media_review_login | brokerrouter_media_review |
| bff | brokerrouter_bff_login | brokerrouter_bff |
| assets | brokerrouter_assets_login | brokerrouter_assets |
| output-worker | brokerrouter_output_worker_login | brokerrouter_media_outputs |
| output-scan | brokerrouter_output_scan_login | brokerrouter_output_scan |
| output-delivery | brokerrouter_output_delivery_login | brokerrouter_output_delivery |
| media-api | brokerrouter_media_api_login | brokerrouter_runtime |
| media-worker | brokerrouter_media_worker_login | brokerrouter_runtime |
| mcp-judge | brokerrouter_mcp_judge_login | brokerrouter_runtime |
| mcp-catalog | brokerrouter_mcp_catalog_login | brokerrouter_mcp_catalog |
| mcp-client | brokerrouter_mcp_client_login | brokerrouter_mcp_client |
| mcp-server | brokerrouter_mcp_server_login | brokerrouter_mcp_server |
| mcp-worker | brokerrouter_mcp_worker_login | brokerrouter_mcp_worker |
| reconciler | brokerrouter_reconciler_login | brokerrouter_reconciler |

reconciler 保留现有权限组对 runtime 的必要继承，不以“唯一直属组”假称没有间接继承。其他用途的递归 membership 集合必须与清单一致；角色的 INHERIT/SET/ADMIN 选项均实际检查。各登录 INHERIT、NOSUPERUSER/NOCREATEDB/NOCREATEROLE/NOREPLICATION/NOBYPASSRLS，只有数据库 CONNECT 与用途组权限；没有 schema CREATE、数据库 CREATE/TEMP、额外直接 ACL、对象所有权或其他角色管理权。每登录连接上限 8、statement/lock/idle-transaction 超时为 15s/3s/60s；全服务连接池与 PostgreSQL max_connections 的总容量仍须编排时明确分配并压测，不能将本包权限测试作为容量认证。

脚本不新建或改写既有 NOLOGIN 权限组。对其特权标志、对象所有权、递归继承及审批决定/资产分块/迁移读取等关键权限矩阵失败关闭；要求先运行当前 `deploy/permissions.sql`。这不是对所有可能新增对象授权的独立全面安全审计；后续 schema/权限变化仍须附带其正反验收并更新契约。

## 文件、发布与引用

`purpose-logins/` 为 0700，每个 `<purpose>.password`、`<purpose>.scram`、`<purpose>.env` 及 manifest 为 0600，均归部署身份。密码由系统随机源产生 32 字节，SCRAM-SHA-256 4096 次由标准库生成并反算验证。每个 `.env` 只含该登录的 TLS DATABASE_URL；不复制 result keyring、S3、模型、IdP 或其他用途签名。manifest 及 SCRAM 是受保护部署状态，只归运维，不能挂载给应用。

后续 Compose 每个 workload 只挂载自身 `<purpose>.env`，再分别引用部署管理的用途配置 Secret：control 的 HUMAN 公钥与治理 keyring；BFF 的真实 OIDC client、独立 AES 与五用途 RSA；各 console/catalog/settings/approvals/media-review 的独立 verifier；资产/output 组件的合适结果/资产 keyring 与各用途最小 S3 IAM；worker 的选定模型凭据。现有根 `mcp-judge.env` 保持原值；新 `purpose-logins/mcp-judge.env` 只有新数据库身份，不能直接代替带 keyring 的工作负载配置。

生成器只向 `.purpose-logins.pending` 写新文件，经逐文件 fsync、硬链接无覆盖提交、manifest 完整验证，再原子发布目录；外部目录上的非阻塞文件锁串行所有本包 create/apply。进程在发布前退出时，新服务无可用完整目录；重跑会保留已完整提交的密码，清理本包未提交的 `.writing-*` 临时文件并继续。有效现成集合仅核对后返回，不覆盖；hash/identity/mode/owner/symlink/额外文件异常均拒绝，不能通过重跑“修好”异常状态。

数据库事务用固定 advisory lock，登记私有 `brokerrouter_deployment.purpose_logins`，所有 CREATE/GRANT/登记一起提交。未登记同名 LOGIN、丢失已登记角色、已登记密码/flags/membership/ACL 变化都拒绝接管或重置；错误使整批回滚。未知事务结果时先保留 Secret 与注册事实，再重跑核对：已提交则核对，未提交则重新创建，不会重复轮换密码。此脚本不提供凭据轮换/迁移旧同名角色的万能恢复开关；正式轮换或迁移需单独受审流程。

server 只收到强 SCRAM verifier；连接先关闭标准 PostgreSQL statement/duration/error-statement 日志，psql 输出被捕获并不回显 SQL、密码或连接串，错误只给 SQLSTATE。若正式环境另外部署 pgaudit、代理或 SQL 捕获系统，须在其既有运维策略中落实该敏感管理步骤的 Secret 排除；不能假定客户端能关闭外部审计。普通日志仍应保留不含凭据的操作人、时间、发布标识与结果。

| 安全错误码 | 含义 |
| --- | --- |
| BR001 / BR002 / BR003 | 管理员/TLS/SCRAM、HBA 或 registry 所有权不符合契约 |
| BR010–BR013 | 用途组缺失/特权、递归混用、所有权或关键权限漂移 |
| BR020 / BR021 / BR022 | 未登记同名、登记后丢失或登记/Secret 冲突 |
| BR023 / BR024 | 已登记登录属性/继承冲突或额外直接授权 |

## 实际验收

```sh
df -h . /private/tmp
PYTHONDONTWRITEBYTECODE=1 python3 deploy/production/test-purpose-logins.py
```

测试只创建任务专用、无外部网络、512 MiB memory/256 MiB PGDATA tmpfs 的锁定 PostgreSQL 容器，使用真实迁移与权限 SQL；不挂 Docker 数据卷，不触碰其他数据库。验证新装、重跑、冲突回滚、递归/实际关键授权漂移、生成器中断保留密码、CREATE ROLE 后终止 backend 全批回滚、坏 manifest、symlink、并发锁、19 用途实际 TLS/SCRAM 连接及 SET ROLE/敏感表正反矩阵；最后删除自身容器与临时凭据。这里只认证本包，不认证完整 R1 编排、真实身份、供应商或恢复产品。
