use crate::config::OutputEncoding;
use axum::{Json, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::fmt;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Starting,
    Running,
    Stopping,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    Separate,
    Combined,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Timeout,
    Cancelled,
    ServerShutdown,
    ParentExited,
    LoggingFailure,
}

impl fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ServerShutdown => "server_shutdown",
            Self::ParentExited => "parent_exited",
            Self::LoggingFailure => "logging_failure",
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Termination {
    pub reason: StopReason,
    pub graceful_attempted: bool,
    pub forced: bool,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskSnapshot {
    pub task_id: Uuid,
    pub route: String,
    pub status: TaskStatus,
    pub output_mode: OutputMode,
    pub output_encoding: OutputEncoding,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination: Option<Termination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub output_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct TaskView {
    pub success: bool,
    #[serde(flatten)]
    pub task: TaskSnapshot,
    pub duration_ms: u128,
    pub output_bytes: OutputSizes,
    pub links: TaskLinks,
}

#[derive(Debug, Default, Serialize)]
pub struct OutputSizes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub combined: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct TaskLinks {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub combined: Option<String>,
    pub cancel: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteRequest {
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AcceptedTask {
    pub success: bool,
    pub task_id: Uuid,
    pub status: TaskStatus,
    pub status_url: String,
}

#[derive(Debug, Deserialize)]
pub struct OutputQuery {
    pub stream: OutputStream,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_output_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Combined,
}

impl OutputStream {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout.log",
            Self::Stderr => "stderr.log",
            Self::Combined => "output.log",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct OutputChunk {
    pub success: bool,
    pub task_id: Uuid,
    pub stream: OutputStream,
    pub encoding: String,
    pub offset: u64,
    pub next_offset: u64,
    pub eof: bool,
    pub content: String,
    pub decoding_errors: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_base64: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BasicResponse<T: Serialize> {
    pub success: bool,
    #[serde(flatten)]
    pub data: T,
}

#[derive(Debug, Serialize)]
pub struct MessageData {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct IndexData {
    pub name: &'static str,
    pub description: &'static str,
    pub repository: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub success: bool,
    pub error: ApiErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ApiErrorDetail {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(ApiErrorBody {
                success: false,
                error: ApiErrorDetail {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 时间格式化不应失败")
}

pub fn elapsed_ms(start: &str, end: Option<&str>) -> u128 {
    let Ok(start) = OffsetDateTime::parse(start, &Rfc3339) else {
        return 0;
    };
    let end = end
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    (end - start).whole_milliseconds().max(0) as u128
}

const fn default_output_limit() -> usize {
    64 * 1024
}
