use crate::{
    app,
    config::{Config, ResolvedConfig},
    model::StopReason,
    store::TaskStore,
};
use anyhow::{Context, Result, bail};
use std::{future::Future, path::Path, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Debug)]
pub enum ManagementAction {
    Stop,
    Restart(Box<ResolvedConfig>),
}

pub async fn run(
    config_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
    log_to_console: bool,
) -> Result<()> {
    let mut config = Config::load(config_path)?;
    let logging_directory = config.logging.directory.clone();
    let _log_guard = init_logging(&config.logging.directory, log_to_console)?;
    let (external_shutdown_tx, external_shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        external_shutdown_tx.send_replace(true);
    });

    loop {
        if config.logging.directory != logging_directory {
            bail!("运行时重启不允许修改 logging.directory；请停止服务后再启动");
        }
        let store = TaskStore::open(
            config.logging.directory.clone(),
            config.logging.retention_seconds,
            config.logging.max_output_bytes_per_task,
        )
        .await?;
        let cleanup_task = store.spawn_cleanup();

        let address = listen_address(&config.server.host, config.server.port);
        let listener = tokio::net::TcpListener::bind(&address)
            .await
            .with_context(|| format!("无法监听 {address}"))?;
        tracing::info!(%address, config = %config.source_path.display(), "command-api 已启动");

        let (management_tx, mut management_rx) = mpsc::channel(1);
        let application = app::build(&config, store.clone(), management_tx);
        let mut external_shutdown_rx = external_shutdown_rx.clone();
        let (action_tx, action_rx) = oneshot::channel();
        let server_result = axum::serve(listener, application)
            .with_graceful_shutdown(async move {
                let action = if *external_shutdown_rx.borrow() {
                    ManagementAction::Stop
                } else {
                    tokio::select! {
                        _ = external_shutdown_rx.changed() => ManagementAction::Stop,
                        action = management_rx.recv() => action.unwrap_or(ManagementAction::Stop),
                    }
                };
                let _ = action_tx.send(action);
            })
            .await;
        let action = action_rx.await.unwrap_or(ManagementAction::Stop);
        let stop_reason = if matches!(&action, ManagementAction::Restart(_)) {
            StopReason::ServerRestart
        } else {
            StopReason::ServerShutdown
        };

        tracing::info!(reason = %stop_reason, "服务正在停止，开始终止执行中的任务");
        store.cancel_all(stop_reason).await;
        let idle = store
            .wait_until_idle(Duration::from_secs(config.execution.shutdown_timeout_seconds))
            .await;
        if !idle {
            tracing::warn!(
                timeout_seconds = config.execution.shutdown_timeout_seconds,
                "等待任务退出超时，立即强制终止遗留任务进程树"
            );
            store.cancel_all(StopReason::ForceKilled).await;
            if !store.wait_until_idle(Duration::from_secs(5)).await {
                bail!("强制终止任务后仍无法释放运行资源");
            }
        }
        cleanup_task.abort();
        let _ = cleanup_task.await;
        server_result.context("HTTP 服务异常退出")?;
        drop(store);

        match action {
            ManagementAction::Stop => {
                tracing::info!("command-api 已停止");
                return Ok(());
            }
            ManagementAction::Restart(next_config) => {
                tracing::info!("command-api 正在使用重新加载的配置启动");
                config = *next_config;
            }
        }
    }
}

pub async fn console_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("无法安装 Ctrl+C 信号处理器");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("无法安装 SIGTERM 信号处理器")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn init_logging(directory: &Path, log_to_console: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let file = tracing_appender::rolling::daily(directory, "command-api.log");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("command_api=info,tower_http=info"));
    let registry = tracing_subscriber::registry().with(filter);
    if log_to_console {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer.and(std::io::stderr))
                    .with_ansi(false),
            )
            .try_init()
            .context("无法初始化日志系统")?;
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(writer).with_ansi(false))
            .try_init()
            .context("无法初始化日志系统")?;
    }
    Ok(guard)
}

fn listen_address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ipv6_listen_address() {
        assert_eq!(listen_address("0.0.0.0", 27415), "0.0.0.0:27415");
        assert_eq!(listen_address("::", 27415), "[::]:27415");
    }
}
