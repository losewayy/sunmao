//! `session/set_config_option` options — the per-session selects every ACP
//! session advertises. `mode` maps onto SPEC §4.6 stances; `effort` is the
//! ThoughtLevel select over the active model's thinking levels.

use agent_client_protocol::schema::v2;

/// The approval-mode select — the client's `session/set_config_option`
/// values map onto SPEC §4.6 stances.
pub fn mode_config(current: sunmao_core::agent::ApprovalMode) -> v2::SessionConfigOption {
    v2::SessionConfigOption::select(
        "mode",
        "Approval mode",
        current.as_str(),
        sunmao_core::agent::ApprovalMode::ALL
            .iter()
            .map(|m| {
                v2::SessionConfigSelectOption::new(
                    m.as_str(),
                    match m {
                        sunmao_core::agent::ApprovalMode::AlwaysAsk => "Ask for approval",
                        sunmao_core::agent::ApprovalMode::Auto => "Auto",
                        sunmao_core::agent::ApprovalMode::ReadOnly => "Read only",
                        sunmao_core::agent::ApprovalMode::FullAccess => "Full access",
                    },
                )
            })
            .collect::<Vec<_>>(),
    )
    .category(v2::SessionConfigOptionCategory::Mode)
    .description("Gate stance per SPEC §4.6 — always_ask/auto/read_only/full_access; deny rules apply in every mode")
}

/// The reasoning-effort select (`effort` config id) — the ACP ThoughtLevel
/// category. `default` is always a choice (it clears the session override);
/// the rest are the levels the active model's catalog advertises, which can
/// be empty — a non-thinking model offers only the reset row.
pub fn effort_config(current: Option<&str>, levels: &[String]) -> v2::SessionConfigOption {
    let mut options = vec![v2::SessionConfigSelectOption::new(
        "default",
        "Default (provider)",
    )];
    options.extend(
        levels
            .iter()
            .map(|l| v2::SessionConfigSelectOption::new(l.as_str(), l.as_str())),
    );
    v2::SessionConfigOption::select(
        "effort",
        "Reasoning effort",
        current.unwrap_or("default"),
        options,
    )
    .category(v2::SessionConfigOptionCategory::ThoughtLevel)
    .description("Session effort override — low/medium/high or the provider's own vocabulary; 'default' clears")
}
