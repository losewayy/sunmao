//! Tool registry + the builtin capabilities.
//!
//! Three execution classes (see SPEC §4.3):
//!   native       — in-process Rust (Read/Write/Edit/Grep/Glob/WebFetch…)
//!   managed      — spawned binaries without a shell (Grep's rg, JobOutput's
//!                  log-dir reader)
//!   shell        — `Bash`: model writes a command string; routed through
//!                  deno_task_shell so bash syntax is identical on Windows.

use crate::context::RwLockRecover;
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
    async fn call(&self, args: Value, ctx: &crate::context::Context) -> anyhow::Result<ToolResult>;
}

pub struct ToolRegistry {
    /// Arc storage under a lock: agent `tools:` whitelists rebuild
    /// filtered registries by cloning handles — no impl needs to be
    /// cloneable — and MCP `list_changed` refresh re-registers into a
    /// live Context (Arc<Context> means no `&mut` anywhere).
    /// Never hold a guard across `.await` — `call` clones the Arc first.
    tools: std::sync::RwLock<BTreeMap<String, Arc<dyn ToolImpl>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: std::sync::RwLock::new(BTreeMap::new()),
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
    /// linger as stale declarations).
    pub fn replace_prefixed(&self, prefix: &str, tools: Vec<Box<dyn ToolImpl>>) {
        let mut map = self.tools.write_or_recover();
        map.retain(|k, _| !k.starts_with(prefix));
        for t in tools {
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
    /// child's surface to exactly what the def allows.
    pub fn filtered(&self, names: &[String]) -> ToolRegistry {
        let r = ToolRegistry::new();
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
        ctx: &crate::context::Context,
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
    r.register(HtmlArtifactTool);
    r.register(TodoWriteTool);
    r.register(crate::task::TaskTool);
    r.register(WebFetchTool);
    r.register(SendMessageTool);
    r
}

mod artifact;
mod fs;
mod kind;
mod pwsh;
mod search;
mod sendmsg;
mod shell;
mod todo;
mod webmod;

pub(crate) use artifact::archive_prev;
pub use artifact::{HtmlArtifactTool, artifact_rev};
pub use fs::{EditTool, ReadTool, WriteTool};
pub use kind::{ShellBackend, ShellResolution, ShellSource};
pub use search::{GlobTool, GrepTool};
pub use sendmsg::SendMessageTool;
pub use shell::{BashTool, JobOutputTool, ShellRun, render_run, run_foreground};
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
            async fn call(&self, _a: Value, _c: &Context) -> anyhow::Result<ToolResult> {
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
