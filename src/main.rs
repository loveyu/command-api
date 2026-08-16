mod app;
mod config;
mod model;
mod process;
mod runtime;
mod secret;
mod service;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::DEFAULT_CONFIG_PATH;
use std::{io::Read, path::PathBuf, process::ExitCode};

#[derive(Debug, Parser)]
#[command(name = "command-api", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 在当前命令行前台运行 HTTP 服务
    Run {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 管理或运行 Windows Service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// 生成或导入 Windows DPAPI 加密 Token
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
}

#[derive(Debug, Subcommand)]
enum SecretAction {
    /// 生成强随机 Token，写入 DPAPI 密文文件并仅在标准输出显示一次 Token
    Generate {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "user")]
        scope: secret::DpapiScope,
    },
    /// 从隐藏终端或标准输入读取 Token，并写入 DPAPI 密文文件
    Protect {
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "user")]
        scope: secret::DpapiScope,
        #[arg(long)]
        stdin: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    /// 由 Windows Service Control Manager 调用
    Run {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 按配置的 LocalService 或 LocalSystem 账户注册自动启动服务（需要管理员权限）
    Install {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 启动已经注册的服务
    Start {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 停止已经注册的服务
    Stop {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 停止并删除已经注册的服务（需要管理员权限）
    Uninstall {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "__worker")
    {
        let code = process::worker_main();
        return ExitCode::from(code.clamp(0, 255) as u8);
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("command-api: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Run { config } => {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(runtime::run(&config, runtime::console_shutdown_signal(), true))
        }
        Command::Service { action } => match action {
            ServiceAction::Run { config } => service::run(&config),
            ServiceAction::Install { config } => service::install(&config),
            ServiceAction::Start { config } => service::start(&config),
            ServiceAction::Stop { config } => service::stop(&config),
            ServiceAction::Uninstall { config } => service::uninstall(&config),
        },
        Command::Secret { action } => match action {
            SecretAction::Generate { output, scope } => {
                let token = secret::generate_and_protect(&output, scope)?;
                println!("{token}");
                Ok(())
            }
            SecretAction::Protect { output, scope, stdin } => {
                let token = if stdin {
                    let mut value = String::new();
                    std::io::stdin().read_to_string(&mut value)?;
                    value.trim_end_matches(['\r', '\n']).to_owned()
                } else {
                    let first = rpassword::prompt_password("Token: ")?;
                    let second = rpassword::prompt_password("再次输入 Token: ")?;
                    anyhow::ensure!(first == second, "两次输入的 Token 不一致");
                    first
                };
                secret::protect_to_file(token.as_bytes(), &output, scope)
            }
        },
    }
}
