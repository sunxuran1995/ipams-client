use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;

static CONNECTION_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct WsServer {
    pub tx: broadcast::Sender<String>,
}

static WS_SERVER: Lazy<Arc<WsServer>> = Lazy::new(|| {
    let (tx, _) = broadcast::channel(256);
    Arc::new(WsServer { tx })
});

pub fn get_server() -> Arc<WsServer> {
    WS_SERVER.clone()
}

pub fn broadcast_message(msg: Value) {
    let server = get_server();
    let text = msg.to_string();
    let _ = server.tx.send(text);
}

/// Single server on `port` handles both WebSocket (`/ws`) and HTTP ping (`/ping`).
pub async fn start_ws_server(port: u16) -> Result<()> {
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("WS/HTTP server listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                tracing::debug!("New connection from {}", peer_addr);
                let server = get_server();
                tokio::spawn(async move {
                    if let Err(e) = dispatch_connection(stream, server).await {
                        tracing::debug!("Connection error from {}: {}", peer_addr, e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Accept error: {}", e);
            }
        }
    }
}

/// Peek at the first bytes to decide whether to handle as WS upgrade or HTTP.
async fn dispatch_connection(mut stream: TcpStream, server: Arc<WsServer>) -> Result<()> {
    let mut peek_buf = [0u8; 512];
    let n = stream.peek(&mut peek_buf).await?;
    let header = std::str::from_utf8(&peek_buf[..n]).unwrap_or("");

    // Detect WebSocket upgrade request
    let is_ws = header.contains("Upgrade: websocket")
        || header.contains("Upgrade: Websocket")
        || header.contains("upgrade: websocket");

    if is_ws {
        handle_ws(stream, server).await
    } else {
        handle_http_ping(stream, header.to_string()).await
    }
}

async fn handle_ws(stream: TcpStream, server: Arc<WsServer>) -> Result<()> {
    let ws_stream = tokio_tungstenite::accept_async(stream).await?;
    let id = CONNECTION_COUNTER.fetch_add(1, Ordering::SeqCst);
    tracing::info!("WebSocket client {} connected", id);

    let (mut write, mut read) = ws_stream.split();
    let mut rx = server.tx.subscribe();

    // Send welcome
    let welcome = serde_json::json!({
        "type": "connected",
        "client_id": id,
        "service": "ipams-client"
    });
    write.send(Message::Text(welcome.to_string())).await?;

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if write.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WS client {} lagged {} messages", id, n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("WebSocket client {} disconnected", id);
                        break;
                    }
                    Some(Err(e)) => {
                        tracing::debug!("WS client {} error: {}", id, e);
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

async fn handle_http_ping(mut stream: TcpStream, request: String) -> Result<()> {
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body) = if path == "/ping" {
        (
            "200 OK",
            r#"{"status":"ok","service":"ipams-client"}"#,
        )
    } else {
        ("404 Not Found", r#"{"error":"not found"}"#)
    };

    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// No-op — kept for backward compat in lib.rs; the combined server handles ping too.
pub async fn start_http_ping_server(_port: u16) -> Result<()> {
    // Ping is handled in start_ws_server now; nothing to do.
    futures_util::future::pending::<()>().await;
    Ok(())
}
