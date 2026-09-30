//! `sunmao eval <file>` — case-driven regression runner. Each case sends a
//! real prompt through the real `AgentLoop` and asserts against the recorded
//! session facts (`SessionLog::events` + the folded transcript), producing a
//! per-case pass/fail report. One fresh session + context per case so cases
//! can't bleed into each other (permissions, read-before-write ledger, hooks
//! all resolve against the case's own cwd).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use serde_json::{json, Value};

use sunmao_core::agent::{AgentLoop, LiveEvent, Observer, TurnOutcome};
use sunmao_core::tool::builtin_registry;
use sunmao_core::{Context, SessionEvent, SessionLog};

use crate::Cli;

#[derive(clap::Args)]
pub struct EvalArgs {
    /// Case file: a JSON object/array, or JSONL — one case object per line.
    pub file: PathBuf,
    /// Write the per-case results as a JSON array to this path.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// One eval case — `expect` vocabulary stays deliberately small (see
/// docs/CONFIG.md): final_contains / tool_called / tool_not_called /
/// max_tool_calls / turns.
#[derive(Debug)]
struct Case {
    name: String,
    prompt: String,
    cwd: Option<String>,
    expect: Expect,
}

#[derive(Default, Debug)]
struct Expect {
    final_contains: Option<String>,
    tool_called: Vec<String>,
    tool_not_called: Vec<String>,
    max_tool_calls: Option<u64>,
    turns: Option<u64>,
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
        if let LiveEvent::Content(c) = ev {
            self.content.lock().unwrap().push_str(c);
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

    let sessions = SessionLog::open(&cli.session_dir, session).await?;
    let mut registry = builtin_registry();
    for tool in sunmao_core::mcp::connect_all(&case_cwd, preset_roots).await {
        registry.register_boxed(tool);
    }
    let mut ctx_raw = Context::new(llm.clone(), sessions, registry, case_cwd.clone())
        .with_extra_plugin_roots(preset_roots.to_vec());
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
    let outcome = agent.run_turn(&case.prompt, &obs).await?;
    if !matches!(outcome, TurnOutcome::Completed) {
        result.failures.push(format!("turn ended: {outcome:?}"));
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

    // assertions read the recorded facts, not the live stream
    let log = ctx.sessions.lock().await;
    let events = log.events().await?;
    let tool_names: Vec<String> = events
        .iter()
        .filter_map(|ev| match ev {
            SessionEvent::ToolCall { call, .. } => Some(call.function.name.clone()),
            _ => None,
        })
        .collect();
    result.tool_calls = tool_names.len();
    let transcript = log.messages().await?;
    drop(log);
    let mut final_text = transcript
        .iter()
        .rev()
        .find(|m| m.role == sunmao_llm::types::Role::Assistant)
        .and_then(|m| m.content.clone())
        .unwrap_or_default();
    if final_text.is_empty() {
        final_text = obs.content.lock().unwrap().clone();
    }
    let names: Vec<&str> = tool_names.iter().map(String::as_str).collect();
    result
        .failures
        .extend(check(&case.expect, &names, &final_text));
    Ok(())
}

/// Pure assertion pass — every `expect` key → a failure string or nothing.
fn check(expect: &Expect, tools: &[&str], final_text: &str) -> Vec<String> {
    let mut fails = Vec::new();
    if let Some(want) = &expect.final_contains {
        if !final_text.contains(want.as_str()) {
            fails.push(format!("final_contains {want:?} missing"));
        }
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
    if let Some(max) = expect.max_tool_calls {
        if tools.len() as u64 > max {
            fails.push(format!(
                "{} tool calls exceeds max_tool_calls {max}",
                tools.len()
            ));
        }
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

/// Case file = one JSON object, a JSON array of objects, or JSONL (one
/// object per line; blank lines and `#`/`//` comments skipped). Whole-file
/// JSON wins so a single-line file still parses as one case.
fn parse_cases(text: &str) -> anyhow::Result<Vec<Case>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| case_from_value(v, &format!("case[{i}]")))
            .collect(),
        Ok(v) => Ok(vec![case_from_value(&v, "case")?]),
        Err(_) => {
            let mut out = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                    continue;
                }
                let v: Value = serde_json::from_str(line)
                    .with_context(|| format!("line {}: not a JSON object", i + 1))?;
                out.push(case_from_value(&v, &format!("line {}", i + 1))?);
            }
            Ok(out)
        }
    }
}

fn case_from_value(v: &Value, at: &str) -> anyhow::Result<Case> {
    let obj = v
        .as_object()
        .with_context(|| format!("{at}: not an object"))?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .with_context(|| format!("{at}: missing \"name\""))?
        .to_string();
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)
        .with_context(|| format!("{at}: missing \"prompt\""))?
        .to_string();
    let cwd = obj.get("cwd").and_then(Value::as_str).map(str::to_string);
    let expect = match obj.get("expect") {
        None => Expect::default(),
        Some(e) => expect_from_value(e, at)?,
    };
    Ok(Case {
        name,
        prompt,
        cwd,
        expect,
    })
}

fn expect_from_value(v: &Value, at: &str) -> anyhow::Result<Expect> {
    let obj = v
        .as_object()
        .with_context(|| format!("{at}: \"expect\" is not an object"))?;
    let str_list = |key: &str| -> anyhow::Result<Vec<String>> {
        match obj.get(key) {
            None => Ok(Vec::new()),
            Some(v) => v
                .as_array()
                .with_context(|| format!("{at}: \"{key}\" is not an array"))?
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .with_context(|| format!("{at}: \"{key}\" entry is not a string"))
                })
                .collect(),
        }
    };
    Ok(Expect {
        final_contains: obj
            .get("final_contains")
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_called: str_list("tool_called")?,
        tool_not_called: str_list("tool_not_called")?,
        max_tool_calls: obj.get("max_tool_calls").and_then(Value::as_u64),
        turns: obj.get("turns").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_single_object() {
        let cases = parse_cases(r#"{"name":"a","prompt":"do x"}"#).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "a");
        assert_eq!(cases[0].prompt, "do x");
        assert!(cases[0].cwd.is_none());
    }

    #[test]
    fn parses_array() {
        let cases =
            parse_cases(r#"[{"name":"a","prompt":"x"},{"name":"b","prompt":"y","cwd":"f"}]"#)
                .unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[1].cwd.as_deref(), Some("f"));
    }

    #[test]
    fn parses_jsonl_skipping_blanks_and_comments() {
        let text = "# heading\n\n{\"name\":\"a\",\"prompt\":\"x\"}\n// c++ style\n{\"name\":\"b\",\"prompt\":\"y\"}\n";
        let cases = parse_cases(text).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[1].name, "b");
    }

    #[test]
    fn malformed_jsonl_names_the_line() {
        let err = parse_cases("{\"name\":\"a\",\"prompt\":\"x\"}\n{oops}\n").unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn malformed_array_element_names_the_index() {
        let err = parse_cases(r#"[{"name":"a","prompt":"x"},{"prompt":"y"}]"#).unwrap_err();
        assert!(err.to_string().contains("case[1]"), "{err}");
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn full_case_shape_parses() {
        let text = r#"{
            "name": "uses-read-before-write",
            "prompt": "fix the typo in note.txt then tell me DONE",
            "cwd": "fixtures/case1",
            "expect": {
                "final_contains": "DONE",
                "tool_called": ["Read", "Write"],
                "tool_not_called": ["Bash"],
                "max_tool_calls": 10,
                "turns": 1
            }
        }"#;
        let cases = parse_cases(text).unwrap();
        let e = &cases[0].expect;
        assert_eq!(e.final_contains.as_deref(), Some("DONE"));
        assert_eq!(e.tool_called, ["Read", "Write"]);
        assert_eq!(e.tool_not_called, ["Bash"]);
        assert_eq!(e.max_tool_calls, Some(10));
        assert_eq!(e.turns, Some(1));
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
}
