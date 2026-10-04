//! 定时任务 — `.sunmao/schedules.json`（启动目录下，一张全局表；每条
//! 任务自带项目 `cwd`）。到点走 `new_session` 起一个全新会话、把
//! prompt 推进它的输入队列：产出物是一条普通会话——出现在侧边栏、可
//! 回退可续聊，执行链与用户输入完全同源，不发明第二条路。
//!
//! 调度循环随 `spawn_host` 启动，15s 一跳；停机期间错过的火次补跑
//! 一次（不是风暴重放——过期多久都只补这一跳，重算 next 越过 now）。

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use sunmao_core::context::MutexRecover;

use super::{Input, Shared, display_path, input_queue_frame, new_session};

/// 触发器形态。`once` 带绝对时刻 `run_at`（epoch ms）；`daily`/`weekly`
/// 带本地时刻 `at`="HH:MM"（weekly 另带 `weekday` 0=周日..6）；
/// `interval` 每 `every_min` 分钟一跳，沿上次排定点顺延保节奏。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SchedKind {
    #[default]
    Once,
    Daily,
    Weekly,
    Interval,
}

/// 一条定时任务 — 文件里就是它、REST 里也是它（serde 直通，无第二形态）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub(crate) struct SchedTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    /// 项目目录的 display 形态（`new_session` 重新 resolve）
    pub cwd: String,
    pub kind: SchedKind,
    pub at: String,
    pub weekday: u32,
    pub every_min: u64,
    /// epoch ms — `once` 的触发时刻
    pub run_at: i64,
    pub enabled: bool,
    /// 下一次触发（epoch ms；0 = 未排定）
    pub next: i64,
    pub last_run: i64,
    /// 上次触发产出的会话 id — UI 里可点进去看结果
    pub last_session: String,
    pub last_error: String,
}

/// schedules.json 的读改写锁 — REST 变更与 tick 补跑都串行过它。
static SCHED: Mutex<()> = Mutex::new(());

fn sched_path(s: &Shared) -> std::path::PathBuf {
    s.cwd.join(".sunmao/schedules.json")
}

pub(crate) fn load(s: &Shared) -> Vec<SchedTask> {
    std::fs::read_to_string(sched_path(s))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(s: &Shared, tasks: &[SchedTask]) -> Result<()> {
    let p = sched_path(s);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(tasks)?)
        .with_context(|| format!("write {}", p.display()))
}

fn now() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}

/// `date` 的 `HH:MM` 本地时刻 — DST 歧义取较早者，不存在的时刻
/// （春季拨快那一小时）返回 None，调用方顺延到次日。
fn local_at(date: chrono::NaiveDate, at: &str) -> Option<chrono::DateTime<chrono::Local>> {
    let t = chrono::NaiveTime::parse_from_str(at, "%H:%M").ok()?;
    date.and_time(t)
        .and_local_timezone(chrono::Local)
        .earliest()
}

/// 严格晚于 `from` 的下一次触发（`once` 已过期/已跑过 → None；`at`
/// 解析失败的 daily/weekly 同样 None——任务留着但不再排定）。
pub(crate) fn next_after(t: &SchedTask, from: chrono::DateTime<chrono::Local>) -> Option<i64> {
    use chrono::Datelike;
    match t.kind {
        SchedKind::Once => (t.run_at > from.timestamp_millis()).then_some(t.run_at),
        SchedKind::Daily | SchedKind::Weekly => {
            for d in 0..370 {
                let date = from.date_naive() + chrono::Days::new(d);
                if t.kind == SchedKind::Weekly && date.weekday().num_days_from_sunday() != t.weekday
                {
                    continue;
                }
                if let Some(cand) = local_at(date, &t.at)
                    && cand > from
                {
                    return Some(cand.timestamp_millis());
                }
            }
            None
        }
        SchedKind::Interval => {
            let step = (t.every_min.max(1)) as i64 * 60_000;
            // 沿既有排定点顺移到未来 — 定闹钟的任务不该被补跑带偏相位
            let mut n = t.next.max(from.timestamp_millis() - step * 366);
            while n <= from.timestamp_millis() {
                n += step;
            }
            Some(n)
        }
    }
}

/// 字段校验（REST 与人手编辑共用这套判定）— 失败是一条可读的 400，
/// 不是排定点上的哑弹。
pub(crate) fn validate(s: &Shared, t: &mut SchedTask) -> Result<(), String> {
    if t.prompt.trim().is_empty() {
        return Err("prompt must be non-empty".into());
    }
    if t.name.trim().is_empty() {
        t.name = t
            .prompt
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(30)
            .collect();
    }
    let cwd = if t.cwd.trim().is_empty() {
        s.cwd.clone()
    } else {
        let p = std::path::PathBuf::from(t.cwd.trim());
        let p = if p.is_absolute() { p } else { s.cwd.join(p) };
        if !p.is_dir() {
            return Err(format!("not a directory: {}", p.display()));
        }
        p.canonicalize().unwrap_or(p)
    };
    t.cwd = display_path(&cwd);
    match t.kind {
        SchedKind::Once if t.run_at <= 0 => return Err("once needs run_at (epoch ms)".into()),
        SchedKind::Daily | SchedKind::Weekly => {
            if chrono::NaiveTime::parse_from_str(&t.at, "%H:%M").is_err() {
                return Err("at must be HH:MM".into());
            }
            if t.kind == SchedKind::Weekly && t.weekday > 6 {
                return Err("weekday must be 0..6".into());
            }
        }
        SchedKind::Interval if t.every_min == 0 => {
            return Err("every_min must be ≥1".into());
        }
        _ => {}
    }
    Ok(())
}

/// 触发一条任务：新会话 + 投 prompt 进它的 FIFO —— 与用户在 composer
/// 按回车完全同一条路（busy 帧、transcript、审批门全同源）。返回会话 id。
pub(crate) async fn fire(s: &Arc<Shared>, t: &SchedTask) -> Result<String> {
    let cwd = std::path::PathBuf::from(&t.cwd);
    let v = new_session(s, Some(cwd), None).await?;
    let id = v["session"].as_str().unwrap_or_default().to_string();
    let host = s.host(&id).context("adopted host missing")?;
    let ticket = host
        .queue_next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    host.queue.lock_or_recover().push_back(Input {
        id: ticket,
        client: 0, // 0 = 无发起 tab — session 切页帧不点名任何人
        text: t.prompt.clone(),
        attachments: Vec::new(),
    });
    host.agent
        .context()
        .input_pending
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    host.queue_notify.notify_one();
    s.emit(input_queue_frame(&host));
    s.emit(serde_json::json!({
        "type": "sched_fired", "task": t.id, "session": id, "name": t.name,
    }));
    Ok(id)
}

/// 到点补跑/更新一体的 tick —— 火一次交回文件前 reload，REST 的并发
/// 编辑不会被 tick 覆盖（读改写整段都在 SCHED 锁里，只有 fire 出锁）。
async fn tick(s: &Arc<Shared>) {
    let now = now();
    let due: Vec<SchedTask> = {
        let _g = SCHED.lock_or_recover();
        load(s)
            .into_iter()
            .filter(|t| t.enabled && t.next > 0 && t.next <= now.timestamp_millis())
            .collect()
    };
    for task in due {
        let res = fire(s, &task).await;
        {
            let _g = SCHED.lock_or_recover();
            let mut tasks = load(s);
            if let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) {
                t.last_run = now.timestamp_millis();
                match &res {
                    Ok(id) => {
                        t.last_session = id.clone();
                        t.last_error.clear();
                    }
                    Err(e) => t.last_error = format!("{e:#}"),
                }
                if t.kind == SchedKind::Once {
                    t.enabled = false;
                    t.next = 0;
                } else {
                    t.next = next_after(t, now).unwrap_or(0);
                }
                let _ = save(s, &tasks);
            }
        }
        if let Err(e) = res {
            tracing::error!("sched {} fire failed: {e:#}", task.id);
        }
        s.emit(serde_json::json!({"type":"schedules_changed"}));
    }
}

/// 调度循环 — `spawn_host` spawn 一次；`interval` 第一跳立即到（把停机
/// 期间的过期任务补掉，而不是再等 15s）。
pub(crate) async fn run(s: Arc<Shared>) {
    let mut t = tokio::time::interval(std::time::Duration::from_secs(15));
    loop {
        t.tick().await;
        tick(&s).await;
    }
}

/// REST 侧共享的读改写 —— 成功改完统一存盘 + 广播 `schedules_changed`。
pub(crate) fn mutate<R>(
    s: &Arc<Shared>,
    f: impl FnOnce(&mut Vec<SchedTask>) -> Result<R, String>,
) -> Result<R, String> {
    let _g = SCHED.lock_or_recover();
    let mut tasks = load(s);
    let r = f(&mut tasks)?;
    save(s, &tasks).map_err(|e| format!("{e:#}"))?;
    s.emit(serde_json::json!({"type":"schedules_changed"}));
    Ok(r)
}
