use super::session_meta;
use crate::serve::host::display_path;

#[test]
fn title_is_first_typed_prompt() {
    let dir = std::env::temp_dir().join(format!("sunmao-meta-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("s-1.jsonl");
    let log = [
        r#"{"type":"started","model":"m","cwd":"x"}"#,
        r#"{"type":"message","message":{"role":"system","content":"identity"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"[hook context] injected"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"\n  fix the drag bug  \nsecond line"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"later prompt"}}"#,
    ];
    std::fs::write(&p, log.join("\n")).unwrap();
    let m = session_meta(&p);
    assert_eq!(m["title"], "fix the drag bug");
    assert!(m["mtime"].as_u64().is_some());

    std::fs::write(&p, log[..3].join("\n")).unwrap();
    assert!(session_meta(&p)["title"].is_null());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The LAST `session_meta` event overrides first-prompt derivation — a
/// rename survives every later read, and still works on a log with no
/// prompt at all.
#[test]
fn session_meta_event_overrides_prompt_title() {
    let dir = std::env::temp_dir().join(format!("sunmao-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("s-2.jsonl");
    let log = [
        r#"{"type":"started","model":"m","cwd":"x"}"#,
        r#"{"type":"message","message":{"role":"user","content":"original prompt"}}"#,
        r#"{"type":"session_meta","title":"first rename"}"#,
        r#"{"type":"session_meta","title":"latest rename"}"#,
    ];
    std::fs::write(&p, log.join("\n")).unwrap();
    assert_eq!(session_meta(&p)["title"], "latest rename");

    std::fs::write(&p, [log[0], log[2]].join("\n")).unwrap();
    assert_eq!(session_meta(&p)["title"], "first rename");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn display_path_strips_verbatim_prefix() {
    let p = |s: &str| display_path(std::path::Path::new(s));
    assert_eq!(p(r"\\?\C:\work\x"), r"C:\work\x");
    assert_eq!(p(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
    assert_eq!(p("/home/u/x"), "/home/u/x");
}

/// A `Shared` whose factory never runs — rename/delete/search are pure
/// filesystem operations on dormant logs, so the make-closure just errors
/// if called.
fn shared_at(cwd: std::path::PathBuf) -> super::super::host::Shared {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use tokio::sync::{broadcast, mpsc};
    let (live, _) = broadcast::channel::<serde_json::Value>(8);
    let (mgmt, _rx) = mpsc::unbounded_channel();
    super::super::host::Shared {
        cwd,
        roots: Vec::new(),
        live,
        sessions: Mutex::new(HashMap::new()),
        factory: crate::serve::SessionFactory {
            make: Box::new(|_, _, _| Box::pin(async { anyhow::bail!("test factory") })),
        },
        model_label: String::new(),
        sandbox_port: 0,
        prompt_override: None,
        driver_override: None,
        approval_ids: Arc::new(AtomicU64::new(0)),
        adopt_lock: tokio::sync::Mutex::new(()),
        adopt_seq: AtomicU64::new(0),
        mgmt,
    }
}

#[tokio::test]
async fn rename_appends_meta_and_delete_removes_log() {
    let root = std::env::temp_dir().join(format!("sunmao-rest-{}", std::process::id()));
    let dir = root.join(".sunmao/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("s-9.jsonl");
    std::fs::write(
        &log,
        concat!(
            "{\"type\":\"started\",\"model\":\"m\",\"cwd\":\"x\"}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hello world\"}}\n",
        ),
    )
    .unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let h = super::HostHandle { s };

    // rename validates + appends the durable fact
    let bad = h
        .request("POST", "/session/s-9/rename", b"{\"title\":\"  \"}")
        .await;
    assert_eq!(bad.status, 400);
    let ok = h
        .request("POST", "/session/s-9/rename", b"{\"title\":\"my chat\"}")
        .await;
    assert_eq!(ok.status, 200);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains(r#""type":"session_meta""#));
    assert_eq!(session_meta(&log)["title"], "my chat");
    let list = h.request("GET", "/sessions", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&list.body).unwrap();
    assert_eq!(v["meta"]["s-9"]["title"], "my chat");

    // the export feed: raw events for dormant logs
    let evs = h.request("GET", "/session/s-9/events", b"").await;
    assert_eq!(evs.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&evs.body).unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 3);
    assert_eq!(
        h.request("GET", "/session/s-nope/events", b"").await.status,
        404
    );

    // delete removes the log; a second delete 404s; unknown ids 404 both ways
    assert_eq!(h.request("DELETE", "/session/s-9", b"").await.status, 200);
    assert!(!log.exists());
    assert_eq!(h.request("DELETE", "/session/s-9", b"").await.status, 404);
    assert_eq!(
        h.request("POST", "/session/s-nope/rename", b"{\"title\":\"x\"}")
            .await
            .status,
        404
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn sessions_search_greps_message_content() {
    let root = std::env::temp_dir().join(format!("sunmao-search-{}", std::process::id()));
    let dir = root.join(".sunmao/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s-hit.jsonl"),
        concat!(
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"fix the drag bug\"}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"drag fixed\"}}\n",
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("s-miss.jsonl"),
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"unrelated\"}}\n",
    )
    .unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let h = super::HostHandle { s };

    let r = h.request("GET", "/sessions?q=drag", b"").await;
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let rows = v["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "s-hit");
    assert_eq!(rows[0]["title"], "fix the drag bug");
    assert!(!rows[0]["hits"].as_array().unwrap().is_empty());

    let none = h.request("GET", "/sessions?q=nosuchword", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&none.body).unwrap();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 0);

    // no query → the rail list shape, unchanged
    let list = h.request("GET", "/sessions", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&list.body).unwrap();
    assert!(v["meta"].is_object());
    let _ = std::fs::remove_dir_all(&root);
}

/// `GET /tasks` — no live host means an empty roster, not an error (the
/// roster is in-memory; dormant sessions have nothing to report).
#[tokio::test]
async fn tasks_route_reports_empty_roster_without_host() {
    let root = std::env::temp_dir().join(format!("sunmao-tasks-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".sunmao/sessions")).unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let h = super::HostHandle { s };

    for path in ["/tasks", "/tasks?sess=s-nope"] {
        let r = h.request("GET", path, b"").await;
        assert_eq!(r.status, 200, "{path}");
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["tasks"].as_array().unwrap().len(), 0, "{path}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// `GET /jobs` + `/jobs/{id}/output` — the filesystem IS the state: a dir
/// with only output.log reads running, exit.json flips it done with the
/// code surfaced verbatim; the output endpoint streams byte-offset chunks.
#[tokio::test]
async fn jobs_route_reads_jobs_dir_layout() {
    let root = std::env::temp_dir().join(format!("sunmao-jobs-{}", std::process::id()));
    let jobs = root.join(".sunmao/jobs");
    std::fs::create_dir_all(jobs.join("j-run")).unwrap();
    std::fs::write(jobs.join("j-run/output.log"), b"line1\nstill going\n").unwrap();
    std::fs::create_dir_all(jobs.join("j-done")).unwrap();
    std::fs::write(jobs.join("j-done/output.log"), b"all done\n").unwrap();
    std::fs::write(jobs.join("j-done/exit.json"), b"{\"exit_code\":3}").unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let h = super::HostHandle { s };

    let r = h.request("GET", "/jobs", b"").await;
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    let rows = v["jobs"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let by_id = |id: &str| rows.iter().find(|j| j["id"] == id).unwrap();
    assert_eq!(by_id("j-run")["running"], true);
    assert!(by_id("j-run")["exit"].is_null());
    assert!(
        by_id("j-run")["preview"]
            .as_str()
            .unwrap()
            .contains("still going")
    );
    assert_eq!(by_id("j-done")["running"], false);
    assert_eq!(by_id("j-done")["exit"], 3);

    let out = h.request("GET", "/jobs/j-done/output", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["chunk"], "all done\n");
    assert_eq!(v["total"], 9);
    // offset continuation + bad ids
    let out = h.request("GET", "/jobs/j-done/output?offset=4", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&out.body).unwrap();
    assert_eq!(v["chunk"], "done\n");
    assert_eq!(h.request("GET", "/jobs/nope/output", b"").await.status, 404);
    let _ = std::fs::remove_dir_all(&root);
}

/// `GET|PUT /ui` — appearance state round-trips through
/// `<project>/.sunmao/ui.json`; a missing file answers `{}`, a non-object
/// body is rejected, and `PUT` emits `ui_changed` on the live bus.
#[tokio::test]
async fn ui_route_persists_appearance_object() {
    let root = std::env::temp_dir().join(format!("sunmao-ui-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".sunmao/sessions")).unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let mut rx = s.live.subscribe();
    let h = super::HostHandle { s };

    let empty = h.request("GET", "/ui", b"").await;
    let v: serde_json::Value = serde_json::from_slice(&empty.body).unwrap();
    assert_eq!(v["ui"], serde_json::json!({}));

    assert_eq!(h.request("PUT", "/ui", b"[1,2]").await.status, 400);
    let ok = h
        .request(
            "PUT",
            "/ui",
            br##"{"accent":"#339CFF","panelOpacity":0.8}"##,
        )
        .await;
    assert_eq!(ok.status, 200);
    let file = std::fs::read_to_string(root.join(".sunmao/ui.json")).unwrap();
    assert!(file.contains("panelOpacity"));
    let reread: serde_json::Value =
        serde_json::from_slice(&h.request("GET", "/ui", b"").await.body).unwrap();
    assert_eq!(reread["ui"]["panelOpacity"], 0.8);
    assert_eq!(
        rx.try_recv().unwrap()["type"].as_str().unwrap(),
        "ui_changed"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// `GET|PUT /shell` — writes `<project>/.sunmao/shell.txt` and reports the
/// kernel's resolution. `SUNMAO_SHELL` in the test env outranks the file,
/// so assertions stay on the file + a resolved backend either way.
#[tokio::test]
async fn shell_route_writes_project_pin() {
    let root = std::env::temp_dir().join(format!("sunmao-shell-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".sunmao/sessions")).unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let h = super::HostHandle { s };

    assert_eq!(
        h.request("PUT", "/shell", br#"{"backend":"fish"}"#)
            .await
            .status,
        400
    );
    let ok = h.request("PUT", "/shell", br#"{"backend":"posix"}"#).await;
    assert_eq!(ok.status, 200);
    assert_eq!(
        std::fs::read_to_string(root.join(".sunmao/shell.txt")).unwrap(),
        "posix\n"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&h.request("GET", "/shell", b"").await.body).unwrap();
    assert!(matches!(v["backend"].as_str().unwrap(), "pwsh" | "posix"));
    assert!(v["pwsh_on_path"].is_boolean());
    let _ = std::fs::remove_dir_all(&root);
}

/// `PUT|GET /wallpaper` — a data URL lands as `.sunmao/wallpapers/
/// custom.{ext}` and reads back with the sniffed Content-Type; junk data
/// URLs and non-whitelisted bytes are rejected, and a second upload rotates
/// the file out instead of accumulating.
#[tokio::test]
async fn wallpaper_route_roundtrips_and_rotates() {
    use base64::Engine;
    let root = std::env::temp_dir().join(format!("sunmao-wall-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".sunmao/sessions")).unwrap();
    let s = std::sync::Arc::new(shared_at(root.clone()));
    let mut rx = s.live.subscribe();
    let h = super::HostHandle { s };
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);

    // nothing stored yet
    assert_eq!(h.request("GET", "/wallpaper", b"").await.status, 404);

    // bad data URLs and non-image bytes get refused, nothing written
    assert_eq!(h.request("PUT", "/wallpaper", b"data:").await.status, 400);
    assert_eq!(
        h.request("PUT", "/wallpaper", b"data:image/jpeg;base64,!!!")
            .await
            .status,
        400
    );
    assert_eq!(
        h.request("PUT", "/wallpaper", b"data:text/plain;base64,aGVsbG8=")
            .await
            .status,
        400
    );
    let gif = format!("data:image/gif;base64,{}", b64(b"GIF89a\0\0\0\0"));
    assert_eq!(
        h.request("PUT", "/wallpaper", gif.as_bytes()).await.status,
        400
    );
    assert!(!root.join(".sunmao/wallpapers").exists());

    // png data URL round-trips; the stored extension drives the GET mime
    let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let put = h
        .request(
            "PUT",
            "/wallpaper",
            format!("data:image/png;base64,{}", b64(&png)).as_bytes(),
        )
        .await;
    assert_eq!(put.status, 200);
    assert!(root.join(".sunmao/wallpapers/custom.png").exists());
    assert_eq!(
        rx.try_recv().unwrap()["type"].as_str().unwrap(),
        "wallpaper_changed"
    );
    let got = h.request("GET", "/wallpaper", b"").await;
    assert_eq!(got.status, 200);
    assert_eq!(got.body, png);
    let ctype = got
        .headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(ctype, "image/png");

    // a jpeg upload (raw bytes, no data URL) rotates the png out — the dir
    // still holds exactly one file and GET serves the new bytes
    let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 9, 9];
    let put2 = h.request("PUT", "/wallpaper", &jpg).await;
    assert_eq!(put2.status, 200);
    assert!(!root.join(".sunmao/wallpapers/custom.png").exists());
    assert!(root.join(".sunmao/wallpapers/custom.jpg").exists());
    let got2 = h.request("GET", "/wallpaper", b"").await;
    assert_eq!(got2.body, jpg);
    assert_eq!(
        got2.headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.as_str())
            .unwrap_or(""),
        "image/jpeg"
    );
    let files: Vec<_> = std::fs::read_dir(root.join(".sunmao/wallpapers"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(files.len(), 1);

    // oversized upload is refused and the stored wallpaper survives
    let big = {
        let mut v = vec![0xFF, 0xD8, 0xFF];
        v.resize(super::ui::WALL_MAX + 1, 0u8);
        v
    };
    assert_eq!(h.request("PUT", "/wallpaper", &big).await.status, 413);
    assert_eq!(h.request("GET", "/wallpaper", b"").await.body, jpg);
    let _ = std::fs::remove_dir_all(&root);
}
