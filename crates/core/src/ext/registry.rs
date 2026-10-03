//! Spawned extension children and the request/reply plumbing around them.
//!
//! Correlation model: each child owns two tasks — a writer draining an
//! mpsc into stdin, a reader parsing stdout lines and resolving oneshots
//! parked in `pending` by `send_request`. Dropping the sender closes
//! stdin; dead children (writer error, reader EOF) fail every parked and
//! future request fast.

use crate::context::MutexRecover;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::hooks::HookOutcome;
use crate::tool::ToolImpl;

/// Per-request budget — an extension that stalls must not stall the loop.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// `ext/shutdown` grace before the child is killed (protocol contract).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Timeout error shape resolves parked requests exactly like a JSON-RPC
/// error reply would, so callers see one failure vocabulary.
fn timeout_frame() -> Value {
    json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "ext request timed out"}})
}

/// `ext/initialize` params — the session facts the contract promises.
pub struct ExtInit {
    pub cwd: String,
    pub session_id: String,
    pub transcript_path: String,
}

/// Mutable half of a child behind the lock — `No MutexGuard across
/// .await` means everything async works on cloned handles, never guards.
/// `pub(crate)` fields exist for the dispatch unit test only.
pub(crate) struct ChildState {
    next_id: u64,
    /// In-flight replies keyed by request id; the reader fills them,
    /// `mark_dead_with` drains them as errors.
    pub(crate) pending: HashMap<u64, oneshot::Sender<Value>>,
    /// `None` once stdin is intentionally closed (shutdown / teardown).
    write_tx: Option<mpsc::UnboundedSender<String>>,
    child: Option<tokio::process::Child>,
}

/// One extension process: spawn state plus its `ext/initialize`-negotiated
/// capabilities. Cheap to `Arc` — every handle shares the same plumbing.
pub struct ExtChild {
    /// `ext__{plugin}__{tool}` namespace stem — from `plugin_name`.
    pub plugin: String,
    /// Events the child subscribed to via `capabilities.events` —
    /// `ext/event` goes only to children that listed the event.
    events: Vec<String>,
    state: Arc<Mutex<ChildState>>,
    /// Reader/writer keepers — aborted on teardown; otherwise they exit on
    /// their own (reader at stdout EOF, writer when the queue closes).
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    writer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ExtChild {
    /// Spawn + `ext/initialize` handshake. Returns the child and the
    /// declared tool list (`[]` unless `capabilities.tools`). Fails —
    /// caller warns and skips — when the child won't start, won't
    /// answer, or answers wrong.
    async fn spawn(
        mut cmd: tokio::process::Command,
        plugin: &str,
        init: &ExtInit,
    ) -> anyhow::Result<(Self, Vec<Value>)> {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // protocol stdout must stay clean — child diagnostics die here
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let state = Arc::new(Mutex::new(ChildState {
            next_id: 0,
            pending: HashMap::new(),
            write_tx: None,
            child: Some(child),
        }));
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<String>();
        let writer_state = state.clone();
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = write_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
            // stdin drop on task exit = the child's EOF.
            mark_dead_with(&writer_state, &timeout_frame());
        });
        let reader_state = state.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            // EOF or read error ends the loop — the child is gone either way.
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(frame) = super::proto::parse_frame(&line) {
                    dispatch(&reader_state, frame);
                }
            }
            mark_dead_with(&reader_state, &timeout_frame());
        });
        state.lock_or_recover().write_tx = Some(write_tx);

        let mut this = Self {
            plugin: plugin.to_string(),
            events: Vec::new(),
            state,
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(Some(writer)),
        };
        let tools = this.handshake(init).await?;
        Ok((this, tools))
    }

    /// `ext/initialize` → `{name, version, capabilities}` — event
    /// subscriptions land on `self.events`; `capabilities.tools` gates
    /// exactly one `ext/tools/list` (the contract's only list call).
    async fn handshake(&mut self, init: &ExtInit) -> anyhow::Result<Vec<Value>> {
        let reply = self
            .send_request(
                "ext/initialize",
                json!({
                    "protocol": 1,
                    "cwd": init.cwd,
                    "session_id": init.session_id,
                    "transcript_path": init.transcript_path,
                }),
            )
            .await?;
        let caps = reply.get("capabilities").cloned().unwrap_or(Value::Null);
        self.events = caps
            .get("events")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if caps.get("tools").and_then(|t| t.as_bool()) == Some(true) {
            let listed = self.send_request("ext/tools/list", json!({})).await?;
            return Ok(listed
                .get("tools")
                .and_then(|t| t.as_array().cloned())
                .unwrap_or_default());
        }
        Ok(Vec::new())
    }

    /// Request/reply with the 30s budget. Dead or dying children resolve
    /// to an error so the caller degrades, never aborts.
    pub async fn send_request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let (id, rx, tx) = {
            let mut st = self.state.lock_or_recover();
            let Some(tx) = st.write_tx.clone() else {
                anyhow::bail!("extension {} is dead", self.plugin);
            };
            st.next_id += 1;
            let id = st.next_id;
            let (req_tx, rx) = oneshot::channel();
            st.pending.insert(id, req_tx);
            (id, rx, tx)
        };
        let mut line = serde_json::to_string(&super::proto::request_frame(id, method, params))?;
        line.push('\n');
        if tx.send(line).is_err() {
            self.mark_dead();
            anyhow::bail!("extension {} is dead", self.plugin);
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(frame)) => super::proto::reply_result(frame),
            // the reader parked nothing / died mid-flight — same "gone" story
            Ok(Err(_)) => {
                self.state.lock_or_recover().pending.remove(&id);
                anyhow::bail!("extension {} is dead", self.plugin)
            }
            Err(_) => {
                self.state.lock_or_recover().pending.remove(&id);
                anyhow::bail!("extension {method} timed out")
            }
        }
    }

    /// Fire-and-forget — `ext/shutdown` is the only notification today.
    pub fn notify(&self, method: &str, params: Value) {
        let tx = {
            let st = self.state.lock_or_recover();
            st.write_tx.clone()
        };
        if let Some(tx) = tx {
            let mut line = serde_json::to_string(&super::proto::notification_frame(method, params))
                .unwrap_or_default();
            line.push('\n');
            let _ = tx.send(line);
        }
    }

    /// Contract tail: `ext/shutdown`, close stdin, ≤2s wait, then kill.
    pub async fn shutdown(&self) {
        self.notify("ext/shutdown", json!({}));
        let child = {
            let mut st = self.state.lock_or_recover();
            st.write_tx = None;
            self.mark_dead_locked(&mut st);
            st.child.take()
        };
        graceful_wait(child).await;
        if let Some(h) = self.reader.lock_or_recover().take() {
            h.abort();
        }
        if let Some(h) = self.writer.lock_or_recover().take() {
            h.abort();
        }
    }

    /// Fail every parked request without a live reply — callers must only
    /// ever see the JSON-RPC error shape, identical to a peer's own error.
    fn mark_dead(&self) {
        mark_dead_with(&self.state, &timeout_frame());
    }

    fn mark_dead_locked(&self, st: &mut ChildState) {
        for (_, tx) in st.pending.drain() {
            let _ = tx.send(timeout_frame());
        }
    }

    /// Synchronous teardown for `Drop`: best-effort `ext/shutdown`, close
    /// stdin (child sees EOF), abort readers, reap detached — `Drop`
    /// cannot await so `wait()`/`kill()` run on a spawned task.
    fn terminate(&self) {
        self.notify("ext/shutdown", json!({}));
        let (child, reader, writer) = {
            let mut st = self.state.lock_or_recover();
            st.write_tx = None;
            self.mark_dead_locked(&mut st);
            (
                st.child.take(),
                self.reader.lock_or_recover().take(),
                self.writer.lock_or_recover().take(),
            )
        };
        if let Some(h) = reader {
            h.abort();
        }
        if let Some(h) = writer {
            h.abort();
        }
        if let Some(mut child) = child {
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        graceful_wait(Some(child)).await;
                    });
                }
                // dropped off-runtime (process teardown): direct kill is all
                // that's left — can't block, can't spawn.
                Err(_) => {
                    let _ = child.start_kill();
                }
            }
        }
    }
}

impl Drop for ExtChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Correlate a reply to its request: exact-id match drains the parked
/// oneshot; notifications and stray ids drop silently.
fn dispatch(state: &Arc<Mutex<ChildState>>, frame: Value) {
    if let Some(id) = super::proto::reply_id(&frame) {
        let tx = state.lock_or_recover().pending.remove(&id);
        if let Some(tx) = tx {
            let _ = tx.send(frame);
        }
    }
}

/// Drain every parked request with a synthetic error frame — EOF, write
/// failure, shutdown: callers all see the same "extension is gone" shape.
fn mark_dead_with(state: &Arc<Mutex<ChildState>>, frame: &Value) {
    let mut st = state.lock_or_recover();
    for (_, tx) in st.pending.drain() {
        let _ = tx.send(frame.clone());
    }
}

/// `ext/shutdown` then stdin EOF buys the child `SHUTDOWN_GRACE` to exit;
/// past that it is killed — a wedged extension must not wedge the exit.
async fn graceful_wait(child: Option<tokio::process::Child>) {
    let Some(mut child) = child else { return };
    if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// A child tool wrapped as `ToolImpl` — schema/description pass through
/// verbatim, so a bad `input_schema` means the tool doesn't exist to the
/// LLM (same rule as `McpTool`).
struct ExtTool {
    child: Arc<ExtChild>,
    /// namespaced `ext__{plugin}__{tool}` — leaked once for `&'static str`
    full_name: String,
    ext_name: String,
    description: String,
    schema: Value,
}

#[async_trait::async_trait]
impl ToolImpl for ExtTool {
    fn name(&self) -> &'static str {
        // tool names are process-lifetime constants (same leak as McpTool)
        Box::leak(self.full_name.clone().into_boxed_str())
    }

    fn decl(&self) -> sunmao_llm::types::Tool {
        sunmao_llm::types::Tool::function(&self.full_name, &self.description, self.schema.clone())
    }

    /// `ext/tools/call` — `{name, arguments}` → `{content, is_error?}`.
    /// The wire name is the extension's own, minus our namespace.
    async fn call(
        &self,
        args: Value,
        _ctx: &std::sync::Arc<crate::context::Context>,
    ) -> anyhow::Result<crate::tool::ToolResult> {
        let reply = self
            .child
            .send_request(
                "ext/tools/call",
                json!({"name": self.ext_name, "arguments": args}),
            )
            .await?;
        Ok(crate::tool::ToolResult {
            output: reply
                .get("content")
                .map(|c| {
                    c.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| c.to_string())
                })
                .unwrap_or_default(),
            ok: reply.get("is_error").and_then(|e| e.as_bool()) != Some(true),
        })
    }
}

/// The session's extension surface — spawned children plus the tools and
/// event subscriptions they negotiated.
pub struct ExtRegistry {
    children: Mutex<Vec<Arc<ExtChild>>>,
    tools: Mutex<Vec<Arc<dyn ToolImpl>>>,
}

impl Default for ExtRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtRegistry {
    pub fn new() -> Self {
        Self {
            children: Mutex::new(Vec::new()),
            tools: Mutex::new(Vec::new()),
        }
    }

    /// Spawn one extension: `{command, args, env}` → initialize →
    /// `ext/tools/list` → register each reply entry under
    /// `ext__{plugin}__{name}`.
    pub async fn connect(
        &self,
        spec: &super::ExtSpec,
        init: &ExtInit,
        plugin_name: &str,
    ) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args).envs(&spec.env);
        let (child, tool_decls) = ExtChild::spawn(cmd, plugin_name, init).await?;
        let child = Arc::new(child);
        for decl in &tool_decls {
            let Some(ext_name) = decl.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let tool = ExtTool {
                child: child.clone(),
                full_name: format!("ext__{plugin_name}__{ext_name}"),
                ext_name: ext_name.to_string(),
                description: decl
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
                schema: decl
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            };
            self.tools.lock_or_recover().push(Arc::new(tool));
        }
        self.children.lock_or_recover().push(child);
        Ok(())
    }

    /// Accumulated `ext__*` tools — sessions register them into their
    /// `ToolRegistry` next to the MCP results.
    pub fn tools(&self) -> Vec<Arc<dyn ToolImpl>> {
        self.tools.lock_or_recover().clone()
    }

    /// `ext/event` to every subscribed child — replies fold into the same
    /// `HookOutcome` the command hooks built, so `block`/`extra_context`/
    /// `updatedInput`/`permissionDecision` mean exactly what hooks mean.
    pub async fn fire_event(&self, event: &str, payload: &Value, outcome: &mut HookOutcome) {
        let children: Vec<Arc<ExtChild>> = self
            .children
            .lock_or_recover()
            .iter()
            .filter(|c| c.events.iter().any(|e| e == event))
            .cloned()
            .collect();
        for child in children {
            match child
                .send_request("ext/event", json!({"event": event, "payload": payload}))
                .await
            {
                Ok(reply) => crate::hooks::apply_ext_reply(&reply, outcome),
                Err(e) => {
                    tracing::warn!("ext/event {} -> {} failed: {e:#}", child.plugin, event)
                }
            }
        }
    }

    /// Graceful tail of every child — frontends call at session end.
    pub async fn shutdown(&self) {
        let children = self.children.lock_or_recover().clone();
        for child in children {
            child.shutdown().await;
        }
    }
}

impl Drop for ExtRegistry {
    /// Session teardown without an explicit `shutdown()` still kills the
    /// children: stdin closes (EOF), readers abort, reaper tasks handle
    /// the process — `Drop` can only delegate, never await.
    fn drop(&mut self) {
        for child in self
            .children
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
        {
            child.terminate();
        }
    }
}

/// Bare correlation state for the dispatch test — no spawned child.
#[cfg(test)]
pub(crate) fn test_state() -> Arc<Mutex<ChildState>> {
    Arc::new(Mutex::new(ChildState {
        next_id: 0,
        pending: HashMap::new(),
        write_tx: None,
        child: None,
    }))
}

#[cfg(test)]
pub(crate) fn test_dispatch(state: &Arc<Mutex<ChildState>>, frame: Value) {
    dispatch(state, frame);
}
