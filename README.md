# command-api

`command-api` 将配置文件中预先声明的脚本暴露为带 Token 鉴权的异步 HTTP API。它只执行配置允许的脚本，支持逐路由和全局并发限制、动态 argv 参数、执行超时、平滑/强制终止进程树、输出持久化、任务状态查询以及服务停止和运行时重启。

> **安全边界**：本服务只适用于受控内网，不适合直接或通过 NAT 端口转发、反向代理暴露到公网。程序允许自由配置监听 IP、CIDR 和明文 HTTP，因此网络边界必须由部署者通过精确 CIDR、mTLS 和系统防火墙共同保护。

项目地址：<https://github.com/loveyu/command-api>

## 平台支持

- Linux x86_64、ARM64（AArch64）和 ARMv7 hard-float（glibc）
- Windows x86_64 和 ARM64（MSVC）
- Windows 10 或更高版本；Windows Server 2016 或更高版本
- Windows 命令行前台模式和 Windows Service 模式

GitHub Release 分别提供 `linux-x86_64`、`linux-aarch64`、`linux-armv7`、`windows-x86_64` 和 `windows-aarch64` 安装包。CI 在 x86_64 与 ARM64 原生 Runner 上执行完整测试；Linux ARMv7 执行交叉编译校验。

Linux 使用独立进程组管理脚本及子进程；Windows 使用 Job Object。任务超时、主动取消或服务停止时，服务先请求整个进程树平滑退出，等待配置的宽限期，仍未退出再强制终止。强制终止接口会跳过宽限期，立即终止整棵进程树。

## 配置

复制示例配置，并准备环境变量 Token 或 Windows DPAPI 密钥：

```bash
cp config.example.yaml config.yaml
```

核心配置示例：

```yaml
server:
  listeners:
    - host: 10.132.1.145
      port: 27415
    - host: 127.0.0.1
      port: 27416

access:
  allowed_cidrs:
    - 10.132.1.1/32
  token_failure_cooldown:
    enabled: true
    seconds: 10
    max_tracked_ips: 4096

# tls:
#   certificate: ./tls/server.crt
#   private_key: ./tls/server.key
#   client_ca_certificate: ./tls/client-ca.crt

auth:
  token:
    provider: windows_dpapi
    file: ./secrets/token.dpapi

logging:
  directory: ./logs
  retention_seconds: 86400
  max_output_bytes_per_task: 104857600

execution:
  max_total_concurrency: 16
  shutdown_timeout_seconds: 30

routes:
  - path: /commands/deploy
    executor: bash
    script: ./scripts/deploy.sh
    fixed_args: [--environment, production]
    request_args:
      enabled: true
      max_count: 32
      max_item_bytes: 4096
      max_total_bytes: 16384
      allowed_values: [status]
      allowed_patterns: ['^[A-Za-z0-9._-]{1,64}$']
    max_concurrency: 2
    max_execution_seconds: 300
    graceful_shutdown_seconds: 10
    merge_stdout_stderr: false
    output_encoding: utf-8
```

`server.listeners` 可以配置一个或多个 IPv4/IPv6 地址与端口组合；所有监听端点共享同一套路由、Token、CIDR、并发限制、任务存储和 TLS 策略。旧版 `server.host + server.port` 单地址写法，以及 `server.hosts + server.port` 多地址共用端口写法继续兼容，但不能与 `listeners` 同时出现。地址和 CIDR 可以自由配置，包括全地址和公网地址；这不代表适合公网部署。

未配置 `tls` 时所有监听端点均使用明文 HTTP；配置 `tls` 后所有端点统一启用 TLS，并强制客户端证书认证（mTLS），服务器证书需要覆盖客户端实际访问的全部地址或名称。明文模式或非回环监听不会阻止启动，但会向日志写入安全警告；前台模式下警告也会显示在控制台。

相对的脚本、工作目录、日志、证书和 DPAPI 密钥路径均相对于配置文件所在目录解析。配置会在启动时校验；明文 Token、空 CIDR、空或重复监听端点、混用新旧监听格式、脚本不存在、路由冲突、限制为零或 `cmd` 启用动态参数时都会拒绝启动。配置修改后可以调用运行时重启接口重新加载；`logging.directory` 是例外，修改它需要先停止服务，再通过命令行或外部服务管理器启动。

`allowed_values` 和 `allowed_patterns` 对每个请求参数执行允许值/正则校验；两者同时配置时，参数满足任一规则即可。LocalSystem 服务的路由只要启用动态参数，就必须至少配置其中一种规则。

### Token 密钥

YAML 不接受明文 Token，只支持以下提供器：

```yaml
# 跨平台：从进程环境读取
auth:
  token:
    provider: environment
    variable: COMMAND_API_TOKEN

# Windows：从 DPAPI 密文文件读取
auth:
  token:
    provider: windows_dpapi
    file: ./secrets/token.dpapi
```

Windows 可以生成强随机 Token，并按当前用户或整机作用域保护：

```powershell
command-api.exe secret generate --scope user --output C:\command-api\secrets\token.dpapi
command-api.exe secret protect --scope machine --output C:\command-api\secrets\token.dpapi
```

`generate` 仅在标准输出显示一次 Token；应立即保存到调用端的密钥存储，避免终端历史和日志。`user` 密文只能由同一 Windows 用户在同一机器解密；`machine` 密文可被同机用户解密，因此必须通过 NTFS ACL 限制密钥文件。两个实例应使用不同 Token。

### CIDR 与认证失败冷却

`access.allowed_cidrs` 必须包含一个或多个 IPv4/IPv6 CIDR。服务仅使用 TCP 对端地址，不信任 `X-Forwarded-For` 等可伪造头。来源不在白名单时返回 `403`。

启用 `token_failure_cooldown` 后，第一次 Token 失败返回 `401` 并按来源 IP 记录冷却；冷却期内后续请求直接返回 `429` 和 `Retry-After`，不会再次校验 Token、解析请求或触发脚本。共享同一 NAT 出口的客户端也会共享冷却状态。

解释器支持：

| `executor` | 默认程序 | 调用形式 |
| --- | --- | --- |
| `sh` | `sh` | `sh <script> <fixed_args> <request_args>` |
| `bash` | `bash` | `bash <script> ...` |
| `zsh` | `zsh` | `zsh <script> ...` |
| `pwsh` | `pwsh` | `pwsh -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <script> ...` |
| `powershell` | `powershell.exe` | Windows PowerShell 5.1 `-ExecutionPolicy Bypass -File` 模式 |
| `cmd` | `cmd.exe` | 仅允许配置固定参数，不允许 HTTP 动态参数 |

可以使用 `program` 为某条路由指定解释器的绝对路径。Windows 推荐安装并使用 PowerShell 7 `pwsh`；如果 Windows PowerShell 5.1 产生本地编码输出，可将 `output_encoding` 配置为 `gbk` 或 `utf-16le`。

## 运行

Linux 或 Windows 命令行前台运行：

```bash
cargo run -- run --config config.yaml
```

release 二进制：

```text
command-api run --config /absolute/path/config.yaml
command-api.exe run --config C:\command-api\config.yaml
```

实际配置文件不包含明文 Token，但配置、DPAPI 密钥、TLS 私钥和日志仍不得提交到 Git，并必须只允许对应服务账户和管理员访问。

## 鉴权与接口

所有接口，包括首页和健康检查，都必须携带：

```http
Authorization: Bearer <token>
```

Token 不支持通过 URL 查询参数传递，以免出现在日志和浏览器历史中。该 Token 同时拥有脚本执行、任务强制终止和服务启停权限，应按管理密钥保护。非回环监听强烈建议同时启用 mTLS、精确的应用层 CIDR 和系统防火墙；明文模式只应在可信、隔离的内网使用。

### 创建任务

以下接口示例按已经启用 mTLS 编写；明文模式应改用 `http://`，并移除 `--cacert`、`--cert` 和 `--key` 参数。

```bash
curl -sS -X POST https://10.132.1.145:27415/commands/example-shell \
  --cacert ca.crt --cert client.crt --key client.key \
  -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/json' \
  -d '{"args":["hello world","--verbose"]}'
```

配置固定参数排在前面，请求数组中的参数依次追加。每个字符串作为独立 argv 传递，不经过 Shell 字符串拼接。

```json
{
  "success": true,
  "task_id": "ac2115d7-3563-4cf8-9aa0-edcc8851f54c",
  "status": "starting",
  "status_url": "/tasks/ac2115d7-3563-4cf8-9aa0-edcc8851f54c"
}
```

成功接受任务返回 `202`。逐路由或全局并发已满时不排队，立即返回 `429`。

### 查询任务

```bash
curl -sS https://10.132.1.145:27415/tasks/<task-id> \
  --cacert ca.crt --cert client.crt --key client.key \
  -H 'Authorization: Bearer <token>'
```

状态包括：`starting`、`running`、`stopping`、`succeeded`、`failed`、`timed_out`、`cancelled`、`killed` 和 `interrupted`。终止结果会说明原因、是否尝试平滑退出、是否强制终止，以及 Linux/Windows 使用的终止方法。

### 查询输出

分离输出模式：

```text
GET /tasks/<id>/output?stream=stdout&offset=0&limit=65536
GET /tasks/<id>/output?stream=stderr&offset=0&limit=65536
```

合并输出模式：

```text
GET /tasks/<id>/output?stream=combined&offset=0&limit=65536
```

`offset` 是原始日志字节偏移，`limit` 最大为 1 MiB。解码失败时响应同时提供 `content_base64`，确保原始数据仍可恢复。合并模式在进程启动前就把 stderr 和 stdout 指向同一个 OS 管道，以保留它们到达服务的相对顺序；合并后无法再区分来源。

### 取消任务

```text
POST /tasks/<id>/cancel
```

取消请求返回 `202`，随后任务进入 `stopping`；宽限期结束后仍未退出则强制终止整个进程树。

### 强制终止任务

```text
POST /tasks/<id>/kill
```

强制终止请求返回 `202`，跳过 `graceful_shutdown_seconds` 并立即终止整个进程树。最终任务状态为 `killed`，终止原因为 `force_killed`，且 `forced` 为 `true`。任务正在执行平滑取消时也可以调用该接口升级为强制终止。

### 停止与重启服务

```text
POST /system/stop
POST /system/restart
```

两个接口均返回 `202`，随后停止接受新连接，并按 `execution.shutdown_timeout_seconds` 处理运行任务：先请求平滑退出，超过总等待时间后立即强制终止遗留进程树。

- `stop`：完成清理后退出进程；Windows Service 会向 SCM 报告已停止，前台模式直接结束。
- `restart`：先校验并重新加载当前 YAML。配置无效时返回 `409` 且保持现有服务运行；配置有效时终止当前运行任务，在同一受管进程内释放并重建任务存储、监听端口和 HTTP 路由。能在全局停机上限内退出的任务最终为 `interrupted/server_restart`；超过 `execution.shutdown_timeout_seconds` 后被升级强杀的任务为 `killed/force_killed`。
- 为保证服务日志写入器和任务日志锁的一致性，运行时重启不允许修改 `logging.directory`；这种修改会返回 `409`，需要使用 `stop` 后由外部管理器重新启动。

重启期间存在短暂不可连接窗口；调用方应在收到 `202` 后带退避重试 `/healthz`。同一轮停止或重启过程中再次调用管理接口会返回 `409`。

## 日志与恢复

```text
logs/
├── command-api.lock
├── command-api.log.YYYY-MM-DD
└── tasks/
    └── <task-id>/
        ├── events.jsonl
        ├── stdout.log
        └── stderr.log
```

合并输出任务使用 `output.log`。状态以追加事件记录，服务重启后仍能查询保留期内的任务；重启时发现的未完成任务会标记为 `interrupted`。同一日志目录只能由一个实例使用。

达到 `max_output_bytes_per_task` 后不再写入额外输出，并将 `output_truncated` 标记为 `true`，脚本继续执行。日志写入失败则终止任务，避免脚本在无法审计的情况下继续运行。

## Windows Service

使用管理员终端注册和管理服务：

```powershell
command-api.exe service install --config C:\command-api\config.yaml
command-api.exe service start --config C:\command-api\config.yaml
command-api.exe service stop --config C:\command-api\config.yaml
command-api.exe service uninstall --config C:\command-api\config.yaml
```

服务账户由 `windows_service.account` 选择：默认 `local_service`，确有高权限脚本需求时可显式设置 `local_system`。程序不接收或保存 Windows 账户密码。LocalSystem 拥有极高本机权限，必须使用独立 Token、端口、配置、日志和严格参数规则，并把路由缩减到必要的系统操作。

必须提前为所选服务账户授予二进制、配置、脚本、证书的读取权限，以及日志目录的修改权限；TLS 私钥和 DPAPI 密钥只允许所选服务账户与 Administrators 读取。

Service 使用配置文件绝对路径，不依赖 Windows Service 默认的 `C:\Windows\System32` 工作目录。SCM 停止事件、命令行 Ctrl+C 和 `/system/stop` 使用同一套任务平滑退出和强制清理流程。`/system/restart` 在现有 Windows Service 进程内重建 HTTP 运行时，因此不需要服务账户拥有额外的 SCM 启动权限。

## Windows 登录用户桌面实例

需要访问用户桌面、HKCU 或交互会话时，使用独立配置执行普通前台进程：

```powershell
command-api.exe run --config "$env:LOCALAPPDATA\CommandApi\user\config.yaml"
```

自动随登录启动时，使用 Task Scheduler 的登录触发器和交互式用户令牌，并选择“仅当用户登录时运行”；不要保存登录密码，也不要默认启用“使用最高权限运行”。桌面实例必须与系统服务使用不同端口、Token、日志目录和路由集合。

## Docker

Docker 镜像不包含 Token 配置和业务脚本，需要只读挂载配置/脚本，并为日志目录提供可写挂载：

```bash
cargo build --release --locked
docker build -t command-api:local .
docker run --rm -p 127.0.0.1:27415:27415 \
  -v "$PWD/config.yaml:/etc/command-api/config.yaml:ro" \
  -v "$PWD/scripts:/etc/command-api/scripts:ro" \
  -v "$PWD/logs:/var/log/command-api" \
  command-api:local
```

容器内配置应使用容器路径，并将 `logging.directory` 设置为 `/var/log/command-api`。

## 开发验证

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --target x86_64-pc-windows-msvc
```

端到端测试覆盖鉴权、参数顺序、输出分离/合并、并发超限、主动取消、超时、立即强制终止、无效配置重启保护、有效重启恢复与服务停止。
