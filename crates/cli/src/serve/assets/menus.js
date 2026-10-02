/* menus/actions — act() dispatch, global click/keyboard/contextmenu */
'use strict';

/* ================= menus / actions ================= */
function sessionMenu(id) {
  const cur = id === sessionId;
  const items = [];
  if (!cur) items.push({ v: 'resume', t: '切换到此会话', icon: 'history', d: '接回它的事件日志继续' });
  items.push({ v: 'fork', t: cur ? '从此分叉' : '分叉此会话', icon: 'fork', d: '复制事件日志，另起一支' });
  items.push({ v: 'rewind', t: '回退到某一轮', icon: 'reset', d: '恢复文件到该轮之前，并分叉会话' });
  items.push('-');
  items.push({ v: 'rename', t: '重命名', icon: 'pen', d: '会话的显示标题，写进事件日志' });
  items.push({ v: 'export', t: '导出为 Markdown', icon: 'download', d: '从事件日志折叠成 .md 文件下载' });
  items.push({ v: 'delete', t: '删除会话', icon: 'trash', d: '移除事件日志文件', warn: true });
  items.push('-');
  items.push({ v: 'copy', t: '复制会话 ID', icon: 'copy', d: id });
  return items;
}
function sessionAction(v, id, at) {
  if (v === 'resume') resumeSession(id);
  else if (v === 'fork') forkSession(id);
  else if (v === 'rewind') rewindPick(id, at);
  else if (v === 'rename') renamePop(id, at);
  else if (v === 'export') exportSession(id);
  else if (v === 'delete') deletePop(id, at);
  else if (v === 'copy') { if (navigator.clipboard) navigator.clipboard.writeText(id).catch(() => {}); toast('已复制会话 ID', 'copy'); }
}
function renamePop(id, at) {
  pop(at, `<div class="lbl">重命名会话</div><div class="field"><input id="rn-in" placeholder="${esc(sessTitle(id) || id)}" spellcheck="false" autocomplete="off"></div>`, { onMount(p) {
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
  pop(at, `<div class="lbl">删除会话</div><div class="mp-list"><div class="empty-hint">将永久删除 ${esc(title)} 的事件日志。</div><button class="mi warn" data-yes="1">${ic('trash')}<span class="mt"><span>确认删除</span></span></button></div>`, { onMount(p) {
    p.addEventListener('click', ev => {
      if (!ev.target.closest('[data-yes]')) return;
      closePop(); deleteSession(id);
    });
  } });
}
async function rewindPick(id, at) {
  let turns = [];
  try { turns = (await api(`/session/${encodeURIComponent(id)}/turns`)).turns || []; }
  catch (e) { toast(`回退列表失败：${e.message}`, 'alert', 'warn'); return; }
  if (!turns.length) return toast('没有可回退的轮次', 'reset');
  const items = turns.map(t => ({ v: String(t.n), t: `第 ${t.n} 轮`, d: t.preview }));
  menuPop(at, [{ label: '回退到此轮之前（会话 + 文件）' }, ...items], v => rewindTo(id, +v));
}
async function rewindTo(id, n) {
  try {
    const r = await api(`/session/${encodeURIComponent(id)}/rewind`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ turn: n, mode: 'both' }) });
    const files = (r.restored || []).length;
    if (r.session) wsSend({ type: 'view', id: r.session });
    toast(`已回退到第 ${n} 轮之前${files ? `，恢复 ${files} 个文件` : ''}`, 'reset');
  }
  catch (e) { toast(`回退失败：${e.message}`, 'alert', 'warn'); }
}
async function submitNote(name, btn) {
  const wrap = btn.closest('.notes'), inp = $('input', wrap), v = inp.value.trim();
  if (!v) return inp.focus();
  try {
    const r = await api(`/artifacts/${encodeURIComponent(name)}/annotate?sess=${encodeURIComponent(sessionId)}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ note: v }) });
    inp.value = '';
    toast((r.result || '已写入').replace(/^\[|\]$/g, ''), 'note');
    refreshNotes(btn.closest('.island'), name);
  } catch (e) { toast(`批注失败：${e.message}`, 'alert', 'warn'); }
}

function act(name, el) {
  switch (name) {
    case 'palette': return openPalette();
    case 'palette-close': return closePalette();
    case 'events': if (popAnchor === el) return closePop(); return pop(el, eventsHTML(), { align: 'end', cls: 'events' });
    case 'dock': return toggleDock();
    case 'dock-tab': return dockTab(el.dataset.tab);
    case 'crumb': return menuPop(el, sessionMenu(sessionId), v => sessionAction(v, sessionId, el));
    case 'help': return menuPop(el, [{ v: 'keys', t: '键盘快捷键', icon: 'keyboard' }, { v: 'about', t: '关于 sunmao', icon: 'info' }], v => { show('settings'); settingsPage(v); }, { place: 'top' });
    case 'notes': return el.closest('.island').classList.toggle('show-notes');
    case 'note-add': return submitNote(el.dataset.name, el);
    case 'sandbox': return toast('沙箱：脚本已禁用（纯文档渲染）· 禁网 · 无同源——交互式页面请「在浏览器中打开」', 'lock');
    case 'island-tall': { const isl = el.closest('.island'), on = isl.classList.toggle('tall'); el.innerHTML = ic(on ? 'shrink' : 'expand'); el.dataset.tip = on ? '收起' : '展开'; return; }
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
        return Promise.resolve(TAURI.openExternal(p)).catch(e => toast(`打开失败：${e}`, 'alert', 'warn'));
      }
      return open('/artifacts/' + encodeURIComponent(isl.dataset.artifact) + q, '_blank');
    }
    case 'think': return el.classList.toggle('open');
    case 'tg': { const g = el.closest('.tools'); g.dataset.user = '1'; return foldGroup(g, !g.classList.contains('fold')); }
    case 'pick-mode': {
      // same four stances the kernel gates on — labels mirror the TUI's /mode
      const MODES = [
        { v: 'always_ask', t: '请求批准', d: '写入与风险命令前询问', icon: 'shield-check' },
        { v: 'auto', t: '自动', d: '读写直接放行，风险命令仍询问', icon: 'shield' },
        { v: 'read_only', t: '只读', d: '只能读取与搜索，不执行命令', icon: 'eye' },
        { v: 'full_access', t: '完全访问', d: '不再询问；deny 规则依旧生效', icon: 'lock', warn: true },
      ];
      return menuPop(el, [{ label: '审批模式' }, ...MODES.map(m => Object.assign({}, m, { on: m.v === approvalMode }))], v => { wsSend({ type: 'mode', sel: v }); }, { place: 'top', align: 'end' });
    }
    case 'pick-model': return modelPop(el);
    case 'pick-effort': return effortPop(el);
    case 'send': return send();
    case 'stop': return wsSend({ type: 'cancel' });
    case 'new-chat': return newChat();
    case 'new-chat-pop': return newChatPop(el);
    case 'upload-wall': return $('#file-wall').click();
    case 'color': return colorPop(el, el.dataset.key);
    case 'font': return fontPop(el, el.dataset.key);
    case 'motion': return motionPop(el);
    case 'shell-pick': return shellPick(el);
    case 'side': S.translucentSidebar = !S.translucentSidebar; return commit();
    case 'reset-ui': S = clone(DEFAULTS); commit(); renderWallGrid(); return toast('已恢复默认外观', 'reset');
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
  img.src = src; lab.textContent = cap || '';
  imv.hidden = false;
}
function closeImv() { $('#imv').hidden = true; $('#imv-img').src = ''; }

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
  const th = t.closest('.tool-h'); if (th) { if ($('.tool-o', th.parentElement)) th.parentElement.classList.toggle('open'); return; }
  const swb = t.closest('.sw'); if (swb && !swb.dataset.act) return swb.setAttribute('aria-checked', String(swb.getAttribute('aria-checked') !== 'true'));
  const g = t.closest('[data-go]'); if (g) return go(g.dataset.go);
  const s = t.closest('[data-sess]'); if (s) return resumeSession(s.dataset.sess);
  const pg = t.closest('[data-page]'); if (pg) return settingsPage(pg.dataset.page);
  const w = t.closest('[data-wall]'); if (w) { S.wallpaper = w.dataset.wall; return commit(); }
  const m = t.closest('.tc[data-mode]'); if (m) { S.mode = m.dataset.mode; return commit(); }
  const a = t.closest('[data-act]'); if (a) return act(a.dataset.act, a);
  const pv = t.closest('[data-pv]'); if (pv) return providerAction(pv.dataset.pv, pv);
  const mc = t.closest('[data-mc]'); if (mc) return toggleCand(mc.dataset.mc);
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
  if (!e.target.closest('input,textarea,.bubble,.cmd,pre,.jp-b,.think-o')) e.preventDefault();
});
let rz = 0;
addEventListener('resize', () => { clearTimeout(rz); rz = setTimeout(() => paintWall(), DEBOUNCE_RESIZE); });
matchMedia('(prefers-color-scheme: light)').addEventListener('change', () => { if (S.mode === 'system') apply(); });
matchMedia('(prefers-reduced-motion: reduce)').addEventListener('change', () => { if (S.motion === 'system') apply(); });

