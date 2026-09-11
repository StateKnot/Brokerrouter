# M3：持久护栏与人工审批契约

档位：`governance-v1`，2026-09-11。适用于 M2 的非流式文本调用，以及实现本文协议的受管 Runtime 工具/产物执行 hook。它不宣称普通 OpenAI SDK 会自动弹窗，也不宣称尚未接入的 Hermes/OpenClaw 已具备重启恢复能力。

## 1. 已实现的边界

- 独立真人控制面、受信用户与 Runtime/Agent/Key 绑定、权威 turn 创建与不可复开生命周期。
- 版本化护栏、租户策略并集，按模型、Key、Runtime、工具名的精确匹配条件选择护栏；不同条件是 AND，多条匹配策略是 OR/并集。
- 模型 `pre/post` 自动执行；Runtime 通过 `tool_input/tool_output/artifact` 检查具名动作。
- `always`（该动作总需人工确认）与 `contains_any`（有限文本字面词项）两种确定性规则。`contains_any` 检查 JSON 字符串值，支持大小写开关，不是语义内容安全分类器，不承诺检测编码混淆、谐音或多模态风险。
- 一张审批可以明确批准一部分护栏，未选部分继续请求审批。服务端 grant 绑定不可变 turn，turn 又绑定真人、Key、Runtime/Agent、session/run/代次；不引入可以复制到其他会话的万能豁免 token。
- 同一操作、同一策略快照与同一待批护栏集合去重；决定、grant、事件在一个数据库事务提交。重复相同决定返回既有决定，冲突决定拒绝。
- 加密隔离、结果重取、顺序事件/确认游标、单次工具执行准入、执行不确定及真人证据核对。

外部语义/多模态护栏服务连接器、自定义转换/脱敏器、规则错误转用户审批的外部服务场景尚未开放；不接受未知规则类型，也不以“已配置”伪装检测通过。后续连接器必须补齐超时、递归防护、费用归属与独立能力认证。

## 2. 身份与部署

| 进程/角色 | 持有内容 | 可以做什么 | 禁止事项 |
| --- | --- | --- | --- |
| Agent/Runtime | 专属虚拟 Key、turn ID、自己的检查点/执行凭据 | 调模型、请求检查、读自身审批/事件、受控 claim/receipt、关闭自身 turn | 创建/续期 turn、代用户批准、修改策略 |
| 数据面 `serve` / `brokerrouter_runtime` | 运行数据库账号、供应商 Secret、内容加密密钥 | 路由、记账、护栏检查和持久状态 | 数据库层无 grant 写入/审批决定/用户注册权限 |
| 用户 BFF/认证渠道 | 用户登录会话、仅 BFF 可用的 RSA 私钥 | 验证实际登录用户、CSRF/渠道回调、防重放交互，签名指定请求 | 私钥/用户会话不得放入 Agent 容器、工具环境或模型上下文 |
| 控制面 `serve-control` / `brokerrouter_governance_control` | 独立数据库账号、BFF 公钥、相同内容加密 keyring | 校验请求签名与当前用户归属、创建 turn、批准/拒绝/反馈、人工核对 | 无预算/消费修改权限，无供应商调用能力 |
| 运维 `brokerrouter_operator` | 每人独立数据库登录身份 | 注册用户/Runtime、发布护栏与策略、撤销身份 | 不把账号发给 Agent，不共享管理员 token |

迁移 0004 后重新执行 `deploy/permissions.sql`。数据面和控制面使用**不同进程、不同数据库登录角色**；两个登录角色不互相继承。控制面默认监听 `127.0.0.1:8081`，放在认证 BFF 内网之后；公网入口配置 TLS、请求体读取超时、源/用户速率限制与连接限制。数据面 HTTP 处理并发上限 64，控制面 32，分别在读取/解析正文前限制；供应商并发另由 M2 配置约束。容量不是 SLA，必须按 LIVE-07 实测。

配置：数据面沿用 M2 Secret；控制面需要相同 `BROKERROUTER_ACTIVE_RESULT_KEY` / `BROKERROUTER_RESULT_KEYS`，并额外注入：

```text
BROKERROUTER_HUMAN_ISSUER=https://your-trusted-bff.example
BROKERROUTER_HUMAN_AUDIENCE=brokerrouter-human-control
BROKERROUTER_HUMAN_PUBLIC_KEYS={"kid-2026-09":"-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----"}
```

公钥最多 16 把，通过 `kid` 平滑轮换；加入新公钥并重启控制面后才切换 BFF 签名私钥。撤销旧公钥后重启所有控制面副本。只接受显式配置的 RS256 公钥，不按请求的 jku/jwk/x5u 下载或信任密钥，不接受 HS256、普通 ID token 或 Agent Key。

## 3. BFF 签名请求档位

Header：`alg=RS256`、`typ=br-human+jwt`、已登记 `kid`。`Authorization: Bearer <JWT>`。

Claims 全部必需：`tenant_id`、`iss`、`sub`、`aud`、`exp`、`nbf`、`iat`、`auth_time`、`jti`、`method`、`path`、`body_sha256`、`interactive=true`。

- `body_sha256` 是**实际 HTTP 正文字节** SHA-256 的小写 hex，不是重新序列化后推测的哈希。`method=POST`，`path` 包含实际路径/查询串；BFF 签名后不得再改变序列化结果。
- JWT 有效期最多 300 秒，iat 允许未来 5 秒时钟偏差，nbf/exp 严格验证；真实登录认证时间不得早于 1 小时。BFF 应在用户明确选择后签发，不能因为模型声称“用户同意”就签发。
- 数据库按签名 `tenant_id + iss + sub` 找实际活跃用户；一个登录身份可以有多个租户成员关系，但不能靠客户端参数借用其他租户。用户归属和 Key 状态在事务内重新检查。
- `(issuer,jti)` 只能绑定同一个租户、用户、方法、路径和正文。重复完全相同的决定是幂等查询，不重签发权限。
- 此档位是 BFF 对**已经认证并发生明确交互**的证明，不是自行实现登录。实际 OIDC/BFF/消息渠道连接与凭据隔离必须完成 LIVE-05 后才认证。

运维流程：`governance-user-add` → `runtime-bind` → `guardrail-publish --file <JSON>` → `policy-put --file <JSON>`。绑定不换绑；更换用户/Runtime/Agent 使用新 Key，旧 Key 撤销。用户、绑定可由运维禁用 `active`；护栏升级仅追加大于当前版本的新版本，旧版本不可修改。

护栏文件示例（非语义合规分类器）：

```json
{"id":"external-send-review","version":1,"name":"向外发送前确认","stage":"tool_input","rule":{"kind":"always"}}
```

策略文件示例（id 为运维生成的稳定 UUID，下面仅为示例）：

```json
{"id":"10000000-0000-4000-8000-000000000001","guardrail_id":"external-send-review","model_id":null,"key_id":null,"runtime_id":"hermes-production","tool_name":"send_message","active":true}
```

`contains_any` 的 rule 格式为 `{"kind":"contains_any","terms":["示例词项"],"case_sensitive":false}`；最多 64 项，每项最多 256 字节。一次阶段最多匹配 256 个护栏，超过返回配置错误，不静默截断。

## 4. Turn 与审批

控制面 `POST /v1/governance/turns` 接收 `key_id,session_id,message_id,run_id,generation,ttl_seconds`。Key 必须绑定当前真人。相同 Key/session/message/generation 幂等，不延长过期时间；run 不同则冲突。TTL 为 60–3600 秒，不能续期/复开。新用户消息、重新生成、分叉、反馈改向由 BFF 创建新 turn/代次，不让 Agent 随意沿用旧 turn。

同 Key/session 只允许一个 active turn；创建新 turn 在同事务关闭该会话旧 turn、记录 superseded 事件并撤销旧作用域的有效性。重放旧创建请求只返回旧终态，不关闭新 turn、不复活旧授权。真正并行的独立任务/会话分叉使用独立 session ID，不能用增加 generation 保留同会话多个可豁免轮次。

受管模型调用必须带 `X-Brokerrouter-Turn-Id`，缺失时拒绝，不降级为无护栏调用。已有 Runtime 绑定即要求 turn；未绑定但命中已启用模型策略也不会静默绕过。Key/用户/绑定/模型硬权限不属于可批准护栏。

兼容模型 API 返回 `409 error.code=approval_required` 与持久 `error.approval`，并设置 `x-should-retry:false`。原生检查 API 返回 `200 {state:"waiting_approval",approval:...}`。调用方必须暂停保存检查点，不以普通网络重试循环处理。

控制面 `POST /v1/approvals/{id}/resolve`：

```json
{"decision":"continue_this_turn","approved_guardrail_ids":["external-send-review"],"feedback":null}
```

另支持 `{"decision":"reject"}` 与 `{"decision":"feedback","feedback":"请先去除手机号"}`。UI 必须从服务端审批展示：模型/工具、阶段、turn/run、动作摘要哈希、护栏名称和风险代码，以及“本轮后续动作也会跳过所选护栏”的明确范围。API 不向 Agent 暴露待批原文或可旁路下载链接；当前也未开放真人原文审阅接口。

- 批准：只签发所选 guardrail ID/version 的服务端 grant。Agent 用原 turn 和原幂等键恢复，不携带自报“已批准”字段。未选护栏可能产生下一张审批。
- 拒绝：当前动作终止，未产生 grant；已生成的输出销毁但费用保留。
- 反馈：无 grant，取消整个旧 turn；BFF 使用反馈驱动**新的 turn/执行代次**。这是对 v0.3 留白的明确决定，避免旧 pending 动作和旧授权在改向后继续生效。
- 用户、Runtime 可关闭 turn：`POST /v1/governance/turns/{id}/close {"state":"completed|cancelled|failed"}`。Runtime 只可关闭自己的 turn，不可创建、续期、复开或审批。数据库触发器同样禁止身份/到期时间改写及终态复开。
- 审批最长 15 分钟且不超过 turn TTL；post 审批还不超过保存结果的 TTL。grant 同样不能超过审批/turn 的期限。到期需建立新 turn，不无限延长豁免。批准不是执行成功，也不是永久可重放授权。

常规策略以**每阶段检查事务的提交点**为准；版本升级不追溯撤回已准入的上游调用。下一次检查使用最新匹配集合与护栏版本；旧待批快照变化时返回 `policy_changed`，重查原操作生成新审批，旧 grant 不覆盖新版本。发送边界再次检查当前 Key/模型/turn；发送已发生后不能承诺撤销外部副作用。结果每次释放均重新检查当前权限和 post 策略。

## 5. 输入、输出与费用

输入检查在预留/上游提交前，等待审批不占用费用预算。恢复重新执行正常准入，仍可能因预算不足或硬权限撤销失败。

输出先按 M2 保存加密结果并结算，再检查 post。检查/审批失败不会将消费回退成零；未经批准的正文和工具调用参数不会出现在结果接口、错误、日志或事件里。批准后由原请求 `/result` 或原幂等调用重取同一结果，不再发供应商 POST。上游已成功而 post 检查时数据库失败，也只允许原结果恢复，不重生成。

M2 请求 `status=succeeded` 表示**供应商调用与计费完成**，不等于内容已释放。待批状态以治理 operation/approval 为准；控制台不得把这两个维度合并或把等待算作供应商故障。

## 6. 工具执行与崩溃恢复

数据面 `/v1/governance/checks` 请求：`turn_id,operation_key,stage,model_id?,tool_name,payload`。工具/产物 stage 与具名工具必需。相同 operation key/stage 不允许更换 turn、模型、工具或参数；其他意见后的新动作使用新 ID。

1. Runtime 必须先持久化消息/工具调用 ID、原始参数、父步骤、已完成结果引用、pending approval、turn/代次；网关不能代替 Runtime 检查点。
2. 检查 `ready` 后，Runtime 生成并持久化 32 字节随机执行 token（64 hex），向 `/v1/governance/operations/{id}/claim` 提交 `token,payload_hash`。服务端在 claim 再检查策略与当前 turn。
3. 仅首次成功响应的 `dispatch_authorized=true` 允许该执行者继续，且执行前仍须验证 Runtime 原生硬权限。重复同 token 返回 `dispatch_authorized=false`；其他执行者被拒绝。响应丢失不能把查询到 `executing` 当作再次执行许可。
4. 执行窗口最多 60 秒且不超过 turn TTL。超时或进程崩溃进入 `execution_unknown`，绝不自动转回 ready。已在执行的长工具允许稍后提交实际回执，期间不把执行权发给另一个 worker。
5. `/receipt` 接收 `token,outcome=completed|execution_unknown,evidence_ref`，只记录原执行者回执；重复相同内容幂等，冲突回执拒绝。evidence_ref 是受管 Runtime 内的稳定引用，不包含原始敏感输出，工具输出发布仍走 `tool_output/artifact` 检查。
6. 人工核对使用控制面 `/v1/governance/operations/{id}/reconcile`，签名正文为 `outcome=confirmed_executed|confirmed_not_executed,evidence_ref`。仅原真人能核对未知操作；确认未执行也不会自动重做，原操作终止，需新动作重新准入。核对不发送外部请求、不修改模型账单。

网关不能替不支持幂等/可查询的外部工具保证 exactly-once。Runtime 必须验证 claim 返回的动作哈希与实际将执行参数一致，保护 Adapter 与凭据，并限制绕过网关/执行 hook 的出口。未经这些部署认证的第三方自托管 Runtime 仅属协作接入，不宣称防篡改治理。

## 7. 查询、事件、留存

- Runtime：`GET /v1/approvals/{id}`；`GET /v1/governance/turns/{id}/approvals` 返回最新 100 张；历史完整发现走事件游标后按 ID 查询。
- BFF：签名 `POST /v1/approvals/query {"turn_id":"..."}`，只能查当前真人拥有的 turn；大规模控制台分页属于 M5。
- `GET /v1/governance/operations/{id}` 返回动作/执行状态、摘要哈希、原执行结果引用；不返回隔离 payload。
- `GET /v1/governance/turns/{id}/events?after=N` 每页最多 100，服务端按 turn 锁分配递增 sequence，与事实同事务提交；`POST .../ack {"sequence":N}` 仅单调推进，拒绝未来位置。Runtime 应按 turn+sequence 去重，并仅在检查点持久化后确认。采用拉取 outbox，不承诺已交付某个 Claw 推送通道。
- turn 关闭或到期后立即逻辑拒绝新操作/结果释放，后台每 30 秒分批最多 100 条清除该 turn 的敏感 payload、结果和 receipt 正文。过期执行态即使尚未被清理线程更新，查询也显示不确定且禁止再 claim。
- 反馈密文在审批期限后再保留 24 小时，读接口超期不返回，控制面后台分批清除；BFF 短 JWT 去重记录过期后清理。审计、摘要、批准决定、grants 和账本事实保留，生产须为这些元数据配置独立备份/归档保留策略，不擅自删除资金事实。
- 清理 backlog 与数据库等待需要监控。多个副本用 `SKIP LOCKED` 分批竞争；尚未完成 LIVE-07 容量与恢复实测，不承诺固定物理销毁延迟。

## 8. 验收状态

本地测试涵盖 RSA 签名/路径正文绑定、真实 PostgreSQL、角色权限、跨用户、版本更新、并发冲突、部分批准、本轮边界、反馈取消、单执行者、不确定人工核对、事务故障回滚、留存清理，以及真实 HTTP 输入/输出两次审批但只收费一次。详情见 [里程碑](milestones.md)。

未完成真实供应商、SDK、用户 BFF、Hermes/OpenClaw 和生产容量认证仍在 [验收待办](acceptance-backlog.md)，不以 fixture 关闭。签名实现使用 [jsonwebtoken 10.4 文档](https://docs.rs/jsonwebtoken/10.4.0/jsonwebtoken/)，加密沿用 M2 的 AES-GCM 信封，不自行实现密码算法。
