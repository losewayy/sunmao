use super::*;

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

/// `session/cancel` must drive the real cancel path: the flag alone
/// never woke a waiter — `notify_waiters` only reaches a Notified that
/// registered *before* it, which is every in-flight select the turn
/// loop armed. A flag-only handler left those parked until the turn
/// ended on its own.
#[tokio::test]
async fn cancel_session_sets_the_flag_and_wakes_registered_waiters() {
    let agent = SunmaoAgent {
        sessions: Mutex::new(HashMap::new()),
        base_url: String::new(),
        api_key: String::new(),
        model: String::new(),
        provider: String::new(),
        preset_names: Vec::new(),
    };
    let cwd = std::env::temp_dir().join(format!("sunmao-acp-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let ctx = Arc::new(Context::new(
        Arc::new(StubLlm),
        sunmao_core::SessionLog::ephemeral(),
        sunmao_core::tool::builtin_registry(),
        cwd.clone(),
    ));
    agent.sessions.lock_or_recover().insert(
        "s1".to_string(),
        Arc::new(Mutex::new(SessionState {
            agent: AgentLoop::new(ctx.clone()),
            ctx: ctx.clone(),
            msg_ids: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })),
    );

    // arm the wake first — enable() registers the waiter so the
    // notification is never lost in the enable gap
    let notified = ctx.cancel_notify.notified();
    let mut notified = std::pin::pin!(notified);
    notified.as_mut().enable();

    agent.cancel_session("s1");

    assert!(ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    tokio::time::timeout(std::time::Duration::from_secs(2), notified)
        .await
        .expect("cancel must wake an in-flight waiter, not just set a flag");

    // unknown id is a no-op, not a panic or a hang
    agent.cancel_session("nope");
    let _ = std::fs::remove_dir_all(&cwd);
}
