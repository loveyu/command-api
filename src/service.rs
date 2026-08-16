#[cfg(not(windows))]
use anyhow::{Result, bail};
#[cfg(not(windows))]
use std::path::Path;

#[cfg(windows)]
mod windows {
    use crate::{config::Config, runtime};
    use anyhow::{Context, Result};
    use std::{
        ffi::OsString,
        path::{Path, PathBuf},
        sync::{OnceLock, mpsc},
        time::Duration,
    };
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
            ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
    static SERVICE_NAME: OnceLock<String> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    pub fn run(config_path: &Path) -> Result<()> {
        let config = Config::load(config_path)?;
        CONFIG_PATH
            .set(config.source_path)
            .map_err(|_| anyhow::anyhow!("Windows Service 配置路径已经初始化"))?;
        SERVICE_NAME
            .set(config.windows_service.name.clone())
            .map_err(|_| anyhow::anyhow!("Windows Service 名称已经初始化"))?;
        service_dispatcher::start(&config.windows_service.name, ffi_service_main)
            .context("无法连接 Windows Service Control Manager")
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service_main() {
            eprintln!("command-api service error: {error:#}");
        }
    }

    fn run_service_main() -> Result<()> {
        let service_name = SERVICE_NAME.get().context("Windows Service 名称未初始化")?.clone();
        let config_path = CONFIG_PATH.get().context("Windows Service 配置路径未初始化")?.clone();
        let (stop_event_tx, stop_event_rx) = mpsc::channel::<()>();
        let status_handle = service_control_handler::register(service_name, move |control| match control {
            ServiceControl::Stop => {
                let _ = stop_event_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

        status_handle.set_service_status(status(ServiceState::StartPending, ServiceControlAccept::empty(), 1))?;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        status_handle.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP, 0))?;
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let status_for_stop = status_handle;
        std::thread::spawn(move || {
            if stop_event_rx.recv().is_ok() {
                let _ = status_for_stop.set_service_status(status(
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    1,
                ));
                let _ = shutdown_tx.send(());
            }
        });
        let result = runtime.block_on(runtime::run(
            &config_path,
            async move {
                let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
            },
            false,
        ));
        status_handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), 0))?;
        result
    }

    fn status(state: ServiceState, accepted: ServiceControlAccept, checkpoint: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint,
            wait_hint: Duration::from_secs(if checkpoint == 0 { 0 } else { 10 }),
            process_id: None,
        }
    }

    pub fn install(config_path: &Path) -> Result<()> {
        let config = Config::load(config_path)?;
        let executable = std::env::current_exe()?.canonicalize()?;
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: config.windows_service.name.clone().into(),
            display_name: config.windows_service.display_name.clone().into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable,
            launch_arguments: vec![
                "service".into(),
                "run".into(),
                "--config".into(),
                config.source_path.as_os_str().to_owned(),
            ],
            dependencies: vec![],
            account_name: Some("NT AUTHORITY\\LocalService".into()),
            account_password: None,
        };
        let service = manager.create_service(
            &info,
            ServiceAccess::QUERY_STATUS
                | ServiceAccess::START
                | ServiceAccess::STOP
                | ServiceAccess::DELETE
                | ServiceAccess::CHANGE_CONFIG,
        )?;
        service.set_description(config.windows_service.description)?;
        Ok(())
    }

    pub fn start(config_path: &Path) -> Result<()> {
        let (manager, config) = manager_and_config(config_path)?;
        let service = manager.open_service(config.windows_service.name, ServiceAccess::START)?;
        service.start::<&str>(&[])?;
        Ok(())
    }

    pub fn stop(config_path: &Path) -> Result<()> {
        let (manager, config) = manager_and_config(config_path)?;
        let service = manager.open_service(
            config.windows_service.name,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )?;
        service.stop()?;
        Ok(())
    }

    pub fn uninstall(config_path: &Path) -> Result<()> {
        let (manager, config) = manager_and_config(config_path)?;
        let service = manager.open_service(
            config.windows_service.name,
            ServiceAccess::DELETE | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            let _ = service.stop();
        }
        service.delete()?;
        Ok(())
    }

    fn manager_and_config(config_path: &Path) -> Result<(ServiceManager, crate::config::ResolvedConfig)> {
        let config = Config::load(config_path)?;
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        Ok((manager, config))
    }
}

#[cfg(windows)]
pub use windows::{install, run, start, stop, uninstall};

#[cfg(not(windows))]
pub fn run(_config_path: &Path) -> Result<()> {
    bail!("Windows Service 仅支持 Windows")
}

#[cfg(not(windows))]
pub fn install(_config_path: &Path) -> Result<()> {
    bail!("Windows Service 仅支持 Windows")
}

#[cfg(not(windows))]
pub fn start(_config_path: &Path) -> Result<()> {
    bail!("Windows Service 仅支持 Windows")
}

#[cfg(not(windows))]
pub fn stop(_config_path: &Path) -> Result<()> {
    bail!("Windows Service 仅支持 Windows")
}

#[cfg(not(windows))]
pub fn uninstall(_config_path: &Path) -> Result<()> {
    bail!("Windows Service 仅支持 Windows")
}
