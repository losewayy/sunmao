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
        approval_ids: Arc::new(AtomicU64::new(0)),
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
