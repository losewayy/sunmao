//! Job-lifecycle discrimination tests.
//!
//! Both cases below fail against the pre-registry kernel: a foreground
//! command that hit its budget was SIGKILLed (no job dir, no growing log),
//! and a finished background job left nothing in the conversation — the
//! model had to poll. They are written against the tool surface only
//! (`Bash` + session log + `Observer`), so the same source runs red on the
//! old kernel and green on the new one.

use crate::context::Context;
use crate::session::SessionLog;
use crate::tool::builtin_registry;
use crate::tool::jobs::{self, JobEntry, JobStatus};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct StubLlm;

#[async_trait::async_trait]
impl sunmao_llm::ProviderAdapter for StubLlm {
    async fn stream(
        &self,
        _req: sunmao_llm::ChatRequest<'_>,
    ) -> anyhow::Result<sunmao_llm::DeltaStream> {
        Ok(Box::pin(futures_util::stream::empty()))
    }
}

/// Records every `LiveEvent` it sees, serialized — the wire shape is the
/// only contract a frontend has, so the assertion reads that same JSON.
struct Recorder(std::sync::Mutex<Vec<serde_json::Value>>);

impl crate::agent::Observer for Recorder {
    fn on_event(&self, ev: &crate::agent::LiveEvent) {
        if let Ok(v) = serde_json::to_value(ev) {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).push(v);
        }
    }
}

fn ctx_with(
    dir: &std::path::Path,
    log: SessionLog,
    sink: Option<Arc<dyn crate::agent::Observer>>,
) -> Arc<Context> {
    let mut c = Context::new(
        Arc::new(StubLlm),
        log,
        builtin_registry(),
        dir.to_path_buf(),
    );
    // Pin the POSIX backend: the deno engine is what these cases exercise,
    // and the ambient `SUNMAO_SHELL` / PATH must not swap pwsh in.
    c.shell = crate::tool::ShellBackend::Posix;
    if let Some(s) = sink {
        let _ = c.live_sink.set(s);
    }
    Arc::new(c)
}

/// Every job log under this context's `.sunmao/jobs/`.
fn job_logs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join(".sunmao").join("jobs"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path().join("output.log"))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Async poll — a blocking sleep would starve the very tasks under test on
/// a current-thread runtime.
async fn wait_until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return f();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A foreground command that hits its budget is moved to the background, not
/// killed: the job keeps producing output into its own log, and the tool
/// result hands the model the job id instead of a corpse.
#[tokio::test]
async fn foreground_timeout_moves_to_background_and_keeps_running() {
    let dir = crate::fresh_test_dir("job-fg-timeout");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let ctx = ctx_with(&dir, log, None);

    let res = ctx
        .tools
        .call(
            "Bash",
            &json!({
                "command": "echo first; sleep 4; echo second; sleep 4; echo third",
                "timeout_secs": 2
            })
            .to_string(),
            &ctx,
        )
        .await;

    assert!(
        !res.output.contains("killed"),
        "a foreground timeout must not kill the command: {}",
        res.output
    );
    assert!(
        wait_until(|| !job_logs(&dir).is_empty(), 5).await,
        "no job log appeared for the timed-out foreground command"
    );
    let lp = job_logs(&dir)[0].clone();
    // the panel marker flips with the hand-off: an inline foreground run is
    // hidden until it detaches, then it is a background job like any other
    let meta = read(&lp.with_file_name("job.json"));
    assert!(
        meta.contains("\"foreground\":false"),
        "a detached job must become visible to the jobs panel: {meta}"
    );
    assert!(
        wait_until(|| read(&lp).contains("first"), 5).await,
        "the job log must already hold what the command produced: {:?}",
        read(&lp)
    );
    assert!(
        wait_until(
            || !lp.with_file_name("exit.json").exists() && read(&lp).contains("second"),
            12
        )
        .await,
        "the command must still be running (and logging) after the tool returned: {:?}",
        read(&lp)
    );
    let before = size(&lp);
    assert!(
        wait_until(|| size(&lp) > before, 12).await,
        "the job log must keep growing while the job runs"
    );
    assert!(
        wait_until(|| lp.with_file_name("exit.json").exists(), 20).await,
        "the detached job never wrote exit.json"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A finished job pushes itself into the conversation: a durable `job_done`
/// fact, a tagged user message on the fold, and a live frame for frontends.
/// The model never has to poll.
#[tokio::test]
async fn finished_job_notifies_the_conversation() {
    let dir = crate::fresh_test_dir("job-done");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let rec = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
    let ctx = ctx_with(&dir, log, Some(rec.clone()));

    let res = ctx
        .tools
        .call(
            "Bash",
            &json!({"command": "echo bg-marker", "background": true}).to_string(),
            &ctx,
        )
        .await;
    assert!(res.ok, "{}", res.output);
    assert!(wait_until(|| !job_logs(&dir).is_empty(), 5).await);
    let lp = job_logs(&dir)[0].clone();
    assert!(
        wait_until(|| lp.with_file_name("exit.json").exists(), 20).await,
        "the background job never exited"
    );

    let path = ctx.sessions.lock().await.path().to_path_buf();
    assert!(
        wait_until(|| read(&path).contains("\"job_done\""), 10).await,
        "no durable job_done fact landed in the session log:\n{}",
        read(&path)
    );
    let msgs = ctx.sessions.lock().await.messages().await.unwrap();
    assert!(
        msgs.iter().any(|m| serde_json::to_string(m)
            .map(|s| s.contains("job-result"))
            .unwrap_or(false)),
        "the fold must carry a tagged job-result message: {msgs:?}"
    );
    let frames = rec.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        frames.iter().any(|v| v["type"] == "job_done"),
        "no live job_done frame reached the observer: {frames:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The model-facing job tools. `JobStop`'s permission tier is the dispatch
/// gate's business — it is a classified mutation (`agent/mode.rs`), so
/// read-only mode refuses it and always-ask prompts; the tool itself has no
/// switch of its own.
#[tokio::test]
async fn job_tools_list_snapshot_and_stop() {
    let dir = crate::fresh_test_dir("job-tools");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let ctx = ctx_with(&dir, log, None);

    let res = ctx
        .tools
        .call(
            "Bash",
            &json!({"command": "echo started; sleep 30", "background": true}).to_string(),
            &ctx,
        )
        .await;
    assert!(res.ok, "{}", res.output);
    let id = res
        .output
        .split_whitespace()
        .find(|t| t.starts_with("j-"))
        .expect("the result must name the job")
        .to_string();
    // The spawn returns before the child's first byte reaches the log: the
    // pipe drain still has to run, and on a loaded box (a full-suite run)
    // that lands after this call. Wait for the log to fill; `JobOutput`
    // itself still never waits, which the snapshot read below proves.
    assert!(
        wait_until(|| !job_logs(&dir).is_empty(), 10).await,
        "no job log appeared for {id}"
    );
    let lp = job_logs(&dir)[0].clone();
    assert!(
        wait_until(|| read(&lp).contains("started"), 10).await,
        "the job never wrote its first line: {:?}",
        read(&lp)
    );

    let list = ctx.tools.call("JobList", "{}", &ctx).await;
    assert!(list.ok, "{}", list.output);
    assert!(list.output.contains(&id), "{}", list.output);
    assert!(list.output.contains("running"), "{}", list.output);

    // a snapshot, not a wait: the job is still sleeping, yet this returns
    let out = ctx
        .tools
        .call("JobOutput", &json!({"id": id}).to_string(), &ctx)
        .await;
    assert!(out.ok, "{}", out.output);
    assert!(
        out.output.contains("started"),
        "the snapshot must carry what the job wrote: {}",
        out.output
    );
    assert!(
        out.output.contains("log:"),
        "the log path for a full Read must be there: {}",
        out.output
    );

    let stop = ctx
        .tools
        .call("JobStop", &json!({"id": id}).to_string(), &ctx)
        .await;
    assert!(stop.ok, "{}", stop.output);
    assert!(
        wait_until(|| lp.with_file_name("exit.json").exists(), 20).await,
        "a stopped job must still settle its exit.json"
    );

    let missing = ctx
        .tools
        .call("JobStop", &json!({"id": "j-nope"}).to_string(), &ctx)
        .await;
    assert!(!missing.ok, "{}", missing.output);
    std::fs::remove_dir_all(&dir).ok();
}

/// A finished inline `Bash` call must leave nothing behind. Its output is the
/// tool result; the job dir the kernel opens for it is scratch, and the jobs
/// panel reads the disk — so `ls`/`cat` used to show up as background jobs.
#[tokio::test]
async fn a_completed_foreground_command_leaves_nothing_behind() {
    let dir = crate::fresh_test_dir("job-fg-clean");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let ctx = ctx_with(&dir, log, None);

    let res = ctx
        .tools
        .call(
            "Bash",
            &json!({"command": "echo fg-marker"}).to_string(),
            &ctx,
        )
        .await;
    assert!(res.ok, "{}", res.output);
    assert!(res.output.contains("fg-marker"), "{}", res.output);
    assert!(
        job_logs(&dir).is_empty(),
        "a finished foreground command must leave no job dir: {:?}",
        job_logs(&dir)
    );
    assert!(
        jobs::snapshot(&ctx.jobs).is_empty(),
        "nor a registry row: {:?}",
        jobs::snapshot(&ctx.jobs)
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A fake row for the registry-shape cases — no process, no runtime.
fn fake_job(id: &str, started_at: u64, status: JobStatus) -> JobEntry {
    JobEntry {
        id: id.into(),
        pid: None,
        command: format!("echo {id}"),
        started_at,
        output_path: std::path::PathBuf::from(format!("{id}/output.log")),
        foreground: false,
        status,
        stop: jobs::KillSwitch::new(),
    }
}

/// The registry is a session roster, not an append-only ledger: without a
/// ceiling a long session's `JobList` grows with every command that ever ran.
/// A running job is never evicted — its stop wire is the only handle
/// `JobStop` has.
#[test]
fn the_registry_keeps_a_ceiling_and_never_drops_a_running_job() {
    // 64 is the registry's documented bound (`jobs::MAX_JOBS`).
    const CEILING: usize = 64;
    const FLOOD: u64 = CEILING as u64 * 3;
    let table = jobs::new_table();
    jobs::register(&table, fake_job("j-live", 1, JobStatus::Detached));
    for i in 0..FLOOD {
        jobs::register(
            &table,
            fake_job(&format!("j-past-{i}"), 10 + i, JobStatus::Exited(0)),
        );
    }
    let snap = jobs::snapshot(&table);
    assert!(
        snap.len() <= CEILING,
        "the roster must stay bounded: {} entries",
        snap.len()
    );
    assert!(
        snap.iter().any(|e| e.id == "j-live"),
        "a running job must never be evicted"
    );
    assert!(
        snap.iter().any(|e| e.id == format!("j-past-{}", FLOOD - 1)),
        "the newest entries must survive: {:?}",
        snap.iter().map(|e| &e.id).collect::<Vec<_>>()
    );
}

/// `JobList` pages: the default view is the newest 20 and `limit` overrides
/// it, so a long session's roster never becomes a wall of text.
#[tokio::test]
async fn job_list_pages_and_takes_a_limit() {
    let dir = crate::fresh_test_dir("job-list-page");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let ctx = ctx_with(&dir, log, None);
    const ROWS: u64 = 30;
    {
        let mut t = ctx.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..ROWS {
            t.push(fake_job(
                &format!("j-page-{i}"),
                1_000 + i,
                JobStatus::Exited(0),
            ));
        }
    }
    let rows = |o: &str| o.lines().filter(|l| l.contains(" | exit 0 | ")).count();

    let all = ctx.tools.call("JobList", "{}", &ctx).await;
    assert!(all.ok, "{}", all.output);
    assert_eq!(
        rows(&all.output),
        20,
        "the default page is 20: {}",
        all.output
    );
    assert!(all.output.contains("newest of 30"), "{}", all.output);
    assert!(all.output.contains(&format!("j-page-{}", ROWS - 1)));

    let five = ctx
        .tools
        .call("JobList", &json!({"limit": 5}).to_string(), &ctx)
        .await;
    assert!(five.ok, "{}", five.output);
    assert_eq!(rows(&five.output), 5, "{}", five.output);
    assert!(five.output.contains(&format!("j-page-{}", ROWS - 1)));
    assert!(
        !five.output.lines().any(|l| l.starts_with("j-page-24 ")),
        "an explicit limit must cut to the newest rows: {}",
        five.output
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// The frontends' `!` local shell goes through the SAME job-aware run as the
/// `Bash` tool: reaching the budget moves it to the background instead of
/// killing it, so a long build the user started stays alive and the prompt is
/// free again. The old kill-on-timeout contract survives only in
/// `run_foreground`, for callers that need a verdict now.
#[tokio::test]
async fn a_local_shell_timeout_moves_to_the_background() {
    let dir = crate::fresh_test_dir("local-shell-bg");
    std::fs::create_dir_all(&dir).unwrap();
    let log = SessionLog::open(dir.join("sess"), "s1").await.unwrap();
    let ctx = ctx_with(&dir, log, None);

    let out = crate::tool::run_local_shell(
        "echo local-first; sleep 4; echo local-second",
        dir.clone(),
        2,
        crate::tool::ShellBackend::Posix,
        &ctx,
    )
    .await
    .expect("the local shell must start");

    let text = out.render();
    assert!(
        !text.contains("killed"),
        "a `!` command that reaches its budget must not be killed: {text}"
    );
    assert!(
        text.contains("moved to the background"),
        "the frontend must be handed the job: {text}"
    );
    let (id, log_path) = match &out {
        crate::tool::LocalShell::Detached { id, log_path, .. } => (id.clone(), log_path.clone()),
        crate::tool::LocalShell::Done(run) => {
            panic!(
                "the timeout must detach, not finish (exit {})",
                run.exit_code
            )
        }
    };
    assert!(
        out.ok(),
        "a run that is still going is not a failure: {text}"
    );
    assert_eq!(
        out.record_code(),
        -1,
        "a detached run has no exit code yet; JobDone carries the real one"
    );
    assert!(
        log_path.starts_with(&dir) && log_path.is_file(),
        "the caller is pointed at the job's own log: {log_path:?}"
    );

    // it is a real job from here on: registered, visible, still logging
    assert!(
        jobs::snapshot(&ctx.jobs)
            .iter()
            .any(|e| e.id == id && e.status.is_running()),
        "the detached local shell must sit in the registry: {:?}",
        jobs::snapshot(&ctx.jobs)
    );
    assert!(
        wait_until(|| read(&log_path).contains("local-second"), 15).await,
        "the detached local shell stopped logging: {:?}",
        read(&log_path)
    );
    let meta = read(&log_path.with_file_name("job.json"));
    assert!(
        meta.contains("\"foreground\":false"),
        "a detached local shell must become visible to the jobs panel: {meta}"
    );
    assert!(
        wait_until(|| log_path.with_file_name("exit.json").exists(), 20).await,
        "the detached local shell never settled its exit.json"
    );
    std::fs::remove_dir_all(&dir).ok();
}
