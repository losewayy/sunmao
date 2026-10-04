//! 会话运行面板的 REST 数据面 — 子代理花名册与后台任务（`.sunmao/jobs/`）
//! 都是"实时状态拉取"端点：状态本身在内核/文件系统里，这里只读不推。
//! 推送侧是 `tasks.changed` / `jobs.changed` 的 Hook 实时事件（走 live
//! 总线），前端收到后回拉这里的 JSON。

use std::sync::Arc;

use sunmao_core::context::MutexRecover;

use super::super::host::{Shared, display_path, log_path, session_project};
use super::{HostResponse, query_arg};

/// `DELETE /session/{id}/grants` — revoke entries of the `Approval::Session`
/// ledger (`GET /session`'s `grants` is its read-only side). Body
/// `{"key":"<grant>"}` removes the one exact-match entry — `key` is the
/// display form (`"Bash rg --files"`), the stored key is tab-separated, so
/// the split is on the FIRST space; `{"key":"*"}` or an absent key clears
/// the ledger. Revocation mirrors the grant's own audit: a durable
/// `approval.revoke` Hook fact. Grants are live-only state — a dormant
/// session has no ledger to edit → 404.
pub(super) async fn grants_delete(s: &Arc<Shared>, id: &str, body: &[u8]) -> HostResponse {
    let Some(host) = s.host(id) else {
        return HostResponse::err(404, "no live session".into());
    };
    let key = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["key"].as_str().map(|k| k.trim().to_string()));
    let (removed, detail) = {
        let mut grants = host.agent.context().session_grants.lock_or_recover();
        match key.as_deref() {
            Some(k) if k != "*" => {
                let stored = k.replacen(' ', "\t", 1);
                if !grants.remove(&stored) {
                    return HostResponse::err(404, "no such grant".into());
                }
                (1, k.to_string())
            }
            _ => {
                let n = grants.len();
                grants.clear();
                (n, format!("{n} grant(s)"))
            }
        }
    };
    {
        let mut log = host.agent.context().sessions.lock().await;
        log.append_audit(&sunmao_core::SessionEvent::Hook {
            event: "approval.revoke".into(),
            detail,
        })
        .await;
    }
    HostResponse::json(serde_json::json!({
        "ok": true,
        "removed": removed,
        "grants": host.agent.session_grants(),
    }))
}

/// `GET /tasks?sess=…` — 被查看宿主的活动子代理花名册（`task_roster()` 的
/// JSON 形态，/`tasks` 文本是同一数据的终端版）。`sess` 缺省取第一个
/// 活动宿主；未运行的会话没有花名册可言 → `tasks: []`（轮询调用方不应
/// 为休眠会话付 404）。
pub(super) fn tasks_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .or_else(|| s.newest_live_id().and_then(|id| s.host(&id)));
    let tasks = host
        .map(|h| {
            h.agent
                .task_roster()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "id": t.id,
                        "lane": t.lane,
                        "agent": t.agent,
                        "prompt": t.prompt,
                        "done": t.done,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    HostResponse::json(serde_json::json!({"tasks": tasks}))
}

/// `GET /hooks?sess=…` — the viewed session's hook trust roster
/// (`engine.roster()` as JSON; `/hooks` text is the terminal rendering of
/// the same rows). Dormant sessions have no loaded engine → `hooks: []`;
/// the GUI's own untrusted-hook notice flows through the `hook.untrusted`
/// live event + replay rows, this endpoint is the on-demand read.
pub(super) fn hooks_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .or_else(|| s.newest_live_id().and_then(|id| s.host(&id)));
    let hooks = host
        .map(|h| {
            h.agent
                .context()
                .hooks
                .roster()
                .iter()
                .map(sunmao_core::hooks::trust::row_json)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    HostResponse::json(serde_json::json!({"hooks": hooks}))
}

/// `PUT /hooks?sess=…` `{index, trusted}` — the settings page's trust
/// toggle: same roster index `/hooks trust|untrust <n>` uses, same
/// `set_hook_trust` ledger write + audit fact it lands.
pub(super) async fn hooks_put(s: &Arc<Shared>, sess: Option<String>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let index = v["index"].as_u64().unwrap_or(0) as usize;
    let trusted = v["trusted"].as_bool().unwrap_or(false);
    let host = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .or_else(|| s.newest_live_id().and_then(|id| s.host(&id)));
    let Some(h) = host else {
        return HostResponse::err(404, "no live session".into());
    };
    match h.agent.set_hook_trust(index, trusted).await {
        Ok(detail) => HostResponse::json(serde_json::json!({"detail": detail})),
        Err(e) => HostResponse::err(400, e),
    }
}

/// `GET /mcp?sess=…` — the viewed session's MCP server roster
/// (`mcp_roster()` status structs; `/mcp` text is the terminal rendering).
pub(super) fn mcp_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .or_else(|| s.newest_live_id().and_then(|id| s.host(&id)));
    let servers = host
        .map(|h| {
            h.agent
                .mcp_roster()
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name, "transport": m.transport,
                        "tools": m.tools, "prompts": m.prompts,
                        "resources": m.resources, "connected": m.connected,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    HostResponse::json(serde_json::json!({"servers": servers}))
}

/// `sess` 指向的项目目录：活动宿主报它自己的 `session_cwd`；休眠日志从
/// `<project>/.sunmao/sessions/<id>.jsonl` 反推项目；都找不到时用启动
/// 目录兜底（jobs 是按项目分桶的，不是按会话）。
fn project_for(s: &Arc<Shared>, sess: Option<String>) -> std::path::PathBuf {
    sess.as_deref()
        .and_then(|id| s.host(id))
        .map(|h| h.agent.session_cwd())
        .or_else(|| {
            sess.as_deref()
                .and_then(|id| log_path(s, id))
                .map(|p| session_project(s, &p))
        })
        .unwrap_or_else(|| s.cwd.clone())
}

/// `output.log` 的末尾 `n` 字节，按字符边界对齐后返回文本 —— 预览和
/// `tail` 端点共用这条"日志即真相"的读取路径。
fn log_tail(path: &std::path::Path, n: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(n);
    // 切点落在 UTF-8 序列中间时前进到下一个边界 — 不裁出半个字符
    // （continuation byte = 0b10xxxxxx）
    let start = (start..=data.len())
        .find(|&i| i == data.len() || data[i] & 0xC0 != 0x80)
        .unwrap_or(data.len());
    sunmao_core::console::console_text(&data[start..])
}

/// `exit.json` 的 `exit_code` —— 存在即完结，`spawn_background` 写完它
/// 就算任务落地；文件缺失/损坏都按"还在跑"对待。
fn job_exit(dir: &std::path::Path) -> Option<i64> {
    std::fs::read_to_string(dir.join("exit.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["exit_code"].as_i64())
}

/// `GET /jobs?sess=…` — 会话项目 `.sunmao/jobs/{id}/` 的目录扫描：
/// `exit` 即完结码（null = 仍在跑），`mtime` 是目录修改时间（启动时刻的
/// 近似），`preview` 是日志末尾。
pub(super) fn jobs_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let dir = project_for(s, sess).join(".sunmao").join("jobs");
    let mut jobs: Vec<serde_json::Value> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| {
                    let id = e.file_name().to_string_lossy().to_string();
                    let exit = job_exit(&e.path());
                    let mtime = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64);
                    serde_json::json!({
                        "id": id,
                        "running": exit.is_none(),
                        "exit": exit,
                        "mtime": mtime,
                        "preview": log_tail(&e.path().join("output.log"), 500),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    jobs.sort_by(|a, b| b["mtime"].as_u64().cmp(&a["mtime"].as_u64()));
    HostResponse::json(serde_json::json!({
        "jobs": jobs,
        "cwd": display_path(dir.parent().and_then(|p| p.parent()).unwrap_or(&dir)),
    }))
}

/// `GET /jobs/{id}/output?sess=…&offset=…` — 日志的更完整一段：默认
/// 64KB 封顶，`offset` 做字节续传（同 `JobOutput` 工具的语义）。id 走
/// 与工具一致的白名单 —— 拒绝路径形状。
pub(super) fn job_output(
    s: &Arc<Shared>,
    id: &str,
    sess: Option<String>,
    query: &str,
) -> HostResponse {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return HostResponse::err(400, "bad job id".into());
    }
    let dir = project_for(s, sess).join(".sunmao").join("jobs").join(id);
    if !dir.is_dir() {
        return HostResponse::err(404, "no such job".into());
    }
    const CAP: usize = 64 * 1024;
    let data = std::fs::read(dir.join("output.log")).unwrap_or_default();
    let offset = query_arg(query, "offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(data.len());
    let end = (offset + CAP).min(data.len());
    HostResponse::json(serde_json::json!({
        "id": id,
        "exit": job_exit(&dir),
        "offset": offset,
        "total": data.len(),
        "chunk": sunmao_core::console::console_text(&data[offset..end]),
    }))
}
