//! Which model a delegation's Sidekick runs on.
//!
//! Fusion requires an explicit session selection for both roles; a call-site
//! model override may replace the configured Sidekick for one delegation.

use std::sync::Arc;

use crate::context::Context;
use sunmao_llm::ProviderAdapter;

pub(crate) fn configured(ctx: &Context) -> Option<String> {
    ctx.fusion_models.read().unwrap().sidekick.clone()
}

/// A call-site selector takes precedence over the required session selection.
pub(crate) fn selector(ctx: &Context, call_site: Option<&str>) -> Option<String> {
    match call_site
        .map(str::trim)
        .filter(|selector| !selector.is_empty())
    {
        Some(selector) => Some(selector.to_string()),
        None => configured(ctx),
    }
}

pub(crate) fn adapter(
    ctx: &Context,
    call_site: Option<&str>,
) -> anyhow::Result<Option<Arc<dyn ProviderAdapter>>> {
    let call_site_pinned = call_site
        .map(str::trim)
        .is_some_and(|selector| !selector.is_empty());
    let Some(selector) = selector(ctx, call_site) else {
        anyhow::bail!("Fusion Sidekick must be selected for this session");
    };
    let Some(models) = ctx.models.as_ref() else {
        anyhow::bail!("Fusion Sidekick needs this session's model catalog");
    };
    if !call_site_pinned && !models.selectors().iter().any(|value| value == &selector) {
        anyhow::bail!("Fusion Sidekick selector `{selector}` is no longer available");
    }
    let Some(adapter) = models.adapter_for(&selector) else {
        anyhow::bail!("Fusion Sidekick selector `{selector}` is no longer available");
    };
    Ok(Some(adapter))
}
