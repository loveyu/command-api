use crate::{
    config::{Config, ResolvedConfig, ResolvedRoute, TokenFailureCooldownConfig, normalize_ip, validate_request_args},
    model::{
        AcceptedTask, ApiError, BasicResponse, ExecuteRequest, IndexData, MessageData, OutputQuery, StopReason,
        TaskStatus,
    },
    process::{ExecutionPermits, start_task},
    runtime::ManagementAction,
    store::TaskStore,
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use uuid::Uuid;
use zeroize::Zeroizing;

pub struct AppState {
    token: crate::secret::TokenVerifier,
    allowed_cidrs: Vec<ipnet::IpNet>,
    token_failure_cooldown: TokenFailureCooldownConfig,
    auth_failures: Mutex<HashMap<IpAddr, Instant>>,
    global_limit: Arc<Semaphore>,
    routes: HashMap<String, Arc<RouteRuntime>>,
    management_tx: mpsc::Sender<ManagementAction>,
    lifecycle_state: AtomicU8,
    config_path: PathBuf,
    logging_directory: PathBuf,
    pub store: Arc<TaskStore>,
}

pub struct RouteRuntime {
    pub config: Arc<ResolvedRoute>,
    limit: Arc<Semaphore>,
}

pub fn build(config: &ResolvedConfig, store: Arc<TaskStore>, management_tx: mpsc::Sender<ManagementAction>) -> Router {
    let routes: HashMap<_, _> = config
        .routes
        .iter()
        .cloned()
        .map(|route| {
            let path = route.path.clone();
            let max = route.max_concurrency;
            (
                path,
                Arc::new(RouteRuntime {
                    config: Arc::new(route),
                    limit: Arc::new(Semaphore::new(max)),
                }),
            )
        })
        .collect();
    let state = Arc::new(AppState {
        token: config.auth.token.clone(),
        allowed_cidrs: config.access.allowed_cidrs.clone(),
        token_failure_cooldown: config.access.token_failure_cooldown.clone(),
        auth_failures: Mutex::new(HashMap::new()),
        global_limit: Arc::new(Semaphore::new(config.execution.max_total_concurrency)),
        routes,
        management_tx,
        lifecycle_state: AtomicU8::new(0),
        config_path: config.source_path.clone(),
        logging_directory: config.logging.directory.clone(),
        store,
    });

    let mut router = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/output", get(get_output))
        .route("/tasks/{task_id}/cancel", post(cancel_task))
        .route("/tasks/{task_id}/kill", post(kill_task))
        .route("/system/stop", post(stop_service))
        .route("/system/restart", post(restart_service));
    for route in state.routes.values() {
        router = router.route(&route.config.path, post(execute).layer(Extension(Arc::clone(route))));
    }
    router
        .fallback(fallback)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(Arc::clone(&state), authenticate))
        .with_state(state)
}

async fn authenticate(State(state): State<Arc<AppState>>, request: Request<Body>, next: Next) -> Response {
    let Some(peer_ip) = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| normalize_ip(peer.0.ip()))
    else {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "source_ip_unavailable",
            "无法确定可信 TCP 来源地址",
        )
        .into_response();
    };
    if !state.allowed_cidrs.iter().any(|cidr| cidr.contains(&peer_ip)) {
        return ApiError::new(StatusCode::FORBIDDEN, "source_ip_denied", "来源 IP 不在允许的 CIDR 中").into_response();
    }

    if let Some(retry_after) = cooldown_remaining(&state, peer_ip).await {
        let mut response = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "token_failure_cooldown",
            "该来源 IP 在 Token 校验失败冷却期内",
        )
        .into_response();
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| Zeroizing::new(token.to_owned()));
    let authorized = match token {
        Some(token) => state.token.verify(&token).await,
        None => false,
    };
    if authorized {
        return next.run(request).await;
    }
    record_auth_failure(&state, peer_ip).await;
    let mut response = ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "缺少或使用了无效的 Bearer Token",
    )
    .into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

async fn cooldown_remaining(state: &AppState, ip: IpAddr) -> Option<u64> {
    if !state.token_failure_cooldown.enabled {
        return None;
    }
    let now = Instant::now();
    let mut failures = state.auth_failures.lock().await;
    match failures.get(&ip).copied() {
        Some(until) if until > now => {
            let remaining = until.duration_since(now);
            Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0))
        }
        Some(_) => {
            failures.remove(&ip);
            None
        }
        None => None,
    }
}

async fn record_auth_failure(state: &AppState, ip: IpAddr) {
    if !state.token_failure_cooldown.enabled {
        return;
    }
    let now = Instant::now();
    let mut failures = state.auth_failures.lock().await;
    failures.retain(|_, until| *until > now);
    if failures.len() >= state.token_failure_cooldown.max_tracked_ips
        && let Some(oldest) = failures.iter().min_by_key(|(_, until)| **until).map(|(ip, _)| *ip)
    {
        failures.remove(&oldest);
    }
    failures.insert(ip, now + Duration::from_secs(state.token_failure_cooldown.seconds));
}

async fn index() -> Json<BasicResponse<IndexData>> {
    Json(BasicResponse {
        success: true,
        data: IndexData {
            name: "command-api",
            description: "通过受控 HTTP API 异步执行预配置脚本",
            repository: "https://github.com/loveyu/command-api",
        },
    })
}

async fn healthz() -> Json<BasicResponse<MessageData>> {
    Json(BasicResponse {
        success: true,
        data: MessageData {
            message: "ok".to_owned(),
        },
    })
}

async fn execute(
    State(state): State<Arc<AppState>>,
    Extension(route): Extension<Arc<RouteRuntime>>,
    Json(request): Json<ExecuteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    validate_request_args(&route.config, &request.args)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, "invalid_arguments", error.to_string()))?;

    let global = state.global_limit.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "global_concurrency_limit",
            "已达到全局并发上限",
        )
    })?;
    let route_permit = route.limit.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "route_concurrency_limit",
            format!(
                "路由 {} 已达到并发上限 {}",
                route.config.path, route.config.max_concurrency
            ),
        )
    })?;

    let mode = if route.config.merge_stdout_stderr {
        crate::model::OutputMode::Combined
    } else {
        crate::model::OutputMode::Separate
    };
    let record = state
        .store
        .create(route.config.path.clone(), mode, route.config.output_encoding)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task_create_failed",
                error.to_string(),
            )
        })?;
    let id = record.id;
    start_task(
        Arc::clone(&state.store),
        record,
        Arc::clone(&route.config),
        request.args,
        ExecutionPermits {
            _global: global,
            _route: route_permit,
        },
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedTask {
            success: true,
            task_id: id,
            status: TaskStatus::Starting,
            status_url: format!("/tasks/{id}"),
        }),
    ))
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<Uuid>,
) -> Result<Json<crate::model::TaskView>, ApiError> {
    state
        .store
        .view(task_id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "task_not_found", "任务不存在或已经过期"))
}

async fn get_output(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<Uuid>,
    Query(query): Query<OutputQuery>,
) -> Result<Json<crate::model::OutputChunk>, ApiError> {
    if state.store.get(task_id).await.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "task_not_found",
            "任务不存在或已经过期",
        ));
    }
    state
        .store
        .output_chunk(task_id, query.stream, query.offset, query.limit)
        .await
        .map(Json)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, "invalid_output_query", error.to_string()))
}

async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    if state.store.get(task_id).await.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "task_not_found",
            "任务不存在或已经过期",
        ));
    }
    let cancelled = state
        .store
        .request_stop(task_id, StopReason::Cancelled)
        .await
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cancel_failed", error.to_string()))?;
    if !cancelled {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "task_finished",
            "任务已经结束，无法取消",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(BasicResponse {
            success: true,
            data: MessageData {
                message: "已请求任务平滑退出".to_owned(),
            },
        }),
    ))
}

async fn kill_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    if state.store.get(task_id).await.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "task_not_found",
            "任务不存在或已经过期",
        ));
    }
    let killed = state
        .store
        .request_stop(task_id, StopReason::ForceKilled)
        .await
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "kill_failed", error.to_string()))?;
    if !killed {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "task_finished",
            "任务已经结束，无法强制终止",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(BasicResponse {
            success: true,
            data: MessageData {
                message: "已请求立即强制终止整个任务进程树".to_owned(),
            },
        }),
    ))
}

async fn stop_service(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    request_management_action(&state, ManagementAction::Stop)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BasicResponse {
            success: true,
            data: MessageData {
                message: "已接受服务停止请求".to_owned(),
            },
        }),
    ))
}

async fn restart_service(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let config = Config::load(&state.config_path).map_err(|error| {
        ApiError::new(
            StatusCode::CONFLICT,
            "restart_config_invalid",
            format!("重新加载配置失败，服务保持运行: {error:#}"),
        )
    })?;
    if config.logging.directory != state.logging_directory {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "restart_log_directory_changed",
            "运行时重启不允许修改 logging.directory；请先停止服务，再通过外部管理器启动",
        ));
    }
    request_management_action(&state, ManagementAction::Restart(Box::new(config)))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(BasicResponse {
            success: true,
            data: MessageData {
                message: "已接受服务重启请求，将使用重新加载的配置恢复服务".to_owned(),
            },
        }),
    ))
}

fn request_management_action(state: &AppState, action: ManagementAction) -> Result<(), ApiError> {
    state
        .lifecycle_state
        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| {
            ApiError::new(
                StatusCode::CONFLICT,
                "service_transition_in_progress",
                "服务停止或重启操作已经开始",
            )
        })?;
    if state.management_tx.try_send(action).is_err() {
        state.lifecycle_state.store(0, Ordering::SeqCst);
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_control_unavailable",
            "服务管理通道不可用",
        ));
    }
    Ok(())
}

async fn fallback() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "接口不存在")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn every_endpoint_requires_authentication() {
        const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hash = crate::secret::generate_pbkdf2_sha256(TOKEN).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.yaml");
        let script = temp.path().join("test.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(
            &config_path,
            format!(
                r#"server: {{ host: 127.0.0.1, port: 27415 }}
access:
  allowed_cidrs: [127.0.0.0/8]
  token_failure_cooldown:
    enabled: true
    seconds: 10
auth:
  token:
    provider: pbkdf2_sha256
    hash: "{hash}"
logging: {{ directory: logs }}
routes:
  - path: /run
    executor: sh
    script: {}
    max_concurrency: 1
    max_execution_seconds: 5
    graceful_shutdown_seconds: 1
"#,
                script.display()
            ),
        )
        .unwrap();
        let config = crate::config::Config::load(config_path).unwrap();
        let store = TaskStore::open(
            config.logging.directory.clone(),
            config.logging.retention_seconds,
            config.logging.max_output_bytes_per_task,
        )
        .await
        .unwrap();
        let (management_tx, _management_rx) = mpsc::channel(1);
        let app = build(&config, store, management_tx);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .extension(ConnectInfo("127.0.0.1:40000".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .extension(ConnectInfo("127.0.0.1:40000".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!((1..=10).contains(&retry_after));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .extension(ConnectInfo("10.0.0.1:40000".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .extension(ConnectInfo("127.0.0.2:40000".parse::<SocketAddr>().unwrap()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
