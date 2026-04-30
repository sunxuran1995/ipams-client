use super::{TaskStatus, TransferTask, UploadTaskDetail};
use crate::transfer::upload::Uploader;
use crate::ws_server;
use anyhow::Result;
use once_cell::sync::Lazy;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Runtime};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

static TASK_STORE: Lazy<Arc<RwLock<HashMap<String, TransferTask>>>> =
    Lazy::new(|| Arc::new(RwLock::new(load_tasks_from_disk())));

/// 每个 upload_id 对应一个 CancellationToken，暂停时 cancel 它
static CANCEL_TOKENS: Lazy<Arc<RwLock<HashMap<String, CancellationToken>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

/// 被暂停的 upload_id 集合（用于循环前的快速检查）
static PAUSED_SET: Lazy<Arc<RwLock<HashSet<String>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashSet::new())));

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
    save_tasks_to_disk(&store);
}

async fn update_task_status(upload_id: &str, status: TaskStatus, error: Option<String>) {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        task.status = status;
        task.error = error;
    }
    save_tasks_to_disk(&store);
}

async fn update_task_progress(upload_id: &str, uploaded_chunks: u32) {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        task.uploaded_chunks = uploaded_chunks;
    }
    save_tasks_to_disk(&store);
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
    let uploader = Uploader::new(upload_id, token.clone())?;
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
    update_task_status(&upload_id, TaskStatus::Running, None).await;

    // 注册 cancellation token
    let cancel_token = register_cancel_token(&upload_id).await;

    ws_server::broadcast_message(json!({
        "type": "upload_start",
        "upload_id": upload_id,
        "filename": detail.original_filename,
        "total_chunks": detail.total_chunks,
    }));

    let uploader = match Uploader::new(&upload_id, token) {
        Ok(u) => u,
        Err(e) => {
            remove_cancel_token(&upload_id).await;
            update_task_status(&upload_id, TaskStatus::Failed, Some(e.to_string())).await;
            return Err(e);
        }
    };
    let store_ref = TASK_STORE.clone();
    let upload_id_cb = upload_id.clone();

    let result = uploader
        .run_upload(file_path, &detail, cancel_token.clone(), move |uploaded, _total| {
            let store = store_ref.clone();
            let uid = upload_id_cb.clone();
            tokio::spawn(async move {
                let mut s = store.write().await;
                if let Some(t) = s.get_mut(&uid) {
                    t.uploaded_chunks = uploaded;
                }
                save_tasks_to_disk(&s);
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
            save_tasks_to_disk(&store);
            ws_server::broadcast_message(json!({
                "type": "upload_complete",
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
            // 通知后端 abort，把 asset 状态改为 deleted，避免前端一直显示上传中
            let upload_id_owned = upload_id.clone();
            tokio::spawn(async move {
                if let Some(token) = crate::auth::load_token() {
                    let cfg = crate::config::get_config();
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()
                        .unwrap_or_default();
                    let url = format!("{}/api/v1/upload/{}/abort", cfg.api_url, upload_id_owned);
                    let _ = client.delete(&url).bearer_auth(&token).send().await;
                }
            });
        }
    }

    Ok(())
}

pub async fn cancel_task(upload_id: &str) -> bool {
    let mut store = TASK_STORE.write().await;
    if let Some(task) = store.get_mut(upload_id) {
        if matches!(task.status, TaskStatus::Pending | TaskStatus::Running | TaskStatus::Paused) {
            task.status = TaskStatus::Cancelled;
            save_tasks_to_disk(&store);
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
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()
                        .unwrap_or_default();
                    let url = format!("{}/api/v1/upload/{}/abort", cfg.api_url, upload_id_owned);
                    let _ = client.delete(&url).bearer_auth(&token).send().await;
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
            save_tasks_to_disk(&store);
            drop(store);
            // cancel 正在执行的上传
            PAUSED_SET.write().await.insert(upload_id.to_string());
            if let Some(token) = CANCEL_TOKENS.read().await.get(upload_id) {
                token.cancel();
            }
            ws_server::broadcast_message(json!({
                "type": "task_status",
                "upload_id": upload_id,
                "status": "paused",
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
            Some(task) if task.status == TaskStatus::Paused => {
                task.status = TaskStatus::Pending;
                let cloned = task.clone();
                save_tasks_to_disk(&store);
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
            // 在 spawn 之前读好 token，避免异步上下文里 keyring 读取失败
            let token = crate::auth::load_token();
            tokio::spawn(async move {
                let cfg = crate::config::get_config();
                // 优先用保存的 chunk_size，保证续传时分片边界一致
                let chunk_size = task_clone.chunk_size.unwrap_or(cfg.chunk_size as u64);
                let detail = crate::transfer::UploadTaskDetail {
                    upload_id: upload_id_owned.clone(),
                    asset_id: String::new(),
                    original_filename: task_clone.filename.clone(),
                    file_size: task_clone.file_size,
                    chunk_size,
                    total_chunks: task_clone.total_chunks,
                    uploaded_chunks: vec![],
                    oss_path: String::new(),
                    oss_upload_id: None,
                };
                if let Err(e) = start_upload(upload_id_owned.clone(), file_path, detail, token).await {
                    tracing::error!("Resume upload failed: {}", e);
                }
            });
            ws_server::broadcast_message(json!({
                "type": "task_status",
                "upload_id": upload_id,
                "status": "pending",
            }));
            return true;
        }
    }
    false
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
        let folder_path = &paths[0];
        if !folder_path.is_dir() {
            tracing::error!("Selected path is not a folder: {:?}", folder_path);
            return Ok(());
        }
        scan_folder_recursive(folder_path)?
    } else {
        // File mode: use paths directly
        paths.into_iter().map(|p| (p, None)).collect()
    };

    tracing::info!("Total files to upload: {}", files_to_upload.len());

    // Get API token
    let api_token = match token.clone().or_else(|| crate::auth::load_token()) {
        Some(t) => t,
        None => {
            tracing::error!("Not authenticated, cannot init upload");
            return Ok(());
        }
    };

    let cfg = crate::config::get_config();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // If folder mode, create folder structure first
    let folder_map = if count == 0 {
        create_folder_structure(&client, &cfg.api_url, &api_token, &project_id, folder_id.as_deref(), &files_to_upload).await?
    } else {
        std::collections::HashMap::new()
    };

    // Upload each file
    for (file_path, relative_path) in files_to_upload {
        let filename = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let file_size = match tokio::fs::metadata(&file_path).await {
            Ok(m) => m.len(),
            Err(e) => {
                tracing::error!("Cannot stat {:?}: {}", file_path, e);
                continue;
            }
        };

        // Determine target folder_id based on relative_path
        let target_folder_id = if let Some(rel_path) = &relative_path {
            // Get parent folder from relative path
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

        // Call /upload/init
        let init_url = format!("{}/api/v1/upload/init", cfg.api_url);
        let body = serde_json::json!({
            "filename": filename,
            "file_size": file_size,
            "project_id": project_id,
            "folder_id": target_folder_id,
            "asset_type_id": asset_type_id,
        });

        let resp = client
            .post(&init_url)
            .bearer_auth(&api_token)
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to init upload for {}: {}", filename, e);
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            tracing::error!("Init upload API error {}: {}", status, text);
            continue;
        }

        let wrapper: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("Failed to parse init response: {}", e);
                continue;
            }
        };

        let upload_id = match wrapper["data"]["upload_id"].as_str() {
            Some(id) => id.to_string(),
            None => {
                tracing::error!("No upload_id in init response: {}", wrapper);
                continue;
            }
        };
        let chunk_size = wrapper["data"]["chunk_size"].as_u64().unwrap_or(10 * 1024 * 1024) as u32;
        let total_chunks = wrapper["data"]["total_chunks"].as_u64().unwrap_or(1) as u32;

        tracing::info!("Initialized upload {} ({} chunks)", upload_id, total_chunks);

        let detail = super::UploadTaskDetail {
            upload_id: upload_id.clone(),
            asset_id: wrapper["data"]["asset_id"].as_str().unwrap_or("").to_string(),
            original_filename: filename.clone(),
            file_size,
            chunk_size: chunk_size as u64,
            total_chunks,
            uploaded_chunks: vec![],
            oss_path: String::new(),
            oss_upload_id: None,
        };

        let task = TransferTask {
            upload_id: upload_id.clone(),
            filename: filename.clone(),
            file_size,
            total_chunks,
            uploaded_chunks: 0,
            status: TaskStatus::Pending,
            error: None,
            created_at: now_ts(),
            file_path: Some(file_path.to_string_lossy().to_string()),
            chunk_size: Some(chunk_size as u64),
            user_id: Some(current_user_id()),
        };
        upsert_task(task).await;

        // Notify frontend
        let _ = app.emit("upload:started", serde_json::json!({
            "upload_id": upload_id,
            "filename": filename,
            "file_size": file_size,
        }));

        // Broadcast to local WS (for ClientTransferDock)
        ws_server::broadcast_message(json!({
            "type": "upload_queued",
            "upload_id": upload_id,
            "filename": filename,
            "total_chunks": total_chunks,
        }));

        let upload_id_owned = upload_id.clone();
        let token_clone = Some(api_token.clone());
        tokio::spawn(async move {
            if let Err(e) = start_upload(upload_id_owned, file_path, detail, token_clone).await {
                tracing::error!("Upload failed: {}", e);
            }
        });
    }

    Ok(())
}

/// Recursively scan folder and return list of (file_path, relative_path)
fn scan_folder_recursive(folder_path: &std::path::Path) -> Result<Vec<(std::path::PathBuf, Option<String>)>> {
    use std::fs;

    let mut files = Vec::new();
    let folder_name = folder_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("folder");

    fn visit_dir(
        dir: &std::path::Path,
        base: &std::path::Path,
        folder_name: &str,
        files: &mut Vec<(std::path::PathBuf, Option<String>)>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dir(&path, base, folder_name, files)?;
            } else if path.is_file() {
                // Calculate relative path from base folder
                let rel_path = path.strip_prefix(base)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|s| format!("{}/{}", folder_name, s));
                files.push((path, rel_path));
            }
        }
        Ok(())
    }

    visit_dir(folder_path, folder_path, folder_name, &mut files)?;
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
pub async fn resume_pending_tasks(_token: Option<String>) {
    let tasks = get_all_tasks().await;
    let cfg = crate::config::get_config();
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

        tracing::info!("Resuming upload: {}", task.upload_id);
        update_task_status(&task.upload_id, TaskStatus::Pending, None).await;

        let upload_id = task.upload_id.clone();
        let chunk_size = task.chunk_size.unwrap_or(cfg.chunk_size as u64);
        let token = crate::auth::load_token();
        tokio::spawn(async move {
            let detail = crate::transfer::UploadTaskDetail {
                upload_id: upload_id.clone(),
                asset_id: String::new(),
                original_filename: task.filename.clone(),
                file_size: task.file_size,
                chunk_size,
                total_chunks: task.total_chunks,
                uploaded_chunks: vec![],
                oss_path: String::new(),
                oss_upload_id: None,
            };
            if let Err(e) = start_upload(upload_id.clone(), file_path, detail, token).await {
                tracing::error!("Resume: upload failed for {}: {}", upload_id, e);
            }
        });
    }
}
