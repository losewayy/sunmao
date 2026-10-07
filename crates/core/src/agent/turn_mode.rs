//! Turn modes (`/mode standard|fusion`) — which *shape* a turn takes,
//! distinct from the approval stance (`approval_mode`, how permissive the
//! gate is) and the loop driver (`loop_driver`, which circuit runs).
//! Standard is the single-adapter turn; Fusion splits a turn into a
//! read-only Lead plus a delegated Sidekick worker; both models are explicit session settings
//! (`agent/fusion.rs`).
//!
//! Durable as `SessionEvent::TurnModeChange` — a dedicated event rather
//! than a `ModeChange` payload: `ModeChange` carries an `ApprovalMode`,
//! and `fusion` is not an approval stance. The mode is per-Context (not
//! Arc-shared like `approval_mode`): a sub-agent context always runs
//! Standard — Sidekicks don't get to delegate another level.

use super::*;

/// How a turn executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    /// The ordinary single-adapter turn — every tool call runs here.
    #[default]
    Standard,
    /// Lead/Sidekick split, enabled only while both explicit session
    /// selectors resolve: this context becomes the read-only Lead, the gate
    /// short-circuits mutations, its surface sheds write tools, and work
    /// delegates to a fresh-context Sidekick through `FusionExecute`.
    Fusion,
}

impl TurnMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fusion => "fusion",
        }
    }

    /// Every spelling the frontends may send.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "standard" | "default" | "单模型" => Some(Self::Standard),
            "fusion" | "fused" => Some(Self::Fusion),
            _ => None,
        }
    }

    /// The canonical selectors, in menu order.
    pub const ALL: [Self; 2] = [Self::Standard, Self::Fusion];
}

impl AgentLoop {
    /// The session's current turn mode.
    pub fn turn_mode(&self) -> TurnMode {
        *self.ctx.turn_mode.read_or_recover()
    }

    /// Switch the turn mode mid-session — `set_approval_mode`'s twin:
    /// the turn fence makes the flip land between turns, the
    /// `TurnModeChange` fact makes it durable, and the live Hook row
    /// announces it. Switching to Fusion arms the per-context
    /// `read_only` flag (the Lead's gate block); switching back disarms
    /// it and drops any live delegation state.
    ///
    /// Refuses `fusion` under the `ptc` driver — its advertised surface is
    /// RunCode alone — and when either required session model is unset or stale.
    /// (TODO: a fusion-aware Ptc surface could advertise FusionExecute alongside RunCode.)
    pub async fn set_turn_mode(
        &self,
        mode: TurnMode,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        if mode == TurnMode::Fusion && self.ctx.loop_driver == LoopDriver::Ptc {
            return Err(
                "fusion needs the standard tool surface — the ptc loop advertises RunCode only"
                    .into(),
            );
        }
        let _turn_permit = self.ctx.turn_lock.lock().await;
        if mode == TurnMode::Fusion
            && let Some(problem) = self.fusion_model_problem()
        {
            return Err(problem);
        }
        *self.ctx.turn_mode.write_or_recover() = mode;
        self.ctx.read_only.store(
            mode == TurnMode::Fusion,
            std::sync::atomic::Ordering::Relaxed,
        );
        // a mode flip dissolves the delegation: a dropped Sidekick's later
        // `steer` would be a steer into nothing, and the verify-fail streak
        // belongs to the abandoned session shape
        *self.ctx.fusion.lock_or_recover() = fusion::FusionState::default();
        {
            let mut log = self.ctx.sessions.lock().await;
            if let Err(e) = log.append(&SessionEvent::TurnModeChange { mode }).await {
                tracing::warn!("turn-mode change not durable: {e:#}");
            }
        }
        observer.on_event(&LiveEvent::Hook {
            event: "turn.mode".into(),
            detail: mode.as_str().to_string(),
        });
        Ok(())
    }

    /// The explicitly configured session Fusion Lead and Sidekick selectors.
    pub fn fusion_models(&self) -> (Option<String>, Option<String>) {
        let settings = self.ctx.fusion_models.read().unwrap().clone();
        (settings.lead, settings.sidekick)
    }

    /// The roles' effort dials — `None` means the session `/effort` applies.
    pub fn fusion_efforts(&self) -> (Option<String>, Option<String>) {
        let settings = self.ctx.fusion_models.read().unwrap().clone();
        (settings.lead_effort, settings.sidekick_effort)
    }

    /// Explain why this session cannot enter Fusion yet.
    pub fn fusion_model_problem(&self) -> Option<String> {
        let (lead, sidekick) = self.fusion_models();
        let Some(models) = self.ctx.models.as_ref() else {
            return Some("Fusion needs a model catalog for this session".into());
        };
        let selectors = models.selectors();
        for (role, selector) in [("Lead", lead.as_deref()), ("Sidekick", sidekick.as_deref())] {
            let Some(selector) = selector else {
                return Some(format!(
                    "Fusion {role} must be explicitly selected for this session"
                ));
            };
            if !selectors.iter().any(|available| available == selector)
                || models.adapter_for(selector).is_none()
            {
                return Some(format!(
                    "Fusion {role} selector `{selector}` is no longer available"
                ));
            }
        }
        None
    }

    /// Whether both session Fusion role selectors still resolve.
    pub fn fusion_ready(&self) -> bool {
        self.fusion_model_problem().is_none()
    }

    pub(crate) fn reconcile_fusion_mode(&self) -> Option<String> {
        if self.turn_mode() != TurnMode::Fusion {
            return None;
        }
        let problem = self.fusion_model_problem()?;
        *self.ctx.turn_mode.write_or_recover() = TurnMode::Standard;
        self.ctx
            .read_only
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *self.ctx.fusion.lock_or_recover() = fusion::FusionState::default();
        Some(problem)
    }

    /// Persist one session Fusion model override at the next turn fence.
    pub async fn set_fusion_model(
        &self,
        role: crate::context::FusionModelRole,
        selector: Option<String>,
    ) -> Result<(), String> {
        let selector = selector
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let _turn_permit = self.ctx.turn_lock.lock().await;
        if self.turn_mode() == TurnMode::Fusion && selector.is_none() {
            return Err("switch to Standard before clearing a Fusion model".into());
        }
        let mut settings = self.ctx.fusion_models.read().unwrap().clone();
        if let Some(selector) = selector.as_deref() {
            let available = self.ctx.models.as_ref().is_some_and(|models| {
                models
                    .selectors()
                    .iter()
                    .any(|candidate| candidate == selector)
                    && models.adapter_for(selector).is_some()
            });
            if !available {
                let role = match role {
                    crate::context::FusionModelRole::Lead => "Lead",
                    crate::context::FusionModelRole::Sidekick => "Sidekick",
                };
                return Err(format!(
                    "unknown or unavailable Fusion {role} selector: {selector}"
                ));
            }
        }
        match role {
            crate::context::FusionModelRole::Lead => settings.lead = selector,
            crate::context::FusionModelRole::Sidekick => settings.sidekick = selector,
        }
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append(&SessionEvent::FusionModelsChange {
                lead: settings.lead.clone(),
                sidekick: settings.sidekick.clone(),
                lead_effort: settings.lead_effort.clone(),
                sidekick_effort: settings.sidekick_effort.clone(),
            })
            .await
            .map_err(|error| format!("Fusion model save failed: {error:#}"))?;
        }
        *self.ctx.fusion_models.write().unwrap() = settings;
        Ok(())
    }

    /// Persist one role's effort dial — same durability contract as
    /// `set_fusion_model` (one `fusion_models_change` fact carries the
    /// whole four-field snapshot). `None` clears back to inheriting the
    /// session `/effort`; effort levels are freeform, matching `/effort`.
    pub async fn set_fusion_effort(
        &self,
        role: crate::context::FusionModelRole,
        level: Option<String>,
    ) -> Result<(), String> {
        let level = level
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "default");
        let _turn_permit = self.ctx.turn_lock.lock().await;
        let mut settings = self.ctx.fusion_models.read().unwrap().clone();
        match role {
            crate::context::FusionModelRole::Lead => settings.lead_effort = level,
            crate::context::FusionModelRole::Sidekick => settings.sidekick_effort = level,
        }
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append(&SessionEvent::FusionModelsChange {
                lead: settings.lead.clone(),
                sidekick: settings.sidekick.clone(),
                lead_effort: settings.lead_effort.clone(),
                sidekick_effort: settings.sidekick_effort.clone(),
            })
            .await
            .map_err(|error| format!("Fusion effort save failed: {error:#}"))?;
        }
        *self.ctx.fusion_models.write().unwrap() = settings;
        Ok(())
    }
}
