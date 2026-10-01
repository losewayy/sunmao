//! File checkpoints — snapshot-before-write ledger under
//! `<project>/.sunmao/checkpoints/{session_id}/`.
//!
//! Every Write/Edit commit point asks `Context::checkpoint_file` to preserve
//! the pre-write bytes *once per file per session*: the first mutation of a
//! file in a session appends one `manifest.jsonl` entry and (when the file
//! already existed) a `{seq}-{hash}.bak` blob. `/rewind` folds the ledger
//! back onto the working tree — entries at or after the target turn restore
//! their earliest recorded pre-state, so reverted code goes back to what it
//! was before the rewound stretch touched it.
//!
//! Deliberate limit: only the *first* write to each file is snapshot. A file
//! touched before a rewind boundary and again after it keeps the earliest
//! snapshot — rewinding mid-stretch restores the session's first pre-state,
//! not a per-turn diff chain.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::session::SessionEvent;
use sunmao_llm::types::Role;

/// Per-session snapshot bookkeeping. `taken` holds absolute paths already
/// snapshotted (both the passed form and its canonicalization, so a file
/// that did not exist at first sight still collides with itself once it
/// does). `seq` is the monotonic slot baked into `.bak` names; `turn` is
/// the current turn ordinal — callers seed it from the log's boundary
/// count and `run_turn` bumps it once per turn.
pub(crate) struct CheckpointState {
    pub(crate) session_id: String,
    pub(crate) turn: u64,
    seq: u64,
    taken: HashSet<PathBuf>,
}

/// One `manifest.jsonl` line — the session's snapshot ledger.
#[derive(Debug, Serialize, Deserialize)]
struct ManifestEntry {
    turn: u64,
    files: Vec<ManifestFile>,
}

/// One snapshotted file inside a manifest entry. `snapshot`/`bytes` are
/// absent/null when `existed` is false — a new file needs no blob; restore
/// is "delete if present".
#[derive(Debug, Serialize, Deserialize)]
struct ManifestFile {
    path: String,
    snapshot: Option<String>,
    bytes: u64,
    existed: bool,
}

/// A user turn boundary in the raw log: its 1-based ordinal among real
/// prompts, the 0-based line index the prompt sits on, and a preview.
pub struct TurnBoundary {
    pub n: u64,
    pub line: usize,
    pub preview: String,
}

/// Where a session's checkpoint dir lives — keyed by the log's file stem,
/// the same id `Context.session_id` exposes.
fn checkpoint_dir(project: &Path, session_id: &str) -> PathBuf {
    project.join(".sunmao").join("checkpoints").join(session_id)
}

/// Project-relative forward-slash key for `path`, or `None` when it lives
/// outside `project` or under `.sunmao` (runtime state — checkpoints,
/// sessions, artifacts — must never snapshot itself; a path that escapes
/// via `..` is refused outright since the manifest feeds restore).
fn rel_key(project: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(project).ok()?;
    if !rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let key = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if key.is_empty() || rel.components().any(|c| c.as_os_str() == ".sunmao") {
        return None;
    }
    Some(key)
}

/// Stable filename stem for a project-relative path — DefaultHasher keeps
/// the zero-dep promise; the manifest records the readable `path` so the
/// hash only has to be collision-safe within one session dir.
fn rel_hash(rel: &str) -> u64 {
    let mut h = DefaultHasher::new();
    rel.hash(&mut h);
    h.finish()
}

/// Rebuild checkpoint state for a session that may already have a ledger
/// (a resumed session must not re-snapshot files it already preserved).
/// `taken` comes back from the recorded rel paths resolved against
/// `project`; `seq` resumes past every issued slot — entry count is the
/// floor, parsed `.bak` prefixes raise it if the ledger ever gains
/// entries without a snapshot slot.
pub(crate) fn load(project: &Path, session_id: &str) -> CheckpointState {
    let mut st = CheckpointState {
        session_id: session_id.to_string(),
        turn: 0,
        seq: 0,
        taken: HashSet::new(),
    };
    let manifest = checkpoint_dir(project, session_id).join("manifest.jsonl");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return st;
    };
    let mut entries = 0u64;
    let mut max_seq = 0u64;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<ManifestEntry>(line) else {
            continue;
        };
        for f in &entry.files {
            entries += 1;
            let abs = project.join(PathBuf::from(&f.path));
            st.taken.insert(abs.clone());
            if let Ok(c) = abs.canonicalize() {
                st.taken.insert(c);
            }
            if let Some(snap) = &f.snapshot
                && let Some(seq) = snap.split('-').next().and_then(|s| s.parse::<u64>().ok())
            {
                max_seq = max_seq.max(seq);
            }
        }
    }
    st.seq = entries.max(max_seq);
    st
}

/// Snapshot `path` on its first mutation this session. Returns the
/// project-relative key when a snapshot was taken, `None` when the file
/// was already recorded or is out of scope (`.sunmao`, outside the
/// project). I/O errors propagate — a silently lost snapshot is a safety
/// hole, not noise.
pub(crate) async fn snapshot_if_new(
    state: &Mutex<CheckpointState>,
    project: &Path,
    path: &Path,
    turn: u64,
) -> Result<Option<String>> {
    let Some(rel) = rel_key(project, path) else {
        return Ok(None);
    };
    let (session_id, seq) = {
        let st = state.lock().unwrap();
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if st.taken.contains(path) || st.taken.contains(&canon) {
            return Ok(None);
        }
        (st.session_id.clone(), st.seq)
    };
    let dir = checkpoint_dir(project, &session_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let existed = tokio::fs::try_exists(path).await.unwrap_or(false);
    let (snapshot, bytes) = if existed {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("cannot snapshot {}", path.display()))?;
        let name = format!("{}-{:016x}.bak", seq, rel_hash(&rel));
        tokio::fs::write(dir.join(&name), &data)
            .await
            .with_context(|| format!("cannot write checkpoint {name}"))?;
        (Some(name), data.len() as u64)
    } else {
        (None, 0)
    };
    let entry = ManifestEntry {
        turn,
        files: vec![ManifestFile {
            path: rel.clone(),
            snapshot,
            bytes,
            existed,
        }],
    };
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("manifest.jsonl"))?;
    file.write_all(&line)?;
    let mut st = state.lock().unwrap();
    st.seq += 1;
    st.taken.insert(path.to_path_buf());
    st.taken
        .insert(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    Ok(Some(rel))
}

/// Scan a session log for user-turn boundaries — the 1-based ordinals
/// `/rewind` addresses. A boundary is a `message` event with role `user`
/// whose content is an actual prompt: hook-injected `[hook context]`
/// lines and folded `<local-shell>` evidence don't count (mid-turn
/// steered messages do — each one begins a new visible user turn).
/// Returns the boundary plus the log's 0-based line index so callers can
/// byte-prefix a fork at exactly that line.
pub fn turn_boundaries(log_path: &Path) -> Vec<TurnBoundary> {
    let Ok(file) = std::fs::File::open(log_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else { break };
        let Ok(ev) = serde_json::from_str::<SessionEvent>(&line) else {
            continue; // corrupt lines are skippable, same as the fold
        };
        let SessionEvent::Message { message } = ev else {
            continue;
        };
        if message.role != Role::User {
            continue;
        }
        let Some(content) = &message.content else {
            continue;
        };
        if content.starts_with("[hook context]") || content.starts_with("<local-shell>") {
            continue;
        }
        let preview = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default();
        out.push(TurnBoundary {
            n: out.len() as u64 + 1,
            line: i,
            preview: preview.chars().take(80).collect(),
        });
    }
    out
}

/// True when `ev` is a turn boundary — the in-memory counterpart of
/// `turn_boundaries`' line scan (used when events are already folded).
pub(crate) fn is_turn_boundary(ev: &SessionEvent) -> bool {
    let SessionEvent::Message { message } = ev else {
        return false;
    };
    message.role == Role::User
        && message
            .content
            .as_deref()
            .is_some_and(|c| !c.starts_with("[hook context]") && !c.starts_with("<local-shell>"))
}

/// Map a 1-based turn number to the line index of its boundary — the byte
/// prefix a session-side rewind keeps. `None` for out-of-range turns.
pub fn boundary_line(log_path: &Path, turn: u64) -> Option<usize> {
    turn_boundaries(log_path)
        .into_iter()
        .find(|b| b.n == turn)
        .map(|b| b.line)
}

/// Copy the first `upto_line` lines of `src` into `dst` — raw bytes, one
/// line at a time, so unknown/corrupt event lines carry over verbatim
/// (the event fold would drop them and a rewind would silently rewrite
/// history it never parsed).
pub fn copy_log_prefix(src: &Path, dst: &Path, upto_line: usize) -> Result<()> {
    let file =
        std::fs::File::open(src).with_context(|| format!("cannot read {}", src.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut out =
        std::fs::File::create(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    let mut buf = Vec::new();
    let mut kept = 0usize;
    while kept < upto_line && reader.read_until(b'\n', &mut buf)? > 0 {
        out.write_all(&buf)?;
        kept += 1;
        buf.clear();
    }
    Ok(())
}

/// Restore working-tree files to their earliest snapshot recorded at or
/// after `upto_turn_exclusive` — the pre-state of the rewound stretch.
/// `existed` entries get their `.bak` bytes written back; `!existed`
/// entries (files the session created) are deleted if present. Returns
/// the project-relative paths touched, in deterministic order.
pub fn restore_files(
    project: &Path,
    session_id: &str,
    upto_turn_exclusive: u64,
) -> Result<Vec<String>> {
    let dir = checkpoint_dir(project, session_id);
    let manifest = dir.join("manifest.jsonl");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(Vec::new());
    };
    // earliest entry per file wins — that's the pre-state the stretch
    // destroyed; later snapshots of the same file are intermediate states.
    let mut firsts: HashMap<String, ManifestFile> = HashMap::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<ManifestEntry>(line) else {
            continue;
        };
        if entry.turn < upto_turn_exclusive {
            continue;
        }
        for f in entry.files {
            firsts.entry(f.path.clone()).or_insert(f);
        }
    }
    let mut restored: Vec<String> = firsts.keys().cloned().collect();
    restored.sort();
    for rel in &restored {
        let f = &firsts[rel];
        let target = project.join(rel);
        if f.existed {
            let snap = f
                .snapshot
                .as_deref()
                .context("manifest entry without snapshot name")?;
            let bytes = std::fs::read(dir.join(snap))
                .with_context(|| format!("missing checkpoint blob {snap}"))?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &bytes)
                .with_context(|| format!("cannot restore {}", target.display()))?;
        } else if target.exists() {
            std::fs::remove_file(&target)
                .with_context(|| format!("cannot remove {}", target.display()))?;
        }
    }
    Ok(restored)
}

/// Move a rewind-fork's ledger lineage: copy `src_id`'s checkpoint dir to
/// `dst_id`, then truncate the manifest to entries before `upto_turn` —
/// the fork keeps exactly the snapshots its prefix could still reference
/// (a `/rewind` inside the fork must restore source-session state, but
/// never claim entries for turns its log doesn't contain).
/// Returns false when the source has no ledger at all.
pub fn fork_checkpoints(
    project: &Path,
    src_id: &str,
    dst_id: &str,
    upto_turn: u64,
) -> Result<bool> {
    let src = checkpoint_dir(project, src_id);
    let manifest = src.join("manifest.jsonl");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return Ok(false);
    };
    let dst = checkpoint_dir(project, dst_id);
    std::fs::create_dir_all(&dst)?;
    for e in crate::sorted_entries(&src) {
        let p = e.path();
        if p.extension().map(|x| x == "bak").unwrap_or(false) {
            std::fs::copy(&p, dst.join(e.file_name()))
                .with_context(|| format!("cannot copy {}", p.display()))?;
        }
    }
    let mut kept = String::new();
    for line in text.lines() {
        let keep = serde_json::from_str::<ManifestEntry>(line)
            .map(|e| e.turn < upto_turn)
            .unwrap_or(true); // unparseable lines are history — keep them
        if keep {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    std::fs::write(dst.join("manifest.jsonl"), kept)?;
    Ok(true)
}

#[cfg(test)]
mod tests;
