# command-api

`command-api` 将配置文件中预先声明的脚本暴露为带 Token 鉴权的异步 HTTP API。它只执行配置允许的脚本，支持逐路由和全局并发限制、动态 argv 参数、执行超时、平滑/强制终止进程树、输出持久化、任务状态查询以及服务停止和运行时重启。

项目地址：<https://github.com/loveyu/command-api>

## 平台支持

- Linux x86_64、ARM64（AArch64）和 ARMv7 hard-float（glibc）
- Windows x86_64 和 ARM64（MSVC）
- Windows 10 或更高版本；Windows Server 2016 或更高版本
- Windows 命令行前台模式和 Windows Service 模式

GitHub Release 分别提供 `linux-x86_64`、`linux-aarch64`、`linux-armv7`、`windows-x86_64` 和 `windows-aarch64` 安装包。CI 在 x86_64 与 ARM64 原生 Runner 上执行完整测试；Linux ARMv7 执行交叉编译校验。

Linux 使用独立进程组管理脚本及子进程；Windows 使用 Job Object。任务超时、主动取消或服务停止时，服务先请求整个进程树平滑退出，等待配置的宽限期，仍未退出再强制终止。强制终止接口会跳过宽限期，立即终止整棵进程树。

## 配置

复制示例配置并修改 Token：

```bash
cp config.example.yaml config.yaml
```

核心配置示例：

```yaml
server:
  host: 0.0.0.0
  port: 27415

auth:
  token: replace-with-a-strong-random-token

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
    max_concurrency: 2
    max_execution_seconds: 300
    graceful_shutdown_seconds: 10
    merge_stdout_stderr: false
    output_encoding: utf-8
```

相对的脚本路径、工作目录和日志目录均相对于配置文件所在目录解析。配置会在启动时校验，脚本不存在、路由冲突、限制为零或 `cmd` 启用动态参数时拒绝启动。配置修改后可以调用运行时重启接口重新加载；`logging.directory` 是例外，修改它需要先停止服务，再通过命令行或外部服务管理器启动。

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

实际配置文件包含 Token，不应提交到 Git。日志目录必须只有服务账户和管理员可读写。

## 鉴权与接口

所有接口，包括首页和健康检查，都必须携带：

```http
Authorization: Bearer <token>
```

Token 不支持通过 URL 查询参数传递，以免出现在代理日志和浏览器历史中。该 Token 同时拥有脚本执行、任务强制终止和服务启停权限，应按管理密钥保护，并在反向代理或防火墙中限制调用来源。

### 创建任务

```bash
curl -sS -X POST http://127.0.0.1:27415/commands/example-shell \
  -H 'Authorization: Bearer replace-with-a-strong-random-token' \
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
curl -sS http://127.0.0.1:27415/tasks/<task-id> \
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

安装命令注册为自动启动的 `LocalService`。必须提前为 `NT AUTHORITY\LOCAL SERVICE` 授予二进制、配置和脚本的读取权限，以及日志目录的修改权限。需要访问网络共享或其他用户资源时，应由管理员在 Windows 服务管理器中修改服务账户；程序不接收或保存服务账户密码。

Service 使用配置文件绝对路径，不依赖 Windows Service 默认的 `C:\Windows\System32` 工作目录。SCM 停止事件、命令行 Ctrl+C 和 `/system/stop` 使用同一套任务平滑退出和强制清理流程。`/system/restart` 在现有 Windows Service 进程内重建 HTTP 运行时，因此不需要 LocalService 账户拥有额外的 SCM 服务启动权限。

## Docker

Docker 镜像不包含 Token 配置和业务脚本，需要只读挂载配置/脚本，并为日志目录提供可写挂载：

```bash
cargo build --release --locked
docker build -t command-api:local .
docker run --rm -p 27415:27415 \
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
