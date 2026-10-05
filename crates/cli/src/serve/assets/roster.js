/* sub-agent roster — dock card fed by GET /tasks, nudged by tasks.changed */
'use strict';

/* ================= sub-agent roster card ================= */
/* The kernel owns the roster; the card is a read replica. Pushes arrive as
   live `hook` events (tasks.changed) emitted at register/finish — each is
   just a nudge, the card re-pulls GET /tasks (throttled). */
let rtTimer = 0;
function refreshRosterSoon() { clearTimeout(rtTimer); rtTimer = setTimeout(refreshRoster, DEBOUNCE_ROSTER); }
async function refreshRoster() {
  if (!sessionId) return renderRoster([]);
  try { renderRoster((await api('/tasks?sess=' + encodeURIComponent(sessionId))).tasks || []); }
  catch { /* host gone between frames — keep the last list */ }
}
function renderRoster(tasks) {
  const box = $('#dt-list');
  if (!box) return;
  // badge lives inside the tab chip now — the tab label already says WHAT,
  // so the count is a bare number, not a phrase that would overflow the pill
  const dc = $('#dt-count'); if (dc) dc.textContent = tasks.length ? String(tasks.length) : '';
  const st = task => task.done === null || task.done === undefined ? 'run' : task.done ? 'done' : 'err';
  box.innerHTML = tasks.map(task => {
    const s = st(task);
    const tag = task.agent ? `<span class="tag">@${esc(task.agent)}</span>` : '';
    // running rows are steerable (the popover sends a task_steer frame);
    // finished rows carry data-sess — the id doubles as the child's log
    // stem, so click lands on the same resume/menu wiring the rail runs
    // (click → 接回日志, right-click → 分叉/导出/删除)
    const row = `<i class="sd ${s === 'run' ? 'run' : s === 'done' ? 'done' : 'off'}"></i><span class="rt-id mono">${esc(task.id)}</span>${tag}<span class="rt-p">${esc(task.prompt || '')}</span>${s === 'run' ? ic('chev-r', 'i xs rt-go') : ''}`;
    if (s === 'run') return `<button class="rt" data-task="${esc(task.id)}" data-tip="${t('发送引导')}|${t('点击输入要插给它的话')}">${row}</button>`;
    return `<button class="rt" data-sess="${esc(task.id)}" data-tip="${t('点击查看运行记录 · 右键更多操作')}">${row}</button>`;
  }).join('') || `<div class="empty-row">${t('没有子代理在跑')}</div>`;
}
function taskSteerPop(anchor, id) {
  if (popAnchor === anchor) return closePop();
  pop(anchor, `<div class="lbl">${esc(t('引导 {id}', { id }))}</div><div class="field"><input id="st-in" placeholder="${t('插一句话给它，回车发送')}" spellcheck="false" autocomplete="off"></div><div class="hint">${t('进入它的引导队列 — 在下一个请求边界并入正在跑的轮次')}</div><div class="field"><button id="st-kill" class="btn btn-sm" data-tip="${t('终止')}|${t('立即停止该子代理（不等下一条消息）')}">${t('终止这个子代理')}</button></div>`, { align: 'end', onMount(p) {
    const inp = $('#st-in', p);
    inp.addEventListener('keydown', e => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const txt = inp.value.trim(); if (!txt) return;
      closePop();
      wsSend({ type: 'task_steer', sess: sessionId, id, text: txt });
      toast(t('已发送给 {id}', { id }), 'arrow-up');
    });
    $('#st-kill', p).addEventListener('click', () => {
      closePop();
      wsSend({ type: 'task_cancel', sess: sessionId, id });
      toast(t('已终止 {id}', { id }), 'x');
    });
    setTimeout(() => inp.focus(), 20);
  } });
}
document.addEventListener('click', e => {
  const b = e.target.closest('[data-task]');
  if (b) taskSteerPop(b, b.dataset.task);
});
