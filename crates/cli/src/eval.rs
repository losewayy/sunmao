//! `sunmao eval <file>` — case-driven regression runner. Each case sends
//! real prompts through the real `AgentLoop` (one turn per `steps[]`
//! entry on a continuing session; the flat `prompt`/`expect` shape is a
//! one-step case) and asserts against the recorded session facts
//! (`SessionLog::events` + the folded transcript), producing a per-case
//! pass/fail report. One fresh session + context per case so cases can't
//! bleed into each other (permissions, read-before-write ledger, hooks
//! all resolve against the case's own cwd).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use sunmao_core::context::MutexRecover;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};

use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionEvent, SessionLog};

use crate::Cli;

mod cases;
use cases::{Case, Expect, parse_cases};

#[derive(clap::Args)]
pub struct EvalArgs {
    /// Case file: a JSON object/array, or JSONL — one case object per line.
    pub file: PathBuf,
    /// Write the per-case results as a JSON array to this path.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

struct CaseResult {
    name: String,
    ok: bool,
    failures: Vec<String>,
    tool_calls: usize,
    turns: u32,
    session: String,
}

/// Silent observer — eval only needs the accumulated assistant text; every
/// other fact is already durable in the session log.
#[derive(Default)]
struct QuietObserver {
    content: std::sync::Mutex<String>,
}

impl Observer for QuietObserver {
    fn on_event(&self, ev: &LiveEvent) {
        if let LiveEvent::Content { text: c } = ev {
            self.content.lock_or_recover().push_str(c);
        }
    }
}

pub async fn run(
    args: &EvalArgs,
    cli: &Cli,
    cwd: &Path,
    preset_roots: &[PathBuf],
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&args.file)
        .with_context(|| format!("cannot read eval file {}", args.file.display()))?;
    let cases = parse_cases(&text).with_context(|| format!("{}", args.file.display()))?;
    if cases.is_empty() {
        bail!("{}: no eval cases", args.file.display());
    }
    // relative case.cwd anchors at the eval file's dir, not the process cwd
    let base_dir = args
        .file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let llm = crate::provider_adapter(cli);

    let mut results = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let r = run_case(i, case, cli, &llm, cwd, &base_dir, preset_roots).await;
        println!("{}", report_line(&r));
        results.push(r);
    }
    let passed = results.iter().filter(|r| r.ok).count();
    println!("eval: {passed}/{} passed", results.len());

    if let Some(path) = &args.report {
        let json = Value::Array(results.iter().map(result_json).collect());
        std::fs::write(path, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("cannot write report {}", path.display()))?;
    }
    if passed != results.len() {
        std::process::exit(1);
    }
    Ok(())
}

/// One case = one fresh session + context, run through the real loop.
/// Setup and turn errors fold into `failures` — one bad case doesn't kill
/// the batch.
async fn run_case(
    index: usize,
    case: &Case,
    cli: &Cli,
    llm: &Arc<dyn sunmao_llm::ProviderAdapter>,
    default_cwd: &Path,
    base_dir: &Path,
    preset_roots: &[PathBuf],
) -> CaseResult {
    // `s-<secs>-c<idx>` — same convention as interactive sessions, the index
    // keeps ids unique when several cases land inside one second
    let session = format!("{}-c{index}", crate::session_id());
    let mut result = CaseResult {
        name: case.name.clone(),
        ok: false,
        failures: Vec::new(),
        tool_calls: 0,
        turns: 1,
        session: session.clone(),
    };

    if let Err(e) = run_case_inner(
        case,
        cli,
        llm,
        default_cwd,
        base_dir,
        preset_roots,
        &session,
        &mut result,
    )
    .await
    {
        result.failures.push(format!("{e:#}"));
    }
    result.ok = result.failures.is_empty();
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_case_inner(
    case: &Case,
    cli: &Cli,
    llm: &Arc<dyn sunmao_llm::ProviderAdapter>,
    default_cwd: &Path,
    base_dir: &Path,
    preset_roots: &[PathBuf],
    session: &str,
    result: &mut CaseResult,
) -> anyhow::Result<()> {
    let case_cwd = match &case.cwd {
        Some(rel) => base_dir.join(rel),
        None => default_cwd.to_path_buf(),
    }
    .canonicalize()
    .context("bad case cwd")?;

    let mut sessions = SessionLog::open(&cli.session_dir, session).await?;
    let registry = builtin_registry();
    let mcp = sunmao_core::mcp::connect_all(&case_cwd, preset_roots).await;
    sunmao_core::mcp::audit_skips(&mcp.skipped, &mut sessions).await;
    for tool in mcp.tools {
        registry.register_boxed(tool);
    }
    let mut ctx_raw = Context::new(llm.clone(), sessions, registry, case_cwd.clone())
        .with_extra_plugin_roots(preset_roots.to_vec());
    ctx_raw.mcp_servers = mcp.servers;
    // --loop outranks every manifest declaration, same as the main session
    if let Some(d) = cli.driver {
        ctx_raw.loop_driver = d;
    }
    ctx_raw.connect_extensions().await;
    // same models.json seam as the main session — the session provider
    // registers as "default" so bare model ids resolve
    ctx_raw.models = Some(Arc::new(sunmao_core::models::ModelResolver::load(
        &case_cwd,
        sunmao_core::models::ProviderDef {
            base_url: cli.base_url.clone(),
            api_key_env: None,
            api_key: Some(cli.api_key.clone()),
            dialect: cli.provider.clone(),
            catalog: Vec::new(),
        },
        "default",
    )));
    let ctx = Arc::new(ctx_raw);
    ctx.hooks
        .fire(
            sunmao_core::hooks::HookEvent::SessionStart,
            &ctx.cwd,
            &sunmao_core::hooks::HookInput {
                source: Some("startup"),
                ..Default::default()
            },
        )
        .await;

    let system = sunmao_core::prompt::PromptAssembler::new(&case_cwd)
        .with_extra_roots(preset_roots)
        .with_driver(ctx.loop_driver)
        .assemble(cli.system.as_deref());
    {
        let mut log = ctx.sessions.lock().await;
        log.append(&SessionEvent::Started {
            model: cli.model.clone(),
            cwd: case_cwd.display().to_string(),
        })
        .await?;
        log.append(&SessionEvent::Message {
            message: sunmao_llm::types::Message::system(system),
        })
        .await?;
    }

    let agent = AgentLoop::new(ctx.clone());
    let obs = QuietObserver::default();
    let multi = case.steps.len() > 1;
    result.turns = case.steps.len() as u32;
    for (i, step) in case.steps.iter().enumerate() {
        // per-step assertions scope to THIS turn — mark the log so prior
        // steps' ToolCalls don't count again, and the observer's buffer so
        // the reply text is just this step's.
        let ev_mark = ctx
            .sessions
            .lock()
            .await
            .events()
            .await
            .map(|e| e.len())
            .unwrap_or(0);
        let obs_mark = obs.content.lock_or_recover().len();

        let outcome = agent.run_turn(&step.prompt, &obs).await?;
        if !matches!(outcome, TurnOutcome::Completed) {
            result
                .failures
                .push(step_fail(multi, i, &format!("turn ended: {outcome:?}")));
        }

        // assertions read the recorded facts, not the live stream
        let tool_names: Vec<String> = {
            let log = ctx.sessions.lock().await;
            log.events()
                .await?
                .iter()
                .skip(ev_mark)
                .filter_map(|ev| match ev {
                    SessionEvent::ToolCall { call, .. } => Some(call.function.name.clone()),
                    _ => None,
                })
                .collect()
        };
        result.tool_calls += tool_names.len();
        let transcript = ctx.sessions.lock().await.messages().await?;
        let mut final_text = transcript
            .iter()
            .rev()
            .find(|m| m.role == sunmao_llm::types::Role::Assistant)
            .and_then(|m| m.content_text())
            .unwrap_or_default();
        if final_text.is_empty() {
            final_text = obs.content.lock_or_recover()[obs_mark..].to_string();
        }
        let names: Vec<&str> = tool_names.iter().map(String::as_str).collect();
        result.failures.extend(
            check(&step.expect, &names, &final_text)
                .into_iter()
                .map(|f| step_fail(multi, i, &f)),
        );
    }
    ctx.hooks
        .fire(
            sunmao_core::hooks::HookEvent::SessionEnd,
            &ctx.cwd,
            &sunmao_core::hooks::HookInput::default(),
        )
        .await;
    // each case is its own session — its extension children end with it
    ctx.ext.shutdown().await;
    Ok(())
}

/// A step-scoped failure reads `step N: <failure>`; single-step cases
/// keep the bare wording (the step index would be noise).
fn step_fail(multi: bool, i: usize, f: &str) -> String {
    if multi {
        format!("step {}: {f}", i + 1)
    } else {
        f.to_string()
    }
}

/// Pure assertion pass — every `expect` key → a failure string or nothing.
fn check(expect: &Expect, tools: &[&str], final_text: &str) -> Vec<String> {
    let mut fails = Vec::new();
    if let Some(want) = &expect.final_contains
        && !final_text.contains(want.as_str())
    {
        fails.push(format!("final_contains {want:?} missing"));
    }
    for t in &expect.tool_called {
        if !tools.contains(&t.as_str()) {
            fails.push(format!("tool {t} never called"));
        }
    }
    for t in &expect.tool_not_called {
        if tools.contains(&t.as_str()) {
            fails.push(format!("tool {t} called despite tool_not_called"));
        }
    }
    if let Some(max) = expect.max_tool_calls
        && tools.len() as u64 > max
    {
        fails.push(format!(
            "{} tool calls exceeds max_tool_calls {max}",
            tools.len()
        ));
    }
    match expect.turns {
        Some(1) | None => {}
        Some(t) => fails.push(format!("turns {t} not supported (v1 runs 1 turn)")),
    }
    fails
}

fn report_line(r: &CaseResult) -> String {
    if r.ok {
        format!("PASS {}  ({} tool calls)", r.name, r.tool_calls)
    } else {
        format!("FAIL {} — {}", r.name, r.failures.join("; "))
    }
}

fn result_json(r: &CaseResult) -> Value {
    json!({
        "name": r.name,
        "ok": r.ok,
        "failures": r.failures,
        "tool_calls": r.tool_calls,
        "turns": r.turns,
        "session": r.session,
    })
}

#[cfg(test)]
mod tests {
    use super::cases::Step;
    use super::*;

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn check_passes_when_all_assertions_hold() {
        let e = Expect {
            final_contains: Some("DONE".into()),
            tool_called: vec!["Read".into()],
            tool_not_called: vec!["Bash".into()],
            max_tool_calls: Some(3),
            turns: Some(1),
        };
        let tools = names(&["Read", "Write"]);
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        assert!(check(&e, &refs, "all good — DONE").is_empty());
    }

    #[test]
    fn check_collects_every_failure() {
        let e = Expect {
            final_contains: Some("DONE".into()),
            tool_called: vec!["Read".into(), "Write".into()],
            tool_not_called: vec!["Bash".into()],
            max_tool_calls: Some(1),
            turns: None,
        };
        let tools = names(&["Bash", "Bash"]);
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let fails = check(&e, &refs, "no marker here");
        assert_eq!(fails.len(), 5);
        assert_eq!(fails[0], "final_contains \"DONE\" missing");
        assert_eq!(fails[1], "tool Read never called");
        assert_eq!(fails[2], "tool Write never called");
        assert_eq!(fails[3], "tool Bash called despite tool_not_called");
        assert_eq!(fails[4], "2 tool calls exceeds max_tool_calls 1");
    }

    #[test]
    fn check_rejects_unsupported_turns() {
        let e = Expect {
            turns: Some(3),
            ..Default::default()
        };
        let fails = check(&e, &[], "");
        assert_eq!(fails, ["turns 3 not supported (v1 runs 1 turn)"]);
    }

    #[test]
    fn report_line_formats_pass_and_fail() {
        let pass = CaseResult {
            name: "a".into(),
            ok: true,
            failures: vec![],
            tool_calls: 2,
            turns: 1,
            session: "s-1-c0".into(),
        };
        assert_eq!(report_line(&pass), "PASS a  (2 tool calls)");
        let fail = CaseResult {
            name: "b".into(),
            ok: false,
            failures: vec![
                "final_contains \"DONE\" missing".into(),
                "tool Write never called".into(),
            ],
            tool_calls: 1,
            turns: 1,
            session: "s-1-c1".into(),
        };
        assert_eq!(
            report_line(&fail),
            "FAIL b — final_contains \"DONE\" missing; tool Write never called"
        );
    }

    /// Scripted provider: each queued response replays its deltas in order.
    /// The same shape as core's MockProvider, local so cli tests don't
    /// reach into core's private fixtures.
    struct StubProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<Vec<sunmao_llm::StreamDelta>>>,
    }

    #[async_trait::async_trait]
    impl sunmao_llm::ProviderAdapter for StubProvider {
        async fn stream(
            &self,
            _req: sunmao_llm::ChatRequest<'_>,
        ) -> anyhow::Result<sunmao_llm::DeltaStream> {
            let deltas = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    vec![
                        sunmao_llm::StreamDelta::Content("done".into()),
                        sunmao_llm::StreamDelta::Finish {
                            reason: Some("stop".into()),
                            usage: None,
                        },
                    ]
                });
            Ok(Box::pin(futures_util::stream::iter(
                deltas.into_iter().map(Ok),
            )))
        }
    }

    /// End-to-end: a two-step case runs two turns on one session, and each
    /// step's expect sees only its own turn's tool calls — step 2's
    /// `tool_not_called`/`max_tool_calls` pass even though step 1 called
    /// Glob earlier in the same log.
    #[tokio::test]
    async fn multi_step_case_scopes_expectations_per_step() {
        use clap::Parser;
        use sunmao_llm::{StreamDelta, ToolCallFragment};

        let dir = std::env::temp_dir().join(format!(
            "sunmao-eval-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let session_dir = dir.join("sessions");
        let cli = Cli::parse_from(["sunmao", "--session-dir", session_dir.to_str().unwrap()]);
        let llm: Arc<dyn sunmao_llm::ProviderAdapter> = Arc::new(StubProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from(vec![
                vec![
                    StreamDelta::ToolCalls(vec![
                        ToolCallFragment {
                            index: 0,
                            id: Some("call_1".into()),
                            name: Some("Glob".into()),
                            arguments: None,
                        },
                        ToolCallFragment {
                            index: 0,
                            arguments: Some("{\"pattern\":\"**/*\"}".into()),
                            ..Default::default()
                        },
                    ]),
                    StreamDelta::Finish {
                        reason: Some("tool_calls".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("listed".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
                vec![
                    StreamDelta::Content("SECOND OK".into()),
                    StreamDelta::Finish {
                        reason: Some("stop".into()),
                        usage: None,
                    },
                ],
            ])),
        });
        let case = Case {
            name: "two-step".into(),
            cwd: None,
            steps: vec![
                Step {
                    prompt: "glob".into(),
                    expect: Expect {
                        tool_called: vec!["Glob".into()],
                        ..Default::default()
                    },
                },
                Step {
                    prompt: "reply".into(),
                    expect: Expect {
                        final_contains: Some("SECOND OK".into()),
                        tool_not_called: vec!["Glob".into()],
                        max_tool_calls: Some(0),
                        ..Default::default()
                    },
                },
            ],
        };
        let mut result = CaseResult {
            name: case.name.clone(),
            ok: false,
            failures: Vec::new(),
            tool_calls: 0,
            turns: 0,
            session: "s-test-c0".into(),
        };
        run_case_inner(&case, &cli, &llm, &dir, &dir, &[], "s-test-c0", &mut result)
            .await
            .unwrap();
        assert_eq!(result.turns, 2);
        assert_eq!(result.tool_calls, 1);
        assert!(result.failures.is_empty(), "{:?}", result.failures);
        // both step prompts landed in ONE session log — steps share the
        // session, they don't each get a fresh one
        let log = std::fs::read_to_string(session_dir.join("s-test-c0.jsonl")).unwrap();
        assert_eq!(log.matches("\"role\":\"user\"").count(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
