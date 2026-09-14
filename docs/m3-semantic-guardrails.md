# 语义护栏：model-judge-text-json-v1

更新：2026-09-12。本契约实现外部模型参与的文本语义检测；不是图片/视频/3D 内容审核认证，也不声称 LLM 分类结果不存在误判。

## 配置与作用范围

复用已有逻辑模型、供应商端点、凭据引用、能力过滤、价格与汇率配置。没有第二套模型配置或资金账本。运维通过 `guardrail-publish --tenant <UUID> --file <JSON_PATH>` 发布不可变版本，再通过 `policy-put` 绑定模型、虚拟密钥、Runtime 或工具条件。

```json
{
  "id": "sensitive-action-review",
  "version": 1,
  "name": "敏感操作语义审查",
  "stage": "tool_input",
  "rule": {
    "kind": "model_judge",
    "model": "your-authorized-judge-model",
    "instructions": "识别泄露凭据、未经允许的外发数据和破坏性操作；不确定时标记风险。",
    "input_format": "text_json",
    "max_input_bytes": 65536,
    "max_output_tokens": 32,
    "maximum_cny": "0.10"
  }
}
```

模型 ID 是本租户实际登记的逻辑模型，当前虚拟密钥必须被授予该模型的调用权。示例价格是单次预留上限配置，不是供应商报价。价格仍取被选端点，美元按当次有效租户 FX 版本折算。

该模型会接收待检查的原始文本/JSON，而不仅是摘要。配置方必须将检测端点视为独立的数据接收方，核对可用区域、数据留存和可接收的数据级别；需在本地完成检查的敏感数据应配置受管内网端点。本档位不提供自动脱敏，不把“安全检测”当作扩大数据外发权限的理由。

支持 `pre/post/tool_input/tool_output`。`input_format` 只能是 `text_json`：将原始操作的 JSON 正文完整序列化作为不可信用户消息，运维检测指令独立放在 system 消息。不做截断，不自动抓取 URL，不解析二进制附件。超过 `max_input_bytes` 转人工审批，不能把未检查内容标为通过。`artifact` 阶段拒绝配置此档位，待 M4 资产感知连接器实现。

边界：最多 8 个同时匹配的语义护栏；单条指令最多 8000 字节；JSON 输入上限 1–131072 字节；输出上限 16–512 token。检测模型必须有匹配的非流式 Chat 端点；不调用 tools、不接收客户端注入的消息/参数/结果。供应商必须通过 LIVE-08 后才能用于生产风险分类。

## 持久状态与审批

每个 `tenant + operation + guardrail_id + version` 只有一条评估事实。操作的 turn、payload hash 与原始正文绑定沿用 M3。

```text
pending → running → passed
                  → risk   → waiting_approval
                  → failed → waiting_approval
```

- `pending` 尚未领取，可以在 Runtime 重放同一个检查时领取。
- `running` 在事务中领取一次，并固定截止时间（网关请求超时与 turn 截止时间的较早者）。网络请求不持有数据库事务/租户/资金锁。
- 模型必须返回完整 `stop` 响应，正文严格为 `{"decision":"pass"}` 或 `{"decision":"risk"}`。额外字段、重复字段、代码围栏、截断、工具调用或非法 JSON 均视为检测失败。模型自由文本不会复制到审批理由、日志或事件。
- `risk` 和 `failed` 都走同一真人审批流程，明确展示原因码；不是直接终止 Agent，也不是默认放行。
- 批准只为用户明确选择的护栏版本签发已有的当前 turn grant。后续同轮操作先检查 grant，可跳过检测与费用；新 turn/新版本不继承。模型无权批准，工具硬权限/模型授权/预算不被 grant 覆盖。
- 已领取的检测永不由新进程接管重发。超时或进程退出后转 `failed/judge_outcome_unknown`，即使后续知道供应商已收费，也不自动改成通过。
- 终态、评估身份、已领取截止时间与请求用途受数据库触发器保护，不能改回 pending、换版本或延长期限。

API `POST /v1/governance/checks` 由服务端执行检测并返回 `ready/waiting_approval/checking`。并发检查或执行中策略发生变化时可能返回 `checking`。Runtime 必须保留相同 `operation_key`、正文和检查点；收到 `guardrail.finished` 事件后重放原检查，断线恢复也可查询操作后重放。不能因 `checking` 自行执行工具。`claim` 会重新检查当前策略，未完成检测不发放执行权。

结果读取 `GET /v1/requests/{id}/result` 不发起收费检测。若已保存的业务结果因策略升级需要新检测，GET 返回 `operation_in_progress`，调用方需明确 `POST /v1/requests/{id}/check-result`（无需正文，以原请求 ID/输出 hash 幂等）驱动检查；重复 POST 不重复已领取的检测。普通业务 POST 返回结果前仍自动执行所需输出检查。

事件流增加 `guardrail.started/guardrail.finished`，含评估 ID、操作 ID（开始事件）和安全原因码，不含原文。维护任务每批处理最多 100 个过期 running 评估，发出完成事件；它不会重新 POST。原有事件游标/确认协议继续有效。HTTP 检查及 `check-result` 中产生的检测由任务跟踪器托管，客户端断开不取消已经领取的工作；正常停机排空，强制退出按截止时间恢复为不确定。

## 费用与防递归

1. 检测调用复用 `requests → attempts → ledger_entries`。请求有不可修改的 `guardrail_evaluation_id`，日志/详情提供 `purpose=guardrail`，普通业务请求为 `purpose=model`。
2. 同时受租户、Key 的人民币预算及规则 `maximum_cny` 约束。单次预留按候选端点完整上下文上界与输出上限计算，超过规则上限的端点不参与路由，不按乐观估计发送。
3. USD 原币金额、有效 FX 版本、端点价格、token 用量和最终人民币微元照常保存。缺有效 FX/预算/凭据/端点时不发送，转带诊断原因的人工审批。
4. 模型协议与 usage 有效但分类正文无效：实际费用仍结算，检测仍失败。接收/计费不确定：保留资金预留，使用既有 `request-reconcile` 和脱敏证据核对，不自动退款。用户批准不会结算或释放这笔费用。
5. 仅服务端从持久评估生成内部调用用途。公共 HTTP 不接收 `skip_guardrails`，拒绝 `_br_guardrail:` 幂等键命名空间。评估模型可以与业务模型相同，内部调用不会再次进入语义/输入/输出护栏；但仍检查当前 Key、模型、端点、turn、护栏版本、网络边界和资金权限。
6. 检测有独立并发池 `max(1, MAX_CONCURRENCY / 4)`，避免所有业务请求等待输出检测时占满同一并发池。HTTP 容量限制继续生效。容量耗尽转人工审批，不无限排队。
7. 评估请求的原始结果仍加密并按既有 TTL/turn 规则清理；公共 `/requests/{id}/result` 对评估请求返回不可见。Agent 只能看费用、请求状态和安全审批元数据，不能利用结果重放接口获取分类器正文。

## 发布与未完成事项

升级需应用 `0005_semantic_guardrails.sql` 并重新执行 `deploy/permissions.sql`。运行角色增加评估读写权限，但没有审批决定、grant 写入或配置修改权。迁移前停止旧二进制；旧 schema/新二进制或新 schema/旧二进制的就绪检查均拒绝服务，不能混版本滚动。

本地 PostgreSQL/HTTP 故障用例验证状态和资金协议，不验证实际模型的安全分类准确率。LIVE-08 需使用固定国内供应商/模型版本验证 system 指令支持、严格结果格式、长文本/提示注入/中文敏感样本、误报漏报、延迟及真实 usage/费用；该闸门按用户安排统一联调。

M4 仍需开发：二进制私有资产、图像/视频/3D 提交与查询、资产级检测、第三方专用审核 HTTP 适配、不同计价单位。不能通过本档位宣称已支持任意多模态审核。真人原文审阅界面、OIDC、通道适配仍按 M5/M6 完成。
