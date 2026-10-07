use super::{Context, RwLockRecover};
use sunmao_llm::ProviderAdapter;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FusionModelSettings {
    pub lead: Option<String>,
    pub sidekick: Option<String>,
    /// Per-role reasoning effort — `None` falls back to the session's
    /// `/effort` (shared with every sub-agent). A set value is the role's
    /// own dial: the Lead reads it instead of the session override, the
    /// Sidekick's child Context pins it at spawn.
    pub lead_effort: Option<String>,
    pub sidekick_effort: Option<String>,
}

impl Context {
    pub(crate) fn effective_selector(&self) -> Option<String> {
        if *self.turn_mode.read().unwrap() == crate::agent::TurnMode::Fusion
            && let Some(lead) = self.fusion_models.read().unwrap().lead.clone()
        {
            return Some(lead);
        }
        self.active_selector.read().unwrap().clone()
    }

    /// Fusion turns use the validated session Lead; Standard turns use the
    /// session override when present, otherwise the baseline adapter.
    pub fn active_llm(&self) -> std::sync::Arc<dyn ProviderAdapter> {
        if *self.turn_mode.read().unwrap() == crate::agent::TurnMode::Fusion
            && let Some(lead) = self.fusion_models.read().unwrap().lead.clone()
            && let Some(adapter) = self
                .models
                .as_ref()
                .and_then(|models| models.adapter_for(&lead))
        {
            return adapter;
        }
        self.llm_override
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.llm.clone())
    }

    /// The effort the next request on THIS context runs at: a Fusion turn's
    /// Lead honors its own dial when set, else the session `/effort`
    /// override (which sub-agents still inherit via the shared Arc).
    /// A Sidekick child's effort is pinned on its Context at spawn — this
    /// reads whatever was pinned, so no per-role branch is needed there.
    pub(crate) fn turn_effort(&self) -> Option<String> {
        if *self.turn_mode.read().unwrap() == crate::agent::TurnMode::Fusion
            && let Some(lead_effort) = self
                .fusion_models
                .read()
                .unwrap()
                .lead_effort
                .clone()
                .filter(|e| !e.is_empty())
        {
            return Some(lead_effort);
        }
        self.reasoning_effort.read_or_recover().clone()
    }
}
