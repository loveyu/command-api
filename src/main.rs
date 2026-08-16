mod app;
mod config;
mod model;
mod process;
mod runtime;
mod service;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::DEFAULT_CONFIG_PATH;
use std::{path::PathBuf, process::ExitCode};

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
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    /// 由 Windows Service Control Manager 调用
    Run {
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// 注册为自动启动的 LocalService 服务（需要管理员权限）
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
    }
}
