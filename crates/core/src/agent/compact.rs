use super::*;

use futures_util::StreamExt;
use sunmao_llm::types::Message;
use sunmao_llm::{ChatRequest, StreamDelta};

impl AgentLoop {
    /// Ask the model to summarize the transcript, then commit a `Compacted`
    /// boundary — the log fold turns it into a fresh system message.
    /// Returns the summary so frontends can show what the fold produced.
    /// Standalone calls (the `/compact` builtin) queue behind the turn
    /// fence; the in-turn auto-compaction uses `compact_inner`, which the
    /// held permit already covers (tokio Mutex isn't reentrant — locking
    /// here too would deadlock the loop).
    pub async fn compact(&self, observer: &dyn Observer, trigger: &str) -> anyhow::Result<String> {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        self.compact_inner(observer, trigger).await
    }

    pub(super) async fn compact_inner(
        &self,
        observer: &dyn Observer,
        trigger: &str,
    ) -> anyhow::Result<String> {
        // PreCompact may veto or annotate the compaction (the dialect's
        // snapshot hook point — context-mode hangs its state capture here).
        let pre = self
            .ctx
            .hooks
            .fire(
                HookEvent::PreCompact,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some(trigger),
                    ..Default::default()
                },
            )
            .await;
        if let Some(reason) = pre.block_reason {
            anyhow::bail!("compaction blocked by hook: {reason}");
        }
        let mut msgs = self.ctx.sessions.lock().await.messages().await?;
        if msgs.is_empty() {
            return Ok(String::new());
        }
        msgs.push(Message::user(
            crate::prompt::PromptAssembler::new(&self.ctx.cwd).assemble_compact(),
        ));
        let req = ChatRequest {
            messages: &msgs,
            tools: None,
            max_tokens: Some(2048),
            temperature: None,
            // compaction is an internal housekeeping request — it keeps the
            // provider default rather than spending the session's effort on
            // a summarization call
            reasoning_effort: None,
        };
        // Compaction is a full provider round trip with no tool call behind
        // it — the longest await in a turn that nothing else interrupts, so
        // a stop pressed during a summary used to wait out the model. It
        // answers the same cancel the rest of the loop does, and it never
        // commits a half summary: a `Compacted` boundary folds the whole
        // transcript into it, so a truncated one would erase the session.
        let llm = self.ctx.active_llm();
        let cancel = self.ctx.cancel_signal();
        let mut stream = tokio::select! {
            s = llm.stream(req) => s?,
            () = cancel.wait() => anyhow::bail!("cancelled by user"),
        };
        let mut summary = String::new();
        loop {
            let delta = tokio::select! {
                d = stream.next() => d,
                () = cancel.wait() => anyhow::bail!("cancelled by user"),
            };
            match delta {
                Some(d) => {
                    if let StreamDelta::Content(c) = d? {
                        summary.push_str(&c);
                    }
                }
                None => break,
            }
        }
        if summary.trim().is_empty() {
            anyhow::bail!("compaction produced empty summary");
        }
        self.ctx
            .sessions
            .lock()
            .await
            .append(&SessionEvent::Compacted {
                summary: summary.clone(),
            })
            .await?;
        crate::hooks::HookEngine::fire_detached(
            &self.ctx.hooks,
            HookEvent::PostCompact,
            &self.ctx.cwd,
            &crate::hooks::HookInput {
                source: Some(trigger),
                ..Default::default()
            },
        );
        observer.on_event(&LiveEvent::ToolDone {
            name: "compact".into(),
            ok: true,
            output: summary.clone(),
            depth: self.ctx.depth,
            lane: self.ctx.lane,
            call_id: None,
            elapsed_ms: 0,
        });
        // the durable Compacted event wipes the transcript on replay — the
        // live mirror does the same for a session that's mid-watch
        observer.on_event(&LiveEvent::Compacted {
            summary: summary.clone(),
        });
        Ok(summary)
    }
}

impl AgentLoop {
    /// Rough token estimate for the current transcript.
    /// Estimated tokens for the *next* request. Trust the provider's own
    /// counter first — the last `Usage` fact's `prompt_tokens` is exact.
    /// The byte heuristic is the fallback for a session that hasn't
    /// reported usage yet (or a dialect that never does), not the
    /// primary source: `serde_json` bytes over-count structure and
    /// under-count CJK by ~2×.
    pub(super) async fn est_tokens(&self) -> usize {
        let events = self
            .ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .unwrap_or_default();
        // a Usage fact older than the last Compacted boundary describes the
        // pre-compact transcript — trusting it re-trips the loop-head check
        // and compacts the fresh summary a second time
        for ev in events.iter().rev() {
            match ev {
                SessionEvent::Usage { usage } => return usage.prompt_tokens as usize,
                SessionEvent::Compacted { .. } => break,
                _ => {}
            }
        }
        // no usage yet — estimate from the folded messages
        let msgs = self
            .ctx
            .sessions
            .lock()
            .await
            .messages()
            .await
            .unwrap_or_default();
        msgs.iter()
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum::<usize>()
            / 4
    }

    /// The auto-compact tripwire, sized to the model the session is *on*.
    /// `context_length_for` reads the provider catalog; a selector that
    /// resolves nowhere or a provider that doesn't advertise a window falls
    /// back to `compact_threshold`. 85% headroom leaves room for the turn
    /// that tips it over.
    pub(super) fn effective_threshold(&self) -> usize {
        let selector = self.ctx.effective_selector();
        let window = selector.as_deref().and_then(|sel| {
            self.ctx
                .models
                .as_ref()
                .and_then(|m| m.context_length_for(sel))
        });
        match window {
            Some(w) => ((w * 85 / 100) as usize).max(1),
            None => self.compact_threshold,
        }
    }
}
