//! Which model a delegation's Sidekick runs on.
//!
//! The pair is the point of Fusion — a strong model plans and verifies as the
//! Lead, a cheaper one executes — so the Sidekick's model is policy of its
//! own: the call site may pin one, `fusion_sidekick` in `.sunmao/models.json`
//! supplies the default, and neither means "inherit the Lead".

use std::sync::Arc;

use crate::context::Context;
use sunmao_llm::ProviderAdapter;

/// The `models.json` key holding the Sidekick's default selector. Like every
/// top-level key this build does not declare as a field, it rides
/// `ModelsFile::extra` — the GUI save path (`ModelsFile::merge_save`)
/// preserves that class of key, so the settings page round-trips it.
///
/// `pub` so the serve layer can name the same literal through
/// `agent::SIDEKICK_KEY`; the module stays crate-private, so that re-export is
/// the only path to it.
pub const KEY: &str = "fusion_sidekick";

/// The configured selector — `None` when unset, blank or not a string (all
/// three mean "inherit the Lead's own adapter").
pub(crate) fn configured(ctx: &Context) -> Option<String> {
    ctx.models
        .as_ref()?
        .file()
        .extra
        .get(KEY)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The call-site selector the Lead pinned wins; else the config; else `None`
/// (inherit the Lead).
pub(crate) fn selector(ctx: &Context, call_site: Option<&str>) -> Option<String> {
    match call_site.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sel) => Some(sel.to_string()),
        None => configured(ctx),
    }
}

/// The Sidekick's adapter.
///
/// Split by selector SOURCE, because the two failures deserve opposite
/// answers: a CALL-SITE selector that does not resolve stays a hard error (the
/// Lead asked for something wrong and should hear about it); one the CONFIG
/// names falls back to inheriting the Lead with a warn, because a renamed
/// provider or a dropped route must not break every delegation.
pub(crate) fn adapter(
    ctx: &Context,
    call_site: Option<&str>,
) -> anyhow::Result<Option<Arc<dyn ProviderAdapter>>> {
    let Some(sel) = selector(ctx, call_site) else {
        return Ok(None);
    };
    let Some(m) = ctx.models.as_ref() else {
        // only a call-site selector can get here without a routing table
        if call_site.is_some() {
            anyhow::bail!("model selector `{sel}` needs .sunmao/models.json routes");
        }
        return Ok(None);
    };
    match m.adapter_for(&sel) {
        Some(a) => Ok(Some(a)),
        None if call_site.is_some() => anyhow::bail!(
            "unknown model selector `{sel}` — available: {}",
            m.describe().join(", ")
        ),
        None => {
            tracing::warn!(
                "{KEY} `{sel}` does not resolve — the Sidekick inherits the Lead's model"
            );
            Ok(None)
        }
    }
}
