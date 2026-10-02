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
