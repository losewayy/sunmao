use super::*;
use std::sync::atomic::AtomicU64;

/// A Shared whose factory never runs — `log_path`/`session_dirs` are
/// pure filesystem lookups, so the make-closure just errors if called.
fn shared_at(cwd: std::path::PathBuf) -> Shared {
    let (live, _) = broadcast::channel::<serde_json::Value>(8);
    let (mgmt, _rx) = mpsc::unbounded_channel::<SessionOp>();
    Shared {
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

#[test]
fn log_path_rejects_escape_to_arbitrary_files() {
    let root = std::env::temp_dir().join(format!("sunmao-logpath-{}", std::process::id()));
    let sessions = root.join(".sunmao/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // a real file OUTSIDE the sessions domain — the escape target
    let outside = root.join("victim.jsonl");
    std::fs::write(&outside, "{\"type\":\"started\"}\n").unwrap();
    // and a real log INSIDE it
    let inside = sessions.join("s-1.jsonl");
    std::fs::write(&inside, "{\"type\":\"started\"}\n").unwrap();
    let s = shared_at(root.clone());

    // the hole: an absolute path used to pass the p.exists() check
    assert_eq!(log_path(&s, outside.to_str().unwrap()), None);
    // relative traversal must not escape either
    assert_eq!(log_path(&s, "../../victim"), None);
    assert_eq!(log_path(&s, "..\\..\\victim"), None);
    // a real log path inside the domain still resolves
    assert_eq!(
        log_path(&s, inside.to_str().unwrap()),
        Some(inside.canonicalize().unwrap())
    );
    // bare ids keep working
    assert_eq!(log_path(&s, "s-1"), Some(inside.clone()));
    assert_eq!(log_path(&s, "s-nope"), None);
    let _ = std::fs::remove_dir_all(&root);
}
