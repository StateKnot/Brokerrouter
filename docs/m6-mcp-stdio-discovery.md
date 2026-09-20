# MCP 受管 stdio 上游发现（M6.2b2b1）

状态：2026-09-20 完成可独立部署的 M6.2b2b1。隔离的 MCP client 进程现在可从部署侧版本化 launcher registry 选择预注册 profile，以官方 `rmcp 3.4.0` 的 MCP `2026-07-28` Discover lifecycle 启动 stdio server，并将完整 `server/discover` 结果写入 M6.2b1 的不可变快照。**本包仍不开放下游 `/mcp`，也不允许任何工具调用绕过现有逐节点治理。**

## 安全与进程边界

- 租户配置只保存 `launcher_profile` 名称，不能提交 program、参数、工作目录或环境变量；profile 必须已存在于 `BROKERROUTER_MCP_STDIO_LAUNCHERS_FILE`。
- registry、可执行文件及其全部父目录都必须是绝对规范路径；文件不得 group/world writable，不得带 setuid/setgid，程序必须有执行位。profile 必须提供小写 SHA-256，启动前逐字节校验 program（最大 512 MiB），同路径内容替换也会失败关闭。非 Unix 平台默认拒绝启动。
- 直接使用 `tokio::process::Command`，不经过 shell。子进程 `env_clear()`，只注入 profile 明确声明的 `{target,source_environment}` 映射；重复 target、缺失/超限值全部失败关闭。
- stdio server 不允许复用 HTTP Bearer/OAuth 配置。所需凭据由部署系统注入 connector 环境，再通过 profile 显式重命名传给子进程；凭据不会进入数据库、命令行或日志。
- program、arguments、环境绑定与环境值共同进入授权上下文指纹；private discovery cache 因 launcher 或 Secret 变化自动分区。
- 单次 lifecycle 超时限制为 1–60 秒；超时、协议错误和异常退出均终止子进程。stderr 直接丢弃，避免第三方 server 把工具输入或 Secret 写进网关日志；server 自身日志必须进入其隔离日志通道。

## 部署配置

参考 [`deploy/mcp-stdio-launchers.example.json`](../deploy/mcp-stdio-launchers.example.json)。其中全零 `program_sha256` 是必须替换的占位值。正式文件应由镜像或只读 Secret/Config volume 提供，不能放在可被 gateway API、Agent 或 MCP server 写入的目录。

```bash
export BROKERROUTER_MCP_STDIO_LAUNCHERS_FILE=/etc/brokerrouter/mcp-stdio-launchers.json
export BROKERROUTER_MCP_FILESYSTEM_ROOT=/srv/approved-read-only-data

brokerrouter --database-url "$DATABASE_URL" mcp-server-discover \
  --tenant "$TENANT_ID" \
  --server "$SERVER_ID" \
  --operation "$OPERATION_ID" \
  --actor "mcp-connector:production"
```

部署单元必须使用 `brokerrouter_mcp_client` 数据库角色、独立 Unix 用户/容器、只读 root filesystem、禁用提权、资源上限和仅所需挂载。profile 里的 program 必须是经过审核、摘要固定并负责建立 OS 沙箱的部署包装器；第三方 MCP server 不能与持有数据库/MCP 解密密钥的 connector 共享可读 `/proc`、用户身份或文件系统。不要直接引用 `node`/`python`、第三方 server 二进制或包管理器浮动入口（例如 `npx`、`uvx latest`）。

## 已验证与未完成边界

单元 wire fixture 已验证 stdio 上连续两次自包含 `server/discover`、2026-07-28 版本选择、private cache、指纹和受控关闭；registry 版本、重复 profile、路径/权限、环境映射、超限和 stdio HTTP-auth 混用均有失败关闭规则。

M6.2b2b2 仍需实现统一下游 `/mcp` POST、虚拟密钥/OAuth 资源认证、Host/Origin 与标准头校验、租户工具目录和治理后转发。M6.2c–d 继续实现持久 Skill 审批、MRTR 及 Hermes/OpenClaw 原生恢复。官方冻结 `2026-07-28` server/client conformance、预生产 HTTPS/OAuth、两个受控真实 server 与容灾/SLO 仍由 LIVE-22 阻断。
