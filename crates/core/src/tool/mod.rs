//! Tool registry + the builtin capabilities.
//!
//! Three execution classes (see SPEC §4.3):
//!   native       — in-process Rust (Read/Write/Edit/Grep/Glob/WebFetch…)
//!   managed      — spawned binaries without a shell (Grep's rg, JobOutput's
//!                  log-dir reader)
//!   shell        — `Bash`: model writes a command string; the resolved
//!                  backend supplies either PowerShell or POSIX syntax.

use crate::context::{MutexRecover, RwLockRecover};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use serde_json::Value;
use sunmao_llm::types::Tool;

pub struct ToolResult {
    pub output: String,
    /// Failed tool calls are still delivered to the model as output —
    /// models recover from errors better when the error is legible.
    pub ok: bool,
}

#[async_trait::async_trait]
pub trait ToolImpl: Send + Sync {
    fn name(&self) -> &'static str;
    fn decl(&self) -> Tool;
    async fn call(
        &self,
        args: Value,
        ctx: &Arc<crate::context::Context>,
    ) -> anyhow::Result<ToolResult>;
}

pub struct ToolRegistry {
    /// Arc storage under a lock: agent `tools:` whitelists rebuild
    /// filtered registries by cloning handles — no impl needs to be
    /// cloneable — and MCP `list_changed` refresh re-registers into a
    /// live Context (Arc<Context> means no `&mut` anywhere).
    /// Never hold a guard across `.await` — `call` clones the Arc first.
    tools: std::sync::RwLock<BTreeMap<String, Arc<dyn ToolImpl>>>,
    /// The `tools:` whitelist a filtered registry was built from — bulk
    /// refresh (`replace_prefixed`, the MCP `list_changed` path) inserts
    /// only names on it; an unfiltered registry refreshes unrestricted.
    /// Without it a child def's whitelist held until the first catalog
    /// bump, then the drain re-registered the server's whole surface.
    allow: Option<std::collections::BTreeSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::sync::RwLock::new(BTreeMap::new()),
            allow: None,
        }
    }

    pub fn register(&self, tool: impl ToolImpl + 'static) {
        self.tools
            .write()
            .unwrap()
            .insert(tool.name().to_string(), Arc::new(tool));
    }

    pub fn register_boxed(&self, tool: Box<dyn ToolImpl>) {
        self.tools
            .write()
            .unwrap()
            .insert(tool.name().to_string(), tool.into());
    }

    /// Register an already-shared impl — extension tools live on the
    /// `ExtRegistry` (the child owns the wire); the session registry just
    /// borrows the same handle.
    pub fn register_arc(&self, tool: Arc<dyn ToolImpl>) {
        self.tools
            .write()
            .unwrap()
            .insert(tool.name().to_string(), tool);
    }

    /// Swap the tools one prefix exposes — an MCP `list_changed` refresh
    /// drops the old `mcp__{server}__*` entries and registers the fresh
    /// catalog in one pass (removed tools really go away, they don't
    /// linger as stale declarations). A filtered registry only takes the
    /// names its `tools:` whitelist allowed — otherwise a bump would
    /// smuggle the whole catalog past the child def's surface.
    pub fn replace_prefixed(&self, prefix: &str, tools: Vec<Box<dyn ToolImpl>>) {
        let mut map = self.tools.write_or_recover();
        map.retain(|k, _| !k.starts_with(prefix));
        for t in tools {
            if self.allow.as_ref().is_some_and(|a| !a.contains(t.name())) {
                continue;
            }
            map.insert(t.name().to_string(), t.into());
        }
    }

    pub fn declarations(&self) -> Vec<Tool> {
        self.tools
            .read()
            .unwrap()
            .values()
            .map(|t| t.decl())
            .collect()
    }

    /// A registry with only `names` — agent `tools:` whitelists trim a
    /// child's surface to exactly what the def allows. The whitelist rides
    /// along on `allow` so a later bulk refresh (MCP `list_changed`) can't
    /// re-add names the def never permitted.
    pub fn filtered(&self, names: &[String]) -> ToolRegistry {
        let r = ToolRegistry {
            tools: std::sync::RwLock::new(BTreeMap::new()),
            allow: Some(names.iter().cloned().collect()),
        };
        let map = self.tools.read_or_recover();
        for n in names {
            if let Some(t) = map.get(n) {
                r.register_arc(t.clone());
            }
        }
        r
    }

    /// Drop one tool — the depth cap strips `Task` from leaf children so
    /// the model never sees a spawner it can't legally use.
    pub fn remove(&self, name: &str) {
        self.tools.write_or_recover().remove(name);
    }

    pub async fn call(
        &self,
        name: &str,
        args_json: &str,
        ctx: &std::sync::Arc<crate::context::Context>,
    ) -> ToolResult {
        // clone the Arc out from under the lock — a std::sync guard isn't
        // Send, and a list_changed refresh mid-call must not deadlock on it
        let tool = self.tools.read_or_recover().get(name).cloned();
        let Some(tool) = tool else {
            return ToolResult {
                output: format!("unknown tool: {name}"),
                ok: false,
            };
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("invalid arguments for {name}: {e}"),
                    ok: false,
                };
            }
        };
        match tool.call(args, ctx).await {
            Ok(r) => r,
            Err(e) => ToolResult {
                output: format!("{name} failed: {e:#}"),
                ok: false,
            },
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A completed exec doesn't guarantee the pipes hit EOF — a detached
/// grandchild (`cmd /c start /b`, a daemonizing child) keeps the write end
/// open forever. Drains get this grace after exec; past it we keep the
/// partial bytes and mark the run's `ended` (the abandoned blocking
/// threads are bounded garbage, not a hang). Shared by the shell tools and
/// the hook executor.
pub(crate) const PIPE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bytes a `pipe_to` drain thread accumulates — shared so a timed-out
/// drain still yields what arrived (`pipe_to` takes `&mut dyn Write`,
/// an owned Vec would be unrecoverable past the timeout).
#[derive(Clone, Default)]
pub(crate) struct SharedBuf(pub(crate) std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedBuf {
    pub(crate) fn text(&self) -> String {
        crate::console::console_text(&self.0.lock_or_recover())
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock_or_recover().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Canonical builtin set for v0.1.
pub fn builtin_registry() -> ToolRegistry {
    let r = ToolRegistry::new();
    r.register(ReadTool);
    r.register(WriteTool);
    r.register(EditTool);
    r.register(BashTool);
    r.register(GlobTool);
    r.register(GrepTool);
    r.register(JobOutputTool);
    r.register(JobListTool);
    r.register(JobStopTool);
    r.register(HtmlArtifactTool);
    r.register(TodoWriteTool);
    r.register(UpdateGoalTool);
    r.register(crate::task::TaskTool);
    // registered unconditionally so filtered surfaces can keep it —
    // `Context::advertised_tools` hides it everywhere except the fusion
    // Lead (same posture as SearchTools).
    r.register(crate::agent::fusion::FusionExecuteTool);
    r.register(WebFetchTool);
    r.register(SendMessageTool);
    r.register(RunCodeTool);
    r.register(SearchToolsTool);
    r
}

mod artifact;
mod foreground;
mod fs;
mod goal;
pub(crate) mod jobs;
mod jobtools;
mod kind;
mod ptc;
mod pwsh;
mod search;
mod sendmsg;
mod shell;
mod timeout;
mod todo;
mod webmod;

pub(crate) use artifact::archive_prev;
pub use artifact::{HtmlArtifactTool, artifact_rev};
pub use foreground::{LocalShell, run_local_shell};
pub use fs::{EditTool, ReadTool, WriteTool};
pub(crate) use goal::GOAL_LINE_PREFIX;
#[cfg(test)]
pub(crate) use goal::apply_blocker;
pub use goal::{BLOCKED_MIN_ROUNDS, GoalState, GoalStatus, UpdateGoalTool, status_name};
pub use jobs::{JobEntry, JobStatus, JobTable};
pub use jobtools::{JobListTool, JobOutputTool, JobStopTool};
pub use kind::{ShellBackend, ShellResolution, ShellSource};
pub use ptc::{RunCodeTool, SearchToolsTool};
pub use search::{GlobTool, GrepTool};
pub use sendmsg::SendMessageTool;
pub use shell::{BashTool, ShellRun, render_run, run_foreground};
pub(crate) use todo::TODOS_LINE_PREFIX;
pub use todo::{
    TodoItem, TodoStatus, TodoWriteTool, inject_text as todos_inject_text, render as render_todos,
};
pub use webmod::WebFetchTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{Context, MutexRecover};
    use crate::session::SessionLog;
    use serde_json::json;
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

    fn test_ctx(dir: &std::path::Path) -> Arc<Context> {
        Arc::new(Context::new(
            Arc::new(StubLlm),
            SessionLog::ephemeral(),
            builtin_registry(),
            dir.to_path_buf(),
        ))
    }

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        crate::fresh_test_dir(tag)
    }

    #[tokio::test]
    async fn edit_refuses_unread_file() {
        let dir = fresh_dir("edit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.txt"), "hello world").unwrap();
        let ctx = test_ctx(&dir);
        let tool = EditTool;
        let res = tool
            .call(
                json!({"path": "t.txt", "old_string": "hello", "new_string": "bye"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.output.contains("not Read this session"));
        // after a Read, the same edit must succeed
        let read = ReadTool;
        read.call(json!({"path": "t.txt"}), &ctx).await.unwrap();
        let res = tool
            .call(
                json!({"path": "t.txt", "old_string": "hello", "new_string": "bye"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.ok);
        assert_eq!(
            std::fs::read_to_string(dir.join("t.txt")).unwrap(),
            "bye world"
        );
    }

    /// Normalized matching must reach mid-line spans: an old_string whose
    /// interior whitespace drifts from the file (`f(  a )` vs `f( a )`)
    /// fails every line-end candidate on a line with trailing code — the
    /// match can only end mid-line, at a token boundary.
    #[tokio::test]
    async fn edit_normalized_match_ends_mid_line() {
        let dir = fresh_dir("edit-norm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t.txt"), "let x = f(  a ); tail();\n").unwrap();
        let ctx = test_ctx(&dir);
        ReadTool.call(json!({"path": "t.txt"}), &ctx).await.unwrap();
        let res = EditTool
            .call(
                json!({"path": "t.txt", "old_string": "f( a );", "new_string": "g(b);"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(res.ok, "{}", res.output);
        assert_eq!(
            std::fs::read_to_string(dir.join("t.txt")).unwrap(),
            "let x = g(b); tail();\n"
        );
    }

    #[tokio::test]
    async fn write_allows_new_file_without_read() {
        let dir = fresh_dir("w");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let res = WriteTool
            .call(json!({"path": "n.txt", "content": "x"}), &ctx)
            .await
            .unwrap();
        assert!(res.ok);
    }

    /// v0.2 acceptance — a crashed/dying tool backend (MCP child exit,
    /// transport error) surfaces as a failed ToolResult, never an
    /// Err that aborts the turn.
    #[tokio::test]
    async fn tool_backend_failure_becomes_failed_result() {
        struct DyingTool;
        #[async_trait::async_trait]
        impl crate::tool::ToolImpl for DyingTool {
            fn name(&self) -> &'static str {
                "mcp__dead__thing"
            }
            fn decl(&self) -> Tool {
                Tool::function("mcp__dead__thing", "dies", json!({}))
            }
            async fn call(&self, _a: Value, _c: &Arc<Context>) -> anyhow::Result<ToolResult> {
                anyhow::bail!("mcp call_tool failed: peer closed")
            }
        }
        let dir = fresh_dir("die");
        std::fs::create_dir_all(&dir).unwrap();
        let reg = builtin_registry();
        reg.register(DyingTool);
        let ctx = Arc::new(Context::new(
            Arc::new(StubLlm),
            SessionLog::ephemeral(),
            reg,
            dir.to_path_buf(),
        ));
        let res = ctx.tools.call("mcp__dead__thing", "{}", &ctx).await;
        assert!(!res.ok);
        assert!(res.output.contains("mcp__dead__thing failed"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// TodoWrite is a whole-list replace: ctx snapshot + durable Todos
    /// event land together, and the echo renders what now stands.
    #[tokio::test]
    async fn todowrite_replaces_and_records() {
        let dir = fresh_dir("todo");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let res = ctx
            .tools
            .call(
                "TodoWrite",
                &json!({"todos": [
                    {"content": "first", "status": "done"},
                    {"content": "second", "status": "in_progress"},
                    {"content": "third", "status": "pending"}
                ]})
                .to_string(),
                &ctx,
            )
            .await;
        assert!(res.ok, "{}", res.output);
        let items = ctx.todos.lock_or_recover().clone();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].status, TodoStatus::InProgress);
        // durable fact, same payload
        let events = ctx.sessions.lock().await.events().await.unwrap();
        match events.last() {
            Some(crate::session::SessionEvent::Todos { items: e }) => assert_eq!(e, &items),
            other => panic!("expected Todos event, got {other:?}"),
        }
        // replace-all: the next write owns the whole list
        let res2 = ctx
            .tools
            .call("TodoWrite", &json!({"todos": []}).to_string(), &ctx)
            .await;
        assert!(res2.ok);
        assert!(ctx.todos.lock_or_recover().is_empty());
    }

    /// More than one in_progress is demoted, not refused — the tool is a
    /// helper, not a gate.
    #[tokio::test]
    async fn todowrite_demotes_extra_in_progress() {
        let dir = fresh_dir("tododem");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let res = ctx
            .tools
            .call(
                "TodoWrite",
                &json!({"todos": [
                    {"content": "a", "status": "in_progress"},
                    {"content": "b", "status": "in_progress"}
                ]})
                .to_string(),
                &ctx,
            )
            .await;
        assert!(res.ok);
        assert!(res.output.contains("demoted"));
        let items = ctx.todos.lock_or_recover().clone();
        assert_eq!(
            items.iter().map(|t| t.status).collect::<Vec<_>>(),
            vec![TodoStatus::InProgress, TodoStatus::Pending]
        );
        // blank content refuses loudly instead
        let bad = ctx
            .tools
            .call(
                "TodoWrite",
                &json!({"todos": [{"content": "  ", "status": "pending"}]}).to_string(),
                &ctx,
            )
            .await;
        assert!(!bad.ok);
    }

    /// A `tools:`-filtered registry must keep its whitelist through an MCP
    /// catalog refresh — `replace_prefixed` used to skip the whitelist check
    /// and insert the server's whole surface into a restricted child's
    /// registry the moment the server pushed a `list_changed`.
    #[test]
    fn filtered_registry_keeps_whitelist_across_mcp_refresh() {
        struct FakeMcp(&'static str);
        #[async_trait::async_trait]
        impl crate::tool::ToolImpl for FakeMcp {
            fn name(&self) -> &'static str {
                self.0
            }
            fn decl(&self) -> Tool {
                Tool::function(self.0, "fake mcp tool", json!({}))
            }
            async fn call(&self, _a: Value, _c: &Arc<Context>) -> anyhow::Result<ToolResult> {
                anyhow::bail!("unused")
            }
        }
        let reg = builtin_registry().filtered(&["Glob".into(), "mcp__srv__a".into()]);
        reg.replace_prefixed(
            "mcp__srv__",
            vec![
                Box::new(FakeMcp("mcp__srv__a")),
                Box::new(FakeMcp("mcp__srv__b")),
            ],
        );
        let names: Vec<String> = reg
            .declarations()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(
            names.iter().any(|n| n == "mcp__srv__a"),
            "whitelisted MCP tool survives a refresh — {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "mcp__srv__b"),
            "a name outside the def's tools: list must not enter via refresh — {names:?}"
        );
    }

    /// A file-backed log's last Todos event seeds Context::new — resume
    /// continuity without replaying the whole fold.
    #[tokio::test]
    async fn todos_reseed_from_persisted_log() {
        let dir = fresh_dir("todoseed");
        std::fs::create_dir_all(&dir).unwrap();
        let sdir = dir.join("sess");
        let mut log = SessionLog::open(&sdir, "s1").await.unwrap();
        log.append(&crate::session::SessionEvent::Todos {
            items: vec![TodoItem {
                content: "stale".into(),
                status: TodoStatus::Pending,
            }],
        })
        .await
        .unwrap();
        log.append(&crate::session::SessionEvent::Todos {
            items: vec![TodoItem {
                content: "fresh".into(),
                status: TodoStatus::InProgress,
            }],
        })
        .await
        .unwrap();
        drop(log);
        let log2 = SessionLog::open(&sdir, "s1").await.unwrap();
        let ctx = Arc::new(Context::new(
            Arc::new(StubLlm),
            log2,
            builtin_registry(),
            dir.clone(),
        ));
        let items = ctx.todos.lock_or_recover().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "fresh");
        assert_eq!(items[0].status, TodoStatus::InProgress);
        std::fs::remove_dir_all(&dir).ok();
    }
}
