//! Session-seed helpers — read-back scans over a persisted log and the
//! per-tool watchdog table. Split from `mod.rs` so the context struct
//! and its constructor stay the file's single story. These are all
//! cold reads: they run once per Context build, never per turn.
//! The `reseed_*` methods do the same job against a swapped-in log's
//! folded events (`/resume`/`/fork`/`/rewind`).

use super::{Context, MutexRecover, RwLockRecover};

/// Recover the task list a persisted log ended on: scan for `Todos`
/// event lines (prefiltered by the serializer's literal prefix) and take
/// the last one. Ephemeral logs and missing files seed empty — a fresh
/// session simply has no list yet.
pub(super) fn seed_todos(path: &std::path::Path) -> Vec<crate::tool::TodoItem> {
    if path.as_os_str().is_empty() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    for line in text.lines().rev() {
        if line.starts_with(crate::tool::TODOS_LINE_PREFIX)
            && let Ok(crate::session::SessionEvent::Todos { items }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return items;
        }
    }
    Vec::new()
}

/// Builtin skills materialize into `~/.sunmao/skills/` so the prompt's
/// skills index can list them (name + description) and the agent Reads
/// the body on demand — progressive disclosure, not baked context.
/// Refresh rule: missing → write; ours (marker intact) and drifted →
/// rewrite; marker removed or hand-edited → their file, leave it.
pub(crate) fn materialize_builtin_skills() {
    const MARK: &str = "<!-- sunmao:builtin";
    const SKILLS: &[(&str, &str)] = &[(
        "sunmao-config",
        include_str!("../../assets/skills/sunmao-config/SKILL.md"),
    )];
    let base = crate::model_knowledge::sunmao_home().join("skills");
    for (name, body) in SKILLS {
        let path = base.join(name).join("SKILL.md");
        let write = match std::fs::read_to_string(&path) {
            Err(_) => true,
            Ok(cur) => cur.starts_with(MARK) && cur != *body,
        };
        if write {
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(&path, body);
        }
    }
}

/// Per-tool watchdog seconds — `assets/tool-timeouts.txt` merged with
/// `.sunmao/tool-timeouts.txt` and plugin dirs, project rows overriding
/// builtins by name (risky-patterns stacks additively; timeouts are a
/// value, not a set, so merge means "last wins" not "append").
pub(crate) fn tool_timeout_table(cwd: &std::path::Path) -> std::collections::HashMap<String, u64> {
    let parse = |text: &str| {
        text.lines()
            .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                let (name, secs) = l.split_once(" | ")?;
                secs.trim()
                    .parse::<u64>()
                    .ok()
                    .map(|s| (name.trim().to_string(), s))
            })
            .collect::<Vec<_>>()
    };
    let mut table: std::collections::HashMap<String, u64> =
        parse(include_str!("../../assets/tool-timeouts.txt"))
            .into_iter()
            .collect();
    let mut layers = vec![
        cwd.join(".sunmao/tool-timeouts.txt"),
        cwd.join(".sunmao/plugin/tool-timeouts.txt"),
    ];
    layers.extend(
        crate::sorted_entries(&cwd.join(".sunmao").join("plugins"))
            .into_iter()
            .chain(crate::sorted_entries(&cwd.join(".claude").join("plugins")))
            .map(|e| e.path().join("tool-timeouts.txt")),
    );
    for f in layers {
        if let Ok(text) = std::fs::read_to_string(&f) {
            table.extend(parse(&text));
        }
    }
    table
}

/// The goal a reopened log left behind — last `Goal` event wins; a log
/// without one seeds `None` (a fresh session simply has no goal yet).
/// Same line-scan trick as `seed_todos`: cheap suffix read, no full fold.
pub(super) fn seed_goal(path: &std::path::Path) -> Option<crate::tool::GoalState> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    for line in text.lines().rev() {
        if line.starts_with(crate::tool::GOAL_LINE_PREFIX)
            && let Ok(crate::session::SessionEvent::Goal { goal }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return Some(goal);
        }
    }
    None
}

/// The approval stance a reopened log left behind — last `ModeChange` wins.
/// Same line-scan trick as `seed_todos`: cheap suffix read, no full fold.
pub(super) fn seed_mode(path: &std::path::Path) -> crate::agent::ApprovalMode {
    if path.as_os_str().is_empty() {
        return Default::default();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    for line in text.lines().rev() {
        if line.contains("\"mode_change\"")
            && let Ok(crate::session::SessionEvent::ModeChange { mode }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return mode;
        }
    }
    Default::default()
}

/// The turn shape a reopened log left behind — last `TurnModeChange` wins.
/// Same line-scan trick as `seed_mode`.
pub(super) fn seed_turn_mode(path: &std::path::Path) -> crate::agent::TurnMode {
    if path.as_os_str().is_empty() {
        return Default::default();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    for line in text.lines().rev() {
        if line.contains("\"turn_mode_change\"")
            && let Ok(crate::session::SessionEvent::TurnModeChange { mode }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return mode;
        }
    }
    Default::default()
}

pub(super) fn seed_fusion_models(path: &std::path::Path) -> super::FusionModelSettings {
    if !path.as_os_str().is_empty()
        && let Ok(text) = std::fs::read_to_string(path)
    {
        for line in text.lines().rev() {
            if line.contains("\"fusion_models_change\"")
                && let Ok(crate::session::SessionEvent::FusionModelsChange {
                    lead,
                    sidekick,
                    lead_effort,
                    sidekick_effort,
                }) = serde_json::from_str::<crate::session::SessionEvent>(line)
            {
                return super::FusionModelSettings {
                    lead,
                    sidekick,
                    lead_effort,
                    sidekick_effort,
                };
            }
        }
    }
    super::FusionModelSettings::default()
}

/// Rebuild the `RunCode` KV store a reopened log left behind — every
/// `PtcStore` line folds in order so later writes win. Ephemeral logs and
/// missing files seed empty.
pub(super) fn seed_ptc_store(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if path.as_os_str().is_empty() {
        return out;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        if line.starts_with("{\"type\":\"ptc_store\"")
            && let Ok(crate::session::SessionEvent::PtcStore { key, value }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            out.insert(key, value);
        }
    }
    out
}

/// The reasoning-effort override a reopened log ended on — the last
/// `effort.change` hook fact wins; its `detail` is the level, with
/// `"default"` spelling the cleared state (no override on the wire).
/// Same line-scan trick as `seed_mode`.
pub(super) fn seed_effort(path: &std::path::Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    for line in text.lines().rev() {
        if line.contains("\"effort.change\"")
            && let Ok(crate::session::SessionEvent::Hook { detail, .. }) =
                serde_json::from_str::<crate::session::SessionEvent>(line)
        {
            return match detail.as_str() {
                "default" => None,
                level => Some(level.to_string()),
            };
        }
    }
    None
}

impl Context {
    /// Re-point the task-list snapshot at the events of a swapped-in log
    /// (`/resume`). Last `Todos` fact wins; a log without one clears it.
    pub fn reseed_todos(&self, events: &[crate::session::SessionEvent]) {
        let items = events
            .iter()
            .rev()
            .find_map(|ev| match ev {
                crate::session::SessionEvent::Todos { items } => Some(items.clone()),
                _ => None,
            })
            .unwrap_or_default();
        *self.todos.lock_or_recover() = items;
    }

    /// Re-point the `RunCode` KV snapshot at a swapped-in log — every
    /// `PtcStore` fact folds in order, later writes win.
    pub fn reseed_ptc_store(&self, events: &[crate::session::SessionEvent]) {
        let mut store = std::collections::BTreeMap::new();
        for ev in events {
            if let crate::session::SessionEvent::PtcStore { key, value } = ev {
                store.insert(key.clone(), value.clone());
            }
        }
        *self.ptc_store.lock_or_recover() = store;
    }

    /// Re-point the goal snapshot at a swapped-in log — last `Goal` fact
    /// wins, a log without one clears it (a rewound/replaced session
    /// must not drag the old session's continuation loop along).
    pub fn reseed_goal(&self, events: &[crate::session::SessionEvent]) {
        let goal = events.iter().rev().find_map(|ev| match ev {
            crate::session::SessionEvent::Goal { goal } => Some(goal.clone()),
            _ => None,
        });
        *self.goal.lock_or_recover() = goal;
    }

    /// Re-point the turn mode at a swapped-in log — last `TurnModeChange`
    /// wins; the `read_only` gate flag follows the mode (a fusion log
    /// resumes read-only, a standard one disarms it). Delegation state
    /// itself does NOT survive: a resumed fusion Lead starts with an
    /// empty `FusionState` — its next delegation spawns a fresh Sidekick
    /// (the abandoned one's log stays resumable on disk, like any
    /// orphaned sub-agent).
    pub fn reseed_turn_mode(&self, events: &[crate::session::SessionEvent]) {
        let mode = events
            .iter()
            .rev()
            .find_map(|ev| match ev {
                crate::session::SessionEvent::TurnModeChange { mode } => Some(*mode),
                _ => None,
            })
            .unwrap_or_default();
        *self.turn_mode.write_or_recover() = mode;
        self.read_only.store(
            mode == crate::agent::TurnMode::Fusion,
            std::sync::atomic::Ordering::Relaxed,
        );
        *self.fusion.lock_or_recover() = crate::agent::fusion::FusionState::default();
    }

    pub(crate) fn reseed_fusion_models(&self, events: &[crate::session::SessionEvent]) {
        let settings = events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::session::SessionEvent::FusionModelsChange {
                    lead,
                    sidekick,
                    lead_effort,
                    sidekick_effort,
                } => Some(super::FusionModelSettings {
                    lead: lead.clone(),
                    sidekick: sidekick.clone(),
                    lead_effort: lead_effort.clone(),
                    sidekick_effort: sidekick_effort.clone(),
                }),
                _ => None,
            })
            .unwrap_or_default();
        *self.fusion_models.write_or_recover() = settings;
    }

    /// Re-point the effort override at a swapped-in log — last
    /// `effort.change` fact wins; `"default"` (or no fact) clears it.
    /// A resumed session must not carry the abandoned log's level.
    pub fn reseed_effort(&self, events: &[crate::session::SessionEvent]) {
        let level = events.iter().rev().find_map(|ev| match ev {
            crate::session::SessionEvent::Hook { event, detail } if event == "effort.change" => {
                match detail.as_str() {
                    "default" => Some(None),
                    level => Some(Some(level.to_string())),
                }
            }
            _ => None,
        });
        *self.reasoning_effort.write_or_recover() = level.flatten();
    }

    /// Rebuild checkpoint state for a swapped-in session — the new log's id
    /// selects its own manifest, and its boundary count seeds `turn` so a
    /// resumed/forked session writes entries under correct ordinals
    /// (`/resume`, `/fork`, `/rewind` all route through `swap_session`).
    /// `events` are the log's already-folded events — the same classifier
    /// `turn_boundaries` applies to raw lines.
    pub(crate) fn reseed_checkpoints(&self, events: &[crate::session::SessionEvent]) {
        let id = self.session_id.read_or_recover().clone();
        let mut st = crate::checkpoints::load(&self.cwd, &id);
        st.turn = events
            .iter()
            .filter(|e| crate::checkpoints::is_turn_boundary(e))
            .count() as u64;
        *self.checkpoints.lock_or_recover() = st;
    }
}

impl Context {
    /// SessionStart, applied the Claude way: `additionalContext` lands in
    /// the transcript as `[hook context]` user messages (not a boundary —
    /// the prefix is excluded from `turn_boundaries`), the same channel
    /// UserPromptSubmit context uses. Every main-session entry — startup,
    /// resume, adopt, swap — routes through here so hook-authored context
    /// can't silently evaporate on one path.
    pub async fn fire_session_start(&self, source: &str) {
        let out = self
            .hooks
            .fire(
                crate::hooks::HookEvent::SessionStart,
                &self.cwd,
                &crate::hooks::HookInput {
                    source: Some(source),
                    mcp_servers: Some(self.mcp_servers.iter().map(|s| s.name.clone()).collect()),
                    ..Default::default()
                },
            )
            .await;
        if let Some(sink) = self.live_sink.get().cloned() {
            for notice in &out.notices {
                sink.on_event(&crate::agent::LiveEvent::Hook {
                    event: "hook warning".into(),
                    detail: notice.clone(),
                });
            }
        }
        if out.extra_context.is_empty() {
            return;
        }
        let mut log = self.sessions.lock().await;
        for extra in out.extra_context {
            if let Err(e) = log
                .append(&crate::session::SessionEvent::Message {
                    message: sunmao_llm::types::Message::user(format!("[hook context] {extra}")),
                })
                .await
            {
                tracing::warn!("SessionStart context append failed: {e:#}");
            }
        }
    }

    /// Reseed the model pick for a swapped-in log — `Started.model` is the
    /// creation-time selector, a later `model.change` audit row overrides
    /// it (the same fold `status()` reads). The override slots stay empty
    /// when the selector resolves nowhere — the baseline adapter is the
    /// honest fallback.
    pub fn reseed_model(&self, events: &[crate::session::SessionEvent]) {
        use crate::session::SessionEvent;
        let mut selector: Option<String> = events.iter().find_map(|e| match e {
            SessionEvent::Started { model, .. } => Some(model.clone()),
            _ => None,
        });
        for e in events {
            if let SessionEvent::Hook { event, detail } = e
                && event == "model.change"
            {
                selector = Some(
                    detail
                        .split_once(" → ")
                        .map(|(sel, _)| sel.to_string())
                        .unwrap_or_else(|| detail.clone()),
                );
            }
        }
        *self.llm_override.write_or_recover() = selector
            .as_deref()
            .and_then(|s| self.models.as_ref()?.adapter_for(s));
        *self.active_selector.write_or_recover() = selector;
    }
}

impl Context {
    /// `mark_read`/`has_read` — the read-before-write ledger the Edit gate
    /// consults. Both raw and canonical spellings land so a relative `Edit`
    /// matches an absolute `Read` (and vice versa).
    pub fn mark_read(&self, path: &std::path::Path) {
        if let Ok(canon) = path.canonicalize() {
            self.read_paths.lock_or_recover().insert(canon);
        }
        self.read_paths.lock_or_recover().insert(path.to_path_buf());
    }

    pub fn has_read(&self, path: &std::path::Path) -> bool {
        let set = self.read_paths.lock_or_recover();
        if set.contains(path) {
            return true;
        }
        path.canonicalize()
            .map(|c| set.contains(&c))
            .unwrap_or(false)
    }

    /// A prior `Approval::Session` covers this exact call?
    pub fn session_granted(&self, tool: &str, specifier: &str) -> bool {
        self.session_grants
            .lock()
            .unwrap()
            .contains(&format!("{tool}\t{specifier}"))
    }

    /// Record a session-scoped grant.
    pub fn grant_session(&self, tool: &str, specifier: &str) {
        self.session_grants
            .lock()
            .unwrap()
            .insert(format!("{tool}\t{specifier}"));
    }

    /// Checkpoint a file before a tool mutates it — the first write in the
    /// session preserves the pre-state under `.sunmao/checkpoints/`; repeat
    /// writes and out-of-scope paths (`.sunmao`, outside the project) are
    /// no-ops. A taken snapshot is also a durable `SessionEvent::Checkpoint`
    /// — the audit spine carries which files a turn preserved. Errors
    /// propagate: a write proceeding without its snapshot would make the
    /// rewind surface lie.
    pub async fn checkpoint_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let turn = self.checkpoints.lock_or_recover().turn;
        if let Some(rel) =
            crate::checkpoints::snapshot_if_new(&self.checkpoints, &self.cwd, path, turn).await?
        {
            let mut log = self.sessions.lock().await;
            log.append(&crate::session::SessionEvent::Checkpoint {
                turn,
                files: vec![rel],
            })
            .await?;
        }
        Ok(())
    }
}
