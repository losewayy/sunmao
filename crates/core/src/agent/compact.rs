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
        };
        let mut stream = self.ctx.active_llm().stream(req).await?;
        let mut summary = String::new();
        while let Some(d) = stream.next().await {
            if let StreamDelta::Content(c) = d? {
                summary.push_str(&c);
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
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::PostCompact,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    source: Some(trigger),
                    ..Default::default()
                },
            )
            .await;
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
