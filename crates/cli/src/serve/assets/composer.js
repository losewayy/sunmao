/* composer — input, steer queue, @-mentions, slash menu */
'use strict';

/* ================= composer ================= */
// paste stash — ≥2 KiB pastes ride as [paste #N] markers and expand into
// <pasted-text> blocks at submit, same convention as the TUI
const PASTE_STASH_LIMIT = 2048;
let pasteStash = [];
// image attachments uploaded this draft — each entry {marker, path, mime};
// send() ships the ones whose marker still sits in the text
let pendingAtts = [];
function expandPastes(text) {
  let out = text;
  pasteStash.forEach((content, i) => {
    const marker = `[paste #${i + 1}]`;
    if (out.includes(marker)) out = out.replace(marker, `\n<pasted-text>\n${content}\n</pasted-text>\n`);
  });
  return out;
}
function autoGrow() { const ta = $('#input'); ta.style.height = '34px'; ta.style.height = Math.min(168, ta.scrollHeight) + 'px'; slashCheck(); atCheck(); }
function send() {
  const ta = $('#input'), text = ta.value.trim();
  if (!text) return ta.focus();
  // frontend-local commands — they never reach the kernel (the kernel's
  // slash list filters them; this is the same rule kept on the send path)
  const lc = text.match(/^\/(\S+)(?:\s|$)/);
  if (lc && ['clear', 'quit', 'exit', 'multiline'].includes(lc[1])) {
    ta.value = ''; autoGrow(); slashCheck(true);
    if (lc[1] === 'clear') { TX.innerHTML = ''; updateHero(); return; }
    if (lc[1] === 'quit' || lc[1] === 'exit') {
      const w = shellWin();
      if (w) w.win('close'); else toast('serve 模式：直接关闭此标签页即可退出', 'info');
      return;
    }
    toast('多行输入：Shift+Enter 换行，Enter 发送', 'info');
    return;
  }
  // `!` local shell — the user runs it, no gate, no turn; output lands in
  // the transcript via the kernel's local_shell fact
  if (text.startsWith('!')) {
    const cmd = text.slice(1).trim();
    ta.value = ''; autoGrow(); slashCheck(true);
    if (!cmd) return;
    if (!wsSend({ type: 'local_shell', cmd })) return toast('未连接到内核，无法执行', 'alert', 'warn');
    return;
  }
  const payload = expandPastes(text);
  const atts = pendingAtts.filter(a => payload.includes(a.marker));
  if (atts.length) pendingAtts = pendingAtts.filter(a => !atts.includes(a));
  ta.value = ''; autoGrow(); slashCheck(true);
  // a busy session steers — the message rides the running turn's next
  // boundary (queued chips show it) instead of becoming the next turn
  const frame = { type: 'prompt', text: payload };
  if (atts.length) frame.attachments = atts.map(a => ({ path: a.path, mime: a.mime }));
  if (!wsSend(frame)) return toast('未连接到内核，无法发送', 'alert', 'warn');
  if (busy) return; // chip feedback comes from the kernel's steer_queue frame
  append(TX, youHTML(payload, true, clock()));
  // first prompt names the session right away — same rule the host applies
  const m = SESSION_META[sessionId] || (SESSION_META[sessionId] = { mtime: Date.now() });
  if (!m.title) { m.title = text.split('\n').map(s => s.trim()).find(Boolean).slice(0, 80); renderRail(); renderCrumb(); }
  updateHero();
  logEv('message', 'user · ' + text.slice(0, 60));
}
// queued steering — the kernel reports the backlog; chips × cancel one
let steerQ = [];
function renderSteerChips() {
  const box = $('#cmp-queue');
  if (!steerQ.length) { box.hidden = true; box.innerHTML = ''; return; }
  box.hidden = false;
  box.innerHTML = steerQ.map((t, i) =>
    `<span class="chip" data-tip="已入队，回合边界注入"><span>${esc(t.length > 40 ? t.slice(0, 40) + '…' : t)}</span><button class="chip-x" data-si="${i}" aria-label="撤回">×</button></span>`).join('');
}
$('#cmp-queue').addEventListener('click', e => {
  const b = e.target.closest('[data-si]');
  if (b) wsSend({ type: 'steer_cancel', idx: +b.dataset.si });
});
$('#input').addEventListener('input', autoGrow);
$('#input').addEventListener('keydown', e => {
  if (atEl) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); atIdx += e.key === 'ArrowDown' ? 1 : -1; atCheck(); return; }
    if ((e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229)) && atAccept()) { e.preventDefault(); return; }
    if (e.key === 'Escape') { e.preventDefault(); closeAt(); return; }
  }
  if (slashEl) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); slashIdx += e.key === 'ArrowDown' ? 1 : -1; renderSlash(); return; }
    if (e.key === 'Tab') {
      if (slashItems.length) { e.preventDefault(); const ta = $('#input'); ta.value = '/' + slashItems[((slashIdx % slashItems.length) + slashItems.length) % slashItems.length] + ' '; slashCheck(true); ta.focus(); autoGrow(); return; }
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
      if (slashItems.length) { e.preventDefault(); const ta = $('#input'); ta.value = '/' + slashItems[((slashIdx % slashItems.length) + slashItems.length) % slashItems.length]; slashCheck(true); send(); return; }
    }
    if (e.key === 'Escape') { e.preventDefault(); slashCheck(true); return; }
  }
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) { e.preventDefault(); send(); }
});
$('#input').addEventListener('paste', e => {
  // images first — a copied screenshot arrives as a File, not text. Upload
  // it to the session's attachments dir and drop a [图片 name] marker; the
  // marker's presence at send decides whether the block rides the prompt.
  const file = e.clipboardData && [...(e.clipboardData.files || [])].find(f => /^image\//.test(f.type));
  if (file) {
    e.preventDefault();
    const ext = (file.type.split('/')[1] || 'png').replace('jpeg', 'jpg');
    const s = e.target.selectionStart;
    (async () => {
      try {
        const bytes = await file.arrayBuffer();
        const r = await api('/attachments?ext=' + ext + '&sess=' + encodeURIComponent(sessionId), { method: 'POST', body: bytes });
        const marker = `[图片 ${r.name}]`;
        pendingAtts.push({ marker, path: r.path, mime: r.mime });
        const ta = $('#input');
        ta.value = ta.value.slice(0, s) + marker + ta.value.slice(s);
        ta.selectionStart = ta.selectionEnd = s + marker.length;
        autoGrow();
        toast(`图片已附加 → ${marker}`, 'note');
      } catch (err) { toast(`图片上传失败：${err.message || err}`, 'alert', 'warn'); }
    })();
    return;
  }
  const text = e.clipboardData && e.clipboardData.getData('text/plain') || '';
  if (text.length >= PASTE_STASH_LIMIT) {
    e.preventDefault();
    const ta = e.target;
    pasteStash.push(text);
    const marker = `[paste #${pasteStash.length}]`;
    const s = ta.selectionStart;
    ta.value = ta.value.slice(0, s) + marker + ta.value.slice(ta.selectionEnd);
    ta.selectionStart = ta.selectionEnd = s + marker.length;
    toast(`已粘贴 ${text.length} 字符 → ${marker}（发送时展开）`, 'note');
  }
  setTimeout(autoGrow);
});

/* ================= @ file mention ================= */
// same pool shape the TUI builds: repo-relative paths, dirs carry a `/`
// suffix (descent marker); fragment = `@…` immediately before the caret
let atEl = null, atIdx = 0, atItems = [], atSeq = 0;
let pathPool = null, pathPoolSess = '';
async function ensurePathPool() {
  if (pathPool && pathPoolSess === sessionId) return pathPool;
  try { pathPool = (await api('/paths?sess=' + encodeURIComponent(sessionId))).paths || []; }
  catch { pathPool = []; }
  pathPoolSess = sessionId;
  return pathPool;
}
function atFragment() {
  const ta = $('#input'), upto = ta.selectionStart;
  const head = ta.value.slice(0, upto);
  const at = head.lastIndexOf('@');
  if (at < 0 || (at > 0 && !/\s/.test(head[at - 1]))) return null;
  const frag = head.slice(at + 1);
  if (/\s/.test(frag)) return null;
  return { start: at + 1, end: upto, frag };
}
function closeAt() { if (atEl) { atEl.remove(); atEl = null; } }
async function atCheck(hide) {
  const seq = ++atSeq;
  if (hide || view !== 'session') return closeAt();
  const f = atFragment();
  if (!f) return closeAt();
  const pool = await ensurePathPool();
  if (seq !== atSeq || !atFragment()) return; // the caret moved meanwhile
  const q = f.frag.toLowerCase();
  atItems = pool.filter(p => p.toLowerCase().includes(q)).slice(0, 40);
  if (!atItems.length) return closeAt();
  atIdx = Math.min(atIdx, atItems.length - 1);
  if (!atEl) {
    atEl = document.createElement('div');
    atEl.className = 'pop glass up show slash';
    const r = $('#composer').getBoundingClientRect();
    atEl.style.left = Math.max(8, r.left + 6) + 'px';
    atEl.style.width = Math.min(420, r.width - 12) + 'px';
    atEl.style.bottom = (innerHeight - r.top + 8) + 'px';
    document.body.appendChild(atEl);
    atEl.addEventListener('click', e => { const b = e.target.closest('.mi'); if (b) pickAt(b.dataset.p); });
  }
  atEl.innerHTML = '<div class="lbl">@ 文件提及 — Enter 选中，目录可继续下钻</div>'
    + atItems.map((p, i) => `<button class="mi${i === atIdx ? ' hl' : ''}" data-p="${esc(p)}">${ic(p.endsWith('/') ? 'folder' : 'file', 'i sm')}<span class="mt mono"><span>${esc(p)}</span></span></button>`).join('');
}
function pickAt(p) {
  const ta = $('#input'), f = atFragment();
  if (!f) return closeAt();
  const isDir = p.endsWith('/');
  const tail = ta.value.slice(f.end);
  let fill = p;
  // files get a trailing space — except when text already follows
  if (!isDir && (!tail || !/^\s/.test(tail))) fill += ' ';
  ta.value = ta.value.slice(0, f.start) + fill + tail;
  ta.selectionStart = ta.selectionEnd = f.start + fill.length;
  if (isDir) { autoGrow(); atCheck(); } else { closeAt(); autoGrow(); }
  ta.focus();
}
function atAccept() {
  if (!atItems.length) return false;
  pickAt(atItems[((atIdx % atItems.length) + atItems.length) % atItems.length]);
  return true;
}

/* ================= slash menu ================= */
let slashEl = null, slashIdx = 0, slashItems = [];
function slashCheck(hide) {
  const ta = $('#input'), v = ta.value;
  const m = v.match(/^\/(\S*)$/);
  if (hide || !m) { if (slashEl) { slashEl.remove(); slashEl = null; } return; }
  const q = m[1].toLowerCase();
  slashItems = slashList.filter(c => c.toLowerCase().includes(q));
  if (!slashItems.length) { if (slashEl) { slashEl.remove(); slashEl = null; } return; }
  slashIdx = Math.min(slashIdx, slashItems.length - 1);
  if (!slashEl) {
    slashEl = document.createElement('div');
    slashEl.className = 'pop glass up show slash';
    const r = $('#composer').getBoundingClientRect();
    slashEl.style.left = Math.max(8, r.left + 6) + 'px';
    slashEl.style.width = Math.min(360, r.width - 12) + 'px';
    slashEl.style.bottom = (innerHeight - r.top + 8) + 'px';
    document.body.appendChild(slashEl);
    slashEl.addEventListener('click', e => { const b = e.target.closest('.mi'); if (b) pickSlash(b.dataset.c); });
  }
  renderSlash();
}
function renderSlash() {
  if (!slashEl) return;
  slashEl.innerHTML = '<div class="lbl">/ 命令 — Enter 执行，Tab 补全</div>'
    + slashItems.map((c, i) => `<button class="mi${i === slashIdx ? ' hl' : ''}" data-c="${esc(c)}">${ic('terminal', 'i sm')}<span class="mt mono"><span>/${esc(c)}</span></span></button>`).join('');
}
function pickSlash(cmd) {
  const ta = $('#input');
  const needsArg = ['resume', 'fork', 'model', 'annotate', 'mode', 'rewind'].includes(cmd);
  ta.value = '/' + cmd + (needsArg ? ' ' : '');
  slashCheck(true);
  ta.focus(); autoGrow();
}
document.addEventListener('mousedown', e => { if (slashEl && !slashEl.contains(e.target) && e.target !== $('#input')) slashCheck(true); if (atEl && !atEl.contains(e.target) && e.target !== $('#input')) closeAt(); }, true);

