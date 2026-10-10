# R1 原生产物接收与部署验收（2026-10-11）

原调用的真实产物字节现在可以进入独立私有接收服务：原 claim 绑定、容量预留、不可变 manifest、分块加密存储、全文件摘要验证和过期清理已通过 PostgreSQL/HTTP 回归。产物保持隔离状态；MP4 声明仅是候选格式，扫描、本人审阅、当前授权下载和原生宿主隔离仍待 B、C 包完成，本报告不将接收功能认定为完整二进制发布认证。

## 接收与身份边界

`serve-assets --runtime-output` 使用独立 `brokerrouter_runtime_assets` 用途。它只允许原工具 claim 预留和写入自己的候选产物；manifest 与原 completed receipt 同事务记录，失联重试恢复同一事实，不领取新工具执行权。每次 complete 从真实存储重新解密并核对每块及全文件 SHA256，不以数据库 available 标记代替字节验证。候选总大小最多 64 MiB、最多 8 件、每块最多 4 MiB。

输入资产 API 不接受保留命名空间，也不能读取新 `runtime_output`。接收身份不能调用模型、读取输入媒体、提交扫描结论、审批或下载。过期释放预留容量、清理密文，保留原 claim、manifest 和 tombstone，重放不续期。客户连接断开后，已领取的后台写入仍完成并结算自己的存储事实。

## 实际验证

| 验证组 | 实际结果 |
| --- | --- |
| 私有接收 PG/HTTP/用途权限 | 11 通过：原 claim、并发与不可变事实、全文件与存储篡改、回执丢失、断连后台归属、到期审计回滚、用途角色正反边界 |
| 共享资产与既有媒体输出 | Assets 10 通过、1 项既有环境专项 ignored；Outputs 44 通过、3 项既有环境专项 ignored。本组未宣称执行这些环境专项 |
| 独立升级、配置与 Compose | 8 通过，含真实 SIGKILL 后恢复新密码、旧 pending v1 恢复、部分/篡改状态拒绝、实际可选 profile 与独立配置 |
| 实际 TLS/SCRAM PG | 2 通过，覆盖新 20 用途正反权限矩阵及旧 19→显式 20 升级 |
| 实际 provisioner 镜像与升级 | 最终组合 3 通过，覆盖新装、角色冲突回滚、拒绝明文管理员连接、旧 19 + 独立 v2 只读挂载升级和幂等恢复；其中 host TLS 升级与上一组重叠，不叠加为独立场景数 |
| 实际 Linux 配置加载器 | 原 14 组件及新 runtime-assets 用途通过；非法用途材料、CA 覆盖、NUL/UTF8/边界/符号链接均失败关闭 |

运维镜像 `brokerrouter-provisioner:runtime-artifacts-20261011` 的实际 Docker image ID/RepoDigest 为 `sha256:e37fe4c7134e32f6ca6a74b7627e0ddea8a447534035c74b1d0fe0518cbd785d`，构建 config digest 为 `sha256:58d30aefacb7d745da2e79d481552c733722a13da0dcfbafcd3c1be43f5eb98f`。容器内实际 `purpose-logins.py` SHA256 与已验收源码相同：`3812d1eaa736b51db191e7fe5126b46fc06eb10bf3a825c0c389d01a14eecef7`。此镜像是用途注册器，不是完整网关运行镜像。

## 旧部署的明确升级

新装生成 version 2 的精确 20 用途；已发布 version 1/19 的 create/default apply 保留原集合。显式升级将原 57 份 Secret 原字节复制到独立 `purpose-logins-v2`，只生成新 runtime-assets 密钥，旧目录及 manifest 不变。真实 SIGKILL 在新密码持久化后恢复同一密码；SQL 同事务核对原 19 个注册事实并新增第 20 个，不轮换已有身份。实际只读容器挂载命令也通过验证，操作入口见 [用途身份说明](../deploy/production/purpose-logins.md)。

runtime-assets 配置独立发布，保留原 14 组件配置。可选 `runtime-artifacts` profile 不公开主机端口，使用 backend 入口与既有 egress 访问固定 S3 HTTPS 端点及其专用 CA；无任意 Agent URL。默认 14 服务和原 48/80 数据库连接预算保持不变，开启 profile 增加 2 个连接。

所有测试仅创建本任务隔离 tmpfs PostgreSQL/注册器容器与合成 Secrets，并完成容器和临时凭据清理；未触碰生产 Secret 或付费供应商。最终执行前 `df` 为 329 GiB 可用，没有复制 target、虚拟环境或依赖缓存。精确九文件摘要、实际日志 SHA 和镜像信息见 [证据索引](evidence/r1-runtime-artifact-deployment-2026-10-11.json)。

## 保留的验收条件

B 包须用固定扫描引擎读取这些真实 MP4 字节，绑定原 source operation、文件摘要、扫描 profile/report 与当前策略；未知或风险内容须完整隔离审阅。对具体文件的批准不能变成同回合其他文件的批准，每次下载仍须检查现行身份、策略、扫描版本及真实期限。

C 包须建立 producer/broker 与 Agent 不共享产物读取挂载的真实隔离，接入固定 typed producer、原生 capture 和受控附件 resolver。0700 同 UID 目录、JSON metadata、宿主路径或已批准文本均不代表二进制隔离。目前文本包继续拒绝附件路径和媒体标记，未开放未经认证的原生附件发布。R2/R4 图片、3D 和其他供应商范围继续保留。
