use super::{TaskStatus, TransferTask, UploadTaskDetail};
use crate::transfer::upload::Uploader;
use crate::ws_server;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

// ── 上传会话日志 ──────────────────────────────────────────────────────────────

/// 单次批量上传会话的统计记录，写入 upload_history.jsonl
#[derive(serde::Serialize)]
struct UploadSessionLog {
    /// 会话开始时间（ISO 8601 UTC）
    time: String,
    /// 本次选择的文件总数
    total: usize,
    /// 成功上传数（含秒传）
    success: u32,
    /// 失败数
    failed: u32,
    /// 失败的文件名列表
    failed_files: Vec<String>,
}

/// 追加一条会话日志到 upload_history.jsonl（JSON Lines 格式）
fn append_upload_log(log: UploadSessionLog) {
    let mut path = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("com.crotonmedia.ipams-client");
    path.push("upload_history.jsonl");

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(line) = serde_json::to_string(&log) {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{}", line);
        }
    }
}

/// 会话级计数器，用于跟踪一批上传的成功/失败数
#[derive(Debug, Default)]
struct SessionCounter {
    total: usize,
    success: std::sync::atomic::AtomicU32,
    failed: std::sync::atomic::AtomicU32,
    failed_files: std::sync::Mutex<Vec<String>>,
    started_at: String,
}

impl SessionCounter {
    fn new(total: usize) -> Self {
        Self {
            total,
            success: std::sync::atomic::AtomicU32::new(0),
            failed: std::sync::atomic::AtomicU32::new(0),
            failed_files: std::sync::Mutex::new(Vec::new()),
            started_at: chrono_now_iso(),
        }
    }

    fn record_success(&self) {
        self.success.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_failure(&self, filename: &str) {
        self.failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut v) = self.failed_files.lock() {
            v.push(filename.to_string());
        }
    }

    fn is_complete(&self) -> bool {
        let done = self.success.load(std::sync::atomic::Ordering::Relaxed)
            + self.failed.load(std::sync::atomic::Ordering::Relaxed);
        done as usize >= self.total
    }

    fn to_log(&self) -> UploadSessionLog {
        let failed_files = self.failed_files.lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        UploadSessionLog {
            time: self.started_at.clone(),
            total: self.total,
            success: self.success.load(std::sync::atomic::Ordering::Relaxed),
            failed: self.failed.load(std::sync::atomic::Ordering::Relaxed),
            failed_files,
        }
    }
}

fn chrono_now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 简单格式化为 ISO 8601 UTC，不引入 chrono 依赖
    let (y, mo, d, h, mi, s) = epoch_to_ymd_hms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

fn epoch_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = secs % 60;
    let total_min = secs / 60;
    let mi = total_min % 60;
    let total_hours = total_min / 60;
    let h = total_hours % 24;
    let mut days = total_hours / 24; // days since 1970-01-01

    let mut year = 1970u32;
    loop {
        let days_in_year = if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = [31u64, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1u32;
    for &dim in &days_in_month {
        if days < dim {
            break;
        }
        days -= dim;
        month += 1;
    }
    (year, month, days as u32 + 1, h as u32, mi as u32, s as u32)
}

static TASK_STORE: Lazy<Arc<RwLock<HashMap<String, TransferTask>>>> =
    Lazy::new(|| Arc::new(RwLock::new(load_tasks_from_disk())));

/// 每个 upload_id 对应一个 CancellationToken，暂停时 cancel 它
static CANCEL_TOKENS: Lazy<Arc<RwLock<HashMap<String, CancellationToken>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

/// 被暂停的 upload_id 集合（用于循环前的快速检查）
static PAUSED_SET: Lazy<Arc<RwLock<HashSet<String>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashSet::new())));

/// 全局取消标志：cancel_all 后设为 true，阻止新任务自动启动
/// 用户主动发起新上传时重置为 false
static GLOBAL_CANCELLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 全局暂停标志：pause_all 后设为 true，阻止新任务自动启动
/// resume_all 或用户主动发起新上传时重置为 false
static GLOBAL_PAUSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 上传失败自动重试次数上限，超过后才进入失败列表
const MAX_RETRY_COUNT: u32 = 3;

/// 全局文件级并发信号量：限制同时上传的文件数，避免大量并发连接耗尽系统资源
/// 每个文件上传占用一个 permit，上传完成（成功/失败/暂停）后释放
const MAX_CONCURRENT_FILES: usize = 3;
static FILE_UPLOAD_SEMAPHORE: Lazy<Arc<tokio::sync::Semaphore>> =
    Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FILES)));

/// 全局共享 HTTP client：所有上传任务复用同一个连接池，
/// 避免每次 Uploader::new() 都创建独立连接池导致套接字耗尽。
/// reqwest::Client 内部是 Arc，clone 只是增加引用计数，不复制连接池。
static SHARED_HTTP_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(4)   // 每个 host 最多保留 4 个空闲连接
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .expect("Failed to build shared HTTP client")
});

/// 当前批量入队会话的取消令牌。
/// enqueue_upload_by_params 开始时创建新令牌，cancel_all_tasks 时 cancel 它，
/// 使 init 阶段的滑动窗口和 upload 通道的消费者都能立即感知取消。
static ENQUEUE_CANCEL_TOKEN: Lazy<Arc<RwLock<CancellationToken>>> =
    Lazy::new(|| Arc::new(RwLock::new(CancellationToken::new())));

pub fn is_paused(upload_id: &str) -> bool {
    PAUSED_SET.try_read().map(|s| s.contains(upload_id)).unwrap_or(false)
}

/// 注册一个新的 CancellationToken，返回给 uploader 使用
pub async fn register_cancel_token(upload_id: &str) -> CancellationToken {
    let token = CancellationToken::new();
    CANCEL_TOKENS.write().await.insert(upload_id.to_string(), token.clone());
    token
}

pub async fn remove_cancel_token(upload_id: &str) {
    CANCEL_TOKENS.write().await.remove(upload_id);
}

fn current_user_id() -> String {
    crate::auth::load_token()
        .and_then(|token| crate::auth::get_user_id_from_token(&token))
        .unwrap_or_else(|| "anonymous".to_string())
}

fn store_path() -> PathBuf {
    let mut path = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."));
    path.push("com.crotonmedia.ipams-client");
    path.push("tasks.json");
    path
}

fn load_tasks_from_disk() -> HashMap<String, TransferTask> {
    let path = store_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Serialize `store` to JSON and write to disk on a blocking thread.
/// The write lock must already be held by the caller; we serialize while
/// holding it (fast), then release the lock and do the actual IO off-thread.
/// This prevents the async runtime from stalling on disk writes, which was
/// causing FILE_UPLOAD_SEMAPHORE permits to be held indefinitely while
/// update_task_status / progress callbacks queued up behind a slow write lock.
fn save_tasks_async(store: &HashMap<String, TransferTask>) {
    if let Ok(json) = serde_json::to_string(store) {
        let path = store_path();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, json);
        });
    }
}

/// Synchronous version — only used in non-async contexts (e.g. Lazy init).
fn save_tasks_to_disk(store: &HashMap<String, TransferTask>) {
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(store) {
        let _ = std::fs::write(&path, json);
    }
}

pub async fn clear_tasks() {
    let mut store = TASK_STORE.write().await;
    store.clear();
    save_tasks_async(&store);
}

/// 清除已完成、已取消、已失败的任务（保留进行中和暂停的）
pub async fn clear_completed_tasks() {
    let user_id = current_user_id();
    let mut store = TASK_STORE.write().await;
    store.retain(|_, task| {
        // 保留其他用户的任务不动
        if task.user_id.as_deref() != Some(&user_id) && task.user_id.is_some() {
            return true;
        }
        // 当前用户：只保留活跃任务
        matches!(task.status, TaskStatus::Running | TaskStatus::Pending | TaskStatus::Paused)
    });
    save_tasks_async(&store);
    ws_server::broadcast_message(json!({ "type": "queue_cleared" }));
}

/// 切换用户后重新加载对应用户的任务列表
pub async fn reload_tasks_for_current_user() {
    let user_id = current_user_id();
    let all_tasks = load_tasks_from_disk();
    // 只保留当前用户的任务
    let user_tasks: HashMap<String, TransferTask> = all_tasks
        .into_iter()
        .filter(|(_, t)| t.user_id.as_deref() == Some(&user_id) || t.user_id.is_none())
        .collect();
    let mut store = TASK_STORE.write().await;
    *store = user_tasks;
    tracing::info!("Task store reloaded for user: {}", user_id);
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 全局 blocking IO 信号量：限制同时在 spawn_blocking 里做文件 IO 的线程数
/// 防止大量并发 blocking 任务耗尽 Windows 内核对象（os error 1450）
const MAX_CONCURRENT_BLOCKING_IO: usize = 4;
static BLOCKING_IO_SEMAPHORE: Lazy<Arc<tokio::sync::Semaphore>> =
    Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_BLOCKING_IO)));

pub async fn get_all_tasks() -> Vec<TransferTask> {
    let user_id = current_user_id();
    let store = TASK_STORE.read().await;
    let mut tasks: Vec<TransferTask> = store.values()
        .filter(|t| t.user_id.as_deref() == Some(&user_id) || t.user_id.is_none())
        .cloned()
        .collect();
    tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    tasks
}

pub async fn get_task(upload_id: &str) -> Option<TransferTask> {
    TASK_STORE.read().await.get(upload_id).cloned()
}

async fn upsert_task(task: TransferTask) {
    let mut store = TASK_STORE.write().await;
    store.insert(task.upload_id.clone(), task);
    save_tasks_async(&store);
}

/// 批量插入多个任务，只写一次磁盘，避免 10000 个文件时写盘 10000 次
async fn upsert_tasks_batch(tasks: Vec<TransferTask>) {
    if tasks.is_empty() {
        return;
    }
    let mut store = TASK_STORE.write().await;
    for task in tasks {
        store.insert(task.upload_id.clone(), task);
    }
    save_tasks_async(&store);
}

async fn update_task_status(upload_id: &str, status: TaskStatus, error: Option<String>) {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        task.status = status;
        task.error = error;
    }
    save_tasks_async(&store);
}

async fn update_task_progress(upload_id: &str, uploaded_chunks: u32) {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        task.uploaded_chunks = uploaded_chunks;
    }
    save_tasks_async(&store);
}

/// Entry point: fetch task detail, show file dialog, then start upload.
/// (Legacy: for pre-created upload_id from backend)
pub async fn enqueue_upload<R: Runtime>(app: &AppHandle<R>, upload_id: &str, token: Option<String>) -> Result<()> {
    tracing::info!("Enqueueing upload for {}", upload_id);

    // Skip if already active
    if let Some(existing) = get_task(upload_id).await {
        if existing.status == TaskStatus::Pending || existing.status == TaskStatus::Running {
            tracing::info!("Upload {} already queued", upload_id);
            return Ok(());
        }
    }

    // Fetch task detail from backend
    let uploader = Uploader::with_client(upload_id, token.clone(), SHARED_HTTP_CLIENT.clone())?;
    let detail = uploader.fetch_task_detail().await?;

    tracing::info!(
        "Task detail: {} ({} chunks of {} bytes)",
        detail.original_filename,
        detail.total_chunks,
        detail.chunk_size
    );

    // Create pending task entry
    let task = TransferTask {
        upload_id: upload_id.to_string(),
        filename: detail.original_filename.clone(),
        file_size: detail.file_size,
        total_chunks: detail.total_chunks,
        uploaded_chunks: 0,
        status: TaskStatus::Pending,
        error: None,
        created_at: now_ts(),
        file_path: None,
        chunk_size: Some(detail.chunk_size),
        user_id: Some(current_user_id()),
        project_id: detail.project_id.clone(),
        folder_id: detail.folder_id.clone(),
        retry_count: 0,
    };
    upsert_task(task).await;

    // Notify frontend to refresh (use emit_all on AppHandle)
    let _ = app.emit(
        "upload:select-file",
        json!({
            "upload_id": upload_id,
            "filename": detail.original_filename,
            "file_size": detail.file_size,
        }),
    );

    // Show window before file picker (required on macOS)
    crate::tray::show_window(app);

    // Show file picker dialog, then start upload
    let app_clone = app.clone();
    let upload_id_owned = upload_id.to_string();

    tokio::spawn(async move {
        match show_file_picker(&app_clone, &detail).await {
            Some(file_path) => {
                tracing::info!("User selected file: {:?}", file_path);
                if let Err(e) = start_upload(upload_id_owned, file_path, detail, token).await {
                    tracing::error!("Upload failed: {}", e);
                }
            }
            None => {
                tracing::info!("User cancelled file selection for {}", upload_id_owned);
                update_task_status(
                    &upload_id_owned,
                    TaskStatus::Cancelled,
                    Some("User cancelled".to_string()),
                )
                .await;
            }
        }
    });

    Ok(())
}

async fn show_file_picker<R: Runtime>(
    app: &AppHandle<R>,
    detail: &UploadTaskDetail,
) -> Option<PathBuf> {
    use tauri_plugin_dialog::{DialogExt, FilePath};

    let filename = detail.original_filename.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = std::sync::Mutex::new(Some(tx));

    app.dialog()
        .file()
        .set_title(format!("选择文件：{}", filename))
        .pick_file(move |result| {
            if let Some(sender) = tx.lock().unwrap().take() {
                let path = result.and_then(|fp| match fp {
                    FilePath::Path(p) => Some(p),
                    _ => None,
                });
                let _ = sender.send(path);
            }
        });

    rx.await.ok().flatten()
}

async fn start_upload(
    upload_id: String,
    file_path: PathBuf,
    detail: UploadTaskDetail,
    token: Option<String>,
) -> Result<()> {
    tracing::info!("start_upload called for {} (GLOBAL_PAUSED={} GLOBAL_CANCELLED={})",
        upload_id,
        GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst),
        GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst),
    );
    // 全局取消标志检查：cancel_all 后不启动新上传
    if GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!("Global cancel active, aborting start_upload for {}", upload_id);
        update_task_status(&upload_id, TaskStatus::Cancelled, Some("已取消".to_string())).await;
        ws_server::broadcast_message(json!({
            "type": "task_cancelled",
            "upload_id": upload_id,
        }));
        return Ok(());
    }

    // 全局暂停标志检查：pause_all 后不启动新上传，改为暂停状态等待恢复
    if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!("Global pause active, suspending start_upload for {}", upload_id);
        update_task_status(&upload_id, TaskStatus::Paused, None).await;
        PAUSED_SET.write().await.insert(upload_id.clone());
        ws_server::broadcast_message(json!({
            "type": "paused",
            "upload_id": upload_id,
        }));
        return Ok(());
    }

    update_task_status(&upload_id, TaskStatus::Running, None).await;

    // 注册 cancellation token
    let cancel_token = register_cancel_token(&upload_id).await;

    ws_server::broadcast_message(json!({
        "type": "upload_start",
        "upload_id": upload_id,
        "filename": detail.original_filename,
        "total_chunks": detail.total_chunks,
    }));

    let uploader = match Uploader::with_client(&upload_id, token, SHARED_HTTP_CLIENT.clone()) {
        Ok(u) => u,
        Err(e) => {
            remove_cancel_token(&upload_id).await;
            update_task_status(&upload_id, TaskStatus::Failed, Some(e.to_string())).await;
            return Err(e);
        }
    };
    let store_ref = TASK_STORE.clone();
    let upload_id_cb = upload_id.clone();

    // 节流：每 5 个分片或最后一个分片才写磁盘，减少锁竞争和 I/O
    let last_saved_chunk = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let result = uploader
        .run_upload(file_path, &detail, cancel_token.clone(), move |uploaded, total| {
            let store = store_ref.clone();
            let uid = upload_id_cb.clone();
            let last_saved = last_saved_chunk.clone();
            tokio::spawn(async move {
                let mut s = store.write().await;
                if let Some(t) = s.get_mut(&uid) {
                    t.uploaded_chunks = uploaded;
                }
                // 只在每 5 个分片、或最后一个分片时写磁盘
                let prev = last_saved.load(std::sync::atomic::Ordering::Relaxed);
                if uploaded >= total || uploaded.saturating_sub(prev) >= 5 {
                    last_saved.store(uploaded, std::sync::atomic::Ordering::Relaxed);
                    save_tasks_async(&s);
                }
            });
        })
        .await;

    // 清理 token
    remove_cancel_token(&upload_id).await;

    match result {
        Ok(()) => {
            let mut store = TASK_STORE.write().await;
            if let Some(task) = store.get_mut(&upload_id) {
                task.status = TaskStatus::Completed;
                task.uploaded_chunks = task.total_chunks;
            }
            save_tasks_async(&store);
            ws_server::broadcast_message(json!({
                "type": "completed",
                "upload_id": upload_id,
            }));
            tracing::info!("Upload {} completed", upload_id);
        }
        Err(e) if e.to_string() == "paused" => {
            // 检查实际状态：可能是暂停也可能是取消
            let status = {
                let store = TASK_STORE.read().await;
                store.get(&upload_id).map(|t| t.status.clone())
            };
            match status {
                Some(TaskStatus::Cancelled) => {
                    tracing::info!("Upload {} cancelled", upload_id);
                    ws_server::broadcast_message(json!({
                        "type": "task_cancelled",
                        "upload_id": upload_id,
                    }));
                }
                _ => {
                    tracing::info!("Upload {} paused", upload_id);
                }
            }
        }        Err(e) => {
            let err_str = e.to_string();
            tracing::error!("Upload {} failed: {}", upload_id, err_str);
            update_task_status(&upload_id, TaskStatus::Failed, Some(err_str)).await;
            // 不调用 /abort，保留后端 upload_task 状态，允许用户续传
            // 只有用户主动取消时才 abort（见 cancel_task）
        }
    }

    Ok(())
}

/// 只执行分片上传阶段（不含验证和 complete），用于批量上传场景。
/// 调用方在此函数返回后可以立即释放并发 permit，然后再调用 finish_upload_task。
async fn start_upload_chunks(
    upload_id: String,
    file_path: PathBuf,
    detail: UploadTaskDetail,
    token: Option<String>,
) -> Result<()> {
    tracing::info!("start_upload_chunks called for {} (GLOBAL_PAUSED={} GLOBAL_CANCELLED={})",
        upload_id,
        GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst),
        GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst),
    );

    // 全局取消标志检查
    if GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!("Global cancel active, aborting start_upload_chunks for {}", upload_id);
        update_task_status(&upload_id, TaskStatus::Cancelled, Some("已取消".to_string())).await;
        ws_server::broadcast_message(json!({
            "type": "task_cancelled",
            "upload_id": upload_id,
        }));
        return Ok(());
    }

    // 全局暂停标志检查
    if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!("Global pause active, suspending start_upload_chunks for {}", upload_id);
        update_task_status(&upload_id, TaskStatus::Paused, None).await;
        PAUSED_SET.write().await.insert(upload_id.clone());
        ws_server::broadcast_message(json!({
            "type": "paused",
            "upload_id": upload_id,
        }));
        return Ok(());
    }

    update_task_status(&upload_id, TaskStatus::Running, None).await;

    // 注册 cancellation token
    let cancel_token = register_cancel_token(&upload_id).await;

    ws_server::broadcast_message(json!({
        "type": "upload_start",
        "upload_id": upload_id,
        "filename": detail.original_filename,
        "total_chunks": detail.total_chunks,
    }));

    let uploader = match Uploader::with_client(&upload_id, token, SHARED_HTTP_CLIENT.clone()) {
        Ok(u) => u,
        Err(e) => {
            remove_cancel_token(&upload_id).await;
            update_task_status(&upload_id, TaskStatus::Failed, Some(e.to_string())).await;
            return Err(e);
        }
    };
    let store_ref = TASK_STORE.clone();
    let upload_id_cb = upload_id.clone();

    let last_saved_chunk = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let result = uploader
        .run_upload_chunks(file_path, &detail, cancel_token.clone(), move |uploaded, total| {
            let store = store_ref.clone();
            let uid = upload_id_cb.clone();
            let last_saved = last_saved_chunk.clone();
            tokio::spawn(async move {
                let mut s = store.write().await;
                if let Some(t) = s.get_mut(&uid) {
                    t.uploaded_chunks = uploaded;
                }
                let prev = last_saved.load(std::sync::atomic::Ordering::Relaxed);
                if uploaded >= total || uploaded.saturating_sub(prev) >= 5 {
                    last_saved.store(uploaded, std::sync::atomic::Ordering::Relaxed);
                    save_tasks_async(&s);
                }
            });
        })
        .await;

    // 清理 token
    remove_cancel_token(&upload_id).await;

    match result {
        Ok(()) => {
            // 分片上传成功，返回 Ok 让调用方释放 permit 后再做 finish
            tracing::info!("Upload {} all chunks done, ready for finish phase", upload_id);
            Ok(())
        }
        Err(e) if e.to_string() == "paused" => {
            let status = {
                let store = TASK_STORE.read().await;
                store.get(&upload_id).map(|t| t.status.clone())
            };
            match status {
                Some(TaskStatus::Cancelled) => {
                    tracing::info!("Upload {} cancelled", upload_id);
                    ws_server::broadcast_message(json!({
                        "type": "task_cancelled",
                        "upload_id": upload_id,
                    }));
                }
                _ => {
                    tracing::info!("Upload {} paused", upload_id);
                }
            }
            Err(e)
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::error!("Upload {} chunks failed: {}", upload_id, err_str);
            update_task_status(&upload_id, TaskStatus::Failed, Some(err_str)).await;
            Err(e)
        }
    }
}

/// 分片上传完成后的收尾阶段：验证 + complete + 更新状态。
/// 此函数应在释放 FILE_UPLOAD_SEMAPHORE permit 之后调用。
async fn finish_upload_task(
    upload_id: &str,
    total_chunks: u32,
    token: Option<String>,
) -> Result<()> {
    let uploader = Uploader::with_client(upload_id, token, SHARED_HTTP_CLIENT.clone())?;

    match uploader.finish_upload(total_chunks).await {
        Ok(()) => {
            let mut store = TASK_STORE.write().await;
            if let Some(task) = store.get_mut(upload_id) {
                task.status = TaskStatus::Completed;
                task.uploaded_chunks = task.total_chunks;
            }
            save_tasks_async(&store);
            ws_server::broadcast_message(json!({
                "type": "completed",
                "upload_id": upload_id,
            }));
            tracing::info!("Upload {} completed (finish phase done)", upload_id);
            Ok(())
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::error!("Upload {} finish phase failed: {}", upload_id, err_str);
            update_task_status(upload_id, TaskStatus::Failed, Some(err_str.clone())).await;
            Err(anyhow::anyhow!(err_str))
        }
    }
}

pub async fn cancel_task(upload_id: &str) -> bool {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        if matches!(task.status, TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused) {
            task.status = TaskStatus::Cancelled;
            save_tasks_async(&store);
            drop(store);
            // cancel 正在执行的上传
            if let Some(token) = CANCEL_TOKENS.read().await.get(upload_id) {
                token.cancel();
            }
            remove_cancel_token(upload_id).await;
            PAUSED_SET.write().await.remove(upload_id);
            // 通知后端 abort，清理 OSS multipart upload
            let upload_id_owned = upload_id.to_string();
            tokio::spawn(async move {
                if let Some(token) = crate::auth::load_token() {
                    let cfg = crate::config::get_config();
                    let url = format!("{}/api/v1/upload/{}/abort", cfg.api_url, upload_id_owned);
                    let _ = SHARED_HTTP_CLIENT.delete(&url).bearer_auth(&token).send().await;
                    tracing::info!("Aborted upload {} on backend", upload_id_owned);
                }
            });
            ws_server::broadcast_message(json!({
                "type": "task_cancelled",
                "upload_id": upload_id,
            }));
            return true;
        }
    }
    false
}

pub async fn pause_task(upload_id: &str) -> bool {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        if task.status == TaskStatus::Running || task.status == TaskStatus::Pending {
            task.status = TaskStatus::Paused;
            save_tasks_async(&store);
            drop(store);
            // cancel 正在执行的上传
            PAUSED_SET.write().await.insert(upload_id.to_string());
            if let Some(token) = CANCEL_TOKENS.read().await.get(upload_id) {
                token.cancel();
            }
            ws_server::broadcast_message(json!({
                "type": "paused",
                "upload_id": upload_id,
            }));
            return true;
        }
    }
    false
}

pub async fn resume_task(upload_id: &str) -> bool {
    let task = {
        let mut store = TASK_STORE.write().await;
        match store.get_mut(upload_id) {
            Some(task) if task.status == TaskStatus::Paused || task.status == TaskStatus::Failed || task.status == TaskStatus::Pending => {
                task.status = TaskStatus::Pending;
                task.retry_count = 0; // 用户手动重试时重置重试计数
                let cloned = task.clone();
                save_tasks_async(&store);
                Some(cloned)
            }
            _ => None,
        }
        // store 写锁在这里释放
    };

    if task.is_some() {
        PAUSED_SET.write().await.remove(upload_id);
        remove_cancel_token(upload_id).await;
    }

    if let Some(task) = task {
        if let Some(file_path_str) = &task.file_path {
            let file_path = PathBuf::from(file_path_str);
            if !file_path.exists() {
                update_task_status(upload_id, TaskStatus::Failed, Some("文件不存在，无法续传".to_string())).await;
                return false;
            }
            let upload_id_owned = upload_id.to_string();
            let task_clone = task.clone();
            let token = crate::auth::load_token();
            tokio::spawn(async move {
                let cfg = crate::config::get_config();

                // 先做 check_and_reinit（网络请求），不占用 FILE_UPLOAD_SEMAPHORE
                let effective_upload_id = if let Some(ref tok) = token {
                    match check_and_reinit_upload(&upload_id_owned, &task_clone, tok, &cfg).await {
                        Ok(new_id) => new_id,
                        Err(e) => {
                            tracing::error!("Failed to check/reinit upload {}: {}", upload_id_owned, e);
                            update_task_status(&upload_id_owned, TaskStatus::Failed, Some(e.to_string())).await;
                            return;
                        }
                    }
                } else {
                    upload_id_owned.clone()
                };

                // 如果 check_and_reinit 发现后端已 completed 并同步了本地状态，直接返回
                {
                    let store = TASK_STORE.read().await;
                    if let Some(t) = store.get(&effective_upload_id) {
                        if t.status == TaskStatus::Completed {
                            tracing::info!("resume_task {}: already completed, skipping upload", effective_upload_id);
                            return;
                        }
                    }
                }

                // reinit 完成后再等信号量，减少 permit 占用时间
                let available = FILE_UPLOAD_SEMAPHORE.available_permits();
                tracing::info!("resume_task {}: waiting for FILE_UPLOAD_SEMAPHORE (available={})", effective_upload_id, available);
                let _permit = match FILE_UPLOAD_SEMAPHORE.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                tracing::info!("resume_task {}: got FILE_UPLOAD_SEMAPHORE permit, starting upload", effective_upload_id);

                // 通知前端任务即将开始上传
                update_task_status(&effective_upload_id, TaskStatus::Running, None).await;
                ws_server::broadcast_message(json!({
                    "type": "upload_start",
                    "upload_id": effective_upload_id,
                    "filename": task_clone.filename,
                    "total_chunks": task_clone.total_chunks,
                }));

                // 优先用保存的 chunk_size，保证续传时分片边界一致
                let chunk_size = task_clone.chunk_size.unwrap_or(cfg.chunk_size as u64);
                let detail = crate::transfer::UploadTaskDetail {
                    upload_id: effective_upload_id.clone(),
                    asset_id: String::new(),
                    original_filename: task_clone.filename.clone(),
                    file_size: task_clone.file_size,
                    chunk_size,
                    total_chunks: task_clone.total_chunks,
                    uploaded_chunks: vec![],
                    oss_path: String::new(),
                    oss_upload_id: None,
                    project_id: task_clone.project_id.clone(),
                    folder_id: task_clone.folder_id.clone(),
                };
                if let Err(e) = start_upload(effective_upload_id.clone(), file_path, detail, token).await {
                    tracing::error!("Resume upload failed: {}", e);
                }
            });
            ws_server::broadcast_message(json!({
                "type": "resumed",
                "upload_id": upload_id,
            }));
            return true;
        }
    }
    false
}

/// 检查后端 upload_task 状态，如果不可续传（aborted/failed/completed）则重新 init，
/// 返回实际应使用的 upload_id（可能是新的）
async fn check_and_reinit_upload(
    upload_id: &str,
    task: &TransferTask,
    token: &str,
    cfg: &crate::config::AppConfig,
) -> Result<String> {
    let client = SHARED_HTTP_CLIENT.clone();

    // 查询后端进度，判断状态
    let progress_url = format!("{}/api/v1/upload/{}/progress", cfg.api_url, upload_id);
    let needs_reinit = match client.get(&progress_url).bearer_auth(token).send().await {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            let status = body["data"]["status"].as_str().unwrap_or("");
            tracing::info!("Backend upload {} status: {}", upload_id, status);
            // 如果后端已经 completed，直接标记本地为完成，不需要 reinit
            if status == "completed" {
                let mut store = TASK_STORE.write().await;
                if let Some(t) = store.get_mut(upload_id) {
                    t.status = TaskStatus::Completed;
                    t.uploaded_chunks = t.total_chunks;
                }
                save_tasks_async(&store);
                ws_server::broadcast_message(json!({
                    "type": "completed",
                    "upload_id": upload_id,
                }));
                tracing::info!("Upload {} already completed on backend, synced local status", upload_id);
                return Ok(upload_id.to_string());
            }
            matches!(status, "aborted" | "failed")
        }
        Ok(r) if r.status() == 404 => {
            tracing::info!("Upload task {} not found on backend, will reinit", upload_id);
            true
        }
        Ok(r) => {
            tracing::warn!("Unexpected status {} checking upload {}", r.status(), upload_id);
            false
        }
        Err(e) => {
            tracing::warn!("Failed to check upload status for {}: {}", upload_id, e);
            false
        }
    };

    if !needs_reinit {
        return Ok(upload_id.to_string());
    }

    tracing::info!("Reinitializing upload for task {} (file: {})", upload_id, task.filename);

    // 优先使用本地 task 中保存的 project_id / folder_id，避免依赖后端查询
    // 如果本地没有（旧版持久化任务），则回退到查询后端 client task detail 接口
    let (project_id, folder_id) = match task.project_id.clone() {
        Some(p) => (p, task.folder_id.clone()),
        None => {
            tracing::info!("task {} has no local project_id, fetching from backend...", upload_id);
            let detail_url = format!("{}/api/v1/client/tasks/upload/{}", cfg.api_url, upload_id);
            match client.get(&detail_url).bearer_auth(token).send().await {
                Ok(r) if r.status().is_success() => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    match body["data"]["project_id"].as_str() {
                        Some(pid) => {
                            let fid = body["data"]["folder_id"].as_str().map(|s| s.to_string());
                            tracing::info!("Fetched project_id={} for task {}", pid, upload_id);
                            (pid.to_string(), fid)
                        }
                        None => {
                            return Err(anyhow::anyhow!(
                                "Cannot reinit upload: unable to determine project_id for task {}",
                                upload_id
                            ));
                        }
                    }
                }
                Ok(r) => {
                    return Err(anyhow::anyhow!(
                        "Cannot reinit upload: backend returned {} when fetching task detail for {}",
                        r.status(),
                        upload_id
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Cannot reinit upload: failed to fetch task detail for {}: {}",
                        upload_id,
                        e
                    ));
                }
            }
        }
    };

    // 重新调用 /upload/init
    let init_url = format!("{}/api/v1/upload/init", cfg.api_url);
    let init_body = serde_json::json!({
        "filename": task.filename,
        "file_size": task.file_size,
        "project_id": project_id,
        "folder_id": folder_id,
    });

    let resp = client
        .post(&init_url)
        .bearer_auth(token)
        .json(&init_body)
        .send()
        .await
        .context("Failed to reinit upload")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Reinit upload API error {}: {}", status, text));
    }

    let wrapper: serde_json::Value = resp.json().await.context("Failed to parse reinit response")?;
    let new_upload_id = wrapper["data"]["upload_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No upload_id in reinit response"))?
        .to_string();

    let new_total_chunks = wrapper["data"]["total_chunks"]
        .as_u64()
        .unwrap_or(task.total_chunks as u64) as u32;
    let new_chunk_size = wrapper["data"]["chunk_size"]
        .as_u64()
        .unwrap_or(task.chunk_size.unwrap_or(20 * 1024 * 1024));

    tracing::info!("Reinitialized upload: old={} new={}", upload_id, new_upload_id);

    // 更新本地 task store：用新的 upload_id 替换旧的
    {
        let mut store = TASK_STORE.write().await;
        if let Some(old_task) = store.remove(upload_id) {
            let mut new_task = old_task;
            new_task.upload_id = new_upload_id.clone();
            new_task.total_chunks = new_total_chunks;
            new_task.chunk_size = Some(new_chunk_size);
            new_task.uploaded_chunks = 0;
            new_task.status = TaskStatus::Pending;
            new_task.error = None;
            store.insert(new_upload_id.clone(), new_task);
            save_tasks_async(&store);
        }
    }

    // 通知前端任务 ID 已变更，触发全量刷新
    ws_server::broadcast_message(json!({
        "type": "task_reinit",
        "old_upload_id": upload_id,
        "new_upload_id": new_upload_id,
    }));

    Ok(new_upload_id)
}

/// New entry point: pick files/folder first, then call /upload/init, then upload.
pub async fn enqueue_upload_by_params<R: Runtime>(
    app: &AppHandle<R>,
    project_id: String,
    folder_id: Option<String>,
    asset_type_id: Option<String>,
    count: u32,
    token: Option<String>,
) -> Result<()> {
    use tauri_plugin_dialog::{DialogExt, FilePath};

    tracing::info!("Picking {} file(s) for project {}", count, project_id);

    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<std::path::PathBuf>>>();
    let tx = std::sync::Mutex::new(Some(tx));

    // count=0 means pick folder
    if count == 0 {
        app.dialog()
            .file()
            .set_title("选择要上传的文件夹")
            .pick_folder(move |result| {
                if let Some(sender) = tx.lock().unwrap().take() {
                    let paths = result.and_then(|fp| match fp {
                        FilePath::Path(p) => Some(vec![p]),
                        _ => None,
                    });
                    let _ = sender.send(paths);
                }
            });
    } else if count == 1 {
        app.dialog()
            .file()
            .set_title("选择要上传的文件")
            .pick_file(move |result| {
                if let Some(sender) = tx.lock().unwrap().take() {
                    let paths = result.and_then(|fp| match fp {
                        FilePath::Path(p) => Some(vec![p]),
                        _ => None,
                    });
                    let _ = sender.send(paths);
                }
            });
    } else {
        app.dialog()
            .file()
            .set_title(format!("选择要上传的 {} 个文件", count))
            .pick_files(move |result| {
                if let Some(sender) = tx.lock().unwrap().take() {
                    let paths = result.map(|fps| {
                        fps.into_iter()
                            .filter_map(|fp| match fp {
                                FilePath::Path(p) => Some(p),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    });
                    let _ = sender.send(paths);
                }
            });
    }

    let paths = match rx.await.ok().flatten() {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::info!("User cancelled file selection");
            return Ok(());
        }
    };

    tracing::info!("User selected {} path(s)", paths.len());

    // If count==0 (folder mode), scan folder recursively
    let files_to_upload = if count == 0 && paths.len() == 1 {
        let folder_path = paths[0].clone();
        if !folder_path.is_dir() {
            tracing::error!("Selected path is not a folder: {:?}", folder_path);
            return Ok(());
        }
        // scan_folder_recursive 是同步 IO，放到 blocking 线程池执行，
        // 避免在 async 上下文里持有大量 ReadDir 句柄耗尽内核对象
        match tokio::task::spawn_blocking(move || scan_folder_recursive(&folder_path)).await {
            Ok(Ok(files)) => files,
            Ok(Err(e)) => {
                tracing::error!("Failed to scan folder: {}", e);
                return Ok(());
            }
            Err(e) => {
                tracing::error!("scan_folder_recursive panicked: {}", e);
                return Ok(());
            }
        }
    } else {
        // File mode: use paths directly
        paths.into_iter().map(|p| (p, None)).collect()
    };

    tracing::info!("Total files to upload: {}", files_to_upload.len());

    // 用户主动发起新上传，清除全局取消标志
    // 注意：不在此处清除 GLOBAL_PAUSED，全局暂停必须由用户显式点击"全部恢复"来解除
    GLOBAL_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);

    // 为本次批量入队创建新的取消令牌，替换旧的（旧令牌已 cancel 也无妨）
    let enqueue_token = {
        let new_token = CancellationToken::new();
        *ENQUEUE_CANCEL_TOKEN.write().await = new_token.clone();
        new_token
    };

    // Get API token
    let api_token = match token.clone().or_else(|| crate::auth::load_token()) {
        Some(t) => t,
        None => {
            tracing::error!("Not authenticated, cannot init upload");
            return Ok(());
        }
    };

    let cfg = crate::config::get_config();
    let client = Arc::new(SHARED_HTTP_CLIENT.clone());

    // If folder mode, create folder structure first
    let folder_map = if count == 0 {
        create_folder_structure(&client, &cfg.api_url, &api_token, &project_id, folder_id.as_deref(), &files_to_upload).await?
    } else {
        std::collections::HashMap::new()
    };

    let folder_map = Arc::new(folder_map);
    let total_files = files_to_upload.len();
    tracing::info!("Starting init phase for {} files", total_files);

    // 本次批量上传的会话计数器，init 成功的文件数（秒传 + 普通上传）才计入 total
    // 注意：init 失败的文件不会进入上传阶段，也不计入 counter，所以用实际 init 成功数
    // 这里先用 total_files 初始化，init 失败时通过 record_failure 补偿
    let session_counter = Arc::new(SessionCounter::new(total_files));

    // ── 阶段一：并发 init（哈希 + /upload/init），限制并发数避免瞬间建立大量连接 ──
    // 每批最多 MAX_CONCURRENT_INIT 个文件同时做哈希+init，完成一个补充一个（滑动窗口）
    const MAX_CONCURRENT_INIT: usize = 8;

    // 用通道把"已 init 完成、待上传"的任务传给上传阶段
    // 通道容量 = MAX_CONCURRENT_FILES * 2，背压控制，避免 init 太快把内存撑爆
    let (upload_tx, mut upload_rx) = tokio::sync::mpsc::channel::<(String, PathBuf, super::UploadTaskDetail)>(
        MAX_CONCURRENT_FILES * 2,
    );

    let project_id_init = project_id.clone();
    let folder_id_init = folder_id.clone();
    let asset_type_id_init = asset_type_id.clone();
    let api_token_init = api_token.clone();
    let cfg_init = cfg.clone();
    let client_init = client.clone();
    let folder_map_init = folder_map.clone();
    let app_init = app.clone();
    let enqueue_token_init = enqueue_token.clone();
    let session_counter_init = session_counter.clone();

    // 在独立 task 里跑 init 阶段，不阻塞当前函数返回
    tokio::spawn(async move {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        // 每个 init 任务的输入
        struct InitInput {
            file_path: PathBuf,
            relative_path: Option<String>,
        }

        let mut in_flight: FuturesUnordered<
            tokio::task::JoinHandle<Result<(String, PathBuf, super::UploadTaskDetail, TransferTask, bool), String>>,
        > = FuturesUnordered::new();

        let mut file_iter = files_to_upload.into_iter();
        let mut batch_tasks: Vec<TransferTask> = Vec::new();
        // 批量写盘阈值：每积累 BATCH_WRITE_SIZE 个任务写一次磁盘
        const BATCH_WRITE_SIZE: usize = 100;

        // 填满初始并发槽
        for _ in 0..MAX_CONCURRENT_INIT {
            if let Some((file_path, relative_path)) = file_iter.next() {
                let input = InitInput { file_path, relative_path };
                in_flight.push(spawn_init_task(
                    input.file_path,
                    input.relative_path,
                    client_init.clone(),
                    cfg_init.clone(),
                    project_id_init.clone(),
                    folder_id_init.clone(),
                    asset_type_id_init.clone(),
                    api_token_init.clone(),
                    folder_map_init.clone(),
                ));
            }
        }

        loop {
            if in_flight.is_empty() {
                break;
            }

            // 同时监听取消信号，cancel_all 时立即退出 init 循环
            let result = tokio::select! {
                r = in_flight.next() => r,
                _ = enqueue_token_init.cancelled() => {
                    tracing::info!("Init phase cancelled by cancel_all");
                    break;
                }
            };

            match result {
                Some(Ok(Ok((upload_id, file_path, detail, task, is_instant)))) => {
                    if is_instant {
                        // 秒传：直接入批量写盘队列，通知前端
                        let _ = app_init.emit("upload:instant", serde_json::json!({
                            "filename": task.filename,
                            "file_size": task.file_size,
                        }));
                        ws_server::broadcast_message(json!({
                            "type": "upload_instant",
                            "upload_id": upload_id,
                            "filename": task.filename,
                            "file_size": task.file_size,
                        }));
                        // 秒传视为成功
                        session_counter_init.record_success();
                        if session_counter_init.is_complete() {
                            let log = session_counter_init.to_log();
                            tokio::task::spawn_blocking(move || append_upload_log(log));
                        }
                        batch_tasks.push(task);
                    } else {
                        // 普通上传：通知前端已入队，然后发给上传阶段
                        let _ = app_init.emit("upload:started", serde_json::json!({
                            "upload_id": upload_id,
                            "filename": task.filename,
                            "file_size": task.file_size,
                        }));
                        ws_server::broadcast_message(json!({
                            "type": "upload_queued",
                            "upload_id": upload_id,
                            "filename": task.filename,
                            "total_chunks": detail.total_chunks,
                        }));
                        batch_tasks.push(task);

                        // 检查全局取消/暂停标志
                        if GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
                            // 不再发送到上传通道，标记为已取消
                            let mut store = TASK_STORE.write().await;
                            if let Some(t) = store.get_mut(&upload_id) {
                                t.status = TaskStatus::Cancelled;
                            }
                            save_tasks_async(&store);
                        } else if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
                            // 不发送到上传通道，标记为暂停状态
                            let mut store = TASK_STORE.write().await;
                            if let Some(t) = store.get_mut(&upload_id) {
                                t.status = TaskStatus::Paused;
                            }
                            save_tasks_async(&store);
                            PAUSED_SET.write().await.insert(upload_id.clone());
                        } else {
                            // 发送到上传通道，同时监听取消（通道满时背压等待，取消时立即放弃）
                            tokio::select! {
                                r = upload_tx.send((upload_id, file_path, detail)) => {
                                    if r.is_err() {
                                        tracing::info!("Upload channel closed, stopping init");
                                        break;
                                    }
                                }
                                _ = enqueue_token_init.cancelled() => {
                                    tracing::info!("Init phase cancelled while sending to upload channel");
                                    break;
                                }
                            }
                        }
                    }

                    // 批量写盘
                    if batch_tasks.len() >= BATCH_WRITE_SIZE {
                        upsert_tasks_batch(std::mem::take(&mut batch_tasks)).await;
                    }
                }
                Some(Ok(Err(failed_filename))) => {
                    // init 失败，spawn_init_task 内部已 log，这里记录到会话计数器
                    session_counter_init.record_failure(&failed_filename);
                    if session_counter_init.is_complete() {
                        let log = session_counter_init.to_log();
                        tokio::task::spawn_blocking(move || append_upload_log(log));
                    }
                }
                Some(Err(e)) => {
                    tracing::error!("Init task panicked: {}", e);
                }
                None => break,
            }

            // 取消后不再补充新文件
            if enqueue_token_init.is_cancelled() {
                break;
            }

            // 补充下一个文件
            if let Some((file_path, relative_path)) = file_iter.next() {
                in_flight.push(spawn_init_task(
                    file_path,
                    relative_path,
                    client_init.clone(),
                    cfg_init.clone(),
                    project_id_init.clone(),
                    folder_id_init.clone(),
                    asset_type_id_init.clone(),
                    api_token_init.clone(),
                    folder_map_init.clone(),
                ));
            }
        }

        // 写入剩余任务
        if !batch_tasks.is_empty() {
            upsert_tasks_batch(batch_tasks).await;
        }

        tracing::info!("Init phase complete for {} files", total_files);
        // upload_tx 在此处 drop，upload_rx 会收到 None，上传阶段自然结束
    });

    // ── 阶段二：上传阶段，从通道消费，使用全局文件信号量限制并发 ──────────────
    // 使用 FuturesUnordered 滑动窗口模式：先从通道取任务，再等 permit，避免大量 task 堆积
    tokio::spawn(async move {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<()>> = FuturesUnordered::new();
        let mut channel_closed = false;

        loop {
            // 如果被取消，退出
            if enqueue_token.is_cancelled() {
                tracing::info!("Upload consumer cancelled by cancel_all, draining channel");
                while upload_rx.try_recv().is_ok() {}
                break;
            }

            // 如果全局暂停，停止从通道消费新任务（已 in-flight 的由 start_upload_chunks 内部检查）
            if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) && channel_closed == false {
                // 等待暂停解除、取消信号、或 in-flight 任务完成
                tokio::select! {
                    biased;
                    _ = enqueue_token.cancelled() => {
                        tracing::info!("Upload consumer cancelled while paused");
                        while upload_rx.try_recv().is_ok() {}
                        break;
                    }
                    _ = async {
                        // 轮询等待暂停解除
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            if !GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
                                break;
                            }
                        }
                    } => {
                        // 暂停已解除，继续消费
                        tracing::info!("Upload consumer: global pause lifted, resuming consumption");
                        continue;
                    }
                    Some(_) = in_flight.next(), if !in_flight.is_empty() => {
                        // in-flight 任务完成，继续循环
                        continue;
                    }
                }
            }

            if !channel_closed && in_flight.len() < MAX_CONCURRENT_FILES + 3 {
                // 还有空位，尝试从通道取任务
                tokio::select! {
                    biased; // 优先检查取消信号

                    _ = enqueue_token.cancelled() => {
                        tracing::info!("Upload consumer cancelled by cancel_all");
                        while upload_rx.try_recv().is_ok() {}
                        break;
                    }

                    msg = upload_rx.recv() => {
                        match msg {
                            Some((upload_id, file_path, detail)) => {
                                let file_sem = FILE_UPLOAD_SEMAPHORE.clone();
                                let token_clone = Some(api_token.clone());
                                let counter = session_counter.clone();
                                let filename = detail.original_filename.clone();
                                let cancel = enqueue_token.clone();
                                in_flight.push(tokio::spawn(async move {
                                    // 等待 permit，同时监听取消
                                    let permit = tokio::select! {
                                        biased;
                                        _ = cancel.cancelled() => {
                                            tracing::info!("Upload worker for {}: cancelled while waiting for permit", upload_id);
                                            update_task_status(&upload_id, TaskStatus::Cancelled, Some("已取消".to_string())).await;
                                            return;
                                        }
                                        p = file_sem.acquire_owned() => {
                                            match p {
                                                Ok(p) => p,
                                                Err(_) => return,
                                            }
                                        }
                                    };
                                    tracing::info!("Upload worker for {}: got permit, starting", upload_id);

                                    // 获取 permit 后检查任务当前状态，避免与 resume_task 重复启动
                                    {
                                        let store = TASK_STORE.read().await;
                                        if let Some(t) = store.get(&upload_id) {
                                            if matches!(t.status, TaskStatus::Running | TaskStatus::Completed | TaskStatus::Cancelled) {
                                                tracing::info!("Upload worker for {}: task already {:?}, skipping", upload_id, t.status);
                                                return;
                                            }
                                        } else {
                                            // 任务已被删除（可能 reinit 换了 ID）
                                            tracing::info!("Upload worker for {}: task not found in store, skipping", upload_id);
                                            return;
                                        }
                                    }

                                    // 获取 permit 后再次检查全局暂停标志，避免竞态
                                    if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
                                        tracing::info!("Upload worker for {}: global pause detected after permit, suspending", upload_id);
                                        update_task_status(&upload_id, TaskStatus::Paused, None).await;
                                        PAUSED_SET.write().await.insert(upload_id.clone());
                                        ws_server::broadcast_message(json!({
                                            "type": "paused",
                                            "upload_id": upload_id,
                                        }));
                                        return;
                                    }

                                    // 阶段 A：分片上传（持有 permit），失败自动重试最多 MAX_RETRY_COUNT 次
                                    let mut retry_attempt = 0u32;
                                    let mut chunks_result;
                                    loop {
                                        chunks_result = start_upload_chunks(
                                            upload_id.clone(), file_path.clone(), detail.clone(), token_clone.clone()
                                        ).await;

                                        match &chunks_result {
                                            Ok(()) => break,
                                            Err(e) if e.to_string() == "paused" => break,
                                            Err(e) => {
                                                retry_attempt += 1;
                                                // 更新 retry_count 到 task store
                                                {
                                                    let mut store = TASK_STORE.write().await;
                                                    if let Some(t) = store.get_mut(&upload_id) {
                                                        t.retry_count = retry_attempt;
                                                    }
                                                    save_tasks_async(&store);
                                                }
                                                if retry_attempt >= MAX_RETRY_COUNT {
                                                    tracing::error!(
                                                        "Upload {} chunks failed after {} retries: {}",
                                                        upload_id, retry_attempt, e
                                                    );
                                                    break;
                                                }
                                                tracing::warn!(
                                                    "Upload {} chunks failed (attempt {}/{}): {}, retrying in 2s...",
                                                    upload_id, retry_attempt, MAX_RETRY_COUNT, e
                                                );
                                                // 通知前端正在重试
                                                ws_server::broadcast_message(json!({
                                                    "type": "task_status",
                                                    "upload_id": upload_id,
                                                    "status": "running",
                                                    "retry_count": retry_attempt,
                                                }));
                                                // 检查全局取消/暂停，避免无意义重试
                                                if GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst)
                                                    || GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst)
                                                {
                                                    break;
                                                }
                                                // 重试前等待 2 秒
                                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                // 重置状态为 Running 以便重试
                                                update_task_status(&upload_id, TaskStatus::Running, None).await;
                                            }
                                        }
                                    }

                                    // 分片上传完成后立即释放 permit
                                    drop(permit);
                                    tracing::info!("Upload worker for {}: permit released", upload_id);

                                    // 阶段 B：验证 + complete（不持有 permit）
                                    match chunks_result {
                                        Ok(()) => {
                                            let actual_status = {
                                                let store = TASK_STORE.read().await;
                                                store.get(&upload_id).map(|t| t.status.clone())
                                            };
                                            match actual_status {
                                                Some(TaskStatus::Running) | Some(TaskStatus::Pending) => {
                                                    // finish 阶段也支持重试
                                                    let mut finish_ok = false;
                                                    for finish_attempt in 0..MAX_RETRY_COUNT {
                                                        match finish_upload_task(&upload_id, detail.total_chunks, token_clone.clone()).await {
                                                            Ok(()) => {
                                                                finish_ok = true;
                                                                break;
                                                            }
                                                            Err(e) => {
                                                                if finish_attempt + 1 >= MAX_RETRY_COUNT {
                                                                    tracing::error!("Upload {} finish failed after {} retries: {}", upload_id, finish_attempt + 1, e);
                                                                } else {
                                                                    tracing::warn!("Upload {} finish failed (attempt {}/{}): {}, retrying in 2s...", upload_id, finish_attempt + 1, MAX_RETRY_COUNT, e);
                                                                    // 重置状态为 Running 以便重试
                                                                    update_task_status(&upload_id, TaskStatus::Running, None).await;
                                                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if finish_ok {
                                                        counter.record_success();
                                                    } else {
                                                        counter.record_failure(&filename);
                                                    }
                                                }
                                                Some(TaskStatus::Cancelled) => {
                                                    tracing::info!("Upload {} was cancelled, skipping finish", upload_id);
                                                    counter.record_failure(&format!("<cancelled> {}", filename));
                                                }
                                                Some(TaskStatus::Paused) => {
                                                    tracing::info!("Upload {} was paused, skipping finish", upload_id);
                                                }
                                                Some(TaskStatus::Completed) => {
                                                    // 已经完成（可能是秒传或其他路径标记的）
                                                    tracing::info!("Upload {} already completed", upload_id);
                                                    counter.record_success();
                                                }
                                                _ => {
                                                    let mut finish_ok = false;
                                                    for finish_attempt in 0..MAX_RETRY_COUNT {
                                                        match finish_upload_task(&upload_id, detail.total_chunks, token_clone.clone()).await {
                                                            Ok(()) => {
                                                                finish_ok = true;
                                                                break;
                                                            }
                                                            Err(e) => {
                                                                if finish_attempt + 1 >= MAX_RETRY_COUNT {
                                                                    tracing::error!("Upload {} finish failed after {} retries: {}", upload_id, finish_attempt + 1, e);
                                                                } else {
                                                                    tracing::warn!("Upload {} finish failed (attempt {}/{}): {}, retrying in 2s...", upload_id, finish_attempt + 1, MAX_RETRY_COUNT, e);
                                                                    update_task_status(&upload_id, TaskStatus::Running, None).await;
                                                                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                                                }
                                                            }
                                                        }
                                                    }
                                                    if finish_ok {
                                                        counter.record_success();
                                                    } else {
                                                        counter.record_failure(&filename);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let err_str = e.to_string();
                                            if err_str != "paused" {
                                                counter.record_failure(&filename);
                                            } else {
                                                counter.record_failure(&format!("<paused> {}", filename));
                                            }
                                            tracing::error!("Upload {} chunks failed: {}", upload_id, err_str);
                                        }
                                    }

                                    if counter.is_complete() {
                                        let log = counter.to_log();
                                        tokio::task::spawn_blocking(move || append_upload_log(log));
                                    }
                                }));
                            }
                            None => {
                                channel_closed = true;
                                tracing::info!("Upload channel closed, waiting for {} in-flight tasks", in_flight.len());
                            }
                        }
                    }

                    // 同时等待已有任务完成
                    Some(_) = in_flight.next(), if !in_flight.is_empty() => {
                        // 一个任务完成了，继续循环（可能有空位取新任务）
                    }
                }
            } else if !in_flight.is_empty() {
                // 没有空位或通道已关闭，等待 in_flight 任务完成
                tokio::select! {
                    biased;
                    _ = enqueue_token.cancelled() => {
                        tracing::info!("Upload consumer cancelled while waiting for in_flight");
                        break;
                    }
                    Some(_) = in_flight.next() => {
                        // 一个任务完成了，继续循环
                    }
                }
            } else {
                // 通道关闭且没有 in_flight 任务，结束
                break;
            }
        }

        tracing::info!("Upload consumer finished");
        // 通知前端所有上传任务已处理完毕，触发全量刷新
        ws_server::broadcast_message(json!({ "type": "all_uploads_complete" }));

        // 检查是否有孤立的 Pending 任务（init 阶段入库但未发送到上传通道的）
        // 这种情况发生在：init 阶段运行时 GLOBAL_PAUSED 被设置，导致部分任务未发送到上传通道
        // 如果全局暂停/取消标志当前未设置，自动恢复这些任务
        if !GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst)
            && !GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst)
        {
            let user_id = current_user_id();
            let orphaned: Vec<String> = {
                let store = TASK_STORE.read().await;
                store.values()
                    .filter(|t| {
                        t.status == TaskStatus::Pending
                            && t.file_path.is_some()
                            && (t.user_id.as_deref() == Some(&user_id) || t.user_id.is_none())
                    })
                    .map(|t| t.upload_id.clone())
                    .collect()
            };
            if !orphaned.is_empty() {
                tracing::info!("Found {} orphaned pending tasks, auto-resuming", orphaned.len());
                // resume_task 内部通过 FILE_UPLOAD_SEMAPHORE 控制实际上传并发
                for uid in orphaned {
                    tokio::spawn(async move {
                        resume_task(&uid).await;
                    });
                }
            }
        }
    });

    Ok(())
}

/// 单个文件的 init 阶段：计算哈希 + 调用 /upload/init
/// 返回 Ok(Some(...)) 成功，Ok(None) 不应出现，Err(filename) 表示 init 失败（携带文件名用于日志）
///
/// The actual work is delegated to `do_init_task` which is `Box::pin`-ned so
/// its (potentially large) state machine lives on the heap rather than being
/// embedded in the `FuturesUnordered` poll chain on the caller's stack.
/// Without this, 8 concurrent init futures can overflow the 4 MB Windows
/// thread stack in debug builds.
#[allow(clippy::too_many_arguments)]
fn spawn_init_task(
    file_path: PathBuf,
    relative_path: Option<String>,
    client: Arc<reqwest::Client>,
    cfg: crate::config::AppConfig,
    project_id: String,
    folder_id: Option<String>,
    asset_type_id: Option<String>,
    api_token: String,
    folder_map: Arc<std::collections::HashMap<String, String>>,
) -> tokio::task::JoinHandle<Result<(String, PathBuf, super::UploadTaskDetail, TransferTask, bool), String>> {
    tokio::spawn(async move {
        // Box::pin forces the inner future's state machine onto the heap.
        // This prevents the large local variables (cfg, folder_map, wrapper, etc.)
        // from inflating the outer future that FuturesUnordered polls on the stack.
        Box::pin(do_init_task(
            file_path,
            relative_path,
            client,
            cfg,
            project_id,
            folder_id,
            asset_type_id,
            api_token,
            folder_map,
        )).await
    })
}

/// Inner implementation of the init task — kept as a separate `async fn` so
/// `Box::pin` in `spawn_init_task` can heap-allocate its state machine.
#[allow(clippy::too_many_arguments)]
async fn do_init_task(
    file_path: PathBuf,
    relative_path: Option<String>,
    client: Arc<reqwest::Client>,
    cfg: crate::config::AppConfig,
    project_id: String,
    folder_id: Option<String>,
    asset_type_id: Option<String>,
    api_token: String,
    folder_map: Arc<std::collections::HashMap<String, String>>,
) -> Result<(String, PathBuf, super::UploadTaskDetail, TransferTask, bool), String> {
    let filename = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // ── 阶段 1：同步 IO（metadata + hash）放进 spawn_blocking ──────────
    // 用 BLOCKING_IO_SEMAPHORE 限制同时在 blocking 线程里做文件 IO 的数量。
    // 注意：permit 在 spawn_blocking 内部获取和释放，不在 async 层持有，
    // 避免大文件哈希期间长时间占用 permit 导致其他 init task 全部卡死。
    let file_path_clone = file_path.clone();
    let io_result = tokio::task::spawn_blocking(move || -> Result<(u64, Option<String>), String> {
        // ── metadata（极快，不需要信号量）────────────────────────────
        let file_size = std::fs::metadata(&file_path_clone)
            .map(|m| m.len())
            .map_err(|e| format!("Cannot stat {:?}: {}", file_path_clone, e))?;

        // ── hash（用信号量限制并发，在 blocking 线程内同步等待）────────
        // 用 try_acquire 非阻塞尝试，获取不到就跳过哈希（不影响上传，只是无法秒传）
        let md5 = {
            let _permit = BLOCKING_IO_SEMAPHORE.try_acquire();
            if _permit.is_ok() {
                use sha2::{Digest, Sha256};
                use std::io::Read;
                (|| -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
                    let mut file = std::fs::File::open(&file_path_clone)?;
                    let mut hasher = Sha256::new();
                    // 1 MB read buffer — kept inside spawn_blocking so it never
                    // lives in an async state machine.
                    let mut buf = vec![0u8; 1024 * 1024];
                    loop {
                        let n = file.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buf[..n]);
                    }
                    let result = hasher.finalize();
                    Ok(result[..16].iter().map(|b| format!("{:02x}", b)).collect())
                })()
                .ok()
                // _permit 在此处 drop，释放信号量
            } else {
                // 信号量繁忙，跳过哈希，后续走普通上传（不影响功能，只是无法秒传）
                tracing::debug!("Hash skipped for {:?}: IO semaphore busy", file_path_clone);
                None
            }
        };

        Ok((file_size, md5))
    })
    .await;

    let (file_size, md5_checksum) = match io_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::error!("{}", e);
            return Err(filename);
        }
        Err(e) => {
            tracing::error!("spawn_blocking panicked for {:?}: {}", file_path, e);
            return Err(filename);
        }
    };

    // ── 阶段 2：确定目标 folder_id ────────────────────────────────────
    let target_folder_id = if let Some(ref rel_path) = relative_path {
        let parent_dir = std::path::Path::new(rel_path).parent();
        if let Some(parent) = parent_dir {
            let parent_str = parent.to_string_lossy().to_string();
            folder_map.get(&parent_str).cloned().or(folder_id.clone())
        } else {
            folder_id.clone()
        }
    } else {
        folder_id.clone()
    };

    ws_server::broadcast_message(json!({
        "type": "upload_hashing",
        "filename": filename,
        "file_size": file_size,
    }));

    // 调用 /upload/init
    let init_url = format!("{}/api/v1/upload/init", cfg.api_url);
    let body = serde_json::json!({
        "filename": filename,
        "file_size": file_size,
        "project_id": project_id,
        "folder_id": target_folder_id,
        "asset_type_id": asset_type_id,
        "md5_checksum": md5_checksum,
    });

    // Drop cfg before the await — it's no longer needed and removing it from
    // the live set shrinks the state machine at this suspension point.
    drop(cfg);

    let resp = match client.post(&init_url).bearer_auth(&api_token).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to init upload for {}: {}", filename, e);
            return Err(filename);
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::error!("Init upload API error {} for {}: {}", status, filename, text);
        return Err(filename);
    }

    // Parse the response body into a plain struct to avoid holding a large
    // serde_json::Value (recursive enum) across subsequent await points.
    #[derive(serde::Deserialize)]
    struct InitResponseData {
        upload_id: Option<String>,
        asset_id: Option<String>,
        chunk_size: Option<u64>,
        total_chunks: Option<u64>,
        is_instant_upload: Option<bool>,
        is_duplicate: Option<bool>,
    }
    #[derive(serde::Deserialize)]
    struct InitResponse {
        data: Option<InitResponseData>,
    }

    let parsed: InitResponse = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to parse init response for {}: {}", filename, e);
            return Err(filename);
        }
    };
    let data = match parsed.data {
        Some(d) => d,
        None => {
            tracing::error!("Empty data in init response for {}", filename);
            return Err(filename);
        }
    };

    // 秒传
    let is_instant = data.is_instant_upload.unwrap_or(false) || data.is_duplicate.unwrap_or(false);

    if is_instant {
        let instant_id = format!("instant-{}-{}", now_ts(), filename);
        let asset_id = data.asset_id.unwrap_or_default();
        let task = TransferTask {
            upload_id: instant_id.clone(),
            filename: filename.clone(),
            file_size,
            total_chunks: 1,
            uploaded_chunks: 1,
            status: TaskStatus::Completed,
            error: None,
            created_at: now_ts(),
            file_path: Some(file_path.to_string_lossy().to_string()),
            chunk_size: None,
            user_id: Some(current_user_id()),
            project_id: Some(project_id),
            folder_id: target_folder_id,
            retry_count: 0,
        };
        let detail = super::UploadTaskDetail {
            upload_id: instant_id.clone(),
            asset_id,
            original_filename: filename,
            file_size,
            chunk_size: 0,
            total_chunks: 1,
            uploaded_chunks: vec![],
            oss_path: String::new(),
            oss_upload_id: None,
            project_id: task.project_id.clone(),
            folder_id: task.folder_id.clone(),
        };
        return Ok((instant_id, file_path, detail, task, true));
    }

    // 普通分片上传
    let upload_id = match data.upload_id {
        Some(id) => id,
        None => {
            tracing::error!("No upload_id in init response for {}", filename);
            return Err(filename);
        }
    };
    let chunk_size = data.chunk_size.unwrap_or(20 * 1024 * 1024);
    let total_chunks = data.total_chunks.unwrap_or(1) as u32;
    let asset_id = data.asset_id.unwrap_or_default();

    // 检查全局取消/暂停，决定初始状态
    let (initial_status, is_paused_flag) = if GLOBAL_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
        (TaskStatus::Cancelled, false)
    } else if GLOBAL_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
        (TaskStatus::Paused, true)
    } else {
        (TaskStatus::Pending, false)
    };

    if is_paused_flag {
        PAUSED_SET.write().await.insert(upload_id.clone());
    }

    let detail = super::UploadTaskDetail {
        upload_id: upload_id.clone(),
        asset_id,
        original_filename: filename.clone(),
        file_size,
        chunk_size,
        total_chunks,
        uploaded_chunks: vec![],
        oss_path: String::new(),
        oss_upload_id: None,
        project_id: Some(project_id.clone()),
        folder_id: target_folder_id.clone(),
    };
    let task = TransferTask {
        upload_id: upload_id.clone(),
        filename,
        file_size,
        total_chunks,
        uploaded_chunks: 0,
        status: initial_status,
        error: None,
        created_at: now_ts(),
        file_path: Some(file_path.to_string_lossy().to_string()),
        chunk_size: Some(chunk_size),
        user_id: Some(current_user_id()),
        project_id: Some(project_id),
        folder_id: target_folder_id,
        retry_count: 0,
    };

    Ok((upload_id, file_path, detail, task, false))
}

/// Recursively scan folder and return list of (file_path, relative_path)
/// 使用显式栈迭代而非递归，避免同时持有多层 ReadDir 句柄耗尽内核对象。
/// 调用方应在 spawn_blocking 里执行此函数。
fn scan_folder_recursive(folder_path: &std::path::Path) -> Result<Vec<(std::path::PathBuf, Option<String>)>> {
    use std::fs;

    let folder_name = folder_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder")
        .to_string();

    let mut files = Vec::new();
    // 用显式栈代替递归，每次只持有一个 ReadDir，处理完立即释放
    let mut dir_stack: Vec<std::path::PathBuf> = vec![folder_path.to_path_buf()];

    while let Some(dir) = dir_stack.pop() {
        // read_dir 返回后立即收集成 Vec，让 ReadDir 句柄尽快释放
        let entries: Vec<_> = match fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(e) => {
                tracing::warn!("Cannot read dir {:?}: {}", dir, e);
                continue;
            }
        };
        // ReadDir 句柄在此处已释放

        for entry in entries {
            let path = entry.path();
            let name_str = entry.file_name();
            let name = name_str.to_string_lossy();
            if name.starts_with('.') {
                continue; // 跳过隐藏文件
            }
            if path.is_dir() {
                dir_stack.push(path);
            } else if path.is_file() {
                let rel_path = path.strip_prefix(folder_path)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|s| format!("{}/{}", folder_name, s.replace('\\', "/")));
                files.push((path, rel_path));
            }
        }
    }

    Ok(files)
}

/// Create folder structure on backend, returns map of relative_dir_path -> folder_id
async fn create_folder_structure(
    client: &reqwest::Client,
    api_url: &str,
    token: &str,
    project_id: &str,
    base_folder_id: Option<&str>,
    files: &[(std::path::PathBuf, Option<String>)],
) -> Result<std::collections::HashMap<String, String>> {
    use std::collections::{HashMap, HashSet};

    let mut folder_map: HashMap<String, String> = HashMap::new();
    let mut created_dirs: HashSet<String> = HashSet::new();

    // Extract unique directory paths
    let mut dirs: Vec<String> = files
        .iter()
        .filter_map(|(_, rel_path)| {
            rel_path.as_ref().and_then(|p| {
                std::path::Path::new(p).parent().map(|parent| parent.to_string_lossy().to_string())
            })
        })
        .collect();
    dirs.sort();
    dirs.dedup();

    // Create folders layer by layer
    for dir_path in dirs {
        if created_dirs.contains(&dir_path) {
            continue;
        }

        let parts: Vec<&str> = dir_path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current_path = String::new();
        let mut parent_id = base_folder_id.map(|s| s.to_string());

        for part in parts {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(part);

            if created_dirs.contains(&current_path) {
                parent_id = folder_map.get(&current_path).cloned();
                continue;
            }

            // Create folder via API
            let create_url = format!("{}/api/v1/projects/{}/folders", api_url, project_id);
            let body = serde_json::json!({
                "name": part,
                "parent_id": parent_id,
            });

            let resp = client
                .post(&create_url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                tracing::error!("Failed to create folder '{}': {} {}", part, status, text);
                continue;
            }

            let wrapper: serde_json::Value = resp.json().await?;
            if let Some(folder_id) = wrapper["data"]["id"].as_str() {
                folder_map.insert(current_path.clone(), folder_id.to_string());
                created_dirs.insert(current_path.clone());
                parent_id = Some(folder_id.to_string());
                tracing::info!("Created folder: {} -> {}", current_path, folder_id);
            }
        }
    }

    Ok(folder_map)
}

/// 启动时恢复未完成的任务（running/pending 且有 file_path 的）
/// 使用信号量限制同时恢复的任务数，避免大量任务同时启动耗尽资源
pub async fn resume_pending_tasks(_token: Option<String>) {
    // 启动恢复时清除全局暂停/取消标志，避免上次退出时的状态阻止任务启动
    GLOBAL_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
    GLOBAL_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);

    let tasks = get_all_tasks().await;

    let mut to_resume: Vec<TransferTask> = Vec::new();
    for task in tasks {
        if task.file_path.is_none() {
            continue;
        }
        if task.status != TaskStatus::Running && task.status != TaskStatus::Pending {
            continue;
        }
        let file_path = PathBuf::from(task.file_path.as_ref().unwrap());
        if !file_path.exists() {
            tracing::warn!("Resume: file not found for {}: {:?}", task.upload_id, file_path);
            update_task_status(&task.upload_id, TaskStatus::Failed, Some("文件不存在，无法续传".to_string())).await;
            continue;
        }
        // 置为 Paused，resume_task 只处理 Paused/Failed 状态
        update_task_status(&task.upload_id, TaskStatus::Paused, None).await;
        to_resume.push(task);
    }

    tracing::info!("resume_pending_tasks: {} tasks to resume", to_resume.len());

    if to_resume.is_empty() {
        return;
    }

    // 直接复用 resume_task，它会处理 check_and_reinit + FILE_UPLOAD_SEMAPHORE + start_upload
    // 不需要额外的信号量，FILE_UPLOAD_SEMAPHORE 已经限制了并发数
    for task in to_resume {
        let upload_id = task.upload_id.clone();
        tokio::spawn(async move {
            resume_task(&upload_id).await;
        });
    }
}

/// 暂停所有正在运行或等待中的任务
pub async fn pause_all_tasks() {
    // 设置全局暂停标志，阻止 enqueue_upload_by_params 中后续文件自动启动
    GLOBAL_PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);

    // 一次性收集所有需要暂停的任务，批量更新状态，只写一次磁盘
    let tokens_to_cancel: Vec<CancellationToken> = {
        let mut store = TASK_STORE.write().await;
        let tokens = CANCEL_TOKENS.read().await;
        let mut tokens_vec = Vec::new();

        for task in store.values_mut() {
            if task.status == TaskStatus::Running || task.status == TaskStatus::Pending {
                task.status = TaskStatus::Paused;
                if let Some(token) = tokens.get(&task.upload_id) {
                    tokens_vec.push(token.clone());
                }
            }
        }
        save_tasks_async(&store);
        tokens_vec
    };

    // 同时 cancel 所有正在运行的上传
    for token in &tokens_to_cancel {
        token.cancel();
    }

    // 把所有 Running/Pending 的 id 加入 PAUSED_SET
    {
        let store = TASK_STORE.read().await;
        let mut paused = PAUSED_SET.write().await;
        for task in store.values() {
            if task.status == TaskStatus::Paused {
                paused.insert(task.upload_id.clone());
            }
        }
    }

    ws_server::broadcast_message(json!({ "type": "all_paused" }));
    tracing::info!("pause_all_tasks: paused {} tasks", tokens_to_cancel.len());
}

/// 恢复所有已暂停的任务
pub async fn resume_all_tasks() {
    // 清除全局暂停标志，必须在启动上传之前清除
    GLOBAL_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);

    // 收集所有暂停的和孤立的等待中的任务
    let upload_ids: Vec<String> = {
        let store = TASK_STORE.read().await;
        store.values()
            .filter(|t| t.status == TaskStatus::Paused || t.status == TaskStatus::Pending)
            .map(|t| t.upload_id.clone())
            .collect()
    };

    // 清理 PAUSED_SET
    {
        let mut paused = PAUSED_SET.write().await;
        for id in &upload_ids {
            paused.remove(id);
        }
    }

    // 先把所有任务状态统一设为 Paused，这样 resume_task 能正确处理
    {
        let mut store = TASK_STORE.write().await;
        for id in &upload_ids {
            if let Some(task) = store.get_mut(id) {
                if task.status == TaskStatus::Pending {
                    task.status = TaskStatus::Paused;
                }
            }
        }
        save_tasks_async(&store);
    }

    // 广播 all_resumed，让前端 WS 收到后触发 loadTasks 刷新
    ws_server::broadcast_message(json!({ "type": "all_resumed" }));

    // 逐个调 resume_task（它会处理状态变更、check_and_reinit、start_upload）
    for upload_id in upload_ids {
        tokio::spawn(async move {
            resume_task(&upload_id).await;
        });
    }
}

/// 取消所有活跃任务
pub async fn cancel_all_tasks() {
    // 1. 设置全局取消标志，阻止后续文件自动启动
    GLOBAL_CANCELLED.store(true, std::sync::atomic::Ordering::SeqCst);

    // 2. 取消当前批量入队会话（init 滑动窗口 + upload 通道消费者立即退出）
    ENQUEUE_CANCEL_TOKEN.read().await.cancel();

    // 3. 一次性收集所有需要取消的 upload_id + 对应的 CancellationToken
    //    同时批量更新状态，只写一次磁盘
    let (upload_ids, tokens_to_cancel): (Vec<String>, Vec<CancellationToken>) = {
        let mut store = TASK_STORE.write().await;
        let tokens = CANCEL_TOKENS.read().await;

        let mut ids = Vec::new();
        let mut tokens_vec = Vec::new();

        for task in store.values_mut() {
            if matches!(task.status, TaskStatus::Running | TaskStatus::Pending | TaskStatus::Paused) {
                task.status = TaskStatus::Cancelled;
                task.error = None;
                if let Some(token) = tokens.get(&task.upload_id) {
                    tokens_vec.push(token.clone());
                }
                ids.push(task.upload_id.clone());
            }
        }
        save_tasks_async(&store); // 只写一次磁盘
        (ids, tokens_vec)
    };

    if upload_ids.is_empty() {
        return;
    }

    // 4. 同时 cancel 所有正在运行的上传（中断分片上传的 HTTP 请求）
    for token in tokens_to_cancel {
        token.cancel();
    }

    // 5. 清理 CANCEL_TOKENS 和 PAUSED_SET
    {
        let mut tokens = CANCEL_TOKENS.write().await;
        let mut paused = PAUSED_SET.write().await;
        for id in &upload_ids {
            tokens.remove(id);
            paused.remove(id);
        }
    }

    // 6. 并发通知后端 abort（不等待结果，fire-and-forget）
    let api_token = crate::auth::load_token();
    let cfg = crate::config::get_config();
    for upload_id in &upload_ids {
        let upload_id = upload_id.clone();
        let token = api_token.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Some(tok) = token {
                let url = format!("{}/api/v1/upload/{}/abort", cfg.api_url, upload_id);
                let _ = SHARED_HTTP_CLIENT.delete(&url).bearer_auth(&tok).send().await;
            }
        });
    }

    // 7. 一次性广播所有取消事件
    for upload_id in &upload_ids {
        ws_server::broadcast_message(json!({
            "type": "task_cancelled",
            "upload_id": upload_id,
        }));
    }

    tracing::info!("cancel_all_tasks: cancelled {} tasks", upload_ids.len());
}
