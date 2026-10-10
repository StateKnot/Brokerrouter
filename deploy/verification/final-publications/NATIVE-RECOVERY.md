# 同版本的策略升级与核心重启验收

使用 `NATIVE.md` 的相同命令参数，将执行脚本替换为 `native_recovery_acceptance.py`。它完整重跑已冻结的原生审批恢复链路，然后发布真实策略 V2，验证旧 grant 被拒绝且只返回新的待审批元数据。独立 HUMAN 控制器再次完整审阅并批准同一来源与候选正文，检查现行身份、审批期限和两个 grant 的来源/轮次期限边界。

随后实际重启固定核心容器，通过真实 HTTPS readiness 后读取同一个 output operation 的完整结果，再启动全新的完整 Hermes 进程。最终要求模型 HTTP/SSE 各一次、原生历史两行、隔离接收器一次、数据库 requests/publications/approvals/grants/finalize 分别为 1/1/2/2/1。任何正文或凭据进入日志即失败。

这仍是合成签名身份和本地接收器验收；不构成真实组织 IdP、外部消息通道或完整 R1 认证。原 12 组和历史 schema31 的 16 组证据保持独立，实际新数量只以新项目的 `native-manifest.json` 为准。
