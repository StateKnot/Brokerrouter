# Hermes 原生最终文本恢复验证

2026-10-11，在固定 schema32 核心镜像、20 个独立用途数据库身份和 19 文件宿主 overlay 上实际完成 17 组验证，耗时 190.43 秒。使用真实 GatewayRunner、TurnRunner、模型 SDK、SessionStore 和 Base sender；模型是本地零费用 HTTP/SSE 接口，本人是独立 purpose-signed 合成身份，接收器是隔离本地通道。

登记已提交但 TCP ACK 丢失时，普通历史和发送正文均为零。本人经实际 workbench 完整审阅、审批后，新的宿主进程恢复同一冻结结果，写入两条普通历史并发送一次；再次重启没有新增模型、历史或发送。

新增输出规则版本 2 后，旧 grant 阻断原正文；新的完整审阅和决定绑定现行身份、规则和同一 output operation，grant 截止时间不超过原来源和 turn。实际重启核心容器，核对 StartedAt 改变及真实 HTTPS ready，再读取同一 operation、同一正文；最后新的宿主进程保持零新增副作用。

最终数据库计数：模型 request 1、publication 1、approval 2、grant 2、费用 finalize 1；上游 HTTP/SSE 各 1、本地发送 1、普通历史 2。项目容器、测试凭据和 Agent 状态已清理。安全证据绑定 103 个编译输入、130 个部署输入和七个验证文件，见 [冻结摘要](evidence/hermes-native-recovery-2026-10-11.json) 与 [可重复步骤](../deploy/verification/final-publications/NATIVE-RECOVERY.md)。原先 12 组同版本验证和 16 组 schema31 验证各自保留，不合并数量或继承认证。

103 个核心编译输入及七个验证文件与本次源码一致。130 个部署快照中 127 个与当前源码一致，三个生产 PKI 脚本保留修复前版本，其 SHA 差异在摘要中明确记录；原生验证自行生成严格 CA，不能据此认定新生产 PKI 脚本已经部署认证。新脚本的独立实际客户端验证见 [PKI 记录](r1-pki-strict-clients.md)。

本包证明原生最终文本的局部恢复能力。真实组织 IdP/MFA、外部通道、原生文件完整集合与文字共同释放、全部控制面编排及容量验收仍未完成，R1 保持开放；固定宿主镜像也不声明为整镜像官方提交。
