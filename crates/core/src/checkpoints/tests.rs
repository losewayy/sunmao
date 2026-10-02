//! Checkpoint tests — the manifest is the assertion surface: every claim
//! the feature makes (first-write-only, new-file markers, byte-exact
//! restore, boundary numbering) is verifiable from the ledger files.

use super::*;
use crate::context::Context;
use crate::context::MutexRecover;
use crate::session::SessionLog;
use crate::tool::{ToolImpl, builtin_registry};
use serde_json::{Value, json};
use std::sync::Arc;
use sunmao_llm::ProviderAdapter;

struct StubLlm;

#[async_trait::async_trait]
impl ProviderAdapter for StubLlm {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

/// A context on a real log so `checkpoint_file` can append events — the
/// session id lands as the log's file stem, same as production.
async fn test_ctx(dir: &std::path::Path, session: &str) -> Arc<Context> {
    let log = SessionLog::open(dir.join(".sunmao/sessions"), session)
        .await
        .unwrap();
    Arc::new(Context::new(
        Arc::new(StubLlm),
        log,
        builtin_registry(),
        dir.to_path_buf(),
    ))
}

fn manifest_lines(dir: &std::path::Path, session: &str) -> Vec<Value> {
    let text =
        std::fs::read_to_string(dir.join(format!(".sunmao/checkpoints/{session}/manifest.jsonl")))
            .unwrap_or_default();
    text.lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// First Write snapshots the pre-state once; a second Write to the same
/// file adds nothing — the ledger is first-write-only by design.
#[tokio::test]
async fn checkpoint_snapshots_first_write_only() {
    let dir = crate::fresh_test_dir("ckpt-first");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "original").unwrap();
    let ctx = test_ctx(&dir, "s-a").await;
    ctx.mark_read(&dir.join("a.txt"));

    let write = crate::tool::WriteTool;
    let res = write
        .call(json!({"path": "a.txt", "content": "v2"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok, "{}", res.output);

    let entries = manifest_lines(&dir, "s-a");
    assert_eq!(entries.len(), 1, "one manifest line per snapshot");
    assert_eq!(entries[0]["files"][0]["path"], "a.txt");
    assert_eq!(entries[0]["files"][0]["existed"], true);
    let snap = entries[0]["files"][0]["snapshot"].as_str().unwrap();
    let bak = std::fs::read(dir.join(format!(".sunmao/checkpoints/s-a/{snap}"))).unwrap();
    assert_eq!(bak, b"original", "the .bak preserves pre-write bytes");

    let res = write
        .call(json!({"path": "a.txt", "content": "v3"}), &ctx)
        .await
        .unwrap();
    assert!(res.ok);
    assert_eq!(manifest_lines(&dir, "s-a").len(), 1, "no re-snapshot");

    // and the durable fact landed in the session log
    let evs = ctx.sessions.lock().await.events().await.unwrap();
    assert!(
        evs.iter().any(|e| matches!(
            e,
            crate::session::SessionEvent::Checkpoint { files, .. } if files == &vec!["a.txt".to_string()]
        )),
        "checkpoint event must be durable"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Writing a file that doesn't exist yet still records a manifest entry —
/// `existed:false`, no blob — so a rewind can delete what the turn created.
#[tokio::test]
async fn checkpoint_marks_new_files() {
    let dir = crate::fresh_test_dir("ckpt-new");
    std::fs::create_dir_all(dir.join("gen")).unwrap();
    let ctx = test_ctx(&dir, "s-b").await;

    let write = crate::tool::WriteTool;
    write
        .call(json!({"path": "gen/out.txt", "content": "fresh"}), &ctx)
        .await
        .unwrap();

    let entries = manifest_lines(&dir, "s-b");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["files"][0]["path"], "gen/out.txt");
    assert_eq!(entries[0]["files"][0]["existed"], false);
    assert!(entries[0]["files"][0]["snapshot"].is_null());
    assert!(
        crate::sorted_entries(&dir.join(".sunmao/checkpoints/s-b"))
            .iter()
            .all(|e| e.file_name() == "manifest.jsonl"),
        "no .bak for a file that never existed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The boundary classifier: real prompts are 1-based ordinals; hook-injected
/// context, folded local-shell output, task results and assistant turns
/// must not count — a `/rewind 2` must land on the second thing the user
/// actually typed.
#[test]
fn rewind_turn_boundaries() {
    let dir = crate::fresh_test_dir("ckpt-bounds");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("s.jsonl");
    let lines = [
        r#"{"type":"started","model":"m","cwd":"x"}"#,
        r#"{"type":"message","message":{"role":"system","content":"id"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"first real prompt"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"[hook context] injected"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"<local-shell>\n$ ls\n</local-shell>"}}"#,
        r#"{"type":"task_done","id":"sub-1","ok":true,"output":"x"}"#,
        r#"{"type":"message","message":{"role":"assistant","content":"hi"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"second real\nmultiline"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"steered mid-turn"}}"#,
        "corrupt line {{{",
    ];
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();

    let b = turn_boundaries(&log);
    assert_eq!(b.len(), 3, "hook/local-shell/taskdone lines never count");
    assert_eq!(b[0].n, 1);
    assert_eq!(b[0].line, 2);
    assert_eq!(b[0].preview, "first real prompt");
    assert_eq!(b[1].n, 2);
    assert_eq!(b[1].line, 7);
    assert_eq!(b[1].preview, "second real");
    assert_eq!(b[2].n, 3, "steered messages are boundaries too");
    assert_eq!(b[2].line, 8);
    assert_eq!(boundary_line(&log, 2), Some(7));
    assert_eq!(boundary_line(&log, 9), None);
    std::fs::remove_dir_all(&dir).ok();
}

/// Restore folds the ledger back: `existed` files get pre-write bytes,
/// `!existed` files the stretch created are deleted, and files snapshotted
/// *before* the boundary stay untouched.
#[tokio::test]
async fn restore_returns_pre_state() {
    let dir = crate::fresh_test_dir("ckpt-restore");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), "v1").unwrap();
    std::fs::write(dir.join("gone.txt"), "v1").unwrap();
    let ctx = test_ctx(&dir, "s-c").await;
    ctx.mark_read(&dir.join("keep.txt"));
    ctx.mark_read(&dir.join("gone.txt"));

    let write = crate::tool::WriteTool;
    // turn 1 touches keep.txt only
    ctx.checkpoints.lock_or_recover().turn = 1;
    write
        .call(json!({"path": "keep.txt", "content": "v2"}), &ctx)
        .await
        .unwrap();
    // turn 2 rewrites keep.txt again (no new snapshot — first-write-only),
    // destroys gone.txt and creates made.txt
    ctx.checkpoints.lock_or_recover().turn = 2;
    write
        .call(json!({"path": "keep.txt", "content": "v3"}), &ctx)
        .await
        .unwrap();
    write
        .call(json!({"path": "gone.txt", "content": "wiped"}), &ctx)
        .await
        .unwrap();
    write
        .call(json!({"path": "made.txt", "content": "new"}), &ctx)
        .await
        .unwrap();

    // rewind to before turn 2: keep.txt reverts to its earliest snapshot
    // inside the stretch (turn 1's snapshot is excluded — wait: turn 1's
    // entry is BEFORE the boundary so keep.txt only has the boundary-side
    // state recorded at turn 2 … which is also its first-and-only snapshot).
    let restored = restore_files(&dir, "s-c", 2).unwrap();
    assert_eq!(
        restored,
        vec!["gone.txt".to_string(), "made.txt".to_string()]
    );
    assert_eq!(std::fs::read_to_string(dir.join("gone.txt")).unwrap(), "v1");
    assert!(!dir.join("made.txt").exists());
    // keep.txt was first snapshot at turn 1 — outside the rewound stretch,
    // so its turn-2 overwrite stays.
    assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "v3");

    // rewinding to before turn 1 restores keep.txt's true pre-state
    let restored = restore_files(&dir, "s-c", 1).unwrap();
    assert!(restored.contains(&"keep.txt".to_string()));
    assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "v1");
    std::fs::remove_dir_all(&dir).ok();
}

/// `snapshot_if_new` refuses `.sunmao` internals — the ledger must never
/// snapshot itself or session logs.
#[tokio::test]
async fn checkpoint_skips_sunmao_internals() {
    let dir = crate::fresh_test_dir("ckpt-skip");
    std::fs::create_dir_all(dir.join(".sunmao/sessions")).unwrap();
    let ctx = test_ctx(&dir, "s-d").await;
    let target = dir.join(".sunmao/sessions/s-d.jsonl");
    ctx.checkpoint_file(&target).await.unwrap();
    assert!(manifest_lines(&dir, "s-d").is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

/// A rewind fork's provenance row lands on the fork file — the copied
/// prefix is followed by a `rewind` audit fact naming the source log and
/// the boundary turn, so replay can tell "continued after a rewind" from
/// "started there".
#[tokio::test]
async fn rewind_fork_stamps_provenance() {
    let dir = crate::fresh_test_dir("ckpt-prov");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("s-src.jsonl");
    let lines = [
        r#"{"type":"started","model":"m","cwd":"x"}"#,
        r#"{"type":"message","message":{"role":"user","content":"first"}}"#,
        r#"{"type":"message","message":{"role":"assistant","content":"ok"}}"#,
        r#"{"type":"message","message":{"role":"user","content":"second"}}"#,
    ];
    std::fs::write(&src, lines.join("\n") + "\n").unwrap();
    let dst = dir.join("s-fork.jsonl");
    copy_log_prefix(&src, &dst, 3).unwrap(); // up to boundary line 3

    let mut log = SessionLog::open_path(&dst).await.unwrap();
    stamp_rewind_provenance(&mut log, "s-src", 2, "session").await;

    let evs = log.events().await.unwrap();
    assert_eq!(evs.len(), 4, "3 prefix rows + the provenance row");
    assert!(
        matches!(
            evs.last(),
            Some(crate::session::SessionEvent::Hook { event, detail })
                if event == "rewind" && detail == "from s-src at turn 2 (session)"
        ),
        "the fork's tail must name where it came from — {evs:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
