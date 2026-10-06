use super::Context;
use sunmao_llm::ProviderAdapter;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FusionModelSettings {
    pub lead: Option<String>,
    pub sidekick: Option<String>,
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
}
