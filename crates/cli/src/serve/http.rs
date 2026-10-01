//! `sunmao serve` 的 HTTP 表面 — axum 适配层。`/ws` 升级进 `ws` 模块，
//! 其余一切经 `rest_dispatch` 落到 `HostHandle::request`（与 Tauri
//! scheme 处理器共用同一张路由表，REST 语义不复制）。
//! 绑 127.0.0.1，无账号——单用户本地前端。

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{Html, Response};
use axum::routing::get;

use super::request::HostResponse;
use super::{HostHandle, HostSpec, ws};

/// `HostResponse` → axum `Response` — the only place status/headers get
/// mapped onto axum types.
fn into_response(r: HostResponse) -> Response {
    let mut b = Response::builder().status(r.status);
    for (k, v) in r.headers {
        b = b.header(k, v);
    }
    b.body(Body::from(r.body)).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::empty())
            .unwrap_or_default()
    })
}

/// Everything except `/ws` funnels into the transport-free route table —
/// one dispatch implementation for axum and the Tauri `sunmao` scheme.
async fn rest_dispatch(host: axum::extract::State<HostHandle>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str();
    let pq = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    // axum's `get` used to answer HEAD / implicitly — keep that parity.
    // 32 MiB: /attachments uploads carry whole images
    let body = to_bytes(body, 32 * 1024 * 1024).await.unwrap_or_default();
    let mut resp = host
        .request(if method == "HEAD" { "GET" } else { method }, pq, &body)
        .await;
    if method == "HEAD" {
        resp.body.clear();
    }
    into_response(resp)
}

/// The MCP Apps sandbox proxy page (spec: host and sandbox MUST be
/// different origins — this rides its own listener, port reported in the
/// ws hello). Same embedded file every request; it has no state.
async fn sandbox_page() -> Html<&'static str> {
    Html(super::SANDBOX_PAGE)
}

/// Serve until the process exits. The web assets are embedded — no node,
/// no build step, `sunmao serve` is the whole deploy story. `listener` is
/// the caller's pre-bound main socket (the Tauri shell used to bind port 0
/// so it could learn the port before opening its webview — it now rides
/// the custom scheme instead); the MCP Apps sandbox proxy still gets its
/// own listener — the spec's double-iframe needs a second origin, reported
/// as `sandbox_port` in the ws hello.
/// `spec.factory` rebuilds the startup Context assembly per session — the
/// host adopts every log as its own AgentLoop instead of swapping one
/// shared Context between tabs.
pub(crate) async fn run(spec: HostSpec, listener: std::net::TcpListener) -> Result<()> {
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let sandbox_port_hint = port.saturating_add(1);
    let sandbox_app = axum::Router::new().route("/sandbox.html", get(sandbox_page));
    let sandbox_listener = tokio::net::TcpListener::bind(("127.0.0.1", sandbox_port_hint))
        .await
        .or(tokio::net::TcpListener::bind(("127.0.0.1", 0u16)).await)?;
    let sandbox_port = sandbox_listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(sandbox_listener, sandbox_app).await;
    });

    let host = super::spawn_host(spec, sandbox_port).await?;

    let app = axum::Router::new()
        .route("/ws", get(ws::ws_upgrade))
        .fallback(rest_dispatch)
        .with_state(host);

    let listener = tokio::net::TcpListener::from_std(listener)?;
    eprintln!("sunmao serve → http://127.0.0.1:{port}  (Ctrl-C to stop)");
    axum::serve(listener, app).await?;
    Ok(())
}
