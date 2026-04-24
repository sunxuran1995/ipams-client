use crate::{auth, config, tray, transfer};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tauri::{AppHandle, Emitter, Runtime};
use url::Url;

/// Parse `ipams://` URL and dispatch to appropriate handler
pub async fn handle_deep_link<R: Runtime>(app: &AppHandle<R>, url_str: &str) -> Result<()> {
    tracing::info!("Handling deep link: {}", url_str);

    let url = Url::parse(url_str).map_err(|e| anyhow!("Invalid URL: {}", e))?;

    match url.host_str() {
        Some("upload") => handle_upload(app, &url).await,
        Some("auth") => handle_auth(app, &url),
        _ => {
            tracing::warn!("Unknown deep link host: {:?}", url.host_str());
            Ok(())
        }
    }
}

fn parse_query(url: &Url) -> HashMap<String, String> {
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// `ipams://auth?token=JWT`
fn handle_auth<R: Runtime>(app: &AppHandle<R>, url: &Url) -> Result<()> {
    let params = parse_query(url);
    let token = params
        .get("token")
        .ok_or_else(|| anyhow!("Missing token parameter"))?;

    auth::save_token(token)?;
    tracing::info!("Auth token saved via deep link");

    // Show main window after login
    tray::show_window(app);

    // Notify frontend — 直接把 token 带过去，前端无需再查 keyring
    let _ = app.emit("auth:token-saved", token.clone());

    Ok(())
}

/// `ipams://upload?project_id=...&folder_id=...&asset_type_id=...&count=N&token=JWT`
/// count=0 means folder upload mode
async fn handle_upload<R: Runtime>(app: &AppHandle<R>, url: &Url) -> Result<()> {
    let params = parse_query(url);

    // 保存 token
    let token = if let Some(t) = params.get("token") {
        // 校验：如果客户端已登录，检查 token 的用户是否一致
        if let Some(current_token) = auth::load_token() {
            let current_user = auth::get_user_id_from_token(&current_token);
            let incoming_user = auth::get_user_id_from_token(t);
            if current_user.is_some() && incoming_user.is_some() && current_user != incoming_user {
                tracing::warn!(
                    "User mismatch: client logged in as {:?}, but upload request from {:?}. Rejecting.",
                    current_user, incoming_user
                );
                // 通知前端用户不匹配
                let _ = app.emit("upload:user-mismatch", ());
                return Ok(());
            }
        }
        if let Err(e) = auth::save_token(t) {
            tracing::warn!("Failed to save token from URL: {}", e);
        } else {
            tracing::info!("Token received from URL and saved");
        }
        Some(t.to_string())
    } else {
        None
    };

    if token.is_none() && !auth::is_logged_in() {
        tracing::info!("Not logged in, opening browser login page");
        open_login_page(app)?;
        return Ok(());
    }

    let project_id = params
        .get("project_id")
        .ok_or_else(|| anyhow!("Missing project_id parameter"))?
        .to_string();
    let folder_id = params.get("folder_id").map(|s| s.to_string());
    let asset_type_id = params.get("asset_type_id").map(|s| s.to_string());

    // mode=folder means folder upload; count=0 also triggers folder mode
    let mode = params.get("mode").map(|s| s.as_str()).unwrap_or("file");
    let count: u32 = if mode == "folder" {
        0
    } else {
        params.get("count").and_then(|s| s.parse().ok()).unwrap_or(1)
    };

    tracing::info!("Upload request: project={} count={} mode={}", project_id, count, mode);

    // Show window
    tray::show_window(app);

    let app_clone = app.clone();
    tokio::spawn(async move {
        if let Err(e) = transfer::manager::enqueue_upload_by_params(
            &app_clone,
            project_id,
            folder_id,
            asset_type_id,
            count,
            token,
        )
        .await
        {
            tracing::error!("Failed to enqueue upload: {}", e);
        }
    });

    Ok(())
}

fn open_login_page<R: Runtime>(app: &AppHandle<R>) -> Result<()> {
    let cfg = config::get_config();
    let login_url = format!("{}/login?client_callback=1", cfg.web_url);
    tracing::info!("Opening login page: {}", login_url);

    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(&login_url, None::<&str>)
        .map_err(|e| anyhow!("Failed to open browser: {}", e))?;

    Ok(())
}
