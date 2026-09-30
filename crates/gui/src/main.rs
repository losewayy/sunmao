//! sunmao desktop shell — GUI phase 2 (GUI.md §8).
//!
//! The web frontend is byte-identical to `sunmao serve`'s: this process
//! embeds the SAME multi-session host (`sunmao::serve_main`) on an
//! ephemeral loopback port, then opens a frameless webview onto it.
//! One host implementation — the shell only supplies the window. The
//! custom titlebar/drag region the DESIGN.md workbench mandates is
//! rendered by the page itself (enabled under `window.__TAURI__`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;

/// The page is served over loopback HTTP, not the tauri:// scheme — Tauri
/// never injects its globals there, so the shell's seam is this script:
/// `window.__sunmaoShell` carries the caption/drag verbs as IPC commands.
/// Capability-gated like any other command (`shell-win`, `shell-drag`).
const SHELL_SHIM: &str = r#"
window.__sunmaoShell = {
  win(op) { return window.__TAURI_INTERNALS__.invoke('shell_win', { op }); },
  drag() { return window.__TAURI_INTERNALS__.invoke('shell_drag'); },
};
"#;

#[tauri::command]
fn shell_win(win: tauri::WebviewWindow, op: &str) {
    match op {
        "min" => {
            let _ = win.minimize();
        }
        "max" => {
            if win.is_maximized().unwrap_or(false) {
                let _ = win.unmaximize();
            } else {
                let _ = win.maximize();
            }
        }
        "close" => {
            let _ = win.close();
        }
        _ => {}
    }
}

/// The titlebar is the drag region — a left-button press anywhere on it
/// (except interactive controls) starts a native window drag.
#[tauri::command]
fn shell_drag(win: tauri::WebviewWindow) {
    let _ = win.start_dragging();
}

fn main() {
    sunmao::init_tracing();
    // Provider/config resolve through the same env-driven Cli the CLI
    // uses (SUNMAO_BASE_URL / API_KEY / MODEL / PROVIDER); argv is ignored.
    let cli = sunmao::Cli::parse_from(["sunmao-gui"]);
    // Pre-bind the host socket so the webview URL is known before the
    // window opens. Port 0 = OS-assigned; the sandbox proxy takes port+1.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0u16)).expect("bind gui host");
    let port = listener.local_addr().expect("local_addr").port();
    // The host owns its own tokio runtime on a dedicated thread — Tauri's
    // main thread belongs to the Win32 event loop.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("host tokio runtime");
        rt.block_on(async move {
            if let Err(e) = sunmao::serve_main(&cli, listener).await {
                tracing::error!("gui host exited: {e:#}");
            }
        });
    });
    let url = format!("http://127.0.0.1:{port}/")
        .parse()
        .expect("gui url");
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![shell_win, shell_drag])
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("sunmao")
                .inner_size(1440.0, 900.0)
                .min_inner_size(960.0, 640.0)
                // frameless — the page renders its own titlebar (DESIGN.md)
                .decorations(false)
                .disable_drag_drop_handler()
                .initialization_script(SHELL_SHIM)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("sunmao gui");
}
