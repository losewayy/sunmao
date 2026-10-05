//! Agent-side state — the session map's value type, provider/resolver
//! factories, the trust-gated cancel entry, and the config-option read.
//!  is the wire (request handlers); this is what the handlers
//! hold between requests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::Error;
use agent_client_protocol::schema::v2;
use sunmao_core::Context;
use sunmao_core::agent::AgentLoop;
use sunmao_core::context::{MutexRecover, RwLockRecover};
use sunmao_llm::OaiClient;

use super::config::{effort_config, mode_config};

/// Session-id disambiguator — `s-<ms>` alone collides within one
/// millisecond between concurrent `session/new` calls.
pub(super) static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) struct SessionState {
    pub(super) agent: AgentLoop,
    pub(super) ctx: Arc<Context>,
    /// MessageId counter shared by every observer this session mints —
    /// prompt turns, config sets and the live_sink relay all draw from it
    /// so a chunk id never repeats for a deduplicating client.
    pub(super) msg_ids: Arc<std::sync::atomic::AtomicU64>,
}

pub(super) struct SunmaoAgent {
    pub(super) sessions: Mutex<HashMap<String, Arc<Mutex<SessionState>>>>,
    pub(super) base_url: String,
    pub(super) api_key: String,
    pub(super) model: String,
    pub(super) provider: String,
    /// `--preset` args, resolved per session — the client's cwd arrives in
    /// the request, not on our CLI.
    pub(super) preset_names: Vec<String>,
}

impl SunmaoAgent {
    pub(super) fn new_llm(&self) -> Arc<dyn sunmao_llm::ProviderAdapter> {
        match self.provider.as_str() {
            "anthropic" => Arc::new(sunmao_llm::AnthropicClient::new(
                self.base_url.clone(),
                self.api_key.clone(),
                self.model.clone(),
            )),
            _ => Arc::new(OaiClient::new(
                self.base_url.clone(),
                self.api_key.clone(),
                self.model.clone(),
            )),
        }
    }

    /// Model routing seam — same resolution as every other frontend.
    pub(super) fn new_resolver(
        &self,
        cwd: &std::path::Path,
    ) -> Arc<sunmao_core::models::ModelResolver> {
        Arc::new(sunmao_core::models::ModelResolver::load(
            cwd,
            sunmao_core::models::ProviderDef {
                base_url: self.base_url.clone(),
                api_key_env: None,
                api_key: Some(self.api_key.clone()),
                dialect: self.provider.clone(),
                catalog: Vec::new(),
            },
            "default",
        ))
    }

    /// Resolve this session's presets against the request cwd. An unknown
    /// name must fail the session — running looser than requested is worse
    /// than refusing.
    pub(super) fn presets(
        &self,
        cwd: &std::path::Path,
    ) -> std::result::Result<Vec<std::path::PathBuf>, Error> {
        sunmao_core::presets::resolve(cwd, &self.preset_names)
            .map_err(|e| invalid_params(format!("presets: {e:#}")))
    }

    /// `session/cancel` — the real cancel path (flag + notify_waiters +
    /// sub-agent cascade + parked-approval drain). Setting `cancelled`
    /// alone left in-flight streams/tools waiting for a wake nobody sent.
    pub(super) fn cancel_session(&self, id: &str) {
        let map = self.sessions.lock_or_recover();
        if let Some(s) = map.get(id) {
            s.lock_or_recover().agent.cancel();
        }
    }
}

pub(super) fn invalid_params(msg: impl ToString) -> Error {
    Error::invalid_params().data(msg.to_string())
}

/// The two selects every session advertises — mode + effort. Read fresh
/// from the ctx each call so replies reflect the state *after* a set.
pub(super) async fn config_options(
    ctx: &Arc<Context>,
    agent_loop: &AgentLoop,
) -> Vec<v2::SessionConfigOption> {
    // bind before the await — a read-guard temporary inside the vec! would
    // hold a !Send lock across it
    let mode = mode_config(*ctx.approval_mode.read_or_recover());
    // the ladder settles the session's default level; the select shows the
    // level in force, so resolve before reading it
    let levels = agent_loop.effort_levels().await;
    agent_loop.resolve_effort_default().await;
    let effort = effort_config(agent_loop.reasoning_effort().as_deref(), &levels);
    vec![mode, effort]
}
