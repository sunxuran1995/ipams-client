pub mod manager;
pub mod upload;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferTask {
    pub upload_id: String,
    pub filename: String,
    pub file_size: u64,
    pub total_chunks: u32,
    pub uploaded_chunks: u32,
    pub status: TaskStatus,
    pub error: Option<String>,
    pub created_at: u64,
    /// 本地文件路径，用于断点续传
    #[serde(default)]
    pub file_path: Option<String>,
    /// 后端返回的 chunk_size，用于续传时保持一致
    #[serde(default)]
    pub chunk_size: Option<u64>,
    /// 任务所属用户 ID，用于多用户隔离
    #[serde(default)]
    pub user_id: Option<String>,
}

impl TransferTask {
    pub fn progress_percent(&self) -> f32 {
        if self.total_chunks == 0 {
            return 0.0;
        }
        (self.uploaded_chunks as f32 / self.total_chunks as f32) * 100.0
    }
}

/// Response from GET /api/v1/client/tasks/upload/{upload_id}  (data 字段内容)
#[derive(Debug, Clone, Deserialize)]
pub struct UploadTaskDetail {
    pub upload_id: String,
    pub asset_id: String,
    pub original_filename: String,
    pub file_size: u64,
    pub chunk_size: u64,
    pub total_chunks: u32,
    /// 已上传的分块编号列表（1-indexed，与后端一致），用于断点续传
    pub uploaded_chunks: Vec<u32>,
    pub oss_path: String,
    pub oss_upload_id: Option<String>,
}

/// Response from GET /api/v1/upload/{upload_id}/progress  (data 字段内容)
/// 只需要断点续传所需的两个字段，其余忽略
#[derive(Debug, Default, Deserialize)]
pub struct UploadProgress {
    #[serde(default)]
    pub uploaded_chunks: Vec<u32>,
    #[serde(default)]
    pub total_chunks: u32,
}

/// 通用 API 响应包装  {"code":0,"msg":"...","data":{...}}
#[derive(Deserialize)]
pub struct ApiResponse<T> {
    pub code: i32,
    #[serde(default)]
    pub msg: Option<String>,
    pub data: Option<T>,
}
