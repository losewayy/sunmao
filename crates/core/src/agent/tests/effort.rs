use super::*;
use crate::context::{MutexRecover, RwLockRecover};

/// A provider that captures the `reasoning_effort` each request carried —
/// proves the session override actually reaches the wire.
struct EffortSpy {
    seen: std::sync::Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for EffortSpy {
    async fn stream(&self, req: ChatRequest<'_>) -> anyhow::Result<DeltaStream> {
        self.seen
            .lock_or_recover()
            .push(req.reasoning_effort.map(String::from));
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamDelta::Content("ok".into())),
            Ok(StreamDelta::Finish {
                reason: Some("stop".into()),
                usage: None,
            }),
        ])))
    }
}

#[tokio::test]
async fn effort_reaches_the_request_and_persists() {
    let spy = Arc::new(EffortSpy {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let ctx = Arc::new(Context::new(
        spy.clone(),
        SessionLog::ephemeral(),
        builtin_registry(),
        std::env::temp_dir(),
    ));
    let agent = AgentLoop::new(ctx.clone());

    // unset → no key on the wire
    agent.run_turn("hi", &NullObserver).await.unwrap();
    agent
        .set_reasoning_effort(Some("high"), &NullObserver)
        .await;
    agent.run_turn("again", &NullObserver).await.unwrap();
    // "default" clears — back to no key
    agent
        .set_reasoning_effort(Some("default"), &NullObserver)
        .await;
    agent.run_turn("once more", &NullObserver).await.unwrap();

    assert_eq!(
        *spy.seen.lock_or_recover(),
        vec![None, Some("high".into()), None]
    );
    // durable fact: the effort change is an audit row, not a memory toggle
    let events = ctx.sessions.lock().await.events().await.unwrap_or_default();
    let changes: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Hook { event, detail } if event == "effort.change" => {
                Some(detail.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(changes, vec!["high", "default"]);
    assert_eq!(agent.reasoning_effort(), None);
}

#[tokio::test]
async fn swap_session_reseeds_effort() {
    // a file-backed log with an effort.change fact seeds the override;
    // swapping to a log without one clears it — the abandoned session's
    // level must not follow the next prompt.
    let dir = crate::fresh_test_dir("effort");
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let path = dir.join("sessions/s-eff.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"started\",\"model\":\"m\",\"cwd\":\"\"}\n{\"type\":\"hook\",\"event\":\"effort.change\",\"detail\":\"low\"}\n",
    )
    .unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(EffortSpy {
            seen: std::sync::Mutex::new(Vec::new()),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    ));
    let agent = AgentLoop::new(ctx.clone());
    agent
        .set_reasoning_effort(Some("xhigh"), &NullObserver)
        .await;

    let log = SessionLog::open_path(&path).await.unwrap();
    agent.swap_session(log).await;
    assert_eq!(agent.reasoning_effort().as_deref(), Some("low"));

    let none_path = dir.join("sessions/s-none.jsonl");
    std::fs::create_dir_all(none_path.parent().unwrap()).unwrap();
    std::fs::write(&none_path, "").unwrap(); // an existing-but-empty log
    let empty = SessionLog::open_path(&none_path).await.unwrap();
    agent.swap_session(empty).await;
    assert_eq!(agent.reasoning_effort(), None);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn seed_effort_reads_a_persisted_log() {
    // `Context::new` on a log carrying effort.change seeds the override —
    // a `--resume` session starts on the level it ended with.
    let dir = crate::fresh_test_dir("effort-seed");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s-seed.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"started\",\"model\":\"m\",\"cwd\":\"\"}\n{\"type\":\"hook\",\"event\":\"effort.change\",\"detail\":\"medium\"}\n{\"type\":\"hook\",\"event\":\"effort.change\",\"detail\":\"default\"}\n",
    )
    .unwrap();
    let log = SessionLog::open_path(&path).await.unwrap();
    let ctx = Context::new(
        Arc::new(EffortSpy {
            seen: std::sync::Mutex::new(Vec::new()),
        }),
        log,
        builtin_registry(),
        dir.clone(),
    );
    // last fact wins — "default" clears
    assert_eq!(ctx.reasoning_effort.read_or_recover().clone(), None);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn effort_levels_read_the_catalog() {
    // the picker rows come from the catalog: declared `thinking` wins, a
    // bare `reasoning` flag falls back to the canonical trio, a model
    // with neither offers nothing.
    let dir = crate::fresh_test_dir("effort-levels");
    std::fs::create_dir_all(&dir).unwrap();
    let mut ctx_raw = Context::new(
        Arc::new(MockProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        SessionLog::ephemeral(),
        builtin_registry(),
        dir.clone(),
    );
    let resolver = crate::models::ModelResolver::load(
        &dir,
        crate::models::ProviderDef {
            base_url: "http://unused".into(),
            api_key_env: None,
            api_key: None,
            dialect: "openai".into(),
            catalog: vec![
                crate::models::CatalogEntry {
                    id: "m-levels".into(),
                    vision: false,
                    context_length: None,
                    thinking: vec!["low".into(), "high".into()],
                    reasoning: false,
                },
                crate::models::CatalogEntry {
                    id: "m-hint".into(),
                    vision: false,
                    context_length: None,
                    thinking: vec![],
                    reasoning: true,
                },
                crate::models::CatalogEntry {
                    id: "m-plain".into(),
                    vision: false,
                    context_length: None,
                    thinking: vec![],
                    reasoning: false,
                },
            ],
        },
        "default",
    );
    assert_eq!(resolver.thinking_levels("m-levels"), vec!["low", "high"]);
    assert_eq!(
        resolver.thinking_levels("m-hint"),
        vec!["low", "medium", "high"]
    );
    assert!(resolver.thinking_levels("m-plain").is_empty());
    assert!(resolver.thinking_levels("ghost").is_empty());
    ctx_raw.models = Some(std::sync::Arc::new(resolver));
    let ctx = std::sync::Arc::new(ctx_raw);
    {
        let mut log = ctx.sessions.lock().await;
        let _ = log
            .append(&SessionEvent::Started {
                model: "m-hint".into(),
                cwd: dir.display().to_string(),
                driver: None,
            })
            .await;
    }
    let agent = AgentLoop::new(ctx.clone());
    // baseline model (no /model swap): levels read off Started.model
    assert_eq!(agent.effort_levels().await, vec!["low", "medium", "high"]);
    *ctx.active_selector.write_or_recover() = Some("default/m-levels".into());
    assert_eq!(agent.effort_levels().await, vec!["low", "high"]);
    std::fs::remove_dir_all(&dir).ok();
}
