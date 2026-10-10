// approval cards — the gate's interactive surface. One pending card per
// ask: approve once / deny / allow-for-session, resolved over the ws as a
// verdict keyed by card id. `pendingApprovals` is the only bookkeeping —
// the wait strip and the top bar both read its size.
const pendingApprovals = new Map();  // id -> card element
const approvalCardBody = new WeakMap();

// Explain the tool category without guessing what a shell command does.
// The original command/detail remains visible as the authoritative action.
function approvalImpact(tool) {
  const name = String(tool || '').replace(/^mcp__.*?__/, 'MCP:');
  if (name === 'Write' || name === 'Edit') return '将修改项目文件；请检查目标路径和具体内容。';
  if (name === 'Bash' || name === 'shell' || name === '!') return '将运行一条命令；可能修改文件、启动程序或产生外部影响。';
  if (name.startsWith('MCP:')) return '将调用第三方 MCP 工具；具体影响取决于该工具，请核对参数。';
  if (name === 'WebFetch') return '将向外部地址请求数据；请检查访问目标。';
  if (name === 'Read' || name === 'Grep' || name === 'Glob') return '将读取项目内容；请核对访问范围。';
  return '此工具可能读取或修改数据；请核对操作详情后再决定。';
}

function approvalCard(ev) {
  const host = msgHost();
  const card = append(host, `<div class="approve glass enter" data-apid="${ev.id}">
    <div class="ap-h"><span class="ai">${ic('alert')}</span><span>${t('需要你的批准')}</span><span class="why">${esc(ev.tool || '')} · ${esc(ev.why || '')}</span></div>
    <div class="cmd"><span class="pr">$</span>${esc(ev.detail || '')}</div>
    <div class="ap-impact" role="note">${esc(approvalImpact(ev.tool))}</div>
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
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy && !goalLive();
  renderRail();
}
function decide(verdict, id) {
  let card, aid;
  if (id != null) { card = pendingApprovals.get(id); aid = id; }
  else { aid = [...pendingApprovals.keys()][0]; card = pendingApprovals.get(aid); }
  if (!card) return;
  const approved = wsSend({ type: 'approval', id: aid, sess: sessionId, verdict }, () => {
    // In Tauri the IPC failure arrives asynchronously: do not leave a
    // resolved-looking card when the kernel never accepted the verdict.
    if (pendingApprovals.has(aid)) return;
    pendingApprovals.set(aid, card);
    card.classList.remove('done');
    card.innerHTML = approvalCardBody.get(card) || card.innerHTML;
    syncWait();
    toast(t('审批未送达，请重试'), 'alert', 'warn');
  });
  if (!approved) {
    toast(t('连接中断，审批未送达；请重试'), 'alert', 'warn');
    return;
  }
  approvalCardBody.set(card, card.innerHTML);
  pendingApprovals.delete(aid);
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
