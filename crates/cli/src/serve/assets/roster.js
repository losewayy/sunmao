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
  $('#dt-count').textContent = tasks.length ? String(tasks.length) : '';
  const st = t => t.done === null || t.done === undefined ? 'run' : t.done ? 'done' : 'err';
  box.innerHTML = tasks.map(t => {
    const s = st(t);
    const tag = t.agent ? `<span class="tag">@${esc(t.agent)}</span>` : '';
    // running rows are steerable — the popover sends a task_steer frame;
    // finished rows are display-only (resume is the composer's grammar)
    const row = `<i class="sd ${s === 'run' ? 'run' : s === 'done' ? 'done' : 'off'}"></i><span class="rt-id mono">${esc(t.id)}</span>${tag}<span class="rt-p">${esc(t.prompt || '')}</span>${s === 'run' ? ic('chev-r', 'i xs rt-go') : ''}`;
    return s === 'run' ? `<button class="rt" data-task="${esc(t.id)}" data-tip="发送引导|点击输入要插给它的话">${row}</button>` : `<div class="rt">${row}</div>`;
  }).join('') || '<div class="empty-row">没有子代理在跑</div>';
}
function taskSteerPop(anchor, id) {
  if (popAnchor === anchor) return closePop();
  pop(anchor, `<div class="lbl">引导 ${esc(id)}</div><div class="field"><input id="st-in" placeholder="插一句话给它，回车发送" spellcheck="false" autocomplete="off"></div><div class="hint">进入它的引导队列 — 在下一个请求边界并入正在跑的轮次</div><div class="field"><button id="st-kill" class="btn btn-sm" data-tip="终止|立即停止该子代理（不等下一条消息）">终止这个子代理</button></div>`, { align: 'end', onMount(p) {
    const inp = $('#st-in', p);
    inp.addEventListener('keydown', e => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const t = inp.value.trim(); if (!t) return;
      closePop();
      wsSend({ type: 'task_steer', sess: sessionId, id, text: t });
      toast('已发送给 ' + id, 'arrow-up');
    });
    $('#st-kill', p).addEventListener('click', () => {
      closePop();
      wsSend({ type: 'task_cancel', sess: sessionId, id });
      toast('已终止 ' + id, 'x');
    });
    setTimeout(() => inp.focus(), 20);
  } });
}
document.addEventListener('click', e => {
  const b = e.target.closest('[data-task]');
  if (b) taskSteerPop(b, b.dataset.task);
});
