use super::*;
use std::sync::atomic::AtomicU64;
use sunmao_core::context::RwLockRecover as _;

/// A Shared whose factory never runs — `log_path`/`session_dirs` are
/// pure filesystem lookups, so the make-closure just errors if called.
fn shared_at(cwd: std::path::PathBuf) -> Shared {
    let (tx, _) = broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(8);
    let live = crate::serve::host::LiveBus(tx);
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
        model_override: None,
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
    let (tx, _) = broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(8);
    let live = crate::serve::host::LiveBus(tx);
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
        model_override: None,
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

// ── the new-session model pick (`default_model`) ──

/// Adapter stub — resolution only needs *an* adapter to hand the context.
struct ModelStub;

#[async_trait::async_trait]
impl sunmao_llm::ProviderAdapter for ModelStub {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

/// A Shared whose factory builds a real Context over a resolver seeded from
/// the project's `models.json`: the model pick is then observable twice — in
/// the log (`Started`) and on the live context (`active_selector`, which is
/// what a turn's adapter follows). `stubs` names the selectors that resolve.
fn shared_with_models(
    cwd: &std::path::Path,
    model_label: &str,
    model_override: Option<&str>,
    stubs: &[&str],
) -> Arc<Shared> {
    let (tx, _) = broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(8);
    let live = crate::serve::host::LiveBus(tx);
    let (mgmt, _rx) = mpsc::unbounded_channel::<SessionOp>();
    let stubs: Vec<String> = stubs.iter().map(|s| s.to_string()).collect();
    let cwd = cwd.to_path_buf();
    Arc::new(Shared {
        cwd: cwd.clone(),
        roots: Vec::new(),
        live,
        sessions: Mutex::new(HashMap::new()),
        factory: crate::serve::SessionFactory {
            make: Box::new(move |log, _approver, session_cwd| {
                let stubs = stubs.clone();
                Box::pin(async move {
                    let mut ctx = sunmao_core::Context::new(
                        Arc::new(ModelStub),
                        log,
                        sunmao_core::tool::builtin_registry(),
                        session_cwd.clone(),
                    );
                    let mut r = sunmao_core::models::ModelResolver::load(
                        &session_cwd,
                        sunmao_core::models::ProviderDef {
                            base_url: "http://local/v1".into(),
                            api_key_env: None,
                            api_key: None,
                            dialect: "openai".into(),
                            catalog: Vec::new(),
                            extra: Default::default(),
                        },
                        "default",
                    );
                    for sel in &stubs {
                        r = r.with_adapter(sel, Arc::new(ModelStub));
                    }
                    ctx.models = Some(Arc::new(r));
                    Ok(ctx)
                })
            }),
        },
        model_label: model_label.into(),
        model_override: model_override.map(|s| s.to_string()),
        sandbox_port: 0,
        prompt_override: None,
        driver_override: None,
        pending_drivers: std::sync::Mutex::new(Default::default()),
        approval_ids: Arc::new(AtomicU64::new(0)),
        mgmt,
        adopt_lock: tokio::sync::Mutex::new(()),
        adopt_seq: AtomicU64::new(0),
    })
}

fn project(tag: &str, models_json: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sunmao-defmodel-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(dir.join(".sunmao/models.json"), models_json).unwrap();
    dir
}

/// `new_session` returns the id; read it back out of the adopt reply.
async fn start(s: &Arc<Shared>, cwd: &std::path::Path) -> Arc<Host> {
    let v = new_session(s, Some(cwd.to_path_buf()), None).await.unwrap();
    let id = v["session"].as_str().unwrap().to_string();
    s.host(&id).expect("the new session is live")
}

async fn started_models(host: &Host) -> Vec<String> {
    host.agent
        .session_events()
        .await
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Started { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect()
}

/// A pinned default is what a fresh session runs on, not just what its log
/// says: `Started` names the model, the swap is what binds the adapter.
#[tokio::test]
async fn new_session_runs_on_the_pinned_default() {
    let dir = project("pick", r#"{"default_model":"p/m1"}"#);
    let s = shared_with_models(&dir, "launch-model", None, &["p/m1"]);
    let host = start(&s, &dir).await;
    assert_eq!(started_models(&host).await, vec!["p/m1"]);
    assert_eq!(
        host.agent
            .context()
            .active_selector
            .read_or_recover()
            .clone(),
        Some("p/m1".to_string()),
        "the session must run on the pinned model, not fall back to the launch one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Precedence, both halves: with no `default_model` the launch model is used
/// exactly as before, and a `--model` the user passed outranks the pin.
#[tokio::test]
async fn launch_model_fills_the_gap_and_an_explicit_flag_wins() {
    let dir = project("prec", r#"{"providers":{}}"#);
    let s = shared_with_models(&dir, "launch-model", None, &[]);
    let host = start(&s, &dir).await;
    assert_eq!(started_models(&host).await, vec!["launch-model"]);
    assert!(
        host.agent
            .context()
            .active_selector
            .read_or_recover()
            .is_none(),
        "nothing pinned = the baseline adapter, unchanged"
    );

    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"default_model":"p/m1"}"#,
    )
    .unwrap();
    let s = shared_with_models(&dir, "launch-model", Some("launch-model"), &["p/m1"]);
    let host = start(&s, &dir).await;
    assert_eq!(
        started_models(&host).await,
        vec!["launch-model"],
        "an explicitly passed model outranks the project default"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session already under way keeps the model in its own log: rewriting
/// `default_model` retargets the NEXT session, never this one — its log is
/// not touched at all.
#[tokio::test]
async fn changing_the_default_leaves_a_running_session_alone() {
    let dir = project("old", r#"{"default_model":"p/m1"}"#);
    let s = shared_with_models(&dir, "launch-model", None, &["p/m1", "p/m2"]);
    let first = start(&s, &dir).await;
    let log = first.agent.session_path().await;
    let before = std::fs::read_to_string(&log).unwrap();

    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"default_model":"p/m2"}"#,
    )
    .unwrap();
    let second = start(&s, &dir).await;

    assert_eq!(started_models(&second).await, vec!["p/m2"]);
    assert_eq!(
        started_models(&first).await,
        vec!["p/m1"],
        "the older session keeps its own model"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().lines().count(),
        before.lines().count(),
        "adopting it again must not append a second Started"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
