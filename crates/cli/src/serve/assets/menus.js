/* menus/actions — act() dispatch, global click/keyboard/contextmenu */
'use strict';

/* ================= menus / actions ================= */
function sessionMenu(id) {
  const cur = id === sessionId;
  const items = [];
  if (!cur) items.push({ v: 'resume', t: t('打开此会话'), icon: 'history' });
  items.push({ v: 'fork', t: t('基于此会话新建对话'), icon: 'fork' });
  items.push({ v: 'rewind', t: t('回退到某一轮'), icon: 'reset' });
  if (cur) items.push({ v: 'compact', t: t('压缩上下文'), icon: 'shrink' });
  items.push('-');
  items.push({ v: 'rename', t: t('重命名'), icon: 'pen' });
  items.push({ v: 'export', t: t('导出为 Markdown'), icon: 'download' });
  items.push({ v: 'delete', t: t('删除会话'), icon: 'trash', warn: true });
  items.push('-');
  items.push({ v: 'copy', t: t('复制会话 ID'), icon: 'copy' });
  return items;
}
function sessionAction(v, id, at) {
  if (v === 'resume') resumeSession(id);
  else if (v === 'fork') forkSession(id);
  else if (v === 'rewind') rewindPick(id, at);
  else if (v === 'compact') wsSend({ type: 'prompt', text: '/compact' });
  else if (v === 'rename') renamePop(id, at);
  else if (v === 'export') exportSession(id);
  else if (v === 'delete') deletePop(id, at);
  else if (v === 'copy') { if (navigator.clipboard) navigator.clipboard.writeText(id).catch(() => {}); toast(t('已复制会话 ID'), 'copy'); }
}
function renamePop(id, at) {
  pop(at, `<div class="lbl">${t('重命名会话')}</div><div class="field"><input id="rn-in" placeholder="${esc(sessTitle(id) || id)}" spellcheck="false" autocomplete="off"></div>`, { onMount(p) {
    const inp = $('#rn-in', p);
    inp.addEventListener('keydown', e => {
      if (e.key !== 'Enter') return;
      e.preventDefault(); const t = inp.value.trim();
      closePop(); if (t) renameSession(id, t);
    });
    setTimeout(() => inp.focus(), 20);
  } });
}
function deletePop(id, at) {
  const title = sessTitle(id) || id;
  pop(at, `<div class="lbl">${t('删除会话')}</div><div class="mp-list"><div class="empty-hint">${t('将永久删除“{title}”及其聊天记录。', { title: esc(title) })}</div><button class="mi warn" data-yes="1">${ic('trash')}<span class="mt"><span>${t('确认删除')}</span></span></button></div>`, { onMount(p) {
    p.addEventListener('click', ev => {
      if (!ev.target.closest('[data-yes]')) return;
      closePop(); deleteSession(id);
    });
  } });
}
async function rewindPick(id, at) {
  let turns = [];
  try { turns = (await api(`/session/${encodeURIComponent(id)}/turns`)).turns || []; }
  catch (e) { toast(t('回退列表失败：{msg}', { msg: e.message }), 'alert', 'warn'); return; }
  if (!turns.length) return toast(t('没有可回退的轮次'), 'reset');
  const items = turns.map(turn => ({ v: String(turn.n), t: t('第 {n} 轮', { n: turn.n }), d: turn.preview }));
  menuPop(at, [{ label: t('选择要回退到的位置') }, ...items], v => rewindTo(id, +v));
}
async function rewindTo(id, n) {
  try {
    const r = await api(`/session/${encodeURIComponent(id)}/rewind`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ turn: n, mode: 'both' }) });
    const files = (r.restored || []).length;
    if (r.session) wsSend({ type: 'view', id: r.session });
    toast(t('已回退到第 {n} 轮之前', { n }) + (files ? t('，恢复 {files} 个文件', { files }) : ''), 'reset');
  }
  catch (e) { toast(t('回退失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
}
// per-message rewind: trim to just before this user turn (files restored
// by the same checkpoint path), then hand its text back to the composer
// so the prompt can be edited and re-sent — the classic edit-and-retry
async function rewindMsg(el) {
  const msg = el.closest('.msg.you'), n = +msg.dataset.turn;
  const text = $('.bubble', msg)?.textContent || '';
  await rewindTo(sessionId, n);
  const ta = $('#input');
  if (text) { ta.value = text; autoGrow(); ta.focus(); }
}
async function submitNote(name, btn) {
  const wrap = btn.closest('.notes'), inp = $('input', wrap), v = inp.value.trim();
  if (!v) return inp.focus();
  try {
    const r = await api(`/artifacts/${encodeURIComponent(name)}/annotate?sess=${encodeURIComponent(sessionId)}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ note: v }) });
    inp.value = '';
    toast((r.result || t('已写入')).replace(/^\[|\]$/g, ''), 'note');
    refreshNotes(btn.closest('.island'), name);
  } catch (e) { toast(t('批注失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
}

/* ---- stop acknowledgement ----
   `busy:false` only arrives once the kernel's round has really ended, so a
   stop click used to change nothing on screen until the turn was already
   over — indistinguishable from a dead button. The click flips this state
   in the same frame; the turn end (`setBusy(false)`), a fresh busy frame,
   or the unlock timer (set above the kernel's hard-stop grace) releases
   it. */
let stopping = false, stopUnlockT = 0;
const STOP_UNLOCK_MS = 14000;
function markStopping() {
  if (!busy || stopping) return;
  stopping = true;
  stopLabel(t('停止生成') + '…');
  const btn = $('#send-btn');
  btn.dataset.act = 'stopping'; // no dispatcher case — a repeat click is inert
  btn.disabled = true;
  btn.classList.add('off');
  clearTimeout(stopUnlockT);
  stopUnlockT = setTimeout(clearStopping, STOP_UNLOCK_MS);
}
function clearStopping() {
  if (!stopping) return;
  stopping = false;
  clearTimeout(stopUnlockT);
  stopLabel(t('执行中'));
  $('#send-btn').classList.remove('off');
  setBusy(busy);
}
// the indicator's text node follows its spinner svg — same tail-replace
// i18n uses, so the icon survives
function stopLabel(text) {
  const ind = $('#cmp-busy');
  const last = ind && [...ind.childNodes].reverse().find(n => n.nodeType === 3 && n.nodeValue.trim());
  if (last) last.nodeValue = text;
}

function act(name, el) {
  switch (name) {
    case 'palette': return openPalette();
    case 'palette-close': return closePalette();
    case 'events': if (popAnchor === el) return closePop(); return pop(el, eventsHTML(), { align: 'end', cls: 'events' });
    case 'dock': return toggleDock();
    case 'rail': return toggleRail();
    case 'dock-tab': return dockTab(el.dataset.tab);
    case 'crumb':
      if (view !== 'session' || !sessionId) return;
      return menuPop(el, sessionMenu(sessionId), v => sessionAction(v, sessionId, el));
    case 'help': return menuPop(el, [{ v: 'keys', t: t('键盘快捷键'), icon: 'keyboard' }, { v: 'about', t: t('关于 {brand}', { brand: brand() }), icon: 'info' }], v => { show('settings'); settingsPage(v); }, { place: 'top' });
    case 'annotate': {
      // a dock browser tab is a real guest webview: its document is out of
      // this page's reach, so the picker runs in there (the shell injects it)
      // and only the finished note comes back
      const bp = el.closest('.dock-pane.br');
      if (bp && nativeBr()) return brAnnToggle(bp);
      return annToggle(el.closest('.island,.dock-pane.br'));
    }
    case 'dock-add': return dockAdd(el);
    case 'note-add': return submitNote(el.dataset.name, el);
    case 'island-tall': { const isl = el.closest('.island'), on = isl.classList.toggle('tall'); el.innerHTML = ic(on ? 'shrink' : 'expand'); el.dataset.tip = on ? t('收起') : t('展开'); return; }
    case 'rev-prev': case 'rev-next': {
      const isl = el.closest('.island');
      const cur = +isl.dataset.revCur || 1, max = +isl.dataset.revMax || 1;
      isl.dataset.revCur = Math.min(Math.max(cur + (name === 'rev-prev' ? -1 : 1), 1), max);
      return setIslandRev(isl);
    }
    case 'island-open': {
      const isl = el.closest('.island');
      const cur = +isl.dataset.revCur || 1, max = +isl.dataset.revMax || 1;
      const q = cur === max ? '' : `?rev=${cur}`;
      // under the shell the artifact URL is a WebView2-internal scheme —
      // an external browser can't resolve it; open the real file path
      if (TAURI) {
        const p = `${cwd}/.sunmao/artifacts/${isl.dataset.artifact}.html`;
        return Promise.resolve(TAURI.openExternal(p)).catch(e => toast(t('打开失败：{msg}', { msg: e }), 'alert', 'warn'));
      }
      return open('/artifacts/' + encodeURIComponent(isl.dataset.artifact) + q, '_blank');
    }
    case 'think': {
      // the body is the button's next sibling (`.think.open + .think-o`), not a
      // child: a descendant lookup returns null, and the fold throws on null
      // after the class was already added — which is exactly "opens, never
      // closes"
      const out = el.nextElementSibling;
      return motion.fold(el, out && out.classList.contains('think-o') ? out : null, !el.classList.contains('open'));
    }
    case 'tg': { const g = el.closest('.tools'); g.dataset.user = '1'; return foldGroup(g, !g.classList.contains('fold')); }
    case 'rewind-turn': return rewindMsg(el);
    case 'pick-mode': {
      // same four stances the kernel gates on — labels mirror the TUI's /mode
      const MODES = [
        { v: 'always_ask', t: t('请求批准'), d: t('写入与风险命令前询问'), icon: 'shield-check' },
        { v: 'auto', t: t('自动'), d: t('读写直接放行，风险命令仍询问'), icon: 'shield' },
        { v: 'read_only', t: t('只读'), d: t('只能读取与搜索，不执行命令'), icon: 'eye' },
        { v: 'full_access', t: t('完全访问'), d: t('不再询问；deny 规则依旧生效'), icon: 'lock', warn: true },
      ];
      return menuPop(el, [{ label: t('审批模式') }, ...MODES.map(m => Object.assign({}, m, { on: m.v === approvalMode }))], v => { wsSend({ type: 'mode', sel: v }); }, { place: 'top', align: 'end' });
    }
    case 'pick-model': return modelPop(el);
    case 'pick-effort': return effortPop(el);
    case 'send': return send();
    case 'cmp-attach': return $('#att-file').click();
    case 'stop': markStopping(); return wsSend({ type: 'cancel' });
    case 'new-chat': return newChat();
    case 'grants-clear': return revokeGrant('*');
    case 'compact': return wsSend({ type: 'prompt', text: '/compact' });
    case 'new-chat-pop': return newChatPop(el);
    case 'rail-mode': S.railGroup = S.railGroup === 'project' ? 'time' : 'project'; save(); return renderRail();
    case 'sched-new': return schedForm();
    case 'sched-edit': return schedForm(el.dataset.id);
    case 'sched-cancel': { const f = $('#sch-form'); if (f) { f.hidden = true; f.innerHTML = ''; } return; }
    case 'sched-save': return schedSave(el.dataset.id);
    case 'sched-toggle': return schedToggle(el.dataset.id);
    case 'sched-del': return schedDel(el.dataset.id);
    case 'sched-run': return schedRun(el.dataset.id);
    case 'upload-wall': return $('#file-wall').click();
    case 'color': return colorPop(el, el.dataset.key);
    case 'font': return fontPop(el, el.dataset.key);
    case 'motion': return motionPop(el);
    case 'lang': return langPick();
    case 'shell-pick': return shellPick(el);
    case 'side': S.translucentSidebar = !S.translucentSidebar; return commit();
    case 'reset-ui': S = clone(DEFAULTS); commit(); renderWallGrid(); return toast(t('已恢复默认外观'), 'reset');
    case 'win-min': { const w = shellWin(); if (w) w.win('min'); return; }
    case 'win-max': { const w = shellWin(); if (w) w.win('max'); return; }
    case 'win-close': { const w = shellWin(); if (w) w.win('close'); return; }
    case 'imv-close': return closeImv();
  }
}

/* ================= image viewer ================= */
// click-to-preview for every attachment surface — composer chips (the
// blob: thumb) and sent/replayed thumbs (the /attachments URL) share one
// overlay; the same full-size URL the kernel serves is what a transcript
// img already points at, so the viewer needs no new fetch path
function openImv(src, cap) {
  const imv = $('#imv'), img = $('#imv-img'), lab = $('#imv-cap');
  imv.classList.remove('out');
  img.src = src; lab.textContent = cap || '';
  imv.hidden = false;
  brOverlay(true);
}
function closeImv() { const v = $('#imv'); v.classList.add('out'); setTimeout(() => { if (v.classList.contains('out')) { v.hidden = true; v.classList.remove('out'); $('#imv-img').src = ''; } }, motion.dur('fast')); brOverlay(null); }

/* ================= global input ================= */
document.addEventListener('click', e => {
  const t = e.target;
  if (t.closest('.pop') && !t.closest('[data-act]')) return;
  // composer chip thumb → viewer (the chip × keeps working — check first)
  const chip = t.closest('.att-chip');
  if (chip && !t.closest('.chip-x')) {
    const img = chip.querySelector('img');
    if (img) { openImv(img.src, (chip.querySelector('.att-n') || {}).textContent); return; }
  }
  // a transcript attachment thumb → viewer
  const im = t.closest('img.att');
  if (im) { openImv(im.src, im.alt || im.title); return; }
  const apb = t.closest('[data-ap]'); if (apb) { const cd = apb.closest('.approve'); return decide(apb.dataset.ap, cd ? +cd.dataset.apid : null); }
  const th = t.closest('.tool-h'); if (th) { const tool = th.parentElement, body = $('.tool-o', tool); if (body) motion.fold(tool, body, !tool.classList.contains('open')); return; }
  const swb = t.closest('.sw'); if (swb && !swb.dataset.act) return swb.setAttribute('aria-checked', String(swb.getAttribute('aria-checked') !== 'true'));
  const g = t.closest('[data-go]'); if (g) return go(g.dataset.go);
  // project-group header — fold/unfold lives on S.railFold (ui.json)
  const fg = t.closest('[data-fold]');
  if (fg) { const m = S.railFold || (S.railFold = {}); m[fg.dataset.fold] = !m[fg.dataset.fold]; save(); return renderRail(); }
  // a session link outside the session view (定时任务's 上次会话, …)
  // must land back on the transcript, not just adopt the host
  const s = t.closest('[data-sess]'); if (s) { if (view !== 'session') show('session'); return resumeSession(s.dataset.sess); }
  const pg = t.closest('[data-page]'); if (pg) return settingsPage(pg.dataset.page);
  const gv = t.closest('[data-gv]'); if (gv) return revokeGrant(gv.dataset.gv);
  const ht = t.closest('[data-ht]'); if (ht) { const [i, tr] = ht.dataset.ht.split(':'); return setHookTrust(+i, tr === '1'); }
  const wx = t.closest('[data-wx]'); if (wx) return wallClear();
  const w = t.closest('[data-wall]'); if (w) { S.wallpaper = w.dataset.wall; return commit(); }
  const m = t.closest('.tc[data-mode]'); if (m) { S.mode = m.dataset.mode; return commit(); }
  const a = t.closest('[data-act]'); if (a) return act(a.dataset.act, a);
  const pv = t.closest('[data-pv]'); if (pv) return providerAction(pv.dataset.pv, pv);
  const mc = t.closest('[data-mc]'); if (mc) return toggleCand(mc.dataset.mc);
  const mf = t.closest('[data-mf]'); if (mf) return toggleField(mf.dataset.mf, mf.dataset.mk, mf.dataset.mid);
});
document.addEventListener('keydown', e => {
  const typing = e.target.closest && e.target.closest('input,textarea,[contenteditable]');
  const mod = e.ctrlKey || e.metaKey, k = e.key.toLowerCase();
  if (e.key === 'Escape') { if (!$('#imv').hidden) return closeImv(); if (!$('#palette').hidden) return closePalette(); if (popEl) return closePop(); if (findBar) return closeFind(); if (view === 'settings') return go('back'); if (typing) e.target.blur(); return; }
  if (mod && k === 'f') { e.preventDefault(); return openFind(); }
  if (mod && k === 'k') { e.preventDefault(); return openPalette(); }
  if (mod && k === 'n') { e.preventDefault(); return newChat(); }
  if (mod && e.key === ',') { e.preventDefault(); return go('settings'); }
  if (mod && e.key === '\\') { e.preventDefault(); return toggleDock(); }
  if (mod && k === 'b') { e.preventDefault(); return toggleRail(); }
  if (e.target.classList && e.target.classList.contains('note-add') === false && e.target.closest && e.target.closest('.note-add') && e.target.matches('input') && e.key === 'Enter') return submitNote(e.target.dataset.name, e.target);
  if (!typing && !mod && !e.altKey && pendingApprovals.size && $('#palette').hidden) {
    if (k === 'y') decide('once'); else if (k === 'n') decide('deny'); else if (k === 'a') decide('session');
  }
});
document.addEventListener('contextmenu', e => {
  const s = e.target.closest('[data-sess]');
  if (s) {
    e.preventDefault(); const id = s.dataset.sess, x = e.clientX, y = e.clientY;
    const at = { getBoundingClientRect: () => ({ left: x, right: x, top: y, bottom: y, width: 0, height: 0 }), contains: () => false };
    closePop();
    return pop(at, menuHTML(sessionMenu(id)), { onMount(p) { p.addEventListener('click', ev => { const b = ev.target.closest('.mi'); if (!b) return; closePop(); sessionAction(b.dataset.v, id, at); }); } });
  }
  if (!e.target.closest('input,textarea,.bubble,.cmd,pre,.think-o')) e.preventDefault();
});
let rz = 0;
addEventListener('resize', () => { clearTimeout(rz); rz = setTimeout(() => paintWall(), DEBOUNCE_RESIZE); });
matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => { if (S.mode === 'system') apply(); });
matchMedia('(prefers-reduced-motion: reduce)').addEventListener('change', () => { if (S.motion === 'system') apply(); });

