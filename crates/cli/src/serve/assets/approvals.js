// approval cards — the gate's interactive surface. One pending card per
// ask: approve once / deny / allow-for-session, resolved over the ws as a
// verdict keyed by card id. `pendingApprovals` is the only bookkeeping —
// the wait strip and the top bar both read its size.
const pendingApprovals = new Map();  // id -> card element

function approvalCard(ev) {
  const host = msgHost();
  const card = append(host, `<div class="approve glass enter" data-apid="${ev.id}">
    <div class="ap-h"><span class="ai">${ic('alert')}</span><span>${t('需要你的批准')}</span><span class="why">${esc(ev.tool || '')} · ${esc(ev.why || '')}</span></div>
    <div class="cmd"><span class="pr">$</span>${esc(ev.detail || '')}</div>
    <div class="acts">
      <button class="btn allow" data-ap="once">${ic('check')}${t('允许一次')}<kbd>Y</kbd></button>
      <button class="btn ghost" data-ap="deny">${t('拒绝')}<kbd>N</kbd></button>
      <button class="btn ghost" data-ap="session">${t('本会话允许')}<kbd>A</kbd></button>
    </div>
  </div>`);
  pendingApprovals.set(ev.id, card);
  syncWait();
  return card;
}
function collapse(card, html) {
  const h0 = card.offsetHeight;
  card.innerHTML = html; card.classList.add('done');
  motion.height(card, h0, card.offsetHeight);
}
function syncWait() {
  $('#cmp-wait').hidden = pendingApprovals.size === 0;
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy && !curGoal;
  renderRail();
}
function decide(verdict, id) {
  let card, aid;
  if (id != null) { card = pendingApprovals.get(id); aid = id; }
  else { aid = [...pendingApprovals.keys()][0]; card = pendingApprovals.get(aid); }
  if (!card) return;
  pendingApprovals.delete(aid);
  wsSend({ type: 'approval', id: aid, sess: sessionId, verdict });
  const [st, title, note] = { once: ['ok', t('已允许'), t('仅本次')], session: ['ok', t('已允许'), t('本会话内相同命令不再询问')], deny: ['err', t('已拒绝'), t('操作未执行')] }[verdict] || ['err', t('已拒绝'), ''];
  const detail = $('.cmd', card) ? $('.cmd', card).textContent.trim() : '';
  collapse(card, `<div class="ap-done">${stIcon(st)}<b>${title}</b><code>${esc(detail)}</code><span>${note}</span></div>`);
  logEv('approval', `verdict ${verdict} · ${aid}`);
  syncWait();
}
function abortTurn() {
  closeMsg();
  for (const tk of runningTools.splice(0)) setTool(tk.el, 'err', t('已中断'));
}
