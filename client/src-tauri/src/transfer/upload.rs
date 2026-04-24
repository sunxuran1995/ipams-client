use super::{ApiResponse, UploadProgress, UploadTaskDetail};
use crate::{auth, config, ws_server};
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use reqwest::Client;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Semaphore;
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
            .build()
            .context("Failed to build HTTP client")?;

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

    pub async fn upload_chunk(&self, chunk_index: u32, data: Bytes) -> Result<()> {
        let url = format!(
            "{}/api/v1/upload/{}/chunk/{}",
            self.api_url, self.upload_id, chunk_index
        );

        let response = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/octet-stream")
            .body(data)
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
    pub async fn run_upload(
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

        let uploaded_set: std::collections::HashSet<u32> =
            existing_progress.uploaded_chunks.iter().cloned().collect();

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
            tracing::info!("All chunks already uploaded, completing...");
            return self.complete_upload().await;
        }

        let semaphore = Arc::new(Semaphore::new(cfg.max_concurrent_chunks));
        let on_progress = Arc::new(on_progress);
        let mut handles = Vec::new();
        let already_uploaded = task.total_chunks - chunks_to_upload.len() as u32;
        let uploaded_count = Arc::new(std::sync::atomic::AtomicU32::new(already_uploaded));
        let upload_id_ref = self.upload_id.clone();

        for chunk_index in chunks_to_upload {
            // 检查是否被暂停/取消
            if cancel_token.is_cancelled() {
                tracing::info!("Upload {} cancelled/paused at chunk {}", upload_id_ref, chunk_index);
                break;
            }

            // acquire semaphore，同时监听取消
            let permit = tokio::select! {
                p = semaphore.clone().acquire_owned() => p?,
                _ = cancel_token.cancelled() => {
                    tracing::info!("Upload {} cancelled while waiting for semaphore", upload_id_ref);
                    break;
                }
            };

            let file_path = file_path.clone();
            let upload_id = self.upload_id.clone();
            let api_url = self.api_url.clone();
            let token = self.token.clone();
            let chunk_size = task.chunk_size as usize;
            let total_chunks = task.total_chunks;
            let on_progress = on_progress.clone();
            let uploaded_count = uploaded_count.clone();
            let cancel = cancel_token.clone();

            let handle = tokio::spawn(async move {
                let _permit = permit;

                // chunk_index 是 1-based，文件偏移量需减 1
                let offset = (chunk_index - 1) as u64 * chunk_size as u64;
                let data = read_chunk(&file_path, offset, chunk_size).await?;

                let client = Client::builder()
                    .timeout(std::time::Duration::from_secs(120))
                    .build()?;

                let url = format!("{}/api/v1/upload/{}/chunk/{}", api_url, upload_id, chunk_index);
                let form = reqwest::multipart::Form::new().part(
                    "file",
                    reqwest::multipart::Part::bytes(data.to_vec())
                        .file_name("chunk")
                        .mime_str("application/octet-stream")
                        .unwrap(),
                );

                // 用 select! 监听取消，HTTP 请求可以被中断
                let response = tokio::select! {
                    r = client.put(&url).bearer_auth(&token).multipart(form).send() => {
                        r.with_context(|| format!("Failed to upload chunk {}", chunk_index))?
                    }
                    _ = cancel.cancelled() => {
                        tracing::info!("Chunk {} cancelled", chunk_index);
                        return Err(anyhow!("paused"));
                    }
                };

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    return Err(anyhow!("Chunk {} upload failed {}: {}", chunk_index, status, body));
                }

                let done = uploaded_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                on_progress(done, total_chunks);

                ws_server::broadcast_message(json!({
                    "type": "upload_progress",
                    "upload_id": upload_id,
                    "uploaded_chunks": done,
                    "total_chunks": total_chunks,
                    "percent": (done as f32 / total_chunks as f32) * 100.0
                }));

                tracing::debug!("Chunk {}/{} uploaded", chunk_index, total_chunks);
                Ok::<(), anyhow::Error>(())
            });

            handles.push(handle);
        }

        // Wait for all chunks
        let mut errors = Vec::new();
        let mut paused = false;
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.to_string() == "paused" => { paused = true; }
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(format!("Task panicked: {}", e)),
            }
        }

        if paused && errors.is_empty() {
            // 任务被暂停，返回特殊错误让调用方知道
            return Err(anyhow!("paused"));
        }

        if !errors.is_empty() {
            return Err(anyhow!("Upload errors: {}", errors.join("; ")));
        }

        // Complete upload
        self.complete_upload().await?;

        // Broadcast completion
        ws_server::broadcast_message(json!({
            "type": "upload_complete",
            "upload_id": self.upload_id,
            "status": "completed"
        }));

        Ok(())
    }
}

async fn read_chunk(file_path: &PathBuf, offset: u64, size: usize) -> Result<Bytes> {
    let mut file = File::open(file_path)
        .await
        .with_context(|| format!("Failed to open file: {:?}", file_path))?;

    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .context("Failed to seek in file")?;

    let mut buf = vec![0u8; size];
    let n = file
        .read(&mut buf)
        .await
        .context("Failed to read chunk from file")?;
    buf.truncate(n);

    Ok(Bytes::from(buf))
}
