use super::*;

/// A slow tab that lets the live bus lag used to die at the first
/// `Lagged` — every later frame (busy, approvals, live rows) never
/// reached it until reconnect. `Lagged` is recoverable: skip the gap.
#[tokio::test]
async fn lagged_bus_survives() {
    let (live, _) =
        tokio::sync::broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(4);
    let live = crate::serve::host::LiveBus(live);
    let (mgmt, _rx) = mpsc::unbounded_channel();
    let s = Arc::new(Shared {
        cwd: std::env::temp_dir(),
        roots: Vec::new(),
        live,
        sessions: std::sync::Mutex::new(Default::default()),
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
    });
    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let _client = Client::connect(s.clone(), out).await;
    // the forwarder hasn't polled yet (this test runtime yields only
    // when we await) — the burst wraps the ring → Lagged on next recv
    for _ in 0..8 {
        let _ = s.live.send(serde_json::json!({"type":"noise"}));
    }
    // drain the hello + lagged gap; the marker must still arrive
    let _ = s.live.send(serde_json::json!({"type":"marker"}));
    let mut seen = false;
    for _ in 0..16 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            Ok(Some(v)) if v.contains("marker") => {
                seen = true;
                break;
            }
            Ok(Some(_)) => continue,
            _ => break,
        }
    }
    assert!(seen, "the forwarder must survive a Lagged gap");
}

/// Reconnect used to take `live_ids().next()` — arbitrary HashMap
/// order, so a reload could land on any session. The default view is
/// the newest *adopted* host (a resumed old log counts as new).
#[tokio::test]
async fn connect_views_the_newest_adopted_session() {
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
    let dir = std::env::temp_dir().join(format!("sunmao-viewing-{}", std::process::id()));
    let sdir = dir.join("sess");
    std::fs::create_dir_all(&sdir).unwrap();
    // a real factory — adopt drives factory.build → Context → AgentLoop
    let s = {
        let (live, _) =
            tokio::sync::broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(8);
        let live = crate::serve::host::LiveBus(live);
        let (mgmt, _rx) = mpsc::unbounded_channel();
        Arc::new(Shared {
            cwd: dir.clone(),
            roots: Vec::new(),
            live,
            sessions: std::sync::Mutex::new(Default::default()),
            factory: crate::serve::SessionFactory {
                make: Box::new(|log, _approver, cwd| {
                    Box::pin(async move {
                        let mut context = sunmao_core::Context::new(
                            Arc::new(StubLlm),
                            log,
                            sunmao_core::tool::builtin_registry(),
                            cwd.clone(),
                        );
                        context.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
                            &cwd,
                            sunmao_core::models::ProviderDef {
                                base_url: "http://unused/v1".into(),
                                dialect: "openai".into(),
                                ..Default::default()
                            },
                            "default",
                        )));
                        Ok(context)
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
        })
    };
    // adopt order ≠ lexical order on purpose — "z" adopted first
    let log = sunmao_core::SessionLog::open(&sdir, "s-z").await.unwrap();
    s.adopt(log, "resume").await.unwrap();
    let log = sunmao_core::SessionLog::open(&sdir, "s-a").await.unwrap();
    s.adopt(log, "resume").await.unwrap();

    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let client = Client::connect(s.clone(), out).await;
    let hello: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["session"], "s-a", "viewing = newest adopted host");
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The turn mode (`standard` | `fusion`) is a per-session axis the page has
/// to draw: the settings switch, the composer chip and the model picker's
/// Lead/Sidekick note all read it. It rides the same frames the approval
/// stance does (`hello` + `replay`) — a frontend that can only fold
/// `turn_mode_change` out of the transcript is guessing on every reconnect.
#[tokio::test]
async fn hello_and_replay_carry_the_turn_mode() {
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
    let dir = std::env::temp_dir().join(format!("sunmao-turn-mode-{}", std::process::id()));
    let sdir = dir.join("sess");
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::create_dir_all(dir.join(".sunmao")).unwrap();
    std::fs::write(
        dir.join(".sunmao/models.json"),
        r#"{"providers":{"default":{"base_url":"http://unused/v1","catalog":[{"id":"lead"},{"id":"sidekick"}]}}}"#,
    )
    .unwrap();
    let s = {
        let (live, _) =
            tokio::sync::broadcast::channel::<std::sync::Arc<crate::serve::host::LiveFrame>>(8);
        let live = crate::serve::host::LiveBus(live);
        let (mgmt, _rx) = mpsc::unbounded_channel();
        Arc::new(Shared {
            cwd: dir.clone(),
            roots: Vec::new(),
            live,
            sessions: std::sync::Mutex::new(Default::default()),
            factory: crate::serve::SessionFactory {
                make: Box::new(|log, _approver, cwd| {
                    Box::pin(async move {
                        let mut context = sunmao_core::Context::new(
                            Arc::new(StubLlm),
                            log,
                            sunmao_core::tool::builtin_registry(),
                            cwd.clone(),
                        );
                        context.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
                            &cwd,
                            sunmao_core::models::ProviderDef {
                                base_url: "http://unused/v1".into(),
                                dialect: "openai".into(),
                                ..Default::default()
                            },
                            "default",
                        )));
                        Ok(context)
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
        })
    };
    let log = sunmao_core::SessionLog::open(&sdir, "s-fuse")
        .await
        .unwrap();
    s.adopt(log, "resume").await.unwrap();
    let host = s.host("s-fuse").unwrap();
    host.agent
        .set_fusion_model(
            sunmao_core::context::FusionModelRole::Lead,
            Some("default/lead".into()),
        )
        .await
        .unwrap();
    host.agent
        .set_fusion_model(
            sunmao_core::context::FusionModelRole::Sidekick,
            Some("default/sidekick".into()),
        )
        .await
        .unwrap();

    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let mut client = Client::connect(s.clone(), out).await;
    let hello: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(
        hello["turn_mode"], "standard",
        "a fresh session is standard"
    );

    // the op the composer chip and the settings switch both emit. The
    // frontend never moves its own state off this: the kernel answer is what
    // the next frame carries, so both directions have to land.
    client
        .handle(serde_json::json!({"type":"mode","sel":"fusion"}))
        .await;
    assert_eq!(
        host.agent.turn_mode(),
        sunmao_core::agent::TurnMode::Fusion,
        "the mode op flips the kernel's turn shape"
    );
    client
        .handle(serde_json::json!({"type":"mode","sel":"standard"}))
        .await;
    assert_eq!(
        host.agent.turn_mode(),
        sunmao_core::agent::TurnMode::Standard,
        "and back"
    );
    client
        .handle(serde_json::json!({"type":"mode","sel":"fusion"}))
        .await;

    // exactly what the frontend's switch sends — the frames have to follow
    client.send_replay(&host).await;
    let replay = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
            // mode and effort changes can both emit frames before replay
            Ok(Some(v)) => {
                let parsed: serde_json::Value = serde_json::from_str(&v).unwrap();
                if parsed["type"] == "replay" {
                    break parsed;
                }
            }
            _ => panic!("replay frame should arrive after queued mode changes"),
        }
    };
    assert_eq!(
        replay["turn_mode"], "fusion",
        "the replay carries the mode it resumed at"
    );
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}
