//! `/schedules` 的 REST 面 — 列表/建改/删除/立即跑。任务模型、持久化
//! 与触发语义全在 `host::sched`（serde 直通）；这里只做参数拼装与
//! 响应塑形，校验失败一律 400 文本。

use std::sync::Arc;

use super::super::host::{Shared, sched};
use super::HostResponse;

/// `GET /schedules` — the whole table; each row also carries `next` /
/// `last_run`/`last_session` for the 定时任务 view's status column.
pub(super) fn list(s: &Arc<Shared>) -> HostResponse {
    HostResponse::json(serde_json::json!({ "tasks": sched::load(s) }))
}

/// `POST /schedules` (create) / `PUT /schedules/{id}` (edit) — body is
/// the task object itself (`serde(default)` fills what the form omits).
/// A fresh task defaults to enabled and gets its `next` armed here; an
/// edit re-arms `next` but keeps the run history.
pub(super) async fn upsert(s: &Arc<Shared>, id: Option<&str>, body: &[u8]) -> HostResponse {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return HostResponse::err(400, format!("bad json: {e}")),
    };
    let mut t: sched::SchedTask = match serde_json::from_value(v.clone()) {
        Ok(t) => t,
        Err(e) => return HostResponse::err(400, format!("bad task: {e}")),
    };
    if id.is_none() && v["enabled"].is_null() {
        t.enabled = true;
    }
    if let Err(e) = sched::validate(s, &mut t) {
        return HostResponse::err(400, e);
    }
    // 排定在这里发生——禁用清空 next；启用的重算严格晚于 now 的一跳
    t.next = if t.enabled {
        sched::next_after(&t, chrono::Local::now()).unwrap_or(0)
    } else {
        0
    };
    let res = sched::mutate(s, |tasks| {
        if let Some(id) = id {
            let old = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or_else(|| "no such task".to_string())?;
            t.id = id.to_string();
            // run history survives the edit — only the schedule re-arms
            t.last_run = old.last_run;
            t.last_session = old.last_session.clone();
            t.last_error = old.last_error.clone();
            *old = t.clone();
        } else {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            t.id = format!("t-{ms}");
            tasks.push(t.clone());
        }
        Ok(t)
    });
    match res {
        Ok(t) => HostResponse::json(serde_json::json!({"task": t})),
        Err(e) if e == "no such task" => HostResponse::err(404, e),
        Err(e) => HostResponse::err(500, e),
    }
}

/// `DELETE /schedules/{id}` — remove the row; a miss is a 404, not a
/// silent ok.
pub(super) fn remove(s: &Arc<Shared>, id: &str) -> HostResponse {
    match sched::mutate(s, |tasks| {
        let n = tasks.len();
        tasks.retain(|t| t.id != id);
        if tasks.len() == n {
            Err("no such task".to_string())
        } else {
            Ok(())
        }
    }) {
        Ok(()) => HostResponse::json(serde_json::json!({"ok": true})),
        Err(e) if e == "no such task" => HostResponse::err(404, e),
        Err(e) => HostResponse::err(500, e),
    }
}

/// `POST /schedules/{id}/run` — fire now without touching the schedule:
/// `next`/`enabled` stay armed, only the last-run record updates.
pub(super) async fn run_now(s: &Arc<Shared>, id: &str) -> HostResponse {
    let task = sched::load(s).into_iter().find(|t| t.id == id);
    let Some(task) = task else {
        return HostResponse::err(404, "no such task".into());
    };
    match sched::fire(s, &task).await {
        Ok(sess) => {
            let _ = sched::mutate(s, |tasks| {
                if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                    t.last_run = chrono::Local::now().timestamp_millis();
                    t.last_session = sess.clone();
                    t.last_error.clear();
                }
                Ok(())
            });
            HostResponse::json(serde_json::json!({"session": sess}))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let _ = sched::mutate(s, |tasks| {
                if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                    t.last_error = msg.clone();
                }
                Ok(())
            });
            HostResponse::err(500, msg)
        }
    }
}
