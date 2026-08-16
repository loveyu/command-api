use crate::{
    config::OutputEncoding,
    model::{
        OutputChunk, OutputMode, OutputSizes, OutputStream, StopReason, TaskLinks, TaskSnapshot, TaskStatus, TaskView,
        elapsed_ms, now_rfc3339,
    },
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use encoding_rs::{Encoding, GBK, UTF_8, UTF_16LE};
use fs2::FileExt;
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex, Notify, RwLock, watch},
};
use uuid::Uuid;

pub struct TaskStore {
    root: PathBuf,
    max_output_bytes: u64,
    retention: Duration,
    tasks: RwLock<HashMap<Uuid, Arc<TaskRecord>>>,
    active_count: AtomicUsize,
    active_changed: Notify,
    _lock: File,
}

pub struct TaskRecord {
    pub id: Uuid,
    pub directory: PathBuf,
    snapshot: RwLock<TaskSnapshot>,
    pub stdout_bytes: AtomicU64,
    pub stderr_bytes: AtomicU64,
    pub combined_bytes: AtomicU64,
    pub captured_bytes: AtomicU64,
    pub output_truncated: AtomicBool,
    cancel: Mutex<Option<watch::Sender<Option<StopReason>>>>,
}

impl TaskStore {
    pub async fn open(root: PathBuf, retention_seconds: u64, max_output_bytes: u64) -> Result<Arc<Self>> {
        fs::create_dir_all(root.join("tasks")).with_context(|| format!("无法创建任务日志目录 {}", root.display()))?;
        let lock_path = root.join("command-api.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("无法打开日志目录锁文件 {}", lock_path.display()))?;
        FileExt::try_lock_exclusive(&lock)
            .with_context(|| format!("日志目录 {} 已被另一个 command-api 实例使用", root.display()))?;

        let store = Arc::new(Self {
            root,
            max_output_bytes,
            retention: Duration::from_secs(retention_seconds),
            tasks: RwLock::new(HashMap::new()),
            active_count: AtomicUsize::new(0),
            active_changed: Notify::new(),
            _lock: lock,
        });
        store.recover().await?;
        store.cleanup_expired().await?;
        Ok(store)
    }

    pub fn max_output_bytes(&self) -> u64 {
        self.max_output_bytes
    }

    pub async fn create(
        &self,
        route: String,
        output_mode: OutputMode,
        output_encoding: OutputEncoding,
    ) -> Result<Arc<TaskRecord>> {
        let id = Uuid::new_v4();
        let directory = self.root.join("tasks").join(id.to_string());
        tokio::fs::create_dir(&directory)
            .await
            .with_context(|| format!("无法创建任务日志目录 {}", directory.display()))?;
        let snapshot = TaskSnapshot {
            task_id: id,
            route,
            status: TaskStatus::Starting,
            output_mode,
            output_encoding,
            created_at: now_rfc3339(),
            started_at: None,
            finished_at: None,
            exit_code: None,
            termination: None,
            error: None,
            output_truncated: false,
        };
        match output_mode {
            OutputMode::Separate => {
                tokio::fs::write(directory.join(OutputStream::Stdout.file_name()), []).await?;
                tokio::fs::write(directory.join(OutputStream::Stderr.file_name()), []).await?;
            }
            OutputMode::Combined => {
                tokio::fs::write(directory.join(OutputStream::Combined.file_name()), []).await?;
            }
        }
        let record = Arc::new(TaskRecord::new(snapshot, directory));
        record.persist().await?;
        self.tasks.write().await.insert(id, record.clone());
        self.active_count.fetch_add(1, Ordering::SeqCst);
        Ok(record)
    }

    pub async fn get(&self, id: Uuid) -> Option<Arc<TaskRecord>> {
        self.tasks.read().await.get(&id).cloned()
    }

    pub async fn view(&self, id: Uuid) -> Option<TaskView> {
        let record = self.get(id).await?;
        Some(record.view().await)
    }

    pub async fn cancel(&self, id: Uuid, reason: StopReason) -> Result<bool> {
        let Some(record) = self.get(id).await else {
            return Ok(false);
        };
        let sender = record.cancel.lock().await;
        let Some(sender) = sender.as_ref() else {
            return Ok(false);
        };
        sender.send(Some(reason)).context("无法通知任务停止")?;
        Ok(true)
    }

    pub async fn cancel_all(&self, reason: StopReason) {
        let records: Vec<_> = self.tasks.read().await.values().cloned().collect();
        for record in records {
            let sender = record.cancel.lock().await;
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(Some(reason));
            }
        }
    }

    pub async fn wait_until_idle(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                if self.active_count.load(Ordering::SeqCst) == 0 {
                    break;
                }
                self.active_changed.notified().await;
            }
        })
        .await
        .is_ok()
    }

    pub fn mark_finished(&self) {
        self.active_count.fetch_sub(1, Ordering::SeqCst);
        self.active_changed.notify_waiters();
    }

    pub async fn output_chunk(&self, id: Uuid, stream: OutputStream, offset: u64, limit: usize) -> Result<OutputChunk> {
        const MAX_LIMIT: usize = 1024 * 1024;
        if limit == 0 || limit > MAX_LIMIT {
            bail!("limit 必须在 1 到 {MAX_LIMIT} 之间");
        }
        let record = self.get(id).await.context("任务不存在")?;
        let snapshot = record.snapshot.read().await.clone();
        match (snapshot.output_mode, stream) {
            (OutputMode::Separate, OutputStream::Combined) => bail!("该任务使用分离输出模式"),
            (OutputMode::Combined, OutputStream::Stdout | OutputStream::Stderr) => {
                bail!("该任务使用合并输出模式")
            }
            _ => {}
        }

        let path = record.directory.join(stream.file_name());
        let mut file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("输出文件尚未生成: {}", path.display()))?;
        let file_size = file.metadata().await?.len();
        if offset > file_size {
            bail!("offset {offset} 超过当前输出大小 {file_size}");
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut read_limit = limit.min((file_size - offset) as usize);
        if snapshot.output_encoding == OutputEncoding::Utf16le && !read_limit.is_multiple_of(2) {
            read_limit -= 1;
        }
        let mut data = vec![0u8; read_limit];
        file.read_exact(&mut data).await?;
        let next_offset = offset + data.len() as u64;
        let (content, decoding_errors) = decode_output(snapshot.output_encoding, &data);

        Ok(OutputChunk {
            success: true,
            task_id: id,
            stream,
            encoding: snapshot.output_encoding.label().to_owned(),
            offset,
            next_offset,
            eof: snapshot.status.is_finished() && next_offset >= file_size,
            content,
            decoding_errors,
            content_base64: decoding_errors.then(|| BASE64.encode(&data)),
        })
    }

    pub fn spawn_cleanup(self: &Arc<Self>) {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if let Err(error) = store.cleanup_expired().await {
                    tracing::error!(%error, "清理过期任务失败");
                }
            }
        });
    }

    async fn recover(&self) -> Result<()> {
        let task_root = self.root.join("tasks");
        let mut entries = tokio::fs::read_dir(&task_root).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let directory = entry.path();
            let Some(mut snapshot) = read_latest_snapshot(&directory.join("events.jsonl"))? else {
                continue;
            };
            if !snapshot.status.is_finished() {
                snapshot.status = TaskStatus::Interrupted;
                snapshot.finished_at = Some(now_rfc3339());
                snapshot.error = Some("服务重启时任务仍处于未完成状态".to_owned());
            }
            let record = Arc::new(TaskRecord::new(snapshot, directory));
            record.refresh_output_sizes().await;
            if record.snapshot.read().await.status == TaskStatus::Interrupted {
                record.persist().await?;
            }
            self.tasks.write().await.insert(record.id, record);
        }
        Ok(())
    }

    async fn cleanup_expired(&self) -> Result<()> {
        let now = OffsetDateTime::now_utc();
        let records: Vec<_> = self.tasks.read().await.values().cloned().collect();
        let mut expired = Vec::new();
        for record in records {
            let snapshot = record.snapshot.read().await;
            let Some(finished_at) = snapshot.finished_at.as_deref() else {
                continue;
            };
            let Ok(finished_at) = OffsetDateTime::parse(finished_at, &Rfc3339) else {
                continue;
            };
            if (now - finished_at).whole_seconds() >= self.retention.as_secs() as i64 {
                expired.push(record.id);
            }
        }
        if expired.is_empty() {
            return Ok(());
        }
        let mut tasks = self.tasks.write().await;
        for id in expired {
            if let Some(record) = tasks.remove(&id)
                && let Err(error) = tokio::fs::remove_dir_all(&record.directory).await
            {
                tracing::warn!(task_id = %id, %error, "删除过期任务日志失败");
            }
        }
        Ok(())
    }
}

impl TaskRecord {
    fn new(snapshot: TaskSnapshot, directory: PathBuf) -> Self {
        Self {
            id: snapshot.task_id,
            directory,
            snapshot: RwLock::new(snapshot),
            stdout_bytes: AtomicU64::new(0),
            stderr_bytes: AtomicU64::new(0),
            combined_bytes: AtomicU64::new(0),
            captured_bytes: AtomicU64::new(0),
            output_truncated: AtomicBool::new(false),
            cancel: Mutex::new(None),
        }
    }

    pub async fn snapshot(&self) -> TaskSnapshot {
        let mut snapshot = self.snapshot.read().await.clone();
        snapshot.output_truncated = self.output_truncated.load(Ordering::Relaxed);
        snapshot
    }

    pub async fn set_cancel_sender(&self, sender: watch::Sender<Option<StopReason>>) {
        *self.cancel.lock().await = Some(sender);
    }

    pub async fn clear_cancel_sender(&self) {
        *self.cancel.lock().await = None;
    }

    pub async fn transition(&self, update: impl FnOnce(&mut TaskSnapshot)) -> Result<()> {
        {
            let mut snapshot = self.snapshot.write().await;
            update(&mut snapshot);
            snapshot.output_truncated = self.output_truncated.load(Ordering::Relaxed);
        }
        self.persist().await
    }

    pub async fn persist(&self) -> Result<()> {
        let snapshot = self.snapshot().await;
        let mut line = serde_json::to_vec(&snapshot)?;
        line.push(b'\n');
        let path = self.directory.join("events.jsonl");
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn view(&self) -> TaskView {
        let snapshot = self.snapshot().await;
        let id = self.id;
        let base = format!("/tasks/{id}");
        let (stdout, stderr, combined) = match snapshot.output_mode {
            OutputMode::Separate => (
                Some(self.stdout_bytes.load(Ordering::Relaxed)),
                Some(self.stderr_bytes.load(Ordering::Relaxed)),
                None,
            ),
            OutputMode::Combined => (None, None, Some(self.combined_bytes.load(Ordering::Relaxed))),
        };
        let links = match snapshot.output_mode {
            OutputMode::Separate => TaskLinks {
                status: base.clone(),
                stdout: Some(format!("{base}/output?stream=stdout")),
                stderr: Some(format!("{base}/output?stream=stderr")),
                combined: None,
                cancel: format!("{base}/cancel"),
            },
            OutputMode::Combined => TaskLinks {
                status: base.clone(),
                stdout: None,
                stderr: None,
                combined: Some(format!("{base}/output?stream=combined")),
                cancel: format!("{base}/cancel"),
            },
        };
        TaskView {
            success: snapshot.status == TaskStatus::Succeeded,
            duration_ms: elapsed_ms(
                snapshot.started_at.as_deref().unwrap_or(&snapshot.created_at),
                snapshot.finished_at.as_deref(),
            ),
            task: snapshot,
            output_bytes: OutputSizes {
                stdout,
                stderr,
                combined,
            },
            links,
        }
    }

    async fn refresh_output_sizes(&self) {
        let mut total = 0u64;
        for (stream, counter) in [
            (OutputStream::Stdout, &self.stdout_bytes),
            (OutputStream::Stderr, &self.stderr_bytes),
            (OutputStream::Combined, &self.combined_bytes),
        ] {
            if let Ok(metadata) = tokio::fs::metadata(self.directory.join(stream.file_name())).await {
                counter.store(metadata.len(), Ordering::Relaxed);
                total = total.saturating_add(metadata.len());
            }
        }
        self.captured_bytes.store(total, Ordering::Relaxed);
    }
}

fn read_latest_snapshot(path: &Path) -> Result<Option<TaskSnapshot>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut latest = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        match serde_json::from_str(&line) {
            Ok(snapshot) => latest = Some(snapshot),
            Err(error) => tracing::warn!(path = %path.display(), %error, "忽略损坏的任务状态记录"),
        }
    }
    Ok(latest)
}

fn decode_output(encoding: OutputEncoding, data: &[u8]) -> (String, bool) {
    let encoding: &'static Encoding = match encoding {
        OutputEncoding::Utf8 => UTF_8,
        OutputEncoding::Gbk => GBK,
        OutputEncoding::Utf16le => UTF_16LE,
    };
    let (text, _, errors) = encoding.decode(data);
    (text.into_owned(), errors)
}
