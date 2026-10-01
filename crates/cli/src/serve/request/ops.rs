//! 会话运行面板的 REST 数据面 — 子代理花名册与后台任务（`.sunmao/jobs/`）
//! 都是"实时状态拉取"端点：状态本身在内核/文件系统里，这里只读不推。
//! 推送侧是 `tasks.changed` / `jobs.changed` 的 Hook 实时事件（走 live
//! 总线），前端收到后回拉这里的 JSON。

use std::sync::Arc;

use super::super::host::Shared;
use super::HostResponse;

/// `GET /tasks?sess=…` — 被查看宿主的活动子代理花名册（`task_roster()` 的
/// JSON 形态，/`tasks` 文本是同一数据的终端版）。`sess` 缺省取第一个
/// 活动宿主；未运行的会话没有花名册可言 → `tasks: []`（轮询调用方不应
/// 为休眠会话付 404）。
pub(super) fn tasks_list(s: &Arc<Shared>, sess: Option<String>) -> HostResponse {
    let host = sess
        .as_deref()
        .and_then(|id| s.host(id))
        .or_else(|| s.live_ids().first().and_then(|id| s.host(id)));
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
