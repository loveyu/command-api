use crate::{app, config::Config, model::StopReason, store::TaskStore};
use anyhow::{Context, Result};
use std::{future::Future, path::Path, time::Duration};
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt, layer::SubscriberExt, util::SubscriberInitExt};

pub async fn run(
    config_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
    log_to_console: bool,
) -> Result<()> {
    let config = Config::load(config_path)?;
    let _log_guard = init_logging(&config.logging.directory, log_to_console)?;
    let store = TaskStore::open(
        config.logging.directory.clone(),
        config.logging.retention_seconds,
        config.logging.max_output_bytes_per_task,
    )
    .await?;
    store.spawn_cleanup();

    let address = listen_address(&config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("无法监听 {address}"))?;
    tracing::info!(%address, config = %config.source_path.display(), "command-api 已启动");

    let application = app::build(&config, store.clone());
    let server_result = axum::serve(listener, application)
        .with_graceful_shutdown(shutdown)
        .await;

    tracing::info!("服务正在停止，开始终止执行中的任务");
    store.cancel_all(StopReason::ServerShutdown).await;
    let idle = store
        .wait_until_idle(Duration::from_secs(config.execution.shutdown_timeout_seconds))
        .await;
    if !idle {
        tracing::warn!(
            timeout_seconds = config.execution.shutdown_timeout_seconds,
            "等待任务退出超时；进程退出时平台进程树保护将强制清理遗留任务"
        );
    }
    server_result.context("HTTP 服务异常退出")?;
    tracing::info!("command-api 已停止");
    Ok(())
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
