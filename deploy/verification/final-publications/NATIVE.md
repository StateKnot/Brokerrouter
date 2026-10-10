# 完整 Hermes 宿主的最终文本部署验收

本脚本独立创建随机 Compose 项目，使用显式配对的固定核心镜像和 schema 版本、TLS/SCRAM PostgreSQL、20 个用途 LOGIN 和独立 runtime/control/approvals HTTPS。它不复用历史 16 组 schema31 验收的数据库或凭据。

需要 Python3、Docker Compose、OpenSSL、已有固定 provisioner/Node/PG/Nginx/Hermes 镜像，以及与核心镜像匹配的源码输入和已经完成安装校验的完整 Hermes 源码。开始前读取 `df`；不复制构建缓存、依赖目录或虚拟环境。

部署摘要必须显式包含 `deploy/permissions.sql` 及 `deploy/production` 下的 `purpose-logins.py`、`create-purpose-secrets.sh`、`prepare-purpose-admin.py`、`pg_hba.conf`、`control-plane-nginx.conf`。编译源码摘要不一定包含这些运维输入；缺文件时在生成任何凭据之前失败。补齐运维快照时保留原编译摘要，单独记录新增文件来源与 SHA256。

```sh
PYTHONDONTWRITEBYTECODE=1 python3 deploy/verification/final-publications/native_acceptance.py \
  --image sha256:<固定核心镜像ID> \
  --schema-version <镜像内实际迁移版本> \
  --source /绝对路径/匹配镜像的冻结部署源码 \
  --host-source /绝对路径/已安装的完整Hermes源码 \
  --stage-script deploy/verification/final-publications/native_stage.py \
  --host-manifest /绝对路径/严格宿主安装摘要.json \
  --core-manifest /绝对路径/核心镜像实际编译输入.json \
  --deployment-manifest /绝对路径/冻结部署源码摘要.json \
  --evidence-dir /绝对路径/新的仓库外证据目录
```

`native_stage.py` 调用实际 GatewayRunner、TurnRunner、主模型 SDK、SessionDB 和 Base 发送账本；运行身份为当前非根 UID/GID，容器 HOME 仅指向自身 Agent 状态目录。只有 runtime CA、虚拟 Key、绑定信息和自身加密状态进入 Agent。控制器在独立主机进程签署 HUMAN 和 approvals 用途请求；Agent 没有审批密钥、数据库凭据或 Docker socket。运行前逐字核对安装摘要和实际编译迁移；运行后确认 Python 源码未变化。测试 CA/leaf 明确包含严格 X.509 所需的用途扩展，不关闭 Python 的默认严格证书校验。

`native_provider.js` 是零费用真实 HTTP 模型夹具，支持完整 OpenAI JSON 和 SSE stop/usage/DONE。`native_ack_edge.js` 是仅用于本测试的故障夹具：固定转发到 gateway，收到成功 record 的完整正文后，保存无正文的 ID/hash/长度事实，再销毁真实客户端 socket。它不改变生产代理或宿主 transport，也不会重试请求。

预期路径为：创建真实原生会话 → 真实模型请求 → 已提交 record 的回执丢失 → 普通历史及正文发送均为零 → 发布当前 post 护栏 → 新宿主进程恢复候选 → 控制器完整审阅并明确批准 → 两条原生 user/final 历史与一次实际隔离接收确认 → 再启动进程，模型、历史和发送均不重复。

合成身份和隔离本地平台接收器不构成外部组织 IdP 或真实外部消息通道认证。该包也不证明完整控制面、供应商计费或多模态部署通过。实际检查数量以新的 `native-manifest.json` 为准；失败日志保留，修复后必须使用新的证据目录和新项目。结束后清理本包生成的凭据、Agent 状态、容器和网络，保留公开源文件与经过脱敏的日志。
