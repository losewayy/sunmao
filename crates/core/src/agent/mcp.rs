//! MCP side-channel plumbing on `AgentLoop` — the parts that keep a
//! session's view of its servers current between turns.
//!
//! `SessionHandler` (mcp/handler.rs) mutates the shared catalogs on
//! server push traffic; `run_turn_blocks` drains the deltas at the turn
//! fence so a `list_changed` mid-turn can't swap the registry between a
//! ToolCall and its ToolResult. Prompts resolve the other way: the
//! frontends ask `/srv:prompt` here at submit time, and a miss falls
//! through to the file-command lookup so plugin `.md`s keep working.

use rmcp::model::GetPromptRequestParams;

use super::{AgentLoop, LiveEvent, Observer};
use crate::session::SessionEvent;

impl AgentLoop {
    /// Apply whatever the MCP servers pushed since this Context last
    /// drained — elicitation declines and refresh failures land as
    /// `mcp.notice` audit facts; a bumped catalog version re-registers
    /// that server's `mcp__{name}__*` tools and records `mcp.refresh`.
    /// Called inside `turn_lock` — the registry swap serializes with
    /// turns the same way every other transcript mutation does.
    pub(crate) async fn drain_mcp(&self, observer: &dyn Observer) {
        for srv in &self.ctx.mcp_servers {
            for notice in srv.drain_notices() {
                {
                    let mut log = self.ctx.sessions.lock().await;
                    let _ = log
                        .append(&SessionEvent::Hook {
                            event: "mcp.notice".into(),
                            detail: notice.clone(),
                        })
                        .await;
                }
                observer.on_event(&LiveEvent::Hook {
                    event: "mcp.notice".into(),
                    detail: notice,
                });
            }
            if srv.version() == srv.seen_version() {
                continue;
            }
            let n = srv.tools().len();
            self.ctx
                .tools
                .replace_prefixed(&format!("mcp__{}__", srv.name), srv.tool_impls());
            srv.mark_seen();
            let detail = format!("{}: catalog refreshed — {n} tools", srv.name);
            {
                let mut log = self.ctx.sessions.lock().await;
                let _ = log
                    .append(&SessionEvent::Hook {
                        event: "mcp.refresh".into(),
                        detail: detail.clone(),
                    })
                    .await;
            }
            observer.on_event(&LiveEvent::Hook {
                event: "mcp.refresh".into(),
                detail,
            });
        }
    }

    /// `/srv:prompt` resolution — `name` is the whole command token
    /// (`"echo:summarize"`), `rest` the whitespace tail mapped onto the
    /// prompt's declared arguments positionally (extras land on the last
    /// declared arg — prompts that take "a sentence" still work). None
    /// when no server owns the name (the caller falls through to file
    /// commands); Some(Err) reports the server's refusal as a note.
    /// Non-text prompt content (images/resources) flattens away — the
    /// result becomes one user message.
    pub async fn mcp_prompt_text(&self, name: &str, rest: &str) -> Option<anyhow::Result<String>> {
        let (srv_name, prompt_name) = name.split_once(':')?;
        if srv_name.is_empty() || prompt_name.is_empty() {
            return None;
        }
        let handle = self.ctx.mcp_servers.iter().find(|s| s.name == srv_name)?;
        let info = handle
            .prompts()
            .into_iter()
            .find(|p| p.name == prompt_name)?;
        let mut params = GetPromptRequestParams::new(info.name.clone());
        if !info.arg_names.is_empty() {
            let mut words = rest.split_whitespace().peekable();
            let mut args = serde_json::Map::new();
            for arg in &info.arg_names {
                let Some(first) = words.next() else { break };
                // the last declared slot soaks up the tail — a prompt's
                // final arg is usually the prose one
                let value = if arg == info.arg_names.last().unwrap() {
                    std::iter::once(first)
                        .chain(words.by_ref())
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    first.to_string()
                };
                args.insert(arg.clone(), serde_json::Value::String(value));
            }
            if !args.is_empty() {
                params = params.with_arguments(args);
            }
        }
        Some(
            handle
                .client
                .peer()
                .get_prompt(params)
                .await
                .map_err(|e| anyhow::anyhow!("{e:#}"))
                .map(|res| {
                    res.messages
                        .iter()
                        .filter_map(|m| m.content.as_text())
                        .map(|t| t.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
        )
    }

    /// Completable `/srv:prompt` names — the slash menus offer these
    /// alongside builtins and file commands.
    pub fn mcp_prompt_names(&self) -> Vec<String> {
        self.ctx
            .mcp_servers
            .iter()
            .flat_map(|s| {
                s.prompts()
                    .into_iter()
                    .map(move |p| format!("{}:{}", s.name, p.name))
            })
            .collect()
    }

    /// Connected server names — SessionStart hook payloads carry them
    /// (`mcp_servers`), so a hook's first read of the session knows which
    /// `mcp__*` namespaces exist.
    pub fn mcp_server_names(&self) -> Vec<String> {
        self.ctx
            .mcp_servers
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }
}
