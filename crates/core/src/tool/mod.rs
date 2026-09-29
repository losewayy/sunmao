//! Tool registry + the four builtin capabilities.
//!
//! Three execution classes (see SPEC §4.3):
//!   native       — in-process Rust (Read/Write/Edit): structured, auditable
//!   managed      — spawned binaries without a shell (not yet wired)
//!   shell        — `Bash`: model writes a command string; routed through
//!                  deno_task_shell so bash syntax is identical on Windows.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
    tools: BTreeMap<String, Box<dyn ToolImpl>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: impl ToolImpl + 'static) {
        self.tools.insert(tool.name().to_string(), Box::new(tool));
    }

    pub fn register_boxed(&mut self, tool: Box<dyn ToolImpl>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn declarations(&self) -> Vec<Tool> {
        self.tools.values().map(|t| t.decl()).collect()
    }

    pub async fn call(
        &self,
        name: &str,
        args_json: &str,
        ctx: &crate::context::Context,
    ) -> ToolResult {
        let Some(tool) = self.tools.get(name) else {
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
                }
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
    let mut r = ToolRegistry::new();
    r.register(ReadTool);
    r.register(WriteTool);
    r.register(EditTool);
    r.register(BashTool);
    r.register(GlobTool);
    r.register(GrepTool);
    r.register(JobOutputTool);
    r.register(HtmlArtifactTool);
    r.register(crate::task::TaskTool);
    r.register(WebFetchTool);
    r
}

mod artifact;
mod fs;
mod search;
mod shell;
mod webmod;

pub use artifact::HtmlArtifactTool;
pub use fs::{EditTool, ReadTool, WriteTool};
pub use search::{GlobTool, GrepTool};
pub use shell::{render_run, run_foreground, BashTool, JobOutputTool, ShellRun};
pub use webmod::WebFetchTool;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
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

    #[tokio::test]
    async fn edit_refuses_unread_file() {
        let dir = std::env::temp_dir().join(format!("sunmao-test-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("sunmao-test-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir);
        let res = WriteTool
            .call(json!({"path": "n.txt", "content": "x"}), &ctx)
            .await
            .unwrap();
        assert!(res.ok);
    }
}
