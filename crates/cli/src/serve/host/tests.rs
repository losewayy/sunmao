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
        driver_override: None,
        pending_drivers: std::sync::Mutex::new(Default::default()),
        approval_ids: Arc::new(AtomicU64::new(0)),
        mgmt,
        adopt_lock: tokio::sync::Mutex::new(()),
        adopt_seq: AtomicU64::new(0),
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

/// Two concurrent adopts of the same session id must yield ONE host —
/// `factory.build().await` opened a check-then-insert window where both
/// callers built their own Context and held their own writer to one log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_adopt_of_one_session_yields_one_host() {
    struct StubLlm;
    #[async_trait::async_trait]
    impl sunmao_llm::ProviderAdapter for StubLlm {
        async fn stream(
            &self,
            _req: sunmao_llm::ChatRequest<'_>,
        ) -> anyhow::Result<sunmao_llm::DeltaStream> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }
    let dir = std::env::temp_dir().join(format!("sunmao-adopt-{}", std::process::id()));
    let sdir = dir.join("sess");
    std::fs::create_dir_all(&sdir).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (live, _) = broadcast::channel::<serde_json::Value>(8);
    let (mgmt, _rx) = mpsc::unbounded_channel::<SessionOp>();
    let calls2 = calls.clone();
    let s = Arc::new(Shared {
        cwd: dir.clone(),
        roots: Vec::new(),
        live,
        sessions: Mutex::new(HashMap::new()),
        factory: crate::serve::SessionFactory {
            make: Box::new(move |log, _approver, cwd| {
                let calls = calls2.clone();
                Box::pin(async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // the window the race exploits — MCP connects sit here
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Ok(sunmao_core::Context::new(
                        Arc::new(StubLlm),
                        log,
                        sunmao_core::tool::builtin_registry(),
                        cwd,
                    ))
                })
            }),
        },
        model_label: String::new(),
        sandbox_port: 0,
        prompt_override: None,
        driver_override: None,
        pending_drivers: std::sync::Mutex::new(Default::default()),
        approval_ids: Arc::new(AtomicU64::new(0)),
        mgmt,
        adopt_lock: tokio::sync::Mutex::new(()),
        adopt_seq: AtomicU64::new(0),
    });
    let log_a = SessionLog::open(&sdir, "sx").await.unwrap();
    let log_b = SessionLog::open(&sdir, "sx").await.unwrap();
    let (a, b) = tokio::join!(s.adopt(log_a, "resume"), s.adopt(log_b, "resume"));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(Arc::ptr_eq(&a, &b), "one session id = one host");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the second adopter must find the first's host, not rebuild"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
