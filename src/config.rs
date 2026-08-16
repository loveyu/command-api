use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub windows_service: WindowsServiceConfig,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub directory: PathBuf,
    #[serde(default = "default_retention_seconds")]
    pub retention_seconds: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes_per_task: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    #[serde(default = "default_max_total_concurrency")]
    pub max_total_concurrency: usize,
    #[serde(default = "default_shutdown_timeout_seconds")]
    pub shutdown_timeout_seconds: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_total_concurrency: default_max_total_concurrency(),
            shutdown_timeout_seconds: default_shutdown_timeout_seconds(),
        }
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsServiceConfig {
    #[serde(default = "default_service_name")]
    pub name: String,
    #[serde(default = "default_service_display_name")]
    pub display_name: String,
    #[serde(default = "default_service_description")]
    pub description: String,
}

impl Default for WindowsServiceConfig {
    fn default() -> Self {
        Self {
            name: default_service_name(),
            display_name: default_service_display_name(),
            description: default_service_description(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub path: String,
    pub executor: Executor,
    #[serde(default)]
    pub program: Option<PathBuf>,
    pub script: PathBuf,
    #[serde(default)]
    pub fixed_args: Vec<String>,
    #[serde(default)]
    pub request_args: RequestArgsConfig,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub max_concurrency: usize,
    pub max_execution_seconds: u64,
    pub graceful_shutdown_seconds: u64,
    #[serde(default)]
    pub merge_stdout_stderr: bool,
    #[serde(default)]
    pub output_encoding: OutputEncoding,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestArgsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_arg_count")]
    pub max_count: usize,
    #[serde(default = "default_max_arg_bytes")]
    pub max_item_bytes: usize,
    #[serde(default = "default_max_total_arg_bytes")]
    pub max_total_bytes: usize,
}

impl Default for RequestArgsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_count: default_max_arg_count(),
            max_item_bytes: default_max_arg_bytes(),
            max_total_bytes: default_max_total_arg_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Executor {
    Sh,
    Bash,
    Zsh,
    Pwsh,
    Powershell,
    Cmd,
}

impl Executor {
    pub fn default_program(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Pwsh => "pwsh",
            Self::Powershell => {
                if cfg!(windows) {
                    "powershell.exe"
                } else {
                    "powershell"
                }
            }
            Self::Cmd => "cmd.exe",
        }
    }

    pub fn launcher_args(self) -> &'static [&'static str] {
        match self {
            Self::Pwsh | Self::Powershell => &["-NoLogo", "-NoProfile", "-NonInteractive", "-File"],
            Self::Cmd => &["/D", "/S", "/C"],
            Self::Sh | Self::Bash | Self::Zsh => &[],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum OutputEncoding {
    #[default]
    #[serde(rename = "utf-8", alias = "utf8")]
    Utf8,
    #[serde(rename = "gbk")]
    Gbk,
    #[serde(rename = "utf-16le", alias = "utf16le")]
    Utf16le,
}

impl OutputEncoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Gbk => "gbk",
            Self::Utf16le => "utf-16le",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub source_path: PathBuf,
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
    pub execution: ExecutionConfig,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub windows_service: WindowsServiceConfig,
    pub routes: Vec<ResolvedRoute>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub path: String,
    pub executor: Executor,
    pub program: PathBuf,
    pub script: PathBuf,
    pub fixed_args: Vec<String>,
    pub request_args: RequestArgsConfig,
    pub working_directory: PathBuf,
    pub env: BTreeMap<String, String>,
    pub max_concurrency: usize,
    pub max_execution_seconds: u64,
    pub graceful_shutdown_seconds: u64,
    pub merge_stdout_stderr: bool,
    pub output_encoding: OutputEncoding,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<ResolvedConfig> {
        let source_path =
            fs::canonicalize(path.as_ref()).with_context(|| format!("无法读取配置文件 {}", path.as_ref().display()))?;
        let content =
            fs::read_to_string(&source_path).with_context(|| format!("无法读取配置文件 {}", source_path.display()))?;
        let config: Config =
            serde_yaml::from_str(&content).with_context(|| format!("无法解析 YAML 配置 {}", source_path.display()))?;
        config.resolve(source_path)
    }

    fn resolve(self, source_path: PathBuf) -> Result<ResolvedConfig> {
        if self.auth.token.trim().is_empty() {
            bail!("auth.token 不能为空");
        }
        if self.execution.max_total_concurrency == 0 {
            bail!("execution.max_total_concurrency 必须大于 0");
        }
        if self.execution.shutdown_timeout_seconds == 0 {
            bail!("execution.shutdown_timeout_seconds 必须大于 0");
        }
        if self.logging.retention_seconds == 0 {
            bail!("logging.retention_seconds 必须大于 0");
        }
        if self.logging.max_output_bytes_per_task == 0 {
            bail!("logging.max_output_bytes_per_task 必须大于 0");
        }
        if self.routes.is_empty() {
            bail!("routes 至少需要配置一条路由");
        }
        validate_service_name(&self.windows_service.name)?;

        let base = source_path.parent().context("配置文件没有父目录")?;
        let log_directory = absolute_from(base, &self.logging.directory);
        fs::create_dir_all(log_directory.join("tasks"))
            .with_context(|| format!("无法创建日志目录 {}", log_directory.display()))?;
        let log_directory = fs::canonicalize(&log_directory)
            .with_context(|| format!("无法访问日志目录 {}", log_directory.display()))?;

        let mut paths = HashSet::new();
        let mut routes = Vec::with_capacity(self.routes.len());
        for route in self.routes {
            validate_route(&route)?;
            if !paths.insert(route.path.clone()) {
                bail!("路由 {} 重复配置", route.path);
            }

            let script = absolute_from(base, &route.script);
            let script = fs::canonicalize(&script)
                .with_context(|| format!("路由 {} 的脚本不存在: {}", route.path, script.display()))?;
            if !script.is_file() {
                bail!("路由 {} 的脚本不是文件: {}", route.path, script.display());
            }

            let working_directory = match route.working_directory {
                Some(directory) => absolute_from(base, &directory),
                None => script.parent().context("脚本没有父目录")?.to_path_buf(),
            };
            let working_directory = fs::canonicalize(&working_directory)
                .with_context(|| format!("路由 {} 的工作目录不存在: {}", route.path, working_directory.display()))?;
            if !working_directory.is_dir() {
                bail!(
                    "路由 {} 的工作目录不是目录: {}",
                    route.path,
                    working_directory.display()
                );
            }

            let program = route
                .program
                .map(|program| absolute_if_path_like(base, program))
                .unwrap_or_else(|| PathBuf::from(route.executor.default_program()));

            routes.push(ResolvedRoute {
                path: route.path,
                executor: route.executor,
                program,
                script,
                fixed_args: route.fixed_args,
                request_args: route.request_args,
                working_directory,
                env: route.env,
                max_concurrency: route.max_concurrency,
                max_execution_seconds: route.max_execution_seconds,
                graceful_shutdown_seconds: route.graceful_shutdown_seconds,
                merge_stdout_stderr: route.merge_stdout_stderr,
                output_encoding: route.output_encoding,
            });
        }

        Ok(ResolvedConfig {
            source_path,
            server: self.server,
            auth: self.auth,
            logging: LoggingConfig {
                directory: log_directory,
                ..self.logging
            },
            execution: self.execution,
            windows_service: self.windows_service,
            routes,
        })
    }
}

fn validate_route(route: &RouteConfig) -> Result<()> {
    let path = route.path.as_str();
    if !path.starts_with('/') || path.len() < 2 || path.ends_with('/') {
        bail!("路由路径必须以 / 开头且不能以 / 结尾: {path}");
    }
    if path.contains(['?', '#']) || path.split('/').any(|segment| segment == "..") {
        bail!("路由路径包含非法内容: {path}");
    }
    if matches!(path, "/" | "/healthz") || path == "/tasks" || path.starts_with("/tasks/") {
        bail!("路由路径与系统接口冲突: {path}");
    }
    if route.max_concurrency == 0 {
        bail!("路由 {path} 的 max_concurrency 必须大于 0");
    }
    if route.max_execution_seconds == 0 {
        bail!("路由 {path} 的 max_execution_seconds 必须大于 0");
    }
    if route.graceful_shutdown_seconds == 0 {
        bail!("路由 {path} 的 graceful_shutdown_seconds 必须大于 0");
    }
    if route.request_args.enabled {
        let script_is_batch = route
            .script
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "bat" | "cmd"));
        let program_is_cmd = route
            .program
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("cmd.exe"));
        if route.executor == Executor::Cmd || script_is_batch || program_is_cmd {
            bail!("路由 {path} 使用 cmd/.bat/.cmd 时禁止启用动态请求参数");
        }
        if route.request_args.max_count == 0
            || route.request_args.max_item_bytes == 0
            || route.request_args.max_total_bytes == 0
        {
            bail!("路由 {path} 的动态参数限制必须大于 0");
        }
    }
    validate_args(path, &route.fixed_args)?;
    Ok(())
}

pub fn validate_request_args(route: &ResolvedRoute, args: &[String]) -> Result<()> {
    if !route.request_args.enabled && !args.is_empty() {
        bail!("路由 {} 不允许传递动态参数", route.path);
    }
    if args.len() > route.request_args.max_count {
        bail!("动态参数数量超过限制 {}", route.request_args.max_count);
    }
    let mut total = 0usize;
    for arg in args {
        if arg.contains('\0') {
            bail!("动态参数不能包含 NUL 字符");
        }
        let size = arg.len();
        if size > route.request_args.max_item_bytes {
            bail!("单个动态参数超过 {} 字节", route.request_args.max_item_bytes);
        }
        total = total.saturating_add(size);
    }
    if total > route.request_args.max_total_bytes {
        bail!("动态参数总大小超过 {} 字节", route.request_args.max_total_bytes);
    }
    Ok(())
}

fn validate_args(route: &str, args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg.contains('\0')) {
        bail!("路由 {route} 的固定参数不能包含 NUL 字符");
    }
    Ok(())
}

fn validate_service_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.contains(['/', '\\']) {
        bail!("windows_service.name 非法: {name}");
    }
    Ok(())
}

fn absolute_from(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

fn absolute_if_path_like(base: &Path, value: PathBuf) -> PathBuf {
    if value.is_absolute() || value.components().count() > 1 {
        absolute_from(base, &value)
    } else {
        value
    }
}

fn default_host() -> String {
    "0.0.0.0".to_owned()
}

const fn default_port() -> u16 {
    27415
}

const fn default_retention_seconds() -> u64 {
    86_400
}

const fn default_max_output_bytes() -> u64 {
    100 * 1024 * 1024
}

const fn default_max_total_concurrency() -> usize {
    16
}

const fn default_shutdown_timeout_seconds() -> u64 {
    30
}

const fn default_max_arg_count() -> usize {
    32
}

const fn default_max_arg_bytes() -> usize {
    4096
}

const fn default_max_total_arg_bytes() -> usize {
    16 * 1024
}

fn default_service_name() -> String {
    "command-api".to_owned()
}

fn default_service_display_name() -> String {
    "Command API".to_owned()
}

fn default_service_description() -> String {
    "通过受控 HTTP API 异步执行预配置脚本".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_argument_order_is_stable() {
        assert_eq!(
            Executor::Pwsh.launcher_args(),
            ["-NoLogo", "-NoProfile", "-NonInteractive", "-File"]
        );
        assert_eq!(Executor::Bash.launcher_args(), [] as [&str; 0]);
    }

    #[test]
    fn cmd_cannot_enable_untrusted_arguments() {
        let route = RouteConfig {
            path: "/cmd".to_owned(),
            executor: Executor::Cmd,
            program: None,
            script: "test.cmd".into(),
            fixed_args: vec![],
            request_args: RequestArgsConfig {
                enabled: true,
                ..Default::default()
            },
            working_directory: None,
            env: BTreeMap::new(),
            max_concurrency: 1,
            max_execution_seconds: 10,
            graceful_shutdown_seconds: 1,
            merge_stdout_stderr: false,
            output_encoding: OutputEncoding::Utf8,
        };
        assert!(validate_route(&route).is_err());
    }
}
