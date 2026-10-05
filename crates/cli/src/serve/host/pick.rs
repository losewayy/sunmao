//! The model a NEW session starts on — `default_model` in the project's
//! `models.json`, plus the swap that makes the session actually run on it.
//! Split out of `host.rs` under the god-file budget.

use super::{Host, Shared};

impl Shared {
    /// An explicitly passed `--model`/`SUNMAO_MODEL` > `default_model` in
    /// *that project's* file (the per-creation `cwd` picks the project) > the
    /// launch model. Only a fresh log consults the file — a session already
    /// under way keeps the model its own `Started`/`model.change` facts name.
    pub(crate) fn start_model(&self, cwd: &std::path::Path) -> String {
        self.model_override
            .clone()
            .or_else(|| sunmao_core::models::default_selector(cwd))
            .unwrap_or_else(|| self.model_label.clone())
    }
}

/// `Started` only *names* the model — the adapter still comes from the launch
/// provider until a swap lands, so a session that started on the project's
/// default has to bind it. The launch model is the no-op case (the baseline
/// adapter already is it); a selector that resolves nowhere keeps that
/// baseline rather than failing the new chat.
pub(super) fn bind(host: &Host, model: &str, launch: &str) {
    if model != launch && host.agent.swap_model(model).is_none() {
        tracing::warn!("{model}: default_model resolves nowhere — running on the launch model");
    }
}
