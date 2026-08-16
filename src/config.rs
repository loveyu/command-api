use anyhow::{Context, Result, bail};
use ipnet::IpNet;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub const DEFAULT_CONFIG_PATH: &str = "config.yaml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub access: AccessConfig,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
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
    pub host: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub token: TokenSource,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenSource {
    Environment { variable: String },
    WindowsDpapi { file: PathBuf },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    pub allowed_cidrs: Vec<IpNet>,
    #[serde(default)]
    pub token_failure_cooldown: TokenFailureCooldownConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenFailureCooldownConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_token_failure_cooldown_seconds")]
    pub seconds: u64,
    #[serde(default = "default_token_failure_max_tracked_ips")]
    pub max_tracked_ips: usize,
}

impl Default for TokenFailureCooldownConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            seconds: default_token_failure_cooldown_seconds(),
            max_tracked_ips: default_token_failure_max_tracked_ips(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub client_ca_certificate: PathBuf,
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
    #[serde(default)]
    pub account: WindowsServiceAccount,
}

impl Default for WindowsServiceConfig {
    fn default() -> Self {
        Self {
            name: default_service_name(),
            display_name: default_service_display_name(),
            description: default_service_description(),
            account: WindowsServiceAccount::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowsServiceAccount {
    #[default]
    LocalService,
    LocalSystem,
}

impl WindowsServiceAccount {
    #[cfg(windows)]
    pub const fn account_name(self) -> &'static str {
        match self {
            Self::LocalService => "NT AUTHORITY\\LocalService",
            Self::LocalSystem => "NT AUTHORITY\\LocalSystem",
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
    #[serde(default)]
    pub allowed_values: Vec<String>,
    #[serde(default)]
    pub allowed_patterns: Vec<String>,
}

impl Default for RequestArgsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_count: default_max_arg_count(),
            max_item_bytes: default_max_arg_bytes(),
            max_total_bytes: default_max_total_arg_bytes(),
            allowed_values: Vec::new(),
            allowed_patterns: Vec::new(),
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
            Self::Pwsh | Self::Powershell => &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ],
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
    pub access: AccessConfig,
    pub tls: Option<ResolvedTlsConfig>,
    pub auth: ResolvedAuthConfig,
    pub logging: LoggingConfig,
    pub execution: ExecutionConfig,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub windows_service: WindowsServiceConfig,
    pub routes: Vec<ResolvedRoute>,
}

#[derive(Debug, Clone)]
pub struct ResolvedTlsConfig {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub client_ca_certificate: PathBuf,
}

#[derive(Clone)]
pub struct ResolvedAuthConfig {
    pub token: Zeroizing<String>,
}

impl std::fmt::Debug for ResolvedAuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedAuthConfig")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub path: String,
    pub executor: Executor,
    pub program: PathBuf,
    pub script: PathBuf,
    pub fixed_args: Vec<String>,
    pub request_args: RequestArgsConfig,
    pub request_arg_patterns: Vec<Regex>,
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
        validate_bind_address(self.server.host)?;
        validate_access(&self.access)?;
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
        let auth = ResolvedAuthConfig {
            token: Zeroizing::new(resolve_token(base, self.auth.token)?),
        };
        let tls = resolve_tls(base, self.tls, self.server.host)?;
        let log_directory = absolute_from(base, &self.logging.directory);
        fs::create_dir_all(log_directory.join("tasks"))
            .with_context(|| format!("无法创建日志目录 {}", log_directory.display()))?;
        let log_directory = fs::canonicalize(&log_directory)
            .with_context(|| format!("无法访问日志目录 {}", log_directory.display()))?;

        let mut paths = HashSet::new();
        let mut routes = Vec::with_capacity(self.routes.len());
        for route in self.routes {
            validate_route(&route)?;
            if self.windows_service.account == WindowsServiceAccount::LocalSystem
                && route.request_args.enabled
                && route.request_args.allowed_values.is_empty()
                && route.request_args.allowed_patterns.is_empty()
            {
                bail!(
                    "LocalSystem 路由 {} 启用动态参数时必须配置 allowed_values 或 allowed_patterns",
                    route.path
                );
            }
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
            let request_arg_patterns = route
                .request_args
                .allowed_patterns
                .iter()
                .map(|pattern| {
                    Regex::new(pattern).with_context(|| format!("路由 {} 的动态参数正则无效: {pattern}", route.path))
                })
                .collect::<Result<Vec<_>>>()?;

            routes.push(ResolvedRoute {
                path: route.path,
                executor: route.executor,
                program,
                script,
                fixed_args: route.fixed_args,
                request_args: route.request_args,
                request_arg_patterns,
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
            access: self.access,
            tls,
            auth,
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
    if matches!(path, "/" | "/healthz")
        || path == "/tasks"
        || path.starts_with("/tasks/")
        || path == "/system"
        || path.starts_with("/system/")
    {
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
        let has_allowlist = !route.request_args.allowed_values.is_empty() || !route.request_arg_patterns.is_empty();
        if has_allowlist
            && !route.request_args.allowed_values.iter().any(|value| value == arg)
            && !route.request_arg_patterns.iter().any(|pattern| pattern.is_match(arg))
        {
            bail!("动态参数不符合路由允许值或正则规则");
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

fn resolve_token(base: &Path, source: TokenSource) -> Result<String> {
    let token = match source {
        TokenSource::Environment { variable } => {
            if variable.trim().is_empty() || variable.contains(['=', '\0']) {
                bail!("auth.token.variable 非法");
            }
            std::env::var(&variable).with_context(|| format!("环境变量 {variable} 未设置或不是 UTF-8"))?
        }
        TokenSource::WindowsDpapi { file } => {
            let path = absolute_from(base, &file);
            let path = fs::canonicalize(&path).with_context(|| format!("DPAPI 密钥文件不存在: {}", path.display()))?;
            crate::secret::unprotect_from_file(&path)?
        }
    };
    crate::secret::validate_token(&token)?;
    Ok(token)
}

fn resolve_tls(base: &Path, tls: Option<TlsConfig>, host: IpAddr) -> Result<Option<ResolvedTlsConfig>> {
    let Some(tls) = tls else {
        if !host.is_loopback() {
            bail!("非 loopback 内网监听必须配置 TLS/mTLS");
        }
        return Ok(None);
    };
    Ok(Some(ResolvedTlsConfig {
        certificate: canonical_file(base, &tls.certificate, "TLS 服务器证书")?,
        private_key: canonical_file(base, &tls.private_key, "TLS 服务器私钥")?,
        client_ca_certificate: canonical_file(base, &tls.client_ca_certificate, "TLS 客户端 CA 证书")?,
    }))
}

fn canonical_file(base: &Path, value: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute_from(base, value);
    let path = fs::canonicalize(&path).with_context(|| format!("{label}不存在: {}", path.display()))?;
    if !path.is_file() {
        bail!("{label}不是文件: {}", path.display());
    }
    Ok(path)
}

fn validate_bind_address(host: IpAddr) -> Result<()> {
    if !is_private_or_local_ip(host) {
        bail!("server.host 必须是明确的私有、loopback 或 link-local IP，禁止公网和全地址监听: {host}");
    }
    Ok(())
}

fn validate_access(access: &AccessConfig) -> Result<()> {
    if access.allowed_cidrs.is_empty() {
        bail!("access.allowed_cidrs 至少需要一个 CIDR");
    }
    for cidr in &access.allowed_cidrs {
        if !is_private_or_local_net(*cidr) {
            bail!("access.allowed_cidrs 仅允许私有、loopback 或 link-local 网段: {cidr}");
        }
    }
    if access.token_failure_cooldown.enabled
        && (access.token_failure_cooldown.seconds == 0 || access.token_failure_cooldown.max_tracked_ips == 0)
    {
        bail!("启用 token_failure_cooldown 时 seconds 和 max_tracked_ips 必须大于 0");
    }
    Ok(())
}

pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(ipv6)),
        value => value,
    }
}

fn is_private_or_local_ip(ip: IpAddr) -> bool {
    match normalize_ip(ip) {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.is_loopback() || is_ipv6_unique_local(ip) || is_ipv6_link_local(ip),
    }
}

fn is_private_or_local_net(net: IpNet) -> bool {
    const PRIVATE_NETS: [&str; 8] = [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "::1/128",
        "fc00::/7",
        "fe80::/10",
    ];
    PRIVATE_NETS
        .iter()
        .filter_map(|value| value.parse::<IpNet>().ok())
        .any(|parent| parent.contains(&net))
}

fn is_ipv6_unique_local(ip: Ipv6Addr) -> bool {
    ip.octets()[0] & 0xfe == 0xfc
}

fn is_ipv6_link_local(ip: Ipv6Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
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

const fn default_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
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

const fn default_token_failure_cooldown_seconds() -> u64 {
    10
}

const fn default_token_failure_max_tracked_ips() -> usize {
    4096
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
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File"
            ]
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

    #[test]
    fn only_private_or_local_networks_are_accepted() {
        assert!(validate_bind_address("10.132.1.145".parse().unwrap()).is_ok());
        assert!(validate_bind_address("127.0.0.1".parse().unwrap()).is_ok());
        assert!(validate_bind_address("0.0.0.0".parse().unwrap()).is_err());
        assert!(validate_bind_address("8.8.8.8".parse().unwrap()).is_err());
        assert!(is_private_or_local_net("10.132.1.1/32".parse().unwrap()));
        assert!(is_private_or_local_net("fc00::/16".parse().unwrap()));
        assert!(!is_private_or_local_net("0.0.0.0/0".parse().unwrap()));
        assert!(!is_private_or_local_net("2001:db8::/32".parse().unwrap()));
    }

    #[test]
    fn normalizes_ipv4_mapped_ipv6() {
        assert_eq!(
            normalize_ip("::ffff:10.132.1.1".parse().unwrap()),
            "10.132.1.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn request_argument_allowlist_accepts_values_and_patterns() {
        let route = ResolvedRoute {
            path: "/safe".to_owned(),
            executor: Executor::Sh,
            program: "sh".into(),
            script: "test.sh".into(),
            fixed_args: Vec::new(),
            request_args: RequestArgsConfig {
                enabled: true,
                allowed_values: vec!["status".to_owned()],
                allowed_patterns: vec!["^[a-z0-9_-]{1,16}$".to_owned()],
                ..Default::default()
            },
            request_arg_patterns: vec![Regex::new("^[a-z0-9_-]{1,16}$").unwrap()],
            working_directory: ".".into(),
            env: BTreeMap::new(),
            max_concurrency: 1,
            max_execution_seconds: 1,
            graceful_shutdown_seconds: 1,
            merge_stdout_stderr: false,
            output_encoding: OutputEncoding::Utf8,
        };
        assert!(validate_request_args(&route, &["status".to_owned(), "node-1".to_owned()]).is_ok());
        assert!(validate_request_args(&route, &["; Remove-Item C:\\*".to_owned()]).is_err());
    }
}
