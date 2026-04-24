use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub api_url: String,
    pub web_url: String,
    pub ws_port: u16,
    pub max_concurrent_chunks: usize,
    pub chunk_size: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api_url: std::env::var("API_URL")
                .unwrap_or_else(|_| option_env!("BUILD_API_URL").unwrap_or("http://localhost:8000").to_string())
                .trim_end_matches('/')
                .to_string(),
            web_url: std::env::var("WEB_URL")
                .unwrap_or_else(|_| option_env!("BUILD_WEB_URL").unwrap_or("http://localhost:5173").to_string())
                .trim_end_matches('/')
                .to_string(),
            ws_port: 17892,
            max_concurrent_chunks: 4,
            chunk_size: 5 * 1024 * 1024, // 5MB
        }
    }
}

static CONFIG: OnceCell<RwLock<AppConfig>> = OnceCell::new();

pub fn init_config() {
    CONFIG.get_or_init(|| RwLock::new(AppConfig::default()));
}

pub fn get_config() -> AppConfig {
    CONFIG
        .get()
        .expect("config not initialized")
        .read()
        .expect("config lock poisoned")
        .clone()
}

pub fn update_config(new_config: AppConfig) {
    if let Some(lock) = CONFIG.get() {
        let mut cfg = lock.write().expect("config lock poisoned");
        *cfg = new_config;
    }
}
