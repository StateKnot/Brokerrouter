# M4.3b 第二包：实际隔离技术检测器

契约 `clamav-ffmpeg-mp4-v1`，2026-09-14。`brokerrouter-scanner` 是独立 Rust 工作负载，通过上一包扫描 API 领取并读取隔离资产，实际调用 ClamAV、FFprobe、FFmpeg，然后持久化证据并提交回执。它没有数据库、S3、模型供应商或真人审批凭据。

本包完成的是**文件技术检测**，不是多模态语义合规。所有结果都设置 `content=unknown`；三个技术步骤通过才设置 `technical=clean,media_type=video/mp4,reason_codes=[semantic_not_run]`。输出继续隔离、不交付；资产感知策略、语义审核、独立真人审阅及批准后交付仍需开发。已实现的本轮护栏豁免不能覆盖技术不安全、不支持或检测失败。

## 硬边界与支持档位

| 边界 | 固定执行方式 |
| --- | --- |
| 输入 | 扫描 API 精确 attempt 的 4 MiB 分块，最多 512 MiB；长度与全文件 SHA-256 独立重算 |
| 杀毒 | ClamAV 实际扫描只读文件；文件上限 512 MiB、展开扫描 1 GiB、1,000 文件、递归 8 层、引擎扫描 90 秒，超限/加密启发式报警；不接受跳过后的“clean” |
| 格式识别 | 仅 MP4 指定 major brand，1 条 H.264/H.265 视频，可有 1 条 AAC 音频；不支持字幕、数据、附件、多视频流、QuickTime 或播放列表 |
| 媒体上限 | 时长 ≤60 秒，≤60 fps，≤3,600 解码帧，单边 ≤3,840、像素 ≤3,840×2,160；音频 8–96 kHz、1–2 声道 |
| 完整解码 | FFmpeg 解码全部流到 null 输出，遇错退出；验证最终 `progress=end`、实际帧数与输出时长；不以截取开头几帧代替完整解码 |
| 文件解析环境 | 每一步独立 Linux 容器，非 root、只读根文件系统、只读单一输入目录、无网络、无 capabilities、no-new-privileges、无 Docker socket/密钥挂载 |
| 资源 | 每容器 1 CPU、2 GiB 内存且不增加交换额度、64 PID、128 FD、禁止 core dump，1 GiB noexec/nosuid/nodev 临时空间 |
| 超时/输出 | 容器内部 120 秒 watchdog；外部 130 秒 watchdog 后强制删除具名容器；stdout/stderr 各最多 64 KiB，超出失败关闭 |

ClamAV 默认可能把超出文件上限的文件跳过；因此这里同时校验真实长度、启用超限报警，并只接受精确的单文件成功回执。参数依据 [ClamAV 扫描说明](https://docs.clamav.net/manual/Usage/Scanning.html)。这不意味着杀毒能证明绝对无恶意内容；格式验证也不证明内容符合业务或法律要求。

FFmpeg 只允许 file 输入协议、mov demuxer 和指定解码器；网络还由容器整体关闭。参考 [FFmpeg 协议白名单](https://ffmpeg.org/ffmpeg-protocols.html)、[遇错退出选项](https://ffmpeg.org/ffmpeg.html)。隔离参数参考 [Docker run](https://docs.docker.com/reference/cli/docker/container/run/)；不使用 privileged、host 网络、宿主根目录挂载或关闭 seccomp。

## 发布与清单

先完成 `serve-output-scans` 的专用数据库/S3 权限和 TLS 部署。检测 worker 运行在专用安全工作负载节点，**不能和 Hermes/OpenClaw/不可信插件共用账户、工作目录或 Docker daemon**。生产使用专用 rootless Docker 或等价加固隔离环境；Docker socket 只归 worker 管理进程持有，绝不进入检测容器。默认容器隔离不是内核漏洞的绝对防线，生产内核/运行时补丁、seccomp/AppArmor 及逃逸演练仍属 LIVE-12。

1. 在受控构建流水线中，准备经过验证的 ClamAV 官方签名库及批准的自定义规则，记录来源、签名验证、版本、更新时间和摘要。`deploy/detector/Dockerfile` 的 build context 必须有 `signatures/`；**严禁以 `tests/detector/signatures` 发布生产镜像**，该目录只有一个无害测试规则，不具备生产恶意文件覆盖率。
2. `BASE_IMAGE` 指定批准的 Debian 镜像摘要；安装系统包、生成 SBOM/漏洞扫描报告并完成固定测试集，发布最终 OCI digest。Dockerfile 是构建配方，不把随构建时获取的包版本当成可复现锁定发布；最终不可变镜像及其供应链证据才是发布对象。运行期禁止在线更新签名库或拉取镜像。
3. 构建 `cargo build --release --locked --bin brokerrouter-scanner`，记录此**实际 worker 二进制** SHA-256。清单同时绑定该摘要、检测镜像摘要和协议，避免升级 worker 命令/边界后仍复用旧清单。
4. 创建 worker 私有目录，权限 0700，使用有容量上限的加密本地文件系统，禁止其他 workload 访问；目录必须为绝对 canonical 路径且不含逗号。目录锁禁止两个进程同时使用同一持久状态。不同副本使用不同目录/凭据，并分别纳入撤销与清理管理。

配置文件没有密钥，字段如下（摘要为格式示例，必须替换；时间必须替换为本次受控发布窗口）：

```json
{
  "api_origin": "https://scanner-api.internal.example/",
  "docker_binary": "/usr/bin/docker",
  "docker_socket": "/run/user/1001/docker.sock",
  "work_dir": "/var/lib/brokerrouter-scanner",
  "manifest": {
    "contract": "clamav-ffmpeg-mp4-v1",
    "image": "registry.internal.example/security/detector@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "worker_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "issued_at": "2026-09-14T00:00:00Z",
    "expires_at": "2026-09-15T00:00:00Z"
  }
}
```

清单有效期不得超过 7 天，过期立即停止领取/续期/执行/回报；生产签名刷新频率通常需要更短的内部安全目标，不能简单延长时间而不更新签名库。发布方对数据库实际新鲜度负责，日期字段不是对官方签名库更新时间的自动证明。改变签名库、镜像、worker、契约或有效期都改变 revision，需要发布更高版本 profile 和新凭据，沿用上一包旧版本隔离规则。

```sh
# 使用将要部署的同一个二进制计算 revision；仅修改 JSON 空白不改变摘要。
brokerrouter-scanner --config /etc/brokerrouter/scanner.json manifest-revision
# 将上面的结果写入对应模型的新 output_scan_profile.engine_revision。
# 由 Secret Manager 注入 BROKERROUTER_SCANNER_TOKEN（sc_br_），不在 CLI 参数中传递。
brokerrouter-scanner --config /etc/brokerrouter/scanner.json run
# 单次处理便于运维验收；退出非零不能当作通过。
brokerrouter-scanner --config /etc/brokerrouter/scanner.json run --once
```

worker 只连接配置的 HTTPS origin（无重定向、无代理、无 URL 凭据/查询参数），本地 fixture 仅允许数值 loopback HTTP。Docker 客户端子进程清空环境，并显式使用工作目录中私有、内容为空的独立 Docker config，避免个人配置将代理凭据注入容器；不能在此 config 中加入 auth、proxy 或 credential helper。镜像必须预先部署，`--pull=never`；本地 image ID 也可按 `sha256:` 固定，发布清单应优先使用已签名 registry digest。

## 恢复、撤销和证据

持久状态先保存随机 claim ID，领取后先保存 attempt ID，再启动外部进程。下载前及每个分块、检测步骤和回报前续期，沿用服务端当前租约、Key/profile/turn/取消检查；检测过程本身没有付费模型调用。取消后的已读取字节无法追溯收回，但下一个 API 边界拒绝，且每个容器有独立时间上限。

下载重试从同一 attempt 的第 0 块开始，必须重新完成长度/SHA 验证。没有完整证据时进程恢复可重做本地技术检测；不会创建新媒体作业或触发供应商生成 POST。失败路径删除本地明文；输出证据、receipt fsync + 原子 rename + 目录 fsync 后才回报服务端。回报失败保留原 receipt，重启重发完全相同结果；先确认回执的证据包确实存在且摘要匹配。服务端已完成但响应丢失时，通过原 claim 恢复完成状态。

每个步骤容器名称仅由 attempt UUID 与固定步骤构造。重启先清理此 attempt 的已知容器，再继续；不使用全局 prune。SIGINT/SIGTERM 取消时强制清理当前 attempt 的容器与输入，保留 claim/receipt；SIGKILL 或主机崩溃依靠容器内部 watchdog 和下一次启动恢复。若 Docker daemon 不可达，不能宣称清理已完成，进程失败并等待运维恢复；清理告警是上线条件。

证据记录绑定资产/作业/attempt、完整 SHA/长度、清单、各步骤固定结果、退出码、输出摘要；不保存原始媒体、stdout/stderr、模型正文、URL 或供应商凭据。技术 clean 一样明确记录 `semantic_detector=not_run`。私有 evidence/receipt 目录达到 4,096 文件或 128 MiB 时停止接单，不自动删除审计事实；运维按保留策略将**已完成**证据加密归档、验证摘要后再清理，不能删除 active receipt 或活跃容器对应状态。存储空间、清单到期、无进展和反复失败需要接入告警。CPU/容器/存储是基础设施成本，不修改已有人民币模型账本，也不伪称生产运营零成本。

## 验收边界

常规测试包含媒体档位严格解析、清单固定/有效期、子进程时间及输出上限、真实 PostgreSQL/HTTP 的提交失败/重启恢复、manifest 漂移、撤销、私有目录和单实例锁。进程 fixture 只验证 orchestration，不能当作检测引擎证据。

另有两项必须显式执行的真实引擎测试：容器内生成有效 MP4 → 实际 ClamAV/FFprobe/FFmpeg → 扫描 API/数据库完整闭环；无害 AV 标记检出、非 MP4 和截断媒体拒绝。它们使用真实二进制和测试特征库，不代表官方恶意库覆盖率或攻击样本质量认证。CI 将这些 ignored 档位显式纳入，而不是静默跳过。

2026-09-14 最终本地 debug/release 各 104 项通过，均显式纳入上述两项引擎专项和两项 S3 专项；fmt、clippy `-D warnings`、release build 通过。镜像/包版本和早期失败修复记录见 [里程碑](milestones.md)。

LIVE-12 仍需验证生产签名库及供应链、真实恶意/畸形/边界样本、误报漏报、长视频资源、只读/网络/权限隔离、进程/宿主机终止恢复、证据归档与容量告警、TLS/工作负载凭据轮换；未关闭该闸门。语义审核、资产策略与真人审阅/审批交付不是“待测”，仍是下一开发包。
