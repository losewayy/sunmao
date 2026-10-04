/* 定时任务 — /schedules REST 的列表 + 建改表单。任务到点由宿主起一条
   普通会话执行；此视图只做编排（增删改、启停、立即跑、看上次产物）。 */
'use strict';

let SCHEDULES = [];
const SCHED_KIND = { once: '单次', daily: '每天', weekly: '每周', interval: '每隔' };
const WD = '日一二三四五六';

async function refreshSchedules() {
  try { SCHEDULES = (await api('/schedules')).tasks || []; } catch { SCHEDULES = []; }
  renderSchedules();
}
function schedDesc(t) {
  switch (t.kind) {
    case 'daily': return `每天 ${t.at || ''}`;
    case 'weekly': return `每周${WD[t.weekday] || '日'} ${t.at || ''}`;
    case 'interval': return `每隔 ${t.every_min || '?'} 分钟`;
    default: return `单次 · ${fmtWhen(t.run_at)}`;
  }
}
function fmtWhen(ms) {
  if (!ms) return '—';
  const d = new Date(ms), today = dayStart(Date.now());
  const hm = pad(d.getHours()) + ':' + pad(d.getMinutes());
  if (ms >= today && ms < today + 864e5) return `今天 ${hm}`;
  if (ms >= today - 864e5 && ms < today) return `昨天 ${hm}`;
  if (ms >= today + 864e5 && ms < today + 2 * 864e5) return `明天 ${hm}`;
  return `${d.getMonth() + 1}/${d.getDate()} ${hm}`;
}
function renderSchedules() {
  const box = $('#sch-list');
  if (!box) return;
  box.innerHTML = SCHEDULES.map(t => {
    const last = t.last_run
      ? `<span>上次 ${fmtWhen(t.last_run)}${t.last_session ? ` · <a data-sess="${esc(t.last_session)}">${esc(t.last_session)}</a>` : ''}${t.last_error ? ` · <em class="err">${esc(t.last_error)}</em>` : ''}</span>`
      : '<span>尚未运行</span>';
    return `<div class="cr">
      <button class="sw" role="switch" data-act="sched-toggle" data-id="${esc(t.id)}" aria-checked="${!!t.enabled}" aria-label="启用"></button>
      <div class="l"><b>${esc(t.name || t.prompt.slice(0, 30))}</b><span>${esc(schedDesc(t))} · ${esc(projectName(t.cwd) || t.cwd)} · ${t.enabled ? '下次 ' + fmtWhen(t.next) : '已停用'}</span>${last}</div>
      <div class="sch-ops">
        <button class="ib sm" data-act="sched-run" data-id="${esc(t.id)}" data-tip="立即运行">${ic('rotate')}</button>
        <button class="ib sm" data-act="sched-edit" data-id="${esc(t.id)}" data-tip="编辑">${ic('pen')}</button>
        <button class="ib sm" data-act="sched-del" data-id="${esc(t.id)}" data-tip="删除">${ic('trash')}</button>
      </div>
    </div>`;
  }).join('') || '<div class="empty-hint">暂无定时任务——到点它会自动开一条会话跑起来。</div>';
}

/* ---- 建/改表单 —— 频率下的条件字段随 kind 显隐 ---- */
const dtLocal = ms => { const d = new Date(ms); return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`; };
function schedForm(id) {
  const t = id ? SCHEDULES.find(x => x.id === id) : null;
  const form = $('#sch-form');
  if (!form) return;
  form.hidden = false;
  const kind = t ? t.kind : 'daily';
  form.innerHTML = `
    <div class="sch-f"><label>名称<span class="opt">可选，缺省取任务首行</span></label><span class="fin"><input id="sf-name" value="${esc(t ? t.name : '')}" placeholder="晨间项目体检" autocomplete="off" spellcheck="false"></span></div>
    <div class="sch-f"><label>任务内容<span class="opt">到点作为新会话的首条输入</span></label><span class="fin tall"><textarea id="sf-prompt" rows="3" placeholder="例如：跑一遍 cargo test，汇总失败项并修复可以自动修的部分" spellcheck="false">${esc(t ? t.prompt : '')}</textarea></span></div>
    <div class="sch-f"><label>频率</label><span class="fin"><button type="button" class="pv-sel" id="sf-kind" data-v="${kind}"><span>${esc(SCHED_KIND[kind])}</span>${ic('chev-d')}</button></span></div>
    <div class="sch-f" id="sf-at" hidden><label>时间</label><span class="fin"><input id="sf-time" type="time" value="${esc(t && t.at ? t.at : '09:00')}"></span></div>
    <div class="sch-f" id="sf-wd" hidden><label>星期</label><span class="fin"><button type="button" class="pv-sel" id="sf-weekday" data-v="${t ? t.weekday : 1}"><span>周${esc(WD[t ? t.weekday : 1])}</span>${ic('chev-d')}</button></span></div>
    <div class="sch-f" id="sf-every" hidden><label>间隔（分钟）</label><span class="fin"><input id="sf-min" type="number" min="1" step="1" value="${t && t.every_min || 60}"></span></div>
    <div class="sch-f" id="sf-once" hidden><label>执行时间</label><span class="fin"><input id="sf-when" type="datetime-local" value="${dtLocal(t && t.run_at ? t.run_at : Date.now() + 3600e3)}"></span></div>
    <div class="sch-f"><label>项目目录</label><span class="fin sch-pick"><input id="sf-cwd" value="${esc(t ? t.cwd : cwd)}" spellcheck="false" autocomplete="off"><button type="button" class="ib sm" id="sf-cwd-pick" data-tip="最近项目">${ic('chev-d')}</button></span></div>
    <div class="sch-acts"><button class="btn ghost sm" data-act="sched-cancel">取消</button><button class="btn allow sm" data-act="sched-save"${t ? ` data-id="${esc(t.id)}"` : ''}>保存</button></div>`;
  // pickers ride the same menuPop chrome as every other selector — a
  // native <select> pops a system-drawn menu that ignores the theme
  const pick = (btn, items, apply) => btn && btn.addEventListener('click', () => menuPop(btn, items.map(o => ({ v: o.v, t: o.t, on: String(o.v) === btn.dataset.v })), v => { btn.dataset.v = v; btn.querySelector('span').textContent = items.find(i => String(i.v) === v).t; if (apply) apply(v); }, { align: 'start' }));
  const kindBtn = $('#sf-kind', form);
  const sync = () => {
    const k = kindBtn.dataset.v;
    $('#sf-at', form).hidden = !(k === 'daily' || k === 'weekly');
    $('#sf-wd', form).hidden = k !== 'weekly';
    $('#sf-every', form).hidden = k !== 'interval';
    $('#sf-once', form).hidden = k !== 'once';
  };
  pick(kindBtn, Object.entries(SCHED_KIND).map(([v, t]) => ({ v, t })), sync);
  pick($('#sf-weekday', form), [1, 2, 3, 4, 5, 6, 0].map(d => ({ v: d, t: '周' + WD[d] })));
  const cwdPick = $('#sf-cwd-pick', form), cwdIn = $('#sf-cwd', form);
  if (cwdPick && (PROJECTS || []).length) cwdPick.addEventListener('click', () => menuPop(cwdPick, PROJECTS.map(p => ({ v: p, t: projectName(p) || p, d: projectName(p) ? p : '', on: p === cwdIn.value })), v => { cwdIn.value = v; }, { align: 'end' }));
  sync();
  form.scrollIntoView({ block: 'nearest', behavior: motion.reduced() ? 'auto' : 'smooth' });
  $('#sf-prompt', form).focus();
}
async function schedSave(id) {
  const form = $('#sch-form'), k = $('#sf-kind', form).dataset.v;
  const body = {
    name: $('#sf-name', form).value.trim(),
    prompt: $('#sf-prompt', form).value.trim(),
    cwd: $('#sf-cwd', form).value.trim(),
    kind: k,
    at: $('#sf-time', form).value || '09:00',
    weekday: +$('#sf-weekday', form).dataset.v || 0,
    every_min: +$('#sf-min', form).value || 60,
    run_at: k === 'once' ? new Date($('#sf-when', form).value).getTime() || 0 : 0,
    enabled: true,
  };
  if (!body.prompt) { $('#sf-prompt', form).focus(); return toast('任务内容不能为空', 'alert', 'warn'); }
  try {
    await api(id ? `/schedules/${encodeURIComponent(id)}` : '/schedules', { method: id ? 'PUT' : 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
    form.hidden = true; form.innerHTML = '';
    toast('定时任务已保存', 'check');
    refreshSchedules();
  } catch (e) { toast(`保存失败：${e.message}`, 'alert', 'warn'); }
}
async function schedToggle(id) {
  const t = SCHEDULES.find(x => x.id === id);
  if (!t) return;
  try {
    await api(`/schedules/${encodeURIComponent(id)}`, { method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(Object.assign({}, t, { enabled: !t.enabled })) });
    refreshSchedules();
  } catch (e) { toast(`切换失败：${e.message}`, 'alert', 'warn'); }
}
async function schedDel(id) {
  try {
    await api(`/schedules/${encodeURIComponent(id)}`, { method: 'DELETE' });
    SCHEDULES = SCHEDULES.filter(t => t.id !== id);
    renderSchedules();
    toast('已删除定时任务', 'trash');
  } catch (e) { toast(`删除失败：${e.message}`, 'alert', 'warn'); }
}
async function schedRun(id) {
  try {
    const r = await api(`/schedules/${encodeURIComponent(id)}/run`, { method: 'POST' });
    toast('已启动 — 会话见侧边栏', 'zap');
    if (r && r.session) { show('session'); wsSend({ type: 'view', id: r.session }); }
  } catch (e) { toast(`启动失败：${e.message}`, 'alert', 'warn'); }
}
