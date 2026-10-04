//! 文件系统浏览数据面 — `GET /fs/pick` 在宿主上弹操作系统的原生目录
//! 选择器（IFileOpenDialog/NSOpenPanel/portal），纯浏览器前端因此拿到
//! 与 Tauri 壳一致的选择体验；`GET /fs/browse` 是逐级列出目录的降级
//! 通道（headless/远程宿主弹不出对话框时用）。
//! 只列目录、不列文件：它回答的是"下一个会话落在哪个项目"，不是通用
//! 文件管理器。

use std::sync::Arc;

use super::super::host::{Shared, display_path};
use super::HostResponse;

/// `GET /fs/browse?dir=…` — one level of the directory tree: `dir` itself
/// (display form), `parent` for the ‹ button, and child dirs. Dot-dirs
/// stay out (project roots live in named dirs; `.sunmao` is never a
/// pick). A missing/gone dir is a 404 — the caller falls back to typing.
pub(super) fn browse(s: &Arc<Shared>, dir: Option<String>) -> HostResponse {
    let dir = dir
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| s.cwd.clone());
    let dir = match dir.canonicalize() {
        Ok(d) if d.is_dir() => d,
        _ => return HostResponse::err(404, "not a directory".into()),
    };
    let mut dirs: Vec<serde_json::Value> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| {
                    serde_json::json!({
                        "name": e.file_name().to_string_lossy(),
                        "path": display_path(&e.path()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    dirs.sort_by_key(|d| d["name"].as_str().unwrap_or("").to_lowercase());
    // a pathological dir (C:\Windows\System32) must not stall the popover
    dirs.truncate(300);
    HostResponse::json(serde_json::json!({
        "dir": display_path(&dir),
        "parent": dir.parent().map(display_path),
        "dirs": dirs,
    }))
}

/// `GET /fs/pick?dir=…` — the NATIVE folder dialog on the host: rfd pops
/// the OS picker (IFileOpenDialog on Windows), the modal blocks until the
/// user picks or cancels — so this endpoint is async and parks the call
/// on a blocking thread. `path` is the selection, null on cancel; a
/// headless host that can't show UI answers 500 and the caller falls back
/// to typing a path.
pub(super) async fn pick(s: &Arc<Shared>, dir: Option<String>) -> HostResponse {
    let start = dir
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| s.cwd.clone());
    match tokio::task::spawn_blocking(move || {
        rfd::FileDialog::new().set_directory(&start).pick_folder()
    })
    .await
    {
        Ok(path) => HostResponse::json(serde_json::json!({
            "path": path.as_ref().map(|p| display_path(p)),
        })),
        Err(e) => HostResponse::err(500, format!("picker unavailable: {e}")),
    }
}
