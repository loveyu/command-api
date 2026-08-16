use crate::{
    app,
    config::{Config, ResolvedConfig, ResolvedTlsConfig},
    model::StopReason,
    store::TaskStore,
};
use anyhow::{Context, Result, bail};
use rustls::{RootCertStore, ServerConfig as RustlsServerConfig, server::WebPkiClientVerifier};
use std::{fs::File, future::Future, io::BufReader, net::SocketAddr, path::Path, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
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
    let _ = rustls::crypto::ring::default_provider().install_default();
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

        let (management_tx, mut management_rx) = mpsc::channel(1);
        let application = app::build(&config, store.clone(), management_tx);
        let handle = axum_server::Handle::new();
        let tls = config.tls.as_ref().map(load_tls_config).transpose()?;
        let mut servers = JoinSet::new();
        if config.tls.is_none() {
            tracing::warn!("未配置 TLS，所有监听端点均使用明文 HTTP；仅应在可信、隔离的网络中使用");
        }
        if config
            .server
            .listeners
            .iter()
            .any(|listener| !listener.host.is_loopback())
        {
            tracing::warn!("检测到非回环监听端点；command-api 不适合暴露到公网，请使用精确 CIDR 和防火墙保护");
        }
        for listener in &config.server.listeners {
            let address = SocketAddr::new(listener.host, listener.port);
            let service = application.clone().into_make_service_with_connect_info::<SocketAddr>();
            let listener_handle = handle.clone();
            let listener_tls = tls.clone();
            tracing::info!(%address, config = %config.source_path.display(), "command-api 正在监听");
            servers.spawn(async move {
                match listener_tls {
                    Some(tls) => {
                        axum_server::bind_rustls(address, tls)
                            .handle(listener_handle)
                            .serve(service)
                            .await
                    }
                    None => axum_server::bind(address).handle(listener_handle).serve(service).await,
                }
            });
        }
        let server: Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> = Box::pin(async move {
            while let Some(result) = servers.join_next().await {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(std::io::Error::other(format!("HTTP 监听任务异常退出: {error}"))),
                }
            }
            Ok(())
        });
        let mut external_shutdown_rx = external_shutdown_rx.clone();
        let (action_tx, action_rx) = oneshot::channel();
        tokio::spawn(async move {
            let action = if *external_shutdown_rx.borrow() {
                ManagementAction::Stop
            } else {
                tokio::select! {
                    _ = external_shutdown_rx.changed() => ManagementAction::Stop,
                    action = management_rx.recv() => action.unwrap_or(ManagementAction::Stop),
                }
            };
            let _ = action_tx.send(action);
            handle.graceful_shutdown(None);
        });
        let server_result = server.await;
        server_result.context("HTTP 服务异常退出")?;
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

fn load_tls_config(config: &ResolvedTlsConfig) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let mut certificate_reader = BufReader::new(
        File::open(&config.certificate)
            .with_context(|| format!("无法读取 TLS 服务器证书 {}", config.certificate.display()))?,
    );
    let certificates = rustls_pemfile::certs(&mut certificate_reader)
        .collect::<std::io::Result<Vec<_>>>()
        .context("无法解析 TLS 服务器证书")?;
    if certificates.is_empty() {
        bail!("TLS 服务器证书为空");
    }

    let mut private_key_reader = BufReader::new(
        File::open(&config.private_key)
            .with_context(|| format!("无法读取 TLS 服务器私钥 {}", config.private_key.display()))?,
    );
    let private_key = rustls_pemfile::private_key(&mut private_key_reader)
        .context("无法解析 TLS 服务器私钥")?
        .context("TLS 服务器私钥为空")?;

    let mut client_ca_reader = BufReader::new(
        File::open(&config.client_ca_certificate)
            .with_context(|| format!("无法读取 TLS 客户端 CA {}", config.client_ca_certificate.display()))?,
    );
    let mut client_roots = RootCertStore::empty();
    for certificate in rustls_pemfile::certs(&mut client_ca_reader) {
        client_roots.add(certificate.context("无法解析 TLS 客户端 CA")?)?;
    }
    if client_roots.is_empty() {
        bail!("TLS 客户端 CA 证书为空");
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots)).build()?;
    let mut server = RustlsServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certificates, private_key)
        .context("TLS 服务器证书与私钥不匹配")?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertifiedKey, generate_simple_self_signed};

    #[test]
    fn formats_ipv6_socket_address() {
        assert_eq!(
            SocketAddr::new("::1".parse().unwrap(), 27415).to_string(),
            "[::1]:27415"
        );
    }

    #[test]
    fn loads_matching_pem_certificate_key_and_client_ca() {
        let temp = tempfile::tempdir().unwrap();
        let CertifiedKey { cert, signing_key } = generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate = temp.path().join("server.crt");
        let private_key = temp.path().join("server.key");
        std::fs::write(&certificate, cert.pem()).unwrap();
        std::fs::write(&private_key, signing_key.serialize_pem()).unwrap();
        let config = ResolvedTlsConfig {
            certificate: certificate.clone(),
            private_key,
            client_ca_certificate: certificate,
        };
        load_tls_config(&config).unwrap();
    }
}
