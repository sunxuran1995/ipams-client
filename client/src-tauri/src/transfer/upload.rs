use super::{ApiResponse, UploadProgress, UploadTaskDetail};
use crate::{auth, config, ws_server};
use anyhow::{anyhow, Context, Result};
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::Client;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct Uploader {
    client: Client,
    upload_id: String,
    api_url: String,
    token: String,
}

impl Uploader {
    pub fn new(upload_id: &str, token: Option<String>) -> Result<Self> {
        let token = token
            .or_else(|| auth::load_token())
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        let cfg = config::get_config();
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            upload_id: upload_id.to_string(),
            api_url: cfg.api_url,
            token,
        })
    }

    /// 使用外部传入的共享 client（推荐），避免每个 Uploader 创建独立连接池
    pub fn with_client(upload_id: &str, token: Option<String>, client: Client) -> Result<Self> {
        let token = token
            .or_else(|| auth::load_token())
            .ok_or_else(|| anyhow!("Not authenticated"))?;
        let cfg = config::get_config();
        Ok(Self {
            client,
            upload_id: upload_id.to_string(),
            api_url: cfg.api_url,
            token,
        })
    }

    pub async fn fetch_task_detail(&self) -> Result<UploadTaskDetail> {
        let url = format!(
            "{}/api/v1/client/tasks/upload/{}",
            self.api_url, self.upload_id
        );
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to fetch task detail")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("API error {}: {}", status, body));
        }

        let wrapper = response
            .json::<ApiResponse<UploadTaskDetail>>()
            .await
            .context("Failed to parse task detail response")?;

        if wrapper.code != 0 {
            return Err(anyhow!(
                "API returned error {}: {}",
                wrapper.code,
                wrapper.msg.unwrap_or_default()
            ));
        }

        wrapper.data.ok_or_else(|| anyhow!("Empty data in task detail response"))
    }

    pub async fn fetch_upload_progress(&self) -> Result<UploadProgress> {
        let url = format!(
            "{}/api/v1/upload/{}/progress",
            self.api_url, self.upload_id
        );
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Failed to fetch upload progress")?;

        if !response.status().is_success() {
            // If 404, assume no progress yet
            if response.status() == 404 {
                return Ok(UploadProgress::default());
            }
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("API error {}: {}", status, body));
        }

        let wrapper = response
            .json::<ApiResponse<UploadProgress>>()
            .await
            .context("Failed to parse upload progress response")?;

        Ok(wrapper.data.unwrap_or_default())
    }

    pub async fn upload_chunk(&self, chunk_index: u32, data: Vec<u8>) -> Result<()> {
        let url = format!(
            "{}/api/v1/upload/{}/chunk/{}",
            self.api_url, self.upload_id, chunk_index
        );

        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::bytes(data)
                .file_name("chunk")
                .mime_str("application/octet-stream")
                .unwrap(),
        );

        let response = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("Failed to upload chunk {}", chunk_index))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Chunk {} upload failed {}: {}", chunk_index, status, body));
        }

        Ok(())
    }

    pub async fn report_progress(&self, chunk_index: u32) -> Result<()> {
        let url = format!(
            "{}/api/v1/client/tasks/upload/{}/progress",
            self.api_url, self.upload_id
        );

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({
                "chunk_index": chunk_index,
                "status": "uploaded"
            }))
            .send()
            .await
            .context("Failed to report progress")?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!("Progress report failed with status {}", status);
        }

        Ok(())
    }

    pub async fn complete_upload(&self) -> Result<()> {
        let url = format!("{}/api/v1/upload/{}/complete", self.api_url, self.upload_id);

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({}))
            .send()
            .await
            .context("Failed to complete upload")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Complete upload failed {}: {}", status, body));
        }

        tracing::info!("Upload {} completed successfully", self.upload_id);
        Ok(())
    }

    /// Main upload routine: reads file, skips already-uploaded chunks, uploads concurrently
    /// 只负责分片上传，不包含验证和 complete 阶段。
    /// 分片全部上传完成后立即返回 Ok(())，调用方可以释放并发 permit，
    /// 然后再调用 finish_upload 完成验证和 complete。
    pub async fn run_upload_chunks(
        &self,
        file_path: PathBuf,
        task: &UploadTaskDetail,
        cancel_token: CancellationToken,
        on_progress: impl Fn(u32, u32) + Send + Sync + 'static,
    ) -> Result<()> {
        let cfg = config::get_config();

        // Fetch existing progress for resume
        let existing_progress = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.fetch_upload_progress()
        ).await
            .unwrap_or(Ok(UploadProgress::default()))
            .unwrap_or(UploadProgress {
                uploaded_chunks: vec![],
                total_chunks: task.total_chunks,
            });

        tracing::info!(
            "Upload {}: already uploaded chunks: {:?}",
            self.upload_id,
            existing_progress.uploaded_chunks.len()
        );

        tracing::info!(
            "Upload {}: uploaded_set = {:?}",
            self.upload_id,
            existing_progress.uploaded_chunks
        );

        let uploaded_set: std::collections::HashSet<u32> =
            existing_progress.uploaded_chunks.into_iter().collect();

        // 后端分块号 1-indexed，与 uploaded_chunks 一致
        let chunks_to_upload: Vec<u32> = (1..=task.total_chunks)
            .filter(|c| !uploaded_set.contains(c))
            .collect();

        tracing::info!(
            "Upload {}: {}/{} chunks to upload",
            self.upload_id,
            chunks_to_upload.len(),
            task.total_chunks
        );

        if chunks_to_upload.is_empty() {
            tracing::info!("All chunks already uploaded for {}, skip to finish phase", self.upload_id);
            return Ok(());
        }

        let max_concurrent = cfg.max_concurrent_chunks;
        let on_progress = Arc::new(on_progress);
        let already_uploaded = task.total_chunks - chunks_to_upload.len() as u32;
        let uploaded_count = Arc::new(std::sync::atomic::AtomicU32::new(already_uploaded));
        let upload_id_ref = self.upload_id.clone();
        // 复用同一个 HTTP client，避免每个 chunk 都重新建立 TLS 连接
        let shared_client = Arc::new(self.client.clone());

        // 使用流式并发：同时最多 max_concurrent 个 chunk 在飞行中
        // 避免一次性 spawn 所有 task 导致 Windows 套接字缓冲区耗尽（WinError 10055）
        let mut in_flight: FuturesUnordered<tokio::task::JoinHandle<Result<(), anyhow::Error>>> =
            FuturesUnordered::new();
        let mut chunk_iter = chunks_to_upload.into_iter();
        let mut errors = Vec::new();
        let mut paused = false;

        // 填满初始并发槽
        'fill: for chunk_index in chunk_iter.by_ref() {
            if cancel_token.is_cancelled() {
                tracing::info!("Upload {} cancelled/paused before chunk {}", upload_id_ref, chunk_index);
                paused = true;
                break 'fill;
            }

            in_flight.push(spawn_chunk_upload(
                chunk_index,
                file_path.clone(),
                shared_client.clone(),
                self.upload_id.clone(),
                self.api_url.clone(),
                self.token.clone(),
                task.chunk_size as usize,
                task.total_chunks,
                on_progress.clone(),
                uploaded_count.clone(),
                cancel_token.clone(),
            ));

            if in_flight.len() >= max_concurrent {
                break;
            }
        }

        // 滑动窗口：每完成一个就补充一个新的
        loop {
            if in_flight.is_empty() {
                break;
            }

            match in_flight.next().await {
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(e))) if e.to_string() == "paused" => {
                    paused = true;
                }
                Some(Ok(Err(e))) => {
                    errors.push(e.to_string());
                }
                Some(Err(e)) => {
                    errors.push(format!("Task panicked: {}", e));
                }
                None => break,
            }

            // 如果已经出错或暂停，不再补充新 chunk
            if !errors.is_empty() || paused || cancel_token.is_cancelled() {
                continue;
            }

            // 补充下一个 chunk
            if let Some(chunk_index) = chunk_iter.next() {
                in_flight.push(spawn_chunk_upload(
                    chunk_index,
                    file_path.clone(),
                    shared_client.clone(),
                    self.upload_id.clone(),
                    self.api_url.clone(),
                    self.token.clone(),
                    task.chunk_size as usize,
                    task.total_chunks,
                    on_progress.clone(),
                    uploaded_count.clone(),
                    cancel_token.clone(),
                ));
            }
        }

        if paused && errors.is_empty() {
            // 任务被暂停，返回特殊错误让调用方知道
            return Err(anyhow!("paused"));
        }

        if !errors.is_empty() {
            return Err(anyhow!("Upload errors: {}", errors.join("; ")));
        }

        Ok(())
    }

    /// 分片上传完成后的收尾阶段：验证所有分片已记录 + 调用 /complete。
    /// 此方法应在释放并发 permit 之后调用，避免阻塞其他文件的上传。
    pub async fn finish_upload(&self, total_chunks: u32) -> Result<()> {
        // Verify all chunks are recorded before completing
        // This guards against silent failures where a chunk returned 200
        // but the etag was not persisted (e.g. DB update matched 0 rows).
        let progress = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.fetch_upload_progress(),
        )
        .await
        .unwrap_or(Ok(UploadProgress::default()))
        .unwrap_or_default();
        let recorded: std::collections::HashSet<u32> = progress.uploaded_chunks.into_iter().collect();
        let missing: Vec<u32> = (1..=total_chunks)
            .filter(|c| !recorded.contains(c))
            .collect();
        if !missing.is_empty() {
            return Err(anyhow!(
                "Chunks not recorded on server after upload: {:?}. Please retry.",
                missing
            ));
        }

        // Complete upload — also guarded by a timeout
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            self.complete_upload(),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow!("complete_upload timed out after 60s")))?;

        // Broadcast completion
        ws_server::broadcast_message(json!({
            "type": "completed",
            "upload_id": self.upload_id,
            "status": "completed"
        }));

        Ok(())
    }

    /// 兼容旧接口：分片上传 + 验证 + complete 一步完成（用于 resume_task 等场景）
    pub async fn run_upload(
        &self,
        file_path: PathBuf,
        task: &UploadTaskDetail,
        cancel_token: CancellationToken,
        on_progress: impl Fn(u32, u32) + Send + Sync + 'static,
    ) -> Result<()> {
        self.run_upload_chunks(file_path, task, cancel_token, on_progress).await?;
        self.finish_upload(task.total_chunks).await
    }
}

/// 创建单个 chunk 上传的 JoinHandle，供流式并发窗口使用
#[allow(clippy::too_many_arguments)]
fn spawn_chunk_upload(
    chunk_index: u32,
    file_path: PathBuf,
    client: Arc<Client>,
    upload_id: String,
    api_url: String,
    token: String,
    chunk_size: usize,
    total_chunks: u32,
    on_progress: Arc<impl Fn(u32, u32) + Send + Sync + 'static>,
    uploaded_count: Arc<std::sync::atomic::AtomicU32>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<Result<(), anyhow::Error>> {
    tokio::spawn(async move {
        // Early cancellation check before doing any IO
        if cancel.is_cancelled() {
            return Err(anyhow!("paused"));
        }

        // chunk_index 是 1-based，文件偏移量需减 1
        let offset = (chunk_index - 1) as u64 * chunk_size as u64;

        // Read the chunk on a blocking thread to avoid holding a large buffer
        // in the async state machine across .await points, which would cause
        // the future's size to exceed the thread stack (STATUS_STACK_BUFFER_OVERRUN
        // on Windows with the default 4 MB stack).
        let file_path_clone = file_path.clone();
        let data = tokio::task::spawn_blocking(move || {
            read_chunk_sync(&file_path_clone, offset, chunk_size)
        })
        .await
        .map_err(|e| anyhow!("Chunk {} read task panicked: {:#}", chunk_index, e))?
        .map_err(|e| anyhow!("Failed to read chunk {} from file: {:#}", chunk_index, e))?;

        // Check cancellation again after the blocking read
        if cancel.is_cancelled() {
            return Err(anyhow!("paused"));
        }

        let url = format!("{}/api/v1/upload/{}/chunk/{}", api_url, upload_id, chunk_index);

        // Build the request before the await boundary so the large `data`
        // buffer is consumed (moved into the request body) and no longer
        // lives in the future state machine during the send().await.
        let request = client
            .put(&url)
            .bearer_auth(&token)
            .multipart(
                reqwest::multipart::Form::new().part(
                    "file",
                    reqwest::multipart::Part::bytes(data)
                        .file_name("chunk")
                        .mime_str("application/octet-stream")
                        .unwrap(),
                ),
            )
            .build()
            .map_err(|e| anyhow!("Failed to build request for chunk {}: {:#}", chunk_index, e))?;

        // 用 select! 监听取消，HTTP 请求可以被中断
        let response = tokio::select! {
            r = client.execute(request) => {
                r.map_err(|e| {
                    tracing::error!(
                        "Chunk {} network error: {:#}\n  is_connect={} is_timeout={} is_request={}",
                        chunk_index, e,
                        e.is_connect(),
                        e.is_timeout(),
                        e.is_request(),
                    );
                    anyhow!("Failed to upload chunk {}: {:#}", chunk_index, e)
                })?
            }
            _ = cancel.cancelled() => {
                tracing::info!("Chunk {} cancelled", chunk_index);
                return Err(anyhow!("paused"));
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Failed to upload chunk {}: HTTP {} - {}",
                chunk_index,
                status,
                body
            ));
        }

        let done = uploaded_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        on_progress(done, total_chunks);

        // 节流 WS 广播：只在每 5 个 chunk、第一个 chunk、最后一个 chunk 时广播
        // 避免大文件上传时每个 chunk 都广播导致前端消息洪泛
        if done == 1 || done >= total_chunks || done % 5 == 0 {
            ws_server::broadcast_message(json!({
                "type": "progress",
                "upload_id": upload_id,
                "uploaded_chunks": done,
                "total_chunks": total_chunks,
                "progress_pct": (done as f32 / total_chunks as f32) * 100.0
            }));
        }

        tracing::debug!("Chunk {}/{} uploaded", chunk_index, total_chunks);
        Ok::<(), anyhow::Error>(())
    })
}

/// Synchronous chunk reader — called via `spawn_blocking` so the large buffer
/// never lives inside an async future's state machine (avoids stack overflow
/// on Windows where the default thread stack is only 4 MB).
fn read_chunk_sync(file_path: &PathBuf, offset: u64, size: usize) -> Result<Vec<u8>> {
    use std::io::{Read, Seek};

    let mut file = std::fs::File::open(file_path)
        .with_context(|| format!("Failed to open file: {:?}", file_path))?;

    file.seek(std::io::SeekFrom::Start(offset))
        .context("Failed to seek in file")?;

    // Use take + read_to_end so the last (smaller) chunk is handled correctly
    // without triggering UnexpectedEof from read_exact.
    let mut buf = Vec::with_capacity(size);
    file.take(size as u64)
        .read_to_end(&mut buf)
        .with_context(|| format!("Failed to read chunk from file: {:?}", file_path))?;

    if buf.is_empty() {
        return Err(anyhow!("Read 0 bytes at offset {} from file: {:?}", offset, file_path));
    }

    Ok(buf)
}
