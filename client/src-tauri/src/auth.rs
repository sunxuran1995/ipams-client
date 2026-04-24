use anyhow::{anyhow, Result};
use keyring::Entry;
use once_cell::sync::Lazy;
use std::sync::RwLock;

const SERVICE_NAME: &str = "com.crotonmedia.ipams-client";
const TOKEN_KEY: &str = "auth_token";

// 内存缓存，作为 keyring 的备份
static TOKEN_CACHE: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));

pub fn save_token(token: &str) -> Result<()> {
    // 先存内存
    if let Ok(mut cache) = TOKEN_CACHE.write() {
        *cache = Some(token.to_string());
    }
    // 再存 keyring（失败不影响内存缓存）
    let entry = Entry::new(SERVICE_NAME, TOKEN_KEY)
        .map_err(|e| anyhow!("Failed to create keyring entry: {}", e))?;
    entry
        .set_password(token)
        .map_err(|e| anyhow!("Failed to save token: {}", e))?;
    tracing::info!("Token saved to keyring");
    Ok(())
}

pub fn load_token() -> Option<String> {
    // 先查内存缓存
    if let Ok(cache) = TOKEN_CACHE.read() {
        if let Some(token) = cache.as_ref() {
            return Some(token.clone());
        }
    }
    // 内存没有再查 keyring
    let entry = Entry::new(SERVICE_NAME, TOKEN_KEY).ok()?;
    match entry.get_password() {
        Ok(token) if !token.is_empty() => {
            // 回填内存缓存
            if let Ok(mut cache) = TOKEN_CACHE.write() {
                *cache = Some(token.clone());
            }
            Some(token)
        }
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("No token in keyring: {}", e);
            None
        }
    }
}

pub fn delete_token() -> Result<()> {
    // 清内存缓存
    if let Ok(mut cache) = TOKEN_CACHE.write() {
        *cache = None;
    }
    let entry = Entry::new(SERVICE_NAME, TOKEN_KEY)
        .map_err(|e| anyhow!("Failed to create keyring entry: {}", e))?;
    entry
        .delete_credential()
        .map_err(|e| anyhow!("Failed to delete token: {}", e))?;
    tracing::info!("Token deleted from keyring");
    Ok(())
}

pub fn is_logged_in() -> bool {
    load_token().is_some()
}

/// 从 JWT token 解析 sub 字段（user_id）
pub fn get_user_id_from_token(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 { return None; }
    let padded = match parts[1].len() % 4 {
        2 => format!("{}==", parts[1]),
        3 => format!("{}=", parts[1]),
        _ => parts[1].to_string(),
    };
    let s = padded.replace('-', "+").replace('_', "/");
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [0u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    let bytes = s.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'=' { break; }
        let b0 = lookup[bytes[i] as usize] as u32;
        let b1 = lookup[bytes[i+1] as usize] as u32;
        let b2 = if bytes[i+2] == b'=' { 0 } else { lookup[bytes[i+2] as usize] as u32 };
        let b3 = if i+3 >= bytes.len() || bytes[i+3] == b'=' { 0 } else { lookup[bytes[i+3] as usize] as u32 };
        let n = (b0 << 18) | (b1 << 12) | (b2 << 6) | b3;
        result.push(((n >> 16) & 0xff) as u8);
        if bytes[i+2] != b'=' { result.push(((n >> 8) & 0xff) as u8); }
        if i+3 < bytes.len() && bytes[i+3] != b'=' { result.push((n & 0xff) as u8); }
        i += 4;
    }
    let payload: serde_json::Value = serde_json::from_slice(&result).ok()?;
    payload["sub"].as_str().map(|s| s.to_string())
}
