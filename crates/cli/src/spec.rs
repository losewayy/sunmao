//! `Cli` → `HostSpec`: the Context assembly both host paths share —
//! provider, MCP servers, presets, model routes, `--loop` override.
//! Every session tab gets its own Context/AgentLoop (built by the
//! factory) — no shared Context swapping mid-tab. The MCP connections are
//! process-wide; each Context registers fresh tool handles sharing them.

use std::sync::Arc;

use anyhow::Context as _;
use sunmao_core::Context;
use sunmao_core::tool::builtin_registry;

use crate::{Cli, open_first_log, provider_adapter, serve};

pub(crate) async fn host_spec(cli: &Cli) -> anyhow::Result<serve::HostSpec> {
    let cwd = cli.cwd.canonicalize().context("bad --cwd")?;
    let preset_roots = sunmao_core::presets::resolve(&cwd, &cli.preset)?;
    let llm = provider_adapter(cli);
    let mcp = sunmao_core::mcp::connect_all(&cwd, &preset_roots).await;
    // untrusted-server skips audit into every session tab's own log —
    // serve connects MCP process-wide before any session exists
    let mcp_skips = mcp.skipped.clone();
    let first_log = open_first_log(cli).await?;
    let system_prompt = sunmao_core::prompt::PromptAssembler::new(&cwd)
        .with_extra_roots(&preset_roots)
        .assemble(cli.system.as_deref());
    let default_provider = sunmao_core::models::ProviderDef {
        base_url: cli.base_url.clone(),
        api_key: Some(cli.api_key.clone()),
        dialect: cli.provider.clone(),
        ..Default::default()
    };
    // `--model`/`SUNMAO_MODEL` outranks a project's `default_model` whenever
    // it named *something* — even the compiled fallback's own value, because
    // an explicit choice is not a fallback. `None` is the only defer case.
    let model_label = cli.model_label().to_string();
    let model_override = cli.model.clone();
    let driver_override = cli.driver;
    let serve_roots = preset_roots.clone();
    let factory = serve::SessionFactory {
        make: Box::new(move |log, approver, cwd| {
            let llm = llm.clone();
            let mcp_servers = mcp.servers.clone();
            let mcp_skips = mcp_skips.clone();
            let preset_roots = preset_roots.clone();
            let driver = driver_override;
            let default_provider = default_provider.clone();
            Box::pin(async move {
                let mut log = log;
                sunmao_core::mcp::audit_skips(&mcp_skips, &mut log).await;
                let registry = builtin_registry();
                for h in &mcp_servers {
                    for t in h.tool_impls() {
                        registry.register_boxed(t);
                    }
                }
                // `cwd` is the *session's* project — adopted logs carry
                // their own root (cross-project resume keeps it)
                let mut ctx_raw = Context::new(llm, log, registry, cwd.clone())
                    .with_extra_plugin_roots(preset_roots);
                ctx_raw.mcp_servers = mcp_servers;
                if let Some(d) = driver {
                    ctx_raw.loop_driver = d;
                }
                ctx_raw.connect_extensions().await;
                ctx_raw.approval = approver;
                ctx_raw.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
                    &cwd,
                    default_provider,
                    "default",
                )));
                Ok(ctx_raw)
            })
        }),
    };
    Ok(serve::HostSpec {
        factory,
        cwd: cwd.clone(),
        roots: serve_roots,
        // --system freezes the prompt; absent it each session's project
        // dir assembles its own (AGENTS.md etc. follow the project)
        prompt_override: cli.system.as_ref().map(|_| system_prompt.clone()),
        model_label,
        model_override,
        first_log,
        driver_override: cli.driver,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    /// The regression F3b named: `--model <compiled-default>` used to
    /// collapse to `None` ("not a choice") and lose to a project's
    /// `default_model`. The Option itself is now the was-it-passed bit.
    #[test]
    fn explicit_default_model_is_still_a_choice() {
        // SAFETY: removing the env var before parsing — clap would read it
        unsafe {
            std::env::remove_var("SUNMAO_MODEL");
        }
        let explicit =
            crate::Cli::try_parse_from(["sunmao", "--model", crate::DEFAULT_MODEL]).unwrap();
        assert_eq!(explicit.model.as_deref(), Some(crate::DEFAULT_MODEL));
        assert_eq!(explicit.model_label(), crate::DEFAULT_MODEL);
        // override Some => start_model never consults the project file
        assert!(explicit.model.is_some());
        let bare = crate::Cli::try_parse_from(["sunmao"]).unwrap();
        assert!(bare.model.is_none(), "unset defers to default_model");
    }
}
