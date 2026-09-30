//! The dispatch gate — declarative rules → hook `permissionDecision` →
//! session grants → risky-pattern classifier → approval prompt. Lives apart
//! from the turn loop: this file is the *decision*, turn.rs is the *drive*.

use super::*;

impl AgentLoop {
    /// `deny` rules are a hard refusal nothing overrides — a session grant
    /// never bypasses them.
    pub(super) async fn gate_call(
        &self,
        tool: &str,
        specifier: &str,
        hook: Option<crate::hooks::HookPermission>,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        use crate::hooks::HookPermission as H;
        use crate::permissions::Verdict;
        match self.ctx.permissions.check(tool, specifier) {
            Verdict::Deny => return Err("denied by permission rules".into()),
            Verdict::PreApproved => return Ok(()),
            Verdict::Ask | Verdict::Default => {}
        }
        if let Some(H::Deny) = hook {
            return Err("denied by hook".into());
        }
        // Session grants sit after both deny gates but before every ask: a
        // grant is a standing answer to a prompt, not an override of a veto.
        if self.ctx.session_granted(tool, specifier) {
            return Ok(());
        }
        if self.ctx.permissions.check(tool, specifier) == Verdict::Ask {
            return self
                .ask(tool, specifier, "matched ask rule", observer)
                .await;
        }
        if let Some(H::Ask) = hook {
            return self
                .ask(tool, specifier, "hook requested approval", observer)
                .await;
        }
        if let Some(H::Allow) = hook {
            return Ok(());
        }
        // default: the risky-pattern classifier (Bash-shaped patterns today)
        if let Some(why) = crate::approval::classify(specifier, &self.ctx.risk_table) {
            return self.ask(tool, specifier, why, observer).await;
        }
        Ok(())
    }

    /// One approval prompt → verdict. `Session` is recorded in
    /// `session_grants` and audited as a durable `Hook` fact.
    pub(super) async fn ask(
        &self,
        tool: &str,
        specifier: &str,
        why: &str,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        // Notification: the loop is about to idle on a human — hooks can
        // relay that (desktop toast, bell). Advisory only; outcome ignored.
        let _ = self
            .ctx
            .hooks
            .fire(
                HookEvent::Notification,
                &self.ctx.cwd,
                &crate::hooks::HookInput {
                    prompt: Some(why),
                    tool_name: Some(tool),
                    tool_input: Some(&serde_json::json!({ "specifier": specifier })),
                    ..Default::default()
                },
            )
            .await;
        match self.ctx.approval.approve(tool, specifier, why).await {
            crate::approval::Approval::Session => {
                self.ctx.grant_session(tool, specifier);
                let detail = format!("{tool}: {specifier}");
                {
                    let mut log = self.ctx.sessions.lock().await;
                    let _ = log
                        .append(&crate::SessionEvent::Hook {
                            event: "approval.session".into(),
                            detail: detail.clone(),
                        })
                        .await;
                }
                observer.on_event(&LiveEvent::Hook {
                    event: "approval.session".into(),
                    detail,
                });
                Ok(())
            }
            crate::approval::Approval::Once => Ok(()),
            crate::approval::Approval::Deny => {
                let detail = format!("{tool}: {specifier} ({why})");
                {
                    let mut log = self.ctx.sessions.lock().await;
                    let _ = log
                        .append(&crate::SessionEvent::Hook {
                            event: "approval.deny".into(),
                            detail: detail.clone(),
                        })
                        .await;
                }
                observer.on_event(&LiveEvent::Hook {
                    event: "approval.deny".into(),
                    detail,
                });
                Err(format!("denied at approval gate ({why})"))
            }
        }
    }
}
