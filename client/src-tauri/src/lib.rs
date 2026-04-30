mod auth;
mod config;
mod deep_link;
mod tray;
mod transfer;
mod ws_server;

use config::AppConfig;
use tauri::{AppHandle, Emitter, Listener, Runtime};
use transfer::manager;
use transfer::TransferTask;

// ── Tauri Commands ─────────────────────────────────────────────────────────

#[tauri::command]
async fn get_tasks() -> Result<Vec<TransferTask>, String> {
    Ok(manager::get_all_tasks().await)
}

#[tauri::command]
async fn cancel_task(upload_id: String) -> Result<bool, String> {
    Ok(manager::cancel_task(&upload_id).await)
}

#[tauri::command]
async fn pause_task(upload_id: String) -> Result<bool, String> {
    Ok(manager::pause_task(&upload_id).await)
}

#[tauri::command]
async fn resume_task(upload_id: String) -> Result<bool, String> {
    Ok(manager::resume_task(&upload_id).await)
}

#[tauri::command]
fn get_config() -> AppConfig {
    config::get_config()
}

#[tauri::command]
fn get_token() -> Option<String> {
    auth::load_token()
}

#[tauri::command]
fn get_current_username() -> Option<String> {
    auth::load_token().and_then(|token| {
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        if parts.len() < 2 { return None; }
        let padded = match parts[1].len() % 4 {
            2 => format!("{}==", parts[1]),
            3 => format!("{}=", parts[1]),
            _ => parts[1].to_string(),
        };
        let decoded = base64_url_decode(&padded);
        serde_json::from_slice::<serde_json::Value>(&decoded).ok().and_then(|payload| {
            payload["username"].as_str()
                .or_else(|| payload["name"].as_str())
                .or_else(|| payload["display_name"].as_str())
                .map(|s| s.to_string())
        })
    })
}

fn base64_url_decode(s: &str) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [0u8; 256];
    for (i, &c) in alphabet.iter().enumerate() { lookup[c as usize] = i as u8; }
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
    result
}

#[tauri::command]
fn save_token(token: String) -> Result<(), String> {
    auth::save_token(&token).map_err(|e| e.to_string())
}

#[tauri::command]
async fn logout() -> Result<(), String> {
    auth::delete_token().map_err(|e| e.to_string())?;
    // 清空内存中的任务列表
    manager::clear_tasks().await;
    Ok(())
}

#[tauri::command]
fn is_logged_in() -> bool {
    auth::is_logged_in()
}

#[tauri::command]
async fn open_login_page<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    let cfg = config::get_config();
    let login_url = format!("{}/login?client_callback=1", cfg.web_url);
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(&login_url, None::<&str>)
        .map_err(|e| e.to_string())
}

// ── App Setup ─────────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Init config
    config::init_config();

    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting IPAMS Client");

    tauri::Builder::default()
        // Single-instance: 若已有实例运行，将启动参数转发给它并退出
        // (仅 Windows/Linux 支持，macOS 通过系统机制保证单实例)
        #[cfg(not(target_os = "macos"))]
        .plugin(
            tauri_plugin_single_instance::init(|app, argv, _cwd| {
                // argv 包含第二个实例的命令行参数，其中可能含 ipams:// URL
                tracing::info!("Second instance launched, argv: {:?}", argv);
                // 聚焦已有窗口
                use tauri::Manager;
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
                // 从参数中提取 ipams:// URL 并处理
                let handle = app.clone();
                for arg in &argv {
                    if arg.starts_with("ipams://") {
                        let url = arg.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = deep_link::handle_deep_link(&handle, &url).await {
                                tracing::error!("Deep link (second instance) error: {}", e);
                            }
                        });
                        break;
                    }
                }
            }),
        )
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Register ipams:// URL scheme (required in dev mode; production uses installer)
            #[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                if let Err(e) = app.deep_link().register("ipams") {
                    tracing::warn!("Failed to register deep link scheme: {}", e);
                } else {
                    tracing::info!("Registered ipams:// URL scheme");
                }
            }

            // Setup system tray
            tray::setup_tray(&handle)?;

            // Start WebSocket / HTTP server in background
            let cfg = config::get_config();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = ws_server::start_ws_server(cfg.ws_port).await {
                    tracing::error!("WS server error: {}", e);
                }
            });

            // Resume interrupted uploads
            let resume_token = auth::load_token();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                manager::resume_pending_tasks(resume_token).await;
            });

            // Register deep link handler
            let handle2 = handle.clone();
            app.listen("deep-link://new-url", move |event: tauri::Event| {
                let raw = event.payload().to_string();
                // Strip surrounding quotes if present (Tauri serialises strings as JSON)
                let url = raw
                    .strip_prefix('"')
                    .and_then(|s: &str| s.strip_suffix('"'))
                    .unwrap_or(&raw)
                    .replace("\\\"", "\"")
                    .to_string();

                let handle3 = handle2.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = deep_link::handle_deep_link(&handle3, &url).await {
                        tracing::error!("Deep link error: {}", e);
                    }
                    // token 保存后重载当前用户的任务列表
                    manager::reload_tasks_for_current_user().await;
                    // 通知前端刷新任务列表
                    let _ = handle3.emit("tasks:reloaded", ());
                });
            });

            // 启动时登录检查：
            // 若命令行参数中已有 ipams:// 深链接（冷启动场景），跳过检查
            // 深链接处理会自动保存 token 并继续任务
            let launched_by_deep_link = std::env::args()
                .any(|a| a.starts_with("ipams://"));

            if !launched_by_deep_link {
                let handle4 = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;
                    if !auth::is_logged_in() {
                        tracing::info!("Not logged in, opening login page");
                        let cfg = config::get_config();
                        let login_url = format!("{}/login?client_callback=1", cfg.web_url);
                        use tauri_plugin_opener::OpenerExt;
                        if let Err(e) = handle4.opener().open_url(&login_url, None::<&str>) {
                            tracing::error!("Failed to open login page: {}", e);
                        }
                    } else {
                        tracing::info!("Token found, user is logged in");
                    }
                });
            } else {
                tracing::info!("Launched by deep link, skipping startup login check");
            }

            tracing::info!("IPAMS Client setup complete");
            Ok(())
        })
        .on_window_event(|window, event| {
            // Intercept close → hide to tray instead
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_tasks,
            cancel_task,
            pause_task,
            resume_task,
            get_config,
            get_token,
            get_current_username,
            save_token,
            logout,
            is_logged_in,
            open_login_page,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
