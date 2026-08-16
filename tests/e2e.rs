use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use reqwest::{Certificate, Client, Identity, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Once,
    time::Duration,
};
use tempfile::TempDir;

struct Server {
    child: Child,
    _temp: TempDir,
    base_url: String,
    secondary_base_url: String,
    client: Client,
    config_path: PathBuf,
    token: String,
}

struct TlsServer {
    child: Child,
    _temp: TempDir,
    base_url: String,
    token: String,
    ca_pem: Vec<u8>,
    client_identity_pem: Vec<u8>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start() -> Self {
        install_crypto_provider();
        const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        const RELOADED_TOKEN: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let temp = tempfile::tempdir().unwrap();
        let port = free_port();
        let secondary_port = distinct_free_port(port);
        let (executor, script_name, script) = test_script();
        let script_path = temp.path().join(script_name);
        fs::write(&script_path, script).unwrap();
        let config_path = temp.path().join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"server:
  listeners:
    - host: 127.0.0.1
      port: {port}
    - host: 127.0.0.1
      port: {secondary_port}
access:
  allowed_cidrs:
    - 127.0.0.0/8
auth:
  token:
    provider: environment
    variable: COMMAND_API_E2E_TOKEN
logging:
  directory: ./logs
  retention_seconds: 3600
  max_output_bytes_per_task: 1048576
execution:
  max_total_concurrency: 2
  shutdown_timeout_seconds: 5
routes:
  - path: /run
    executor: {executor}
    script: {script}
    fixed_args: [fixed]
    request_args:
      enabled: true
      max_count: 4
      max_item_bytes: 1024
      max_total_bytes: 2048
    max_concurrency: 1
    max_execution_seconds: 10
    graceful_shutdown_seconds: 1
  - path: /merged
    executor: {executor}
    script: {script}
    fixed_args: [fixed]
    request_args:
      enabled: true
    max_concurrency: 1
    max_execution_seconds: 10
    graceful_shutdown_seconds: 1
    merge_stdout_stderr: true
  - path: /timeout
    executor: {executor}
    script: {script}
    fixed_args: [fixed]
    request_args:
      enabled: true
    max_concurrency: 1
    max_execution_seconds: 1
    graceful_shutdown_seconds: 1
"#,
                script = yaml_path(&script_path),
            ),
        )
        .unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_command-api"))
            .arg("run")
            .arg("--config")
            .arg(&config_path)
            .env("COMMAND_API_E2E_TOKEN", TOKEN)
            .env("COMMAND_API_E2E_TOKEN_RELOADED", RELOADED_TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let server = Self {
            child,
            _temp: temp,
            base_url: format!("http://127.0.0.1:{port}"),
            secondary_base_url: format!("http://127.0.0.1:{secondary_port}"),
            client: Client::new(),
            config_path,
            token: TOKEN.to_owned(),
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&self) {
        for base_url in [&self.base_url, &self.secondary_base_url] {
            for _ in 0..100 {
                if self
                    .client
                    .get(format!("{base_url}/healthz"))
                    .bearer_auth(&self.token)
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(
                self.client
                    .get(format!("{base_url}/healthz"))
                    .bearer_auth(&self.token)
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == StatusCode::OK),
                "command-api did not become ready on {base_url}"
            );
        }
    }

    async fn execute(&self, route: &str, args: &[&str]) -> reqwest::Response {
        self.client
            .post(format!("{}{route}", self.base_url))
            .bearer_auth(&self.token)
            .json(&json!({ "args": args }))
            .send()
            .await
            .unwrap()
    }

    async fn wait_finished(&self, id: &str) -> Value {
        for _ in 0..200 {
            let response = self
                .client
                .get(format!("{}/tasks/{id}", self.base_url))
                .bearer_auth(&self.token)
                .send()
                .await;
            let Ok(response) = response else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let Ok(value) = response.json::<Value>().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            if matches!(
                value["status"].as_str(),
                Some("succeeded" | "failed" | "timed_out" | "cancelled" | "killed" | "interrupted")
            ) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("task {id} did not finish");
    }

    async fn output(&self, id: &str, stream: &str) -> Value {
        self.client
            .get(format!("{}/tasks/{id}/output?stream={stream}", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn wait_process_exit(&mut self) {
        for _ in 0..200 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("command-api did not stop");
    }
}

impl TlsServer {
    async fn start() -> Self {
        install_crypto_provider();
        const TOKEN: &str = "123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0";
        let temp = tempfile::tempdir().unwrap();
        let port = free_port();
        let (executor, script_name, script) = test_script();
        let script_path = temp.path().join(script_name);
        fs::write(&script_path, script).unwrap();
        let (ca_pem, server_certificate, server_key, client_identity_pem) = test_pki();
        fs::write(temp.path().join("ca.crt"), &ca_pem).unwrap();
        fs::write(temp.path().join("server.crt"), server_certificate).unwrap();
        fs::write(temp.path().join("server.key"), server_key).unwrap();
        let config_path = temp.path().join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"server:
  host: 127.0.0.1
  port: {port}
access:
  allowed_cidrs: [127.0.0.0/8]
tls:
  certificate: ./server.crt
  private_key: ./server.key
  client_ca_certificate: ./ca.crt
auth:
  token:
    provider: environment
    variable: COMMAND_API_TLS_TEST_TOKEN
logging:
  directory: ./logs
routes:
  - path: /run
    executor: {executor}
    script: {script}
    max_concurrency: 1
    max_execution_seconds: 5
    graceful_shutdown_seconds: 1
"#,
                script = yaml_path(&script_path),
            ),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_command-api"))
            .arg("run")
            .arg("--config")
            .arg(&config_path)
            .env("COMMAND_API_TLS_TEST_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let server = Self {
            child,
            _temp: temp,
            base_url: format!("https://127.0.0.1:{port}"),
            token: TOKEN.to_owned(),
            ca_pem,
            client_identity_pem,
        };
        server.wait_ready().await;
        server
    }

    fn client(&self, with_identity: bool) -> Client {
        let mut builder = Client::builder()
            .tls_backend_rustls()
            .tls_certs_only([Certificate::from_pem(&self.ca_pem).unwrap()]);
        if with_identity {
            builder = builder.identity(Identity::from_pem(&self.client_identity_pem).unwrap());
        }
        builder.build().unwrap()
    }

    async fn wait_ready(&self) {
        let client = self.client(true);
        let mut last_error = None;
        for _ in 0..300 {
            match client
                .get(format!("{}/healthz", self.base_url))
                .bearer_auth(&self.token)
                .send()
                .await
            {
                Ok(response) if response.status() == StatusCode::OK => return,
                Ok(response) => last_error = Some(format!("HTTP {}", response.status())),
                Err(error) => last_error = Some(format!("{error:#}")),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "mTLS command-api did not become ready; last probe result: {}",
            last_error.as_deref().unwrap_or("no probe result")
        );
    }
}

#[tokio::test]
async fn executes_arguments_captures_streams_merges_and_times_out() {
    let server = Server::start().await;

    let unauthorized = server.client.get(format!("{}/", server.base_url)).send().await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let response = server
        .client
        .post(format!("{}/run", server.secondary_base_url))
        .bearer_auth(&server.token)
        .json(&json!({ "args": ["secondary", "listener"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(
        server.wait_finished(accepted["task_id"].as_str().unwrap()).await["status"],
        "succeeded"
    );

    let response = server.execute("/run", &["hello world", "tail"]).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["task_id"].as_str().unwrap();
    let task = server.wait_finished(id).await;
    if task["status"] != "succeeded" {
        let stdout = server.output(id, "stdout").await;
        let stderr = server.output(id, "stderr").await;
        panic!("task failed: {task:#}; stdout: {stdout:#}; stderr: {stderr:#}");
    }
    let stdout = server.output(id, "stdout").await;
    let stderr = server.output(id, "stderr").await;
    assert!(
        stdout["content"]
            .as_str()
            .unwrap()
            .contains("OUT:fixed|hello world|tail")
    );
    assert!(stderr["content"].as_str().unwrap().contains("ERR:tail"));

    let response = server.execute("/merged", &["merged", "line"]).await;
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["task_id"].as_str().unwrap();
    assert_eq!(server.wait_finished(id).await["status"], "succeeded");
    let output = server.output(id, "combined").await;
    let content = output["content"].as_str().unwrap();
    assert!(content.contains("OUT:fixed|merged|line"));
    assert!(content.contains("ERR:line"));

    let response = server.execute("/timeout", &["sleep", "30"]).await;
    let accepted: Value = response.json().await.unwrap();
    let cancellable_id = accepted["task_id"].as_str().unwrap();
    let rejected = server.execute("/timeout", &["sleep", "30"]).await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    let cancel = server
        .client
        .post(format!("{}/tasks/{cancellable_id}/cancel", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(cancel.status(), StatusCode::ACCEPTED);
    assert_eq!(server.wait_finished(cancellable_id).await["status"], "cancelled");

    let response = server.execute("/timeout", &["sleep", "30"]).await;
    let accepted: Value = response.json().await.unwrap();
    let kill_id = accepted["task_id"].as_str().unwrap();
    let kill = server
        .client
        .post(format!("{}/tasks/{kill_id}/kill", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(kill.status(), StatusCode::ACCEPTED);
    let killed = server.wait_finished(kill_id).await;
    assert_eq!(killed["status"], "killed");
    assert_eq!(killed["termination"]["reason"], "force_killed");
    assert_eq!(killed["termination"]["graceful_attempted"], false);
    assert_eq!(killed["termination"]["forced"], true);

    let response = server.execute("/timeout", &["sleep", "30"]).await;
    let accepted: Value = response.json().await.unwrap();
    let timeout_id = accepted["task_id"].as_str().unwrap();
    let task = server.wait_finished(timeout_id).await;
    assert_eq!(task["status"], "timed_out");
    assert_eq!(task["termination"]["reason"], "timeout");
}

#[tokio::test]
async fn validates_restart_configuration_restarts_and_stops() {
    let mut server = Server::start().await;

    let unauthorized = server
        .client
        .post(format!("{}/system/stop", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let valid_config = fs::read_to_string(&server.config_path).unwrap();
    fs::write(&server.config_path, "not: [valid").unwrap();
    let invalid_restart = server
        .client
        .post(format!("{}/system/restart", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_restart.status(), StatusCode::CONFLICT);
    server.wait_ready().await;
    let reloaded_config = valid_config.replace(
        "variable: COMMAND_API_E2E_TOKEN",
        "variable: COMMAND_API_E2E_TOKEN_RELOADED",
    );
    fs::write(&server.config_path, reloaded_config).unwrap();

    let response = server.execute("/run", &["sleep", "30"]).await;
    let accepted: Value = response.json().await.unwrap();
    let interrupted_id = accepted["task_id"].as_str().unwrap();
    let restart = server
        .client
        .post(format!("{}/system/restart", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(restart.status(), StatusCode::ACCEPTED);
    server.token = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_owned();

    let interrupted = server.wait_finished(interrupted_id).await;
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["termination"]["reason"], "server_restart");
    assert!(server.child.try_wait().unwrap().is_none());

    let response = server.execute("/run", &["after", "restart"]).await;
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(
        server.wait_finished(accepted["task_id"].as_str().unwrap()).await["status"],
        "succeeded"
    );

    let stop = server
        .client
        .post(format!("{}/system/stop", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(stop.status(), StatusCode::ACCEPTED);
    server.wait_process_exit().await;
}

#[tokio::test]
async fn requires_a_trusted_mtls_client_certificate() {
    let server = TlsServer::start().await;
    let without_identity = server
        .client(false)
        .get(format!("{}/healthz", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await;
    assert!(without_identity.is_err());

    let valid = server
        .client(true)
        .get(format!("{}/healthz", server.base_url))
        .bearer_auth(&server.token)
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), StatusCode::OK);
}

#[test]
fn pbkdf2_sha256_cli_outputs_a_complete_phc_yaml_fragment_without_the_token() {
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut child = Command::new(env!("CARGO_BIN_EXE_command-api"))
        .args(["secret", "hash", "--stdin", "--rounds", "1000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(TOKEN.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let yaml = String::from_utf8(output.stdout).unwrap();
    assert!(!yaml.contains(TOKEN));
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    let token = &value["auth"]["token"];
    assert_eq!(token["provider"].as_str(), Some("pbkdf2_sha256"));
    assert!(
        token["hash"]
            .as_str()
            .unwrap()
            .starts_with("$pbkdf2-sha256$i=1000,l=32$")
    );
}

fn test_pki() -> (Vec<u8>, String, String, Vec<u8>) {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
    let ca_pem = ca_certificate.pem().into_bytes();
    let issuer = Issuer::new(ca_params, ca_key);

    let mut server_params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate().unwrap();
    let server_certificate = server_params.signed_by(&server_key, &issuer).unwrap().pem();
    let server_key = server_key.serialize_pem();

    let mut client_params = CertificateParams::new(vec!["command-api-test-client".to_owned()]).unwrap();
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_key = KeyPair::generate().unwrap();
    let client_certificate = client_params.signed_by(&client_key, &issuer).unwrap().pem();
    let client_identity = format!("{client_certificate}{}", client_key.serialize_pem()).into_bytes();
    (ca_pem, server_certificate, server_key, client_identity)
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("install rustls ring provider");
    });
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn distinct_free_port(excluded: u16) -> u16 {
    loop {
        let port = free_port();
        if port != excluded {
            return port;
        }
    }
}

fn yaml_path(path: &Path) -> String {
    format!(
        "'{}'",
        path.display().to_string().replace('\\', "/").replace('\'', "''")
    )
}

#[cfg(unix)]
fn test_script() -> (&'static str, &'static str, &'static str) {
    (
        "sh",
        "integration.sh",
        r#"#!/bin/sh
printf 'OUT:%s|%s|%s\n' "$1" "$2" "$3"
printf 'ERR:%s\n' "$3" >&2
if [ "$2" = "sleep" ]; then
  trap '' TERM
  sleep "$3" &
  wait
fi
"#,
    )
}

#[cfg(windows)]
fn test_script() -> (&'static str, &'static str, &'static str) {
    (
        "powershell",
        "integration.ps1",
        r#"param([string]$Fixed, [string]$First, [string]$Second)
Write-Output "OUT:$Fixed|$First|$Second"
[Console]::Error.WriteLine("ERR:$Second")
if ($First -eq "sleep") { Start-Sleep -Seconds ([int]$Second) }
"#,
    )
}
