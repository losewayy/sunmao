//! Session-scoped reasoning effort — the `/effort` + GUI chip + ACP
//! ThoughtLevel surface. The override lives on `ctx.reasoning_effort`
//! (shared with sub-agents like `approval_mode`), persists as a
//! `Hook{event:"effort.change"}` fact — same durable audit shape
//! `model.change` uses — and reseeds from the log on resume.

use super::*;
use crate::context::RwLockRecover;

impl AgentLoop {
    /// The session's effort override — `None` = the provider's default.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.ctx.reasoning_effort.read_or_recover().clone()
    }

    /// Set (or clear, with `None`) the effort override — durable as an
    /// `effort.change` hook fact and announced live so "who dialed the
    /// thinking up when" is reconstructible, same contract as
    /// `set_approval_mode`. Takes the turn fence for the same reason.
    /// `Some("default")` by convention also clears — frontends spell the
    /// reset choice that way.
    pub async fn set_reasoning_effort(&self, level: Option<&str>, observer: &dyn Observer) {
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let detail = match level {
            Some(l) if l != "default" && !l.trim().is_empty() => l.trim().to_string(),
            _ => "default".to_string(),
        };
        *self.ctx.reasoning_effort.write_or_recover() =
            (detail != "default").then(|| detail.clone());
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append_audit(&SessionEvent::Hook {
                event: "effort.change".into(),
                detail: detail.clone(),
            })
            .await;
        }
        observer.on_event(&LiveEvent::Hook {
            event: "effort.change".into(),
            detail,
        });
    }

    /// The thinking levels the *current* model advertises — the picker's
    /// row set (`/effort` bare, GUI chip, ACP ThoughtLevel select).
    /// Reads `active_selector` after a `/model` swap, else the session's
    /// `Started.model`; a provider whose catalog never learned levels
    /// yields the canonical low/medium/high only when the model advertises
    /// reasoning support at all (otherwise empty — a non-thinking model
    /// earns no picker).
    pub async fn effort_levels(&self) -> Vec<String> {
        let Some(models) = self.ctx.models.as_ref() else {
            return Vec::new();
        };
        let selector = {
            let s = self.ctx.active_selector.read_or_recover().clone();
            match s {
                Some(s) => Some(s),
                // baseline adapter — its wire name is the `Started` fact
                None => {
                    let events = self
                        .ctx
                        .sessions
                        .lock()
                        .await
                        .events()
                        .await
                        .unwrap_or_default();
                    events.iter().find_map(|ev| match ev {
                        SessionEvent::Started { model, .. } => Some(model.clone()),
                        _ => None,
                    })
                }
            }
        };
        selector
            .map(|sel| models.thinking_levels(&sel))
            .unwrap_or_default()
    }
}
