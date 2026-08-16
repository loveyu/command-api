# command-api 项目指南

## 项目用途

本项目通过带 Bearer Token 鉴权的 HTTP API，异步执行配置文件中预先声明的脚本。支持 Linux 和 Windows，Windows 可在命令行前台运行，也可注册为 Windows Service。

## 技术与结构

- Rust 2024 Edition
- Axum 0.8 + Tokio
- YAML 配置
- Linux 使用进程组管理脚本进程树
- Windows 使用 Job Object 管理脚本进程树
- 任务状态及 stdout/stderr 原始内容持久化到日志目录

## 行为约束

- 所有 HTTP 接口，包括 `/` 和 `/healthz`，都必须通过 Bearer Token 鉴权。
- 脚本路由只允许 POST。
- 动态参数必须作为独立 argv 追加到配置固定参数之后，不能拼接 Shell 命令。
- `.bat/.cmd` 和 `cmd.exe` 不允许接收来自 HTTP 的动态参数。
- 超时或取消时先尝试平滑退出，宽限期结束后必须强制终止整个进程树。
- 并发上限不排队，超限立即返回 HTTP 429。
- 任务输出必须落盘；合并模式在进程启动时合并两个 OS 输出句柄。

## 开发与验证

修改 Rust 代码后至少执行：

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo check --locked --target x86_64-pc-windows-msvc
```

涉及进程管理或 Windows Service 时，还需要在实际 Windows 10/Server 2016+ 环境运行测试。

## 操作约定

- 默认在当前分支工作，不主动新建分支。
- commit 中不要添加 `Co-Authored-By` 行。
- 文档、TODO 和维护说明优先使用简体中文。
- 使用 `sudo` 前先执行 `sudo -n -l`。
