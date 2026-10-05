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
        self.advertised_levels().await
    }

    /// Fix the session's default effort from the catalog's ladder, and
    /// re-fix it when a catalog edit moves the ladder under a session that
    /// never chose for itself.
    ///
    /// There is no "provider default" rung in the UI: a session has a level
    /// from the start, and that level is the second-strongest the active
    /// model advertises (`[high, max]` starts at `high`). The top of a
    /// ladder is a deliberate reach — cost, latency — rather than a place
    /// to begin, and the bottom throws away a thinking model. `effort_default`
    /// is the provenance marker that keeps a catalog edit from overwriting a
    /// level the user actually picked; a model with no ladder leaves both
    /// fields alone (nothing to pick from).
    pub async fn resolve_effort_default(&self) {
        let levels = self.advertised_levels().await;
        let next = default_effort_level(&levels);
        let mut marker = self.ctx.effort_default.write_or_recover();
        if *marker == next {
            return;
        }
        {
            let mut cur = self.ctx.reasoning_effort.write_or_recover();
            if *cur == *marker {
                *cur = next.clone();
            }
        }
        *marker = next;
    }

    /// The ladder for the session's current model — the body behind both
    /// `effort_levels` and `resolve_effort_default`.
    async fn advertised_levels(&self) -> Vec<String> {
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

/// The default rung of a ladder: the catalog lists levels weakest→strongest,
/// so one below the top (`len - 2`). A single-rung ladder is its own
/// default; an empty one has none.
fn default_effort_level(levels: &[String]) -> Option<String> {
    levels.get(levels.len().saturating_sub(2)).cloned()
}
