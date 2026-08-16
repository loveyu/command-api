mod platform;

use crate::{
    config::{Executor, ResolvedRoute},
    model::{OutputMode, OutputStream, StopReason, TaskStatus, Termination, now_rfc3339},
    store::{TaskRecord, TaskStore},
};
use anyhow::{Context, Result, bail};
use os_pipe::{PipeReader, PipeWriter};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant as StdInstant},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{OwnedSemaphorePermit, watch},
    time::Instant,
};

#[derive(Debug, Serialize, Deserialize)]
struct WorkerSpec {
    executor: Executor,
    program: PathBuf,
    script: PathBuf,
    fixed_args: Vec<String>,
    request_args: Vec<String>,
    working_directory: PathBuf,
    env: BTreeMap<String, String>,
    graceful_shutdown_seconds: u64,
}

pub struct ExecutionPermits {
    pub _global: OwnedSemaphorePermit,
    pub _route: OwnedSemaphorePermit,
}

pub fn start_task(
    store: Arc<TaskStore>,
    record: Arc<TaskRecord>,
    route: Arc<ResolvedRoute>,
    request_args: Vec<String>,
    permits: ExecutionPermits,
) {
    let stop_rx = record.subscribe_stop();
    tokio::spawn(async move {
        if let Err(error) = run_task(&store, &record, &route, request_args, permits, stop_rx).await {
            tracing::error!(task_id = %record.id, %error, "任务执行器异常退出");
            let error_message = format!("任务执行器异常: {error:#}");
            let _ = record
                .transition(|snapshot| {
                    if !snapshot.status.is_finished() {
                        snapshot.status = TaskStatus::Failed;
                        snapshot.finished_at = Some(now_rfc3339());
                        snapshot.error = Some(error_message);
                    }
                })
                .await;
        }
        store.mark_finished();
    });
}

async fn run_task(
    store: &Arc<TaskStore>,
    record: &Arc<TaskRecord>,
    route: &ResolvedRoute,
    request_args: Vec<String>,
    permits: ExecutionPermits,
    mut stop_rx: watch::Receiver<Option<StopReason>>,
) -> Result<()> {
    let mode = if route.merge_stdout_stderr {
        OutputMode::Combined
    } else {
        OutputMode::Separate
    };
    let capture = Capture::new(mode)?;
    let output_tasks = capture.start_readers(record.clone(), store.max_output_bytes())?;

    let executable = std::env::current_exe().context("无法确定 command-api 可执行文件路径")?;
    let mut command = Command::new(executable);
    command.arg("__worker").stdin(Stdio::piped()).kill_on_drop(false);
    let mut tree = platform::ProcessTree::new()?;
    tree.prepare_command(&mut command);
    capture.apply(&mut command);

    let mut child = command.spawn().context("无法启动内部任务 Worker")?;
    drop(command);
    tree.attach(&child)?;
    let mut control = child.stdin.take().context("无法打开 Worker 控制管道")?;
    let spec = WorkerSpec {
        executor: route.executor,
        program: route.program.clone(),
        script: route.script.clone(),
        fixed_args: route.fixed_args.clone(),
        request_args,
        working_directory: route.working_directory.clone(),
        env: route.env.clone(),
        graceful_shutdown_seconds: route.graceful_shutdown_seconds,
    };
    let mut initial_message = serde_json::to_vec(&spec)?;
    initial_message.push(b'\n');
    control
        .write_all(&initial_message)
        .await
        .context("无法初始化任务 Worker")?;
    control.flush().await?;

    record
        .transition(|snapshot| {
            snapshot.status = TaskStatus::Running;
            snapshot.started_at = Some(now_rfc3339());
        })
        .await?;

    let deadline = Instant::now() + Duration::from_secs(route.max_execution_seconds);
    let mut child_wait = Box::pin(child.wait());
    let mut stop_reason = None;
    let exit_status = loop {
        tokio::select! {
            result = &mut child_wait => break result.context("等待 Worker 退出失败")?,
            _ = tokio::time::sleep_until(deadline) => {
                let (status, actual_reason) = stop_process(
                    &tree,
                    &mut control,
                    &mut child_wait,
                    route,
                    StopReason::Timeout,
                    record,
                    &mut stop_rx,
                )
                .await?;
                stop_reason = Some(actual_reason);
                break status;
            }
            changed = stop_rx.changed() => {
                if changed.is_err() {
                    continue;
                }
                let reason = *stop_rx.borrow_and_update();
                if let Some(reason) = reason {
                    let (status, actual_reason) = stop_process(
                        &tree,
                        &mut control,
                        &mut child_wait,
                        route,
                        reason,
                        record,
                        &mut stop_rx,
                    )
                    .await?;
                    stop_reason = Some(actual_reason);
                    break status;
                }
            }
        }
    };

    if stop_reason.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if tokio::time::timeout(remaining, tree.wait_empty()).await.is_err() {
            stop_reason = Some(StopReason::Timeout);
            tree.force()?;
            record
                .transition(|snapshot| {
                    snapshot.status = TaskStatus::Stopping;
                    snapshot.termination = Some(Termination {
                        reason: StopReason::Timeout,
                        graceful_attempted: false,
                        forced: true,
                        method: platform::force_method().to_owned(),
                        signal: platform::force_signal().map(str::to_owned),
                        message: Some("主脚本已经退出，但子进程超过最大执行时间，已强制终止进程树".to_owned()),
                    });
                })
                .await?;
            tree.wait_empty().await?;
        }
    } else {
        tree.wait_empty().await?;
    }

    drop(control);
    for task in output_tasks {
        task.await.context("输出读取任务崩溃")??;
    }

    let exit_code = exit_status.code();
    let final_status = match stop_reason {
        Some(StopReason::Timeout) => TaskStatus::TimedOut,
        Some(StopReason::Cancelled) => TaskStatus::Cancelled,
        Some(StopReason::ForceKilled) => TaskStatus::Killed,
        Some(StopReason::ServerShutdown | StopReason::ServerRestart | StopReason::ParentExited) => {
            TaskStatus::Interrupted
        }
        Some(StopReason::LoggingFailure) => TaskStatus::Failed,
        None if exit_status.success() => TaskStatus::Succeeded,
        None => TaskStatus::Failed,
    };
    // A terminal task must no longer consume either concurrency slot. Release
    // the permits before publishing the terminal status so clients can safely
    // submit the next task after observing completion.
    drop(permits);
    record
        .transition(|snapshot| {
            snapshot.status = final_status;
            snapshot.finished_at = Some(now_rfc3339());
            snapshot.exit_code = exit_code;
            if final_status == TaskStatus::Failed && snapshot.error.is_none() {
                snapshot.error = Some(match stop_reason {
                    Some(StopReason::LoggingFailure) => "写入任务日志失败，任务已终止".to_owned(),
                    _ => format!(
                        "脚本退出码非零: {}",
                        exit_code.map_or_else(|| "unknown".to_owned(), |v| v.to_string())
                    ),
                });
            }
        })
        .await?;
    Ok(())
}

async fn stop_process(
    tree: &platform::ProcessTree,
    control: &mut tokio::process::ChildStdin,
    child_wait: &mut std::pin::Pin<Box<impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>>>,
    route: &ResolvedRoute,
    reason: StopReason,
    record: &TaskRecord,
    stop_rx: &mut watch::Receiver<Option<StopReason>>,
) -> Result<(std::process::ExitStatus, StopReason)> {
    if reason == StopReason::ForceKilled {
        let status = force_stop_process(tree, child_wait, record, reason, false, None).await?;
        return Ok((status, reason));
    }

    record
        .transition(|snapshot| {
            snapshot.status = TaskStatus::Stopping;
            snapshot.termination = Some(Termination {
                reason,
                graceful_attempted: true,
                forced: false,
                method: platform::graceful_method().to_owned(),
                signal: platform::graceful_signal().map(str::to_owned),
                message: Some(format!("已请求平滑退出，宽限期 {} 秒", route.graceful_shutdown_seconds)),
            });
        })
        .await?;

    let _ = control.write_all(b"terminate\n").await;
    let _ = control.flush().await;
    tree.graceful()?;

    let grace_deadline = Instant::now() + Duration::from_secs(route.graceful_shutdown_seconds);
    let mut force_requested = Box::pin(wait_for_force_kill(stop_rx));
    let mut worker_status = match tokio::select! {
        status = tokio::time::timeout_at(grace_deadline, child_wait.as_mut()) => Some(status),
        () = &mut force_requested => None,
    } {
        Some(Ok(status)) => Some(status.context("等待 Worker 平滑退出失败")?),
        Some(Err(_)) => None,
        None => {
            let forced_reason = StopReason::ForceKilled;
            let status = force_stop_process(tree, child_wait, record, forced_reason, true, None).await?;
            return Ok((status, forced_reason));
        }
    };
    let tree_stopped = if worker_status.is_some() {
        match tokio::select! {
            result = tokio::time::timeout_at(grace_deadline, tree.wait_empty()) => Some(result),
            () = &mut force_requested => None,
        } {
            Some(Ok(result)) => {
                result?;
                true
            }
            Some(Err(_)) => false,
            None => {
                let forced_reason = StopReason::ForceKilled;
                let status =
                    force_stop_process(tree, child_wait, record, forced_reason, true, worker_status.take()).await?;
                return Ok((status, forced_reason));
            }
        }
    } else {
        false
    };
    if tree_stopped {
        return Ok((worker_status.expect("worker status checked above"), reason));
    }

    let status = force_stop_process(tree, child_wait, record, reason, true, worker_status.take()).await?;
    Ok((status, reason))
}

async fn wait_for_force_kill(stop_rx: &mut watch::Receiver<Option<StopReason>>) {
    loop {
        if *stop_rx.borrow_and_update() == Some(StopReason::ForceKilled) {
            return;
        }
        if stop_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn force_stop_process(
    tree: &platform::ProcessTree,
    child_wait: &mut std::pin::Pin<Box<impl std::future::Future<Output = std::io::Result<std::process::ExitStatus>>>>,
    record: &TaskRecord,
    reason: StopReason,
    graceful_attempted: bool,
    worker_status: Option<std::process::ExitStatus>,
) -> Result<std::process::ExitStatus> {
    record
        .transition(|snapshot| {
            snapshot.status = TaskStatus::Stopping;
            snapshot.termination = Some(Termination {
                reason,
                graceful_attempted,
                forced: true,
                method: platform::force_method().to_owned(),
                signal: platform::force_signal().map(str::to_owned),
                message: Some(if graceful_attempted {
                    if reason == StopReason::ForceKilled {
                        "收到强制终止请求，已立即终止整个进程树".to_owned()
                    } else {
                        "平滑退出宽限期已结束，已强制终止整个进程树".to_owned()
                    }
                } else {
                    "已立即强制终止整个进程树，未尝试平滑退出".to_owned()
                }),
            });
        })
        .await?;
    tree.force()?;
    let status = match worker_status {
        Some(status) => status,
        None => child_wait.as_mut().await.context("等待 Worker 强制退出失败")?,
    };
    tree.wait_empty().await?;
    Ok(status)
}

struct Capture {
    stdout: Option<PipeWriter>,
    stderr: Option<PipeWriter>,
    readers: Vec<(OutputStream, PipeReader)>,
}

impl Capture {
    fn new(mode: OutputMode) -> Result<Self> {
        match mode {
            OutputMode::Separate => {
                let (stdout_reader, stdout_writer) = os_pipe::pipe()?;
                let (stderr_reader, stderr_writer) = os_pipe::pipe()?;
                Ok(Self {
                    stdout: Some(stdout_writer),
                    stderr: Some(stderr_writer),
                    readers: vec![
                        (OutputStream::Stdout, stdout_reader),
                        (OutputStream::Stderr, stderr_reader),
                    ],
                })
            }
            OutputMode::Combined => {
                let (reader, writer) = os_pipe::pipe()?;
                let stderr_writer = writer.try_clone()?;
                Ok(Self {
                    stdout: Some(writer),
                    stderr: Some(stderr_writer),
                    readers: vec![(OutputStream::Combined, reader)],
                })
            }
        }
    }

    fn apply(mut self, command: &mut Command) {
        command.stdout(Stdio::from(self.stdout.take().expect("stdout writer")));
        command.stderr(Stdio::from(self.stderr.take().expect("stderr writer")));
    }

    fn start_readers(&self, record: Arc<TaskRecord>, limit: u64) -> Result<Vec<tokio::task::JoinHandle<Result<()>>>> {
        let mut tasks = Vec::with_capacity(self.readers.len());
        for (stream, reader) in &self.readers {
            let reader = reader.try_clone()?;
            let record = record.clone();
            let stream = *stream;
            tasks.push(tokio::task::spawn_blocking(move || {
                if let Err(error) = copy_output(reader, &record, stream, limit) {
                    record.request_stop_now(StopReason::LoggingFailure);
                    return Err(error);
                }
                Ok(())
            }));
        }
        Ok(tasks)
    }
}

fn copy_output(mut reader: PipeReader, record: &TaskRecord, stream: OutputStream, limit: u64) -> Result<()> {
    let path = record.directory.join(stream.file_name());
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let counter = match stream {
        OutputStream::Stdout => &record.stdout_bytes,
        OutputStream::Stderr => &record.stderr_bytes,
        OutputStream::Combined => &record.combined_bytes,
    };
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let already = record.captured_bytes.fetch_add(read as u64, Ordering::Relaxed);
        let allowed = limit.saturating_sub(already).min(read as u64) as usize;
        if allowed > 0 {
            file.write_all(&buffer[..allowed])?;
            file.flush()?;
            counter.fetch_add(allowed as u64, Ordering::Relaxed);
        }
        if allowed < read {
            record.output_truncated.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}

pub fn worker_main() -> i32 {
    match run_worker() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("command-api worker error: {error:#}");
            127
        }
    }
}

fn run_worker() -> Result<i32> {
    platform::worker_setup()?;
    let mut reader = BufReader::new(std::io::stdin());
    let mut initial = String::new();
    if reader.read_line(&mut initial)? == 0 {
        bail!("未收到 Worker 初始化消息");
    }
    let spec: WorkerSpec = serde_json::from_str(initial.trim_end()).context("Worker 初始化消息无效")?;
    let stop_requested = Arc::new(AtomicBool::new(false));
    let stop_for_reader = stop_requested.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    stop_for_reader.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(_) if line.trim() == "terminate" => {
                    stop_for_reader.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(_) => {}
            }
        }
    });

    let mut command = build_script_command(&spec);
    let mut child = command
        .spawn()
        .with_context(|| format!("无法通过 {} 启动脚本 {}", spec.program.display(), spec.script.display()))?;
    let mut graceful_sent_at = None;
    let exit_status = loop {
        if stop_requested.load(Ordering::SeqCst) && graceful_sent_at.is_none() {
            platform::worker_graceful()?;
            graceful_sent_at = Some(StdInstant::now());
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(started) = graceful_sent_at
            && started.elapsed() >= Duration::from_secs(spec.graceful_shutdown_seconds)
        {
            platform::worker_force_after_parent_loss();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    platform::worker_wait_for_descendants()?;
    Ok(exit_status.code().unwrap_or(1))
}

fn build_script_command(spec: &WorkerSpec) -> std::process::Command {
    let mut command = std::process::Command::new(&spec.program);
    command
        .args(spec.executor.launcher_args())
        .arg(&spec.script)
        .args(&spec.fixed_args)
        .args(&spec.request_args)
        .current_dir(&spec.working_directory)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_arguments_precede_request_arguments() {
        let spec = WorkerSpec {
            executor: Executor::Bash,
            program: "bash".into(),
            script: "script.sh".into(),
            fixed_args: vec!["--fixed".into(), "one".into()],
            request_args: vec!["--request".into(), "two words".into()],
            working_directory: ".".into(),
            env: BTreeMap::new(),
            graceful_shutdown_seconds: 1,
        };
        let command = build_script_command(&spec);
        let args: Vec<_> = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["script.sh", "--fixed", "one", "--request", "two words"]);
    }
}
