//! The dispatch gate — declarative rules → hook `permissionDecision` →
//! session grants → risky-pattern classifier → approval prompt, all
//! downstream of the session's approval mode (SPEC §4.6). Lives apart from
//! the turn loop: this file is the *decision*, turn.rs is the *drive*.

use super::*;
use crate::agent::mode::{ApprovalMode, call_mutates};

impl AgentLoop {
    /// `deny` rules are a hard refusal nothing overrides — a session grant
    /// never bypasses them, and neither does `full_access` mode.
    /// `args` is the (possibly hook-rewritten) tool input — `call_mutates`
    /// needs `Bash.command`, which `specifier` already is, but other tools
    /// may classify on more fields later.
    pub(super) async fn gate_call(
        &self,
        tool: &str,
        args: &serde_json::Value,
        specifier: &str,
        hook: Option<crate::hooks::HookPermission>,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        use crate::hooks::HookPermission as H;
        use crate::permissions::Verdict;
        match self.ctx.permissions.check(tool, specifier) {
            Verdict::Deny => {
                self.fire_denied(tool, specifier, "permission rules", observer)
                    .await;
                return Err("denied by permission rules".into());
            }
            Verdict::Ask | Verdict::PreApproved | Verdict::Default => {}
        }
        if let Some(H::Deny) = hook {
            self.fire_denied(tool, specifier, "hook veto", observer)
                .await;
            return Err("denied by hook".into());
        }
        // read_only refuses mutations outright — the refusal is an audit
        // fact, same durability as a denied prompt verdict. Ahead of
        // grants on purpose: a standing answer is not a license to write
        // under a mode that forbids writing.
        let mode = self.approval_mode();
        if mode == ApprovalMode::ReadOnly
            && call_mutates(tool, args, &self.ctx.readonly_verbs, self.ctx.shell)
        {
            let detail = format!("{tool}: {specifier}");
            self.audit_fact("mode.readonly.block", &detail, observer)
                .await;
            return Err(format!("blocked by read_only mode: {tool}"));
        }
        // Chained Bash hides extra surfaces behind `&&`/`;`/`|` — segments
        // get their own deny/grant/ask adjudication because whole-call
        // rules can't see inside the chain. In Auto/FullAccess the segment
        // pass is the whole decision (the whole-call classifier would
        // substring-match a segment and re-prompt past its grant), so it
        // reports whether it fully adjudicated.
        let segmented = if tool == "Bash" {
            self.bash_segments_check(specifier, mode, observer).await?
        } else {
            false
        };
        // Standing answers — an allow rule, a session grant, a hook allow —
        // stop every remaining ask, but never a deny (checked above at both
        // whole-call and segment shape).
        if self.ctx.permissions.check(tool, specifier) == Verdict::PreApproved
            || self.ctx.session_granted(tool, specifier)
        {
            return Ok(());
        }
        if mode == ApprovalMode::FullAccess {
            return Ok(());
        }
        if self.ctx.permissions.check(tool, specifier) == Verdict::Ask {
            return self
                .ask(tool, specifier, "matched ask rule", observer)
                .await;
        }
        if let Some(H::Ask) = hook {
            return self
                .ask(tool, specifier, "hook requested approval", observer)
                .await;
        }
        // always_ask: every mutating call prompts — rules/grants already
        // answered above; safe reads still pass.
        if mode == ApprovalMode::AlwaysAsk
            && call_mutates(tool, args, &self.ctx.readonly_verbs, self.ctx.shell)
        {
            return self.ask(tool, specifier, "always_ask mode", observer).await;
        }
        if segmented {
            // multi-segment Bash already ran its ask/classifier pass per
            // segment — the whole string is only the concatenation of what
            // was just adjudicated
            return Ok(());
        }
        if let Some(H::Allow) = hook {
            return Ok(());
        }
        // default: the risky-pattern classifier. Bash-shaped patterns only —
        // feeding a Task prompt or a file path through the shell table
        // substring-matches prose ("explain curl" asks!) instead of judging
        // the command that will actually run.
        if tool == "Bash"
            && let Some(why) = crate::approval::classify(specifier, &self.ctx.risk_table)
        {
            return self.ask(tool, specifier, why, observer).await;
        }
        // external-directory tier: a Write/Edit/Artifact whose resolved
        // path leaves the project root asks in every mode below
        // FullAccess — the risky-pattern table sees Bash strings, not
        // "../sibling/x.rs" as a path. Canonicalize both sides so `..`
        // and symlink hops can't fake containment; a path that won't
        // canonicalize (doesn't exist yet) resolves against its parent.
        if ["Write", "Edit"].contains(&tool)
            && mode != ApprovalMode::FullAccess
            && let Some(p) = args["path"].as_str()
            && Self::outside_project(&self.ctx.cwd, p)
        {
            return self
                .ask(tool, specifier, "writes outside the project root", observer)
                .await;
        }
        Ok(())
    }

    /// External-directory tier helper — `path` is checked against the
    /// project root after canonicalization. An unresolvable target (a
    /// file that doesn't exist yet) falls back to its nearest canonical
    /// ancestor so a `Write` to a new file is judged by where it would
    /// land, not by where it is. Symlinks re-route through canonicalize —
    /// a `link -> ../outside` resolves to the real target.
    pub(super) fn outside_project(cwd: &std::path::Path, path: &str) -> bool {
        let joined = cwd.join(path);
        let cand = joined.canonicalize().or_else(|_| {
            joined
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.join(joined.file_name().unwrap_or_default()))
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
        });
        match (cand, cwd.canonicalize()) {
            (Ok(target), Ok(root)) => !target.starts_with(root),
            // an unresolvable side can't prove containment — treat as
            // outside (fail-closed is the gate's posture everywhere)
            _ => true,
        }
    }

    /// Per-segment adjudication for chained Bash. A deny anywhere vetoes
    /// the whole command, named by segment so the human sees which part
    /// tripped it; a session grant or ask rule applies at segment shape
    /// for the same reason. Only multi-segment commands take this path —
    /// single commands are fully covered by the whole-call checks.
    ///
    /// Returns `true` when the segments pass and the call was fully
    /// adjudicated at segment shape (Auto/FullAccess): the caller then
    /// skips the whole-call classifier, which would substring-match a
    /// granted segment and re-prompt. AlwaysAsk/ReadOnly keep the
    /// whole-call pass — read_only's whole-call `call_mutates` already
    /// caught chained mutations before we get here.
    async fn bash_segments_check(
        &self,
        specifier: &str,
        mode: ApprovalMode,
        observer: &dyn Observer,
    ) -> Result<bool, String> {
        use crate::permissions::Verdict;
        // pwsh segments come from the quote-aware splitter — the deno AST
        // can't parse real PowerShell (`$x`, `|`, `2>`) and would either
        // refuse or mis-split.
        let segments = match self.ctx.shell {
            crate::tool::ShellBackend::Pwsh => crate::agent::mode::pwsh_segments(specifier),
            crate::tool::ShellBackend::Posix => crate::preflight::shell_segments(specifier),
        };
        if segments.len() <= 1 {
            return Ok(false);
        }
        for seg in segments {
            if self.ctx.permissions.check("Bash", &seg) == Verdict::Deny {
                return Err(format!("denied by permission rules (segment: {seg})"));
            }
            // a session grant answers the segment's ask forever —
            // re-prompting on every chained command made Session
            // grants useless for the exact commands that need them
            if self.ctx.session_granted("Bash", &seg) {
                continue;
            }
            if mode == ApprovalMode::FullAccess {
                continue;
            }
            if mode == ApprovalMode::Auto {
                if self.ctx.permissions.check("Bash", &seg) == Verdict::Ask {
                    self.ask("Bash", &seg, "matched ask rule", observer).await?;
                    continue;
                }
                if let Some(why) = crate::approval::classify(&seg, &self.ctx.risk_table) {
                    self.ask("Bash", &seg, why, observer).await?;
                }
            }
            // AlwaysAsk/ReadOnly: segment-level asks stay folded into the
            // whole-call decision — one prompt per call, one refusal per
            // mutating command.
        }
        Ok(mode == ApprovalMode::Auto || mode == ApprovalMode::FullAccess)
    }

    /// MCP Apps bridge (SEP-1865): an island's `tools/call` request is a
    /// real tool call — same visibility check, same dispatch gate, same
    /// audit. `server_tool` is the bare name the server knows; the wire
    /// name is `mcp__{server}__{tool}` (that's what rules/grants match —
    /// identical specifier shape as a model-initiated call).
    /// Returns the serialized `CallToolResult` for the island's reply.
    pub async fn mcp_app_call(
        &self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
        observer: &dyn Observer,
    ) -> Result<serde_json::Value, String> {
        let wire = format!("mcp__{server}__{tool}");
        let handle = self
            .ctx
            .mcp_servers
            .iter()
            .find(|s| s.name == server)
            .ok_or_else(|| format!("no such mcp server: {server}"))?;
        let catalog = handle.tools();
        let info = catalog
            .iter()
            .find(|t| t.server_tool == tool)
            .ok_or_else(|| format!("no such tool on {server}: {tool}"))?;
        if !info.app_visible {
            return Err(format!("{wire}: not callable from apps (visibility)"));
        }
        // the UI is not a gate bypass — declarative rules, grants, modes and
        // the classifier all apply exactly as they do to model calls. The
        // specifier carries the wire name so `Tool(spec)` permission rules
        // can match bridge calls ("" matched nothing, silently bypassing
        // user deny/allow tables).
        self.gate_call(&wire, &args, &wire, None, observer).await?;
        let mut params = rmcp::model::CallToolRequestParams::new(tool.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        }
        let res = handle
            .client
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| format!("mcp call failed: {e:#}"))?;
        self.audit_fact("mcp.app_call", &format!("{wire} (island)"), observer)
            .await;
        serde_json::to_value(&res).map_err(|e| format!("result serialize: {e}"))
    }

    /// `resources/read` proxy for islands — any declared resource URI on the
    /// same server. Read-only by nature; still audit-logged.
    pub async fn mcp_resource_read(
        &self,
        server: &str,
        uri: &str,
        observer: &dyn Observer,
    ) -> Result<serde_json::Value, String> {
        let handle = self
            .ctx
            .mcp_servers
            .iter()
            .find(|s| s.name == server)
            .ok_or_else(|| format!("no such mcp server: {server}"))?;
        if !uri.starts_with("ui://") && !uri.contains("://") {
            return Err("bad resource uri".into());
        }
        let rr = handle
            .client
            .peer()
            .read_resource_once(rmcp::model::ReadResourceRequestParams::new(uri.to_string()))
            .await
            .map_err(|e| format!("mcp resource read failed: {e:#}"))?;
        self.audit_fact("mcp.app_read", &format!("{server}: {uri}"), observer)
            .await;
        match rr {
            rmcp::model::ReadResourceResponse::Complete(r) => {
                serde_json::to_value(&r).map_err(|e| format!("result serialize: {e}"))
            }
            _ => Err("unexpected resource response shape".into()),
        }
    }

    /// Public audit lane for frontend-sourced facts (island open-link
    /// requests, ui/message, view logs) — durable in the log, visible on
    /// the live spine. `event`/`detail` are advisory text, nothing more.
    pub async fn audit_ui_event(&self, event: &str, detail: &str, observer: &dyn Observer) {
        self.audit_fact(event, detail, observer).await;
    }

    /// Durable + live audit fact for a gate decision — deny paths and
    /// mode blocks must be reconstructible from the log alone.
    pub(crate) async fn audit_fact(&self, event: &str, detail: &str, observer: &dyn Observer) {
        {
            let mut log = self.ctx.sessions.lock().await;
            log.append_audit(&SessionEvent::Hook {
                event: event.to_string(),
                detail: detail.to_string(),
            })
            .await;
        }
        observer.on_event(&LiveEvent::Hook {
            event: event.to_string(),
            detail: detail.to_string(),
        });
    }

    /// A settled refusal → `PermissionDenied` hook. Pure observability —
    /// the decision already happened (permission table, hook veto, card
    /// verdict, cancel); listeners get the same `PreToolUse` payload shape
    /// plus a `denied_by` qualifier so a hook can tell a rule block from
    /// a human's "no".
    async fn fire_denied(
        &self,
        tool: &str,
        specifier: &str,
        denied_by: &str,
        observer: &dyn Observer,
    ) {
        self.audit_fact(
            "permission.denied",
            &format!("{tool}: {specifier} ({denied_by})"),
            observer,
        )
        .await;
        // PermissionDenied is observability-only — the verdict is already
        // settled and audited; a slow hook must not delay the tool's
        // refusal back to the model.
        crate::hooks::HookEngine::fire_detached(
            &self.ctx.hooks,
            HookEvent::PermissionDenied,
            &self.ctx.cwd,
            &crate::hooks::HookInput {
                tool_name: Some(tool),
                tool_input: Some(&serde_json::json!({
                    "specifier": specifier, "denied_by": denied_by,
                })),
                ..Default::default()
            },
        );
    }

    /// One approval prompt → verdict. `Session` is recorded in
    /// `session_grants` and audited as a durable `Hook` fact.
    pub(super) async fn ask(
        &self,
        tool: &str,
        specifier: &str,
        why: &str,
        observer: &dyn Observer,
    ) -> Result<(), String> {
        // Notification: the loop is about to idle on a human — hooks can
        // relay that (desktop toast, bell). Advisory only; detached — a
        // slow notify-send must not delay the card the user is waiting for.
        crate::hooks::HookEngine::fire_detached(
            &self.ctx.hooks,
            HookEvent::Notification,
            &self.ctx.cwd,
            &crate::hooks::HookInput {
                prompt: Some(why),
                tool_name: Some(tool),
                tool_input: Some(&serde_json::json!({ "specifier": specifier })),
                ..Default::default()
            },
        );
        match self.ctx.approval.approve(tool, specifier, why).await {
            crate::approval::Approval::Session => {
                self.ctx.grant_session(tool, specifier);
                let detail = format!("{tool}: {specifier}");
                self.audit_fact("approval.session", &detail, observer).await;
                Ok(())
            }
            crate::approval::Approval::Once => Ok(()),
            crate::approval::Approval::Deny { reason } => {
                let detail = format!("{tool}: {specifier} ({why})");
                self.audit_fact("approval.deny", &detail, observer).await;
                self.fire_denied(tool, specifier, "verdict deny", observer)
                    .await;
                // the denial reason names *why it couldn't be answered*
                // (non-interactive session) when the approver supplies one —
                // the failed ToolResult shows it so the model can route
                // around the refusal
                Err(match reason {
                    Some(r) => format!("denied at approval gate ({why}): {r}"),
                    None => format!("denied at approval gate ({why})"),
                })
            }
            crate::approval::Approval::Cancelled => {
                // the turn ended while the card was parked — nobody answered;
                // the label matters: a "denied" result implies a human said no
                self.audit_fact(
                    "approval.cancelled",
                    &format!("{tool}: {specifier}"),
                    observer,
                )
                .await;
                self.fire_denied(tool, specifier, "cancelled", observer)
                    .await;
                Err(format!("cancelled at approval gate ({why})"))
            }
        }
    }
}
