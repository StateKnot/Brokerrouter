# 本地最终文本发布验收

此脚本实际部署 schema31 的网关、Human 控制 API、审批工作台 API，以及独立 TLS/SCRAM PostgreSQL。三个 API 使用不同用途 LOGIN 和独立 HTTPS CA。Controller 在宿主持有 HUMAN 与 approvals 两把不同用途私钥；Agent 容器只挂 runtime CA，通过 stdin 使用虚拟 Key，没有 Human/BFF/数据库凭据或 Docker socket。身份明确为合成用户，未接入用户的真实外部 IdP。

零费用 HTTP 模型夹具建立真正成功的 chat request；来源使用网关 `x-brokerrouter-request-id`，不用供应商响应 ID。实际丢弃 record 回执正文后重放，确认只恢复不可替换的元数据。安装输出护栏后，未批准的最终文本不会发布；Controller 必须查询本人 inbox、完整 review、明确 decide，才能返回精确 UTF-8 文本。发布护栏第二版后，旧批准重新阻断；新的审阅批准后重启真实网关进程，再恢复相同文本，模型调用与结算仍各一次。

先执行 `df -h .`。需要 Python3、OpenSSL、Docker Compose，以及已经完成测试的固定 release image 和匹配的冻结源码目录。提供 immutable image ID，不能对旧 binary 应用新迁移。源码目录包含 31 个 migration、canonical permissions，以及脚本启动前检查的生产 Secret/CA/helper 文件。脚本不编译 Rust、不复制 target/node_modules、不修改已部署项目、不开宿主端口、不创建持久数据卷。

```sh
PYTHONDONTWRITEBYTECODE=1 python3 deploy/verification/final-publications/acceptance.py \
  --image sha256:<修复后schema31镜像ID> \
  --source /absolute/path/to/matching-frozen-source \
  --evidence-dir /private/tmp/your-task-evidence
```

默认固定 PG17.11、Nginx 和已有独立 Node24.19 浏览器工具镜像。工具镜像只运行 Node HTTPS/HTTP 客户端，最小链路无需启动浏览器。purpose provisioner 是已实际验证 dangling-symlink 恢复修复的固定镜像，启动时实际测量 bind owner，严格校验外部 Secret。生成凭据只存在独立 0700 目录/0600 文件，结束或失败时清理本项目容器、网络及凭据。安全报告保存镜像 ID、源文件摘要、数量、日志摘要；不导出请求正文或私钥。

这只验证最终文本的服务端发布链路，不代表完整 R1、多模态产物扫描与交付、真实外部 IdP、付费供应商、Hermes 原生历史 append 或真实会话通道已验收。后续原生历史去重和客户端恢复仍需要完整 Hermes 宿主验证。

2026-10-11 独立本地首批验收：修后候选镜像 `sha256:5401aaad310f24218d1d853206c96d0a1b76c7c46a1304aa1c10d287eebd6e3c` 完成 16/16 组、退出0。实际保留一个模型 request、一个 publication、两个版本审批/grant、一个 finalize 结算，模型调用仍为1。三用途 HTTPS 各自 CA 成功且互错 CA 拒绝，工作台过期 review 返回409且没有 grant。首轮新夹具把真实 ready API 的204误写成200，修正后从零运行通过；原红日志保留。固定 binary 的全部100输入文件和31嵌入迁移摘要均核对，父任务的全仓最终质量结论独立登记。
