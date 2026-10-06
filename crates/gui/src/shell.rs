//! The shell seam — SHELL_SHIM (injected into every frame via the
//! initialization script) plus the shell command surface it calls:
//! caption verbs, drag, notify, external-open, dir picker, zoom, and the
//! native guest webviews that back dock browser tabs.
//!
//! Two Windows traps shape the signatures below, both from a window that
//! hosts a dock guest (a `Window::add_child` webview):
//!
//! - `tauri::WebviewWindow` as a command argument resolves through
//!   `Window::is_webview_window()`, which is false as soon as ANY child
//!   webview exists — every command taking it then fails with "current
//!   webview is not a WebviewWindow". So the verbs take `tauri::Window`
//!   (or `tauri::Webview` where the call needs webview-only API); those
//!   args resolve to `CommandItem::webview().window()` and never fail.
//! - `Window::add_child` deadlocks in a *synchronous* command: the sync
//!   body runs on the UI thread inside the WebView2 IPC callback, so
//!   `add_child`'s on-main-thread fast path builds the guest re-entrantly
//!   and its controller creation never completes. `shell_webview` is
//!   `async` so the body runs on the runtime and `add_child` hands the
//!   build to the event loop instead.

use tauri::Manager as _;

use crate::Gui;

/// The page lives on a custom scheme — Tauri never injects its JS API
/// (`window.__TAURI__`) there, but `__TAURI_INTERNALS__` (invoke +
/// transformCallback) IS injected into every frame, so the shell seam is
/// this script: `__sunmaoShell` carries the caption/drag verbs plus a
/// minimal `Channel` class — the wire twin of `@tauri-apps/api`'s
/// Channel (register a persistent callback, buffer out-of-order messages
/// by index, serialize to `__CHANNEL__:{id}` when invoked over JSON).
/// Host frames arrive as already-parsed JSON objects on `onmessage`.
pub(crate) const SHELL_SHIM: &str = r#"
window.__sunmaoShell = {
  win(op) { return window.__TAURI_INTERNALS__.invoke('shell_win', { op }); },
  drag() { return window.__TAURI_INTERNALS__.invoke('shell_drag'); },
  openExternal(path) { return window.__TAURI_INTERNALS__.invoke('shell_open', { path }); },
  notify(title, body) { return window.__TAURI_INTERNALS__.invoke('shell_notify', { title, body }); },
  zoom(op) { return window.__TAURI_INTERNALS__.invoke('shell_zoom', { op }); },
  setZoom(factor) { return window.__TAURI_INTERNALS__.invoke('shell_zoom', { op: factor }); },
  pickDir(dir) { return window.__TAURI_INTERNALS__.invoke('shell_pick_dir', { dir }); },
  webview(op) { return window.__TAURI_INTERNALS__.invoke('shell_webview', { op }); },
  Channel: class {
    constructor() {
      this.onmessage = () => {};
      this.idx = 0;
      this.pending = {};
      this.id = window.__TAURI_INTERNALS__.transformCallback(m => {
        if (m && m.end) {
          window.__TAURI_INTERNALS__.unregisterCallback(this.id);
          return;
        }
        const i = m.index;
        if (i === this.idx) {
          this.onmessage(m.message);
          this.idx++;
          while (this.idx in this.pending) {
            const p = this.pending[this.idx];
            delete this.pending[this.idx];
            this.onmessage(p);
            this.idx++;
          }
        } else {
          this.pending[i] = m.message;
        }
      });
    }
    toJSON() { return `__CHANNEL__:${this.id}`; }
  },
};
// In-shell zoom: Ctrl/Cmd+= (in), - (out), 0 (reset). Native webview zoom
// hotkeys stay off (IsZoomControlEnabled=false) so the keydown path is the
// single authority and the Rust-side level can't desync. state.js installs
// `sunmaoZoom` (op → factor → persist into .sunmao/ui.json via PUT /ui);
// before its scripts land the invoke path is the direct fallback.
window.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && !e.altKey) {
    const op = (e.key === '=' || e.key === '+') ? 'in'
      : (e.key === '-' || e.key === '_') ? 'out'
      : (e.key === '0' || e.key === ')') ? 'reset' : null;
    if (op) {
      e.preventDefault();
      if (window.sunmaoZoom) window.sunmaoZoom(op);
      else window.__sunmaoShell.zoom(op);
    }
  }
});
"#;

#[tauri::command]
/// `op` is a verb ("min"/"max"/"close"/"new") or the `state` query behind the
/// caption's maximize/restore glyph — one command because the page's shell
/// seam exposes one verb (`__sunmaoShell.win`).
pub(crate) fn shell_win(
    app: tauri::AppHandle,
    win: tauri::Window,
    op: &str,
) -> Result<serde_json::Value, String> {
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
        // maximize, restore, Aero snap and un-snapping by drag all leave this
        // answer correct — the page asks after each resize instead of tracking
        // its own guess from the last click
        "state" => {
            return Ok(serde_json::json!({
                "maximized": win.is_maximized().unwrap_or(false)
            }));
        }
        "close" => {
            let _ = win.close();
        }
        "new" => {
            // a second window on the same host — its session_events attach
            // claims a client keyed by this window's label (per-window tab).
            // `windows()`, not `webview_windows()`: a dock guest would hide
            // this window from the latter and reuse the same "win-N" label
            let n = app.windows().len() + 1;
            let label = format!("win-{n}");
            let _ = tauri::WebviewWindowBuilder::new(
                &app,
                label,
                tauri::WebviewUrl::External("http://sunmao.localhost/".parse().expect("gui url")),
            )
            .title("sunmao")
            .decorations(false)
            .disable_drag_drop_handler()
            .initialization_script(SHELL_SHIM)
            .build();
        }
        _ => {}
    }
    Ok(serde_json::Value::Null)
}

/// The titlebar is the drag region — a left-button press anywhere on it
/// (except interactive controls) starts a native window drag.
#[tauri::command]
pub(crate) fn shell_drag(win: tauri::Window) {
    let _ = win.start_dragging();
}

/// OS notification — the page fires this when a turn ends or an approval
/// lands while the window is unfocused (focus check stays page-side).
#[tauri::command]
pub(crate) fn shell_notify(app: tauri::AppHandle, title: &str, body: &str) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt as _;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| e.to_string())
}

/// Open an external http(s) URL in the system browser — dock browser tabs
/// use it for "open in external browser". Deliberately URLs only: local
/// FILE opening must go through `POST /session/{id}/open`, which validates
/// the path against what the viewed session actually surfaced — letting
/// the webview name any file path here would bypass that whole check.
#[tauri::command]
pub(crate) fn shell_open(app: tauri::AppHandle, path: &str) -> Result<(), String> {
    if !(path.starts_with("http://") || path.starts_with("https://")) {
        return Err(format!("shell_open is for http(s) urls only: {path}"));
    }
    use tauri_plugin_opener::OpenerExt as _;
    app.opener()
        .open_url(path, None::<&str>)
        .map_err(|e| format!("open {path}: {e}"))
}

/// Native folder picker — the 新对话 project popover's 浏览 row calls
/// this (WebView2 has no directory chooser the page can reach). `dir`
/// seeds the dialog at the viewed session's project; the resolved value
/// is the picked path or null on cancel.
#[tauri::command]
pub(crate) fn shell_pick_dir(win: tauri::Window, dir: Option<String>) -> Option<String> {
    use tauri_plugin_dialog::DialogExt as _;
    let mut d = win.dialog().file().set_title("选择项目目录");
    if let Some(p) = dir.filter(|p| !p.is_empty()) {
        d = d.set_directory(p);
    }
    d.blocking_pick_folder().map(|p| p.to_string())
}

/// Browser-style zoom ladder — ±20% steps, 20%..500% clamp.
fn zoom_step(cur: f64, dir: f64) -> f64 {
    (cur * dir).clamp(0.2, 5.0)
}

/// Page zoom driven by the Ctrl/Cmd+=/-/0 keydown listener in SHELL_SHIM.
/// Child webviews for the dock's browser tabs — real guest webviews
/// (WebView2 on Windows) composited over a pane rect, not iframes, so
/// X-Frame-Options can't refuse them (github.com etc. embed fine). The
/// page keeps them aligned by sending the pane's rect on every move;
/// `rect` parks them offscreen rather than tearing them down.
///
/// `async` is load-bearing, not stylistic: see the module note above —
/// the sync form deadlocks on Windows the moment it builds a guest.
/// Ops that read back (`ann-poll`) return their payload as JSON; the
/// fire-and-forget ops answer null.
#[tauri::command]
pub(crate) async fn shell_webview(
    win: tauri::Window,
    op: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let label = format!("br-{}", op["id"].as_i64().unwrap_or(0));
    let rect = |v: &serde_json::Value| -> (tauri::LogicalPosition<f64>, tauri::LogicalSize<f64>) {
        (
            tauri::LogicalPosition::new(
                v["x"].as_f64().unwrap_or(0.0),
                v["y"].as_f64().unwrap_or(0.0),
            ),
            tauri::LogicalSize::new(
                v["w"].as_f64().unwrap_or(1.0).max(1.0),
                v["h"].as_f64().unwrap_or(1.0).max(1.0),
            ),
        )
    };
    let find = || win.app_handle().get_webview(&label);
    let place = |w: &tauri::Webview, pos, size| {
        let _ = w.set_position(pos);
        let _ = w.set_size(size);
    };
    match op["op"].as_str().unwrap_or("") {
        "create" => {
            // an empty/un-normalized url ("", "github.com") failed Url::parse
            // and the `?` short-circuited before add_child — the pane then
            // sat empty forever; default to about:blank instead
            let url = op["url"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("about:blank")
                .parse::<tauri::Url>()
                .map_err(|e| e.to_string())?;
            let (pos, size) = rect(&op["rect"]);
            // create is idempotent: invokes are handled concurrently, so a
            // duplicate can arrive before the first build registers — an
            // existing entry is re-placed here, a losing add_child re-finds
            // the winner and re-places it instead of failing
            if let Some(w) = find() {
                place(&w, pos, size);
                let _ = w.navigate(url);
                return Ok(serde_json::Value::Null);
            }
            let built = win.add_child(
                tauri::webview::WebviewBuilder::new(
                    label.clone(),
                    tauri::WebviewUrl::External(url),
                ),
                pos,
                size,
            );
            if let Err(e) = built {
                match find() {
                    Some(w) => place(&w, pos, size),
                    None => {
                        tracing::error!("dock guest {label} could not be created: {e}");
                        return Err(e.to_string());
                    }
                }
            }
        }
        "rect" => {
            let (pos, size) = rect(&op["rect"]);
            if let Some(w) = find() {
                place(&w, pos, size);
            }
        }
        "nav" => {
            if let Some(w) = find()
                && let Ok(url) = op["url"]
                    .as_str()
                    .unwrap_or("about:blank")
                    .parse::<tauri::Url>()
            {
                let _ = w.navigate(url);
            }
        }
        "close" => {
            if let Some(w) = find() {
                let _ = w.close();
            }
        }
        // Element picker for this pane's guest. The page can't reach that
        // document (separate webview, foreign origin), so the picker ships
        // into the guest and reports through `window.__smAnn`; `ann-poll`
        // reads it back one-shot (`eval_with_callback` is the return path —
        // an eval that never returns would need a channel we don't want to
        // expose to a foreign page).
        "annotate" => {
            if let Some(w) = find() {
                // the picker runs in the *browsed page's* realm, which knows
                // nothing about our i18n. Evaluating the tables there twice
                // would redeclare their consts and kill the picker, so they
                // are wrapped in a function and cached on the guest's window;
                // the language is then pinned to the shell's (the site's own
                // navigator.language is not ours to trust). Both halves of the
                // dictionary go in: the picker's own copy lands in the panels
                // one.
                let lang = match op["lang"].as_str() {
                    Some("en") => "en",
                    _ => "zh",
                };
                let prelude = [
                    "window.__smI18n = window.__smI18n || (function () {\n",
                    sunmao::I18N_EN_PANELS_JS,
                    "\n",
                    sunmao::I18N_EN_JS,
                    "\n",
                    sunmao::I18N_JS,
                    "\nreturn { t: t, brand: brand, setLang: function (l) { uiLang = l; } }; })();\n",
                    "window.__smI18n.setLang(\"",
                    lang,
                    "\"); window.t = window.__smI18n.t; window.brand = window.__smI18n.brand;",
                ]
                .concat();
                let _ = w.eval(&prelude);
                let _ = w.eval(sunmao::ANNOTATE_JS);
                // the gesture is the guest's from here on — Esc must reach it
                // without a click in the page first
                let _ = w.set_focus();
            }
        }
        "ann-poll" => {
            let Some(w) = find() else {
                return Ok(serde_json::Value::Null);
            };
            let (tx, rx) = std::sync::mpsc::channel();
            w.eval_with_callback(
                "(function(){const v=window.__smAnn||null;window.__smAnn=null;return v})()",
                move |s| {
                    let _ = tx.send(s);
                },
            )
            .map_err(|e| e.to_string())?;
            let payload = rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .unwrap_or_default();
            return Ok(serde_json::Value::String(payload));
        }
        _ => {}
    }
    Ok(serde_json::Value::Null)
}

/// Native webview hotkeys stay disabled so this map stays the single
/// source of truth for the factor. `op` is a ladder step ("in"/"out"/
/// "reset") or an absolute factor (the page's `setZoom` + the ui.json
/// restore send numbers — JS numbers serialize into `op` verbatim).
/// Takes the calling `Webview` (not the window): `set_zoom` is webview
/// API, and a `WebviewWindow` arg stops resolving once a dock guest
/// exists. The zoom map stays keyed by window label so a guest webview
/// shares its host window's factor.
#[tauri::command]
pub(crate) fn shell_zoom(win: tauri::Webview, state: tauri::State<'_, Gui>, op: serde_json::Value) {
    let label = win.window().label().to_string();
    let next = match op.as_str() {
        Some(step) => {
            let zooms = state.zooms.lock().expect("zoom map");
            let cur = *zooms.get(&label).unwrap_or(&1.0);
            match step {
                "in" => zoom_step(cur, 1.2),
                "out" => zoom_step(cur, 1.0 / 1.2),
                "reset" => 1.0,
                _ => return,
            }
        }
        None => match op.as_f64() {
            Some(f) => f.clamp(0.2, 5.0),
            None => return,
        },
    };
    if win.set_zoom(next).is_ok() {
        state.zooms.lock().expect("zoom map").insert(label, next);
    }
}
