use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;

struct Server {
    child: Child,
    _temp: TempDir,
    base_url: String,
    client: Client,
    config_path: PathBuf,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let port = free_port();
        let (executor, script_name, script) = test_script();
        let script_path = temp.path().join(script_name);
        fs::write(&script_path, script).unwrap();
        let config_path = temp.path().join("config.yaml");
        fs::write(
            &config_path,
            format!(
                r#"server:
  host: 127.0.0.1
  port: {port}
auth:
  token: integration-secret
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
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let server = Self {
            child,
            _temp: temp,
            base_url: format!("http://127.0.0.1:{port}"),
            client: Client::new(),
            config_path,
            token: "integration-secret".to_owned(),
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&self) {
        for _ in 0..100 {
            if self
                .client
                .get(format!("{}/healthz", self.base_url))
                .bearer_auth(&self.token)
                .send()
                .await
                .is_ok_and(|response| response.status() == StatusCode::OK)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("command-api did not become ready");
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

#[tokio::test]
async fn executes_arguments_captures_streams_merges_and_times_out() {
    let server = Server::start().await;

    let unauthorized = server.client.get(format!("{}/", server.base_url)).send().await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

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
        .bearer_auth("integration-secret")
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
        .bearer_auth("integration-secret")
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
        .bearer_auth("integration-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_restart.status(), StatusCode::CONFLICT);
    server.wait_ready().await;
    let reloaded_config = valid_config.replace("token: integration-secret", "token: integration-secret-reloaded");
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
    server.token = "integration-secret-reloaded".to_owned();

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

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
