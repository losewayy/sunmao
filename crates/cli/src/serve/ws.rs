//! `/ws` 的 socket 侧 — WebSocket 字节 I/O 包在传输无关的 `Client`
//! 外面：出站帧经 unbounded 通道落到唯一写者任务（扇出与应答不抢
//! sink），入站文本帧解析后交给 `Client::handle`。

use std::sync::Arc;

use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use tokio::sync::mpsc;

use super::host::Shared;
use super::{HostHandle, client::Client};

pub(super) async fn ws_upgrade(
    State(host): State<HostHandle>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    // a browser ws always carries Origin — a foreign page's socket must not
    // reach the agent (it could drive prompts AND answer its own approval
    // prompts). Loopback Origin or absent (CLI clients) is the door.
    let ok = match headers.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => {
            let body = o
                .trim_start_matches("http://")
                .trim_start_matches("https://");
            let h = body
                .trim_start_matches('[')
                .split([':', ']'])
                .next()
                .unwrap_or("");
            h == "127.0.0.1" || h.eq_ignore_ascii_case("localhost") || h == "::1"
        }
    };
    if !ok {
        return (axum::http::StatusCode::FORBIDDEN, "not a loopback origin").into_response();
    }
    ws.on_upgrade(move |socket| ws_client(host.s.clone(), socket))
        .into_response()
}

/// One browser tab: the `Client` owns viewer state; this wrapper owns the
/// socket halves and the single-writer discipline.
async fn ws_client(s: Arc<Shared>, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message as WsMsg;
    use futures_util::{SinkExt, StreamExt};
    let (mut ws_tx, mut ws_rx) = socket.split();
    // serialize ws writes through a channel — broadcast and replies both
    // feed it so no two writers race on the sink.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if ws_tx.send(WsMsg::Text(m.into())).await.is_err() {
                break;
            }
        }
    });

    let mut client = Client::connect(s, out_tx).await;

    while let Some(Ok(msg)) = ws_rx.next().await {
        let WsMsg::Text(text) = msg else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        client.handle(v).await;
    }
    drop(client);
    writer.abort();
}
