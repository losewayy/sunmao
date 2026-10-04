/* composer — input, steer queue, @-mentions, slash menu */
'use strict';

/* ================= composer ================= */
// paste stash — ≥2 KiB pastes ride as [paste #N] markers and expand into
// <pasted-text> blocks at submit, same convention as the TUI
const PASTE_STASH_LIMIT = 2048;
let pasteStash = [];
// image attachments uploaded this draft — each entry {path, mime, name,
// thumb(blob url), file?}; chips alone carry the draft — send() ships all
// pending ones, the × button retracts; no text marker needed
let pendingAtts = [];
function expandPastes(text) {
  let out = text;
  pasteStash.forEach((content, i) => {
    const marker = `[paste #${i + 1}]`;
    // replaceAll — a marker the user duplicated should expand at every
    // site, not just the first; and pruning happens in send() so a stale
    // stash can't re-inject hour-old clipboard text into a new draft
    if (out.includes(marker)) out = out.replaceAll(marker, `\n<pasted-text>\n${content}\n</pasted-text>\n`);
  });
  return out;
}
function autoGrow() { const ta = $('#input'); ta.style.height = motion.px('--h-input', 52) + 'px'; ta.style.height = Math.min(motion.px('--h-input-max', 168), ta.scrollHeight) + 'px'; slashCheck(); atCheck(); }
function send() {
  const ta = $('#input'), text = ta.value.trim();
  if (!text && !pendingAtts.length) return ta.focus();
  if (!text) {
    // attachments-only prompt — nothing to expand; the kernel counts a
    // bare image as a turn (client.rs gate), so we can ship it directly
    const atts = pendingAtts.slice();
    const restore = () => {
      pendingAtts = atts.map(a => a.file ? { ...a, thumb: URL.createObjectURL(a.file) } : a);
      renderAtts(); toast('发送失败，草稿已恢复', 'alert', 'warn');
    };
    if (!wsSend({ type: 'prompt', text: '', attachments: atts.map(a => ({ path: a.path, mime: a.mime })) }, restore)) return toast('未连接到内核，无法发送', 'alert', 'warn');
    for (const a of atts) if (a.thumb) URL.revokeObjectURL(a.thumb);
    pendingAtts = []; renderAtts(); autoGrow();
    return;
  }
  // frontend-local commands — they never reach the kernel (the kernel's
  // slash list filters them; this is the same rule kept on the send path)
  const lc = text.match(/^\/(\S+)(?:\s|$)/);
  if (lc && ['clear', 'quit', 'exit', 'multiline'].includes(lc[1])) {
    ta.value = ''; autoGrow(); slashCheck(true);
    if (lc[1] === 'clear') { clearTranscript(); return; }
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
    if (!cmd) { ta.value = ''; autoGrow(); slashCheck(true); return; }
    // clear AFTER a live send — and pass a restore for the Tauri lane,
    // whose failure only surfaces async via the invoke rejection
    if (!wsSend({ type: 'local_shell', cmd }, () => {
      ta.value = text; autoGrow(); slashCheck(true);
      toast('执行失败，命令已恢复到输入框', 'alert', 'warn');
    })) return toast('未连接到内核，无法执行', 'alert', 'warn');
    ta.value = ''; autoGrow(); slashCheck(true);
    return;
  }
  const payload = expandPastes(text);
  // chips are the carrier — no in-text marker; every pending attachment
  // rides this prompt (the × on a chip is how one gets retracted)
  const atts = pendingAtts.slice();
  // a busy session steers — the message rides the running turn's next
  // boundary (queued chips show it) instead of becoming the next turn;
  // attachments can't steer (text-only), so they stay a queued turn
  const frame = { type: 'prompt', text: payload };
  if (atts.length) frame.attachments = atts.map(a => ({ path: a.path, mime: a.mime }));
  // send first, clear second — a dead socket must leave the draft intact.
  // Snapshot what the clear destroys so the Tauri lane's async failure can
  // rebuild it (thumbs re-create from the retained File, not the revoked
  // object URL).
  const draftAt = { text, atts: pendingAtts.slice(), stash: pasteStash.slice() };
  const restoreDraft = () => {
    pendingAtts = draftAt.atts.map(a => a.file ? { ...a, thumb: URL.createObjectURL(a.file) } : a);
    pasteStash = draftAt.stash;
    renderAtts();
    ta.value = draftAt.text; autoGrow(); slashCheck(true);
    toast('发送失败，草稿已恢复', 'alert', 'warn');
  };
  if (!wsSend(frame, restoreDraft)) return toast('未连接到内核，无法发送', 'alert', 'warn');
  // consume the stash wholesale — the draft shipped; a `[paste #N]` typed
  // into a NEW draft must never resurrect the old clipboard content
  pasteStash = [];
  for (const a of pendingAtts) if (a.thumb) URL.revokeObjectURL(a.thumb);
  pendingAtts = []; // sent or retracted — either way the draft is clean
  renderAtts();
  ta.value = ''; autoGrow(); slashCheck(true);
  // no optimistic bubble — the kernel echoes the accepted prompt back as a
  // `user_message` live event, so the bubble AND its attachment thumbs are
  // drawn from the kernel's busy/idle truth, not this tab's possibly-stale
  // flag (audit-gui #4); a local append here is what double-rendered them.
  // first prompt names the session right away — same rule the host applies
  const m = SESSION_META[sessionId] || (SESSION_META[sessionId] = { mtime: Date.now() });
  if (!m.title) { m.title = text.split('\n').map(s => s.trim()).find(Boolean).slice(0, 80); renderRail(); renderCrumb(); }
  updateHero();
  logEv('message', 'user · ' + text.slice(0, 60));
}
// two queues, one chip row — steer chips (⚡, injected at the running
// turn's next request boundary) lead; queued prompts (FIFO, next turn)
// follow. The kernel is the single source: chips render `input_queue` /
// `steer_queue` broadcasts verbatim, controls only send ops back.
let steerQ = [];
let inputQ = [];
function renderQueueChips() {
  const box = $('#cmp-queue');
  if (!steerQ.length && !inputQ.length) { box.hidden = true; box.innerHTML = ''; return; }
  box.hidden = false;
  const clip = t => esc(t.length > 40 ? t.slice(0, 40) + '…' : t);
  box.innerHTML =
    steerQ.map((t, i) =>
      `<span class="chip q-steer" data-tip="引导已入队 · 下个请求边界注入"><span>${ic('zap', 'i xs')} ${clip(t)}</span><button class="chip-x" data-si="${i}" aria-label="撤回">×</button></span>`).join('') +
    inputQ.map(q =>
      `<span class="chip q-in" data-tip="排队中 · 点击编辑"><button class="chip-btn" data-mv="${q.id},-1" aria-label="前移">${ic('chev-l', 'i xs')}</button><button class="chip-btn" data-mv="${q.id},1" aria-label="后移">${ic('chev-r', 'i xs')}</button><button class="chip-t" data-qedit="${q.id}">${clip(q.text)}</button><button class="chip-x" data-qx="${q.id}" aria-label="移除">×</button></span>`).join('');
}
$('#cmp-queue').addEventListener('click', e => {
  const b = e.target.closest('[data-si],[data-mv],[data-qx],[data-qedit]');
  if (!b) return;
  if (b.dataset.si !== undefined) return wsSend({ type: 'steer_cancel', idx: +b.dataset.si });
  if (b.dataset.qx !== undefined) return wsSend({ type: 'input_remove', id: +b.dataset.qx });
  if (b.dataset.mv !== undefined) {
    const [id, dir] = b.dataset.mv.split(',');
    return wsSend({ type: 'input_move', id: +id, dir: +dir });
  }
  // inline edit — swap the label for an input seeded with the FULL text
  // (the chip shows a 40-char clip); Enter/blur commits, Esc discards
  const q = inputQ.find(x => x.id === +b.dataset.qedit);
  if (!q) return;
  const chip = b.closest('.chip');
  const inp = document.createElement('input');
  inp.value = q.text; inp.spellcheck = false;
  b.replaceWith(inp);
  inp.focus(); inp.setSelectionRange(inp.value.length, inp.value.length);
  let done = false;
  const commit = save => {
    if (done) return; done = true;
    if (save && inp.value.trim() && inp.value !== q.text)
      wsSend({ type: 'input_edit', id: q.id, text: inp.value.trim() });
    else renderQueueChips();
  };
  inp.addEventListener('keydown', ev => {
    ev.stopPropagation();
    if (ev.key === 'Enter') { ev.preventDefault(); commit(true); }
    if (ev.key === 'Escape') { ev.preventDefault(); commit(false); }
  });
  inp.addEventListener('blur', () => commit(true));
});
// Ctrl+Enter — explicit steer: the text rides the running turn's next
// request boundary instead of queueing behind it. With a non-empty queue
// it's ALSO the expedite: the driver drains steer before the FIFO, so a
// Ctrl+Enter lands ahead of every queued prompt. Text-only — a draft
// carrying attachments can't steer; falls back to the queue with a note.
function steerSend() {
  const ta = $('#input'), text = ta.value.trim();
  if (!text) return ta.focus();
  if (pendingAtts.length) {
    toast('引导只带文本 — 附件消息走 Enter 排队', 'alert', 'warn');
    return send();
  }
  const payload = expandPastes(text);
  const restore = () => { ta.value = text; autoGrow(); slashCheck(true); toast('发送失败，草稿已恢复', 'alert', 'warn'); };
  if (!wsSend({ type: 'steer', text: payload }, restore)) return toast('未连接到内核，无法发送', 'alert', 'warn');
  pasteStash = [];
  ta.value = ''; autoGrow(); slashCheck(true);
  logEv('message', 'steer · ' + payload.slice(0, 60));
}
// attachment thumbs above the input — chips ARE the carrier; the wire
// payload ships {path,mime} per chip, the text stays free of markers
function renderAtts() {
  const box = $('#cmp-atts');
  box.hidden = !pendingAtts.length;
  box.innerHTML = pendingAtts.map((a, i) =>
    `<span class="att-chip"><img src="${a.thumb || attURL(a.path)}" alt=""><span class="att-n">${esc(a.name || attBase(a.path))}</span><button class="chip-x" data-ri="${i}" aria-label="移除附件">×</button></span>`).join('');
}
function removeAtt(i) {
  const a = pendingAtts[i];
  if (!a) return;
  pendingAtts.splice(i, 1);
  if (a.thumb) URL.revokeObjectURL(a.thumb);
  renderAtts(); autoGrow();
}
$('#cmp-atts').addEventListener('click', e => {
  const b = e.target.closest('[data-ri]');
  if (b) removeAtt(+b.dataset.ri);
});
$('#input').addEventListener('input', autoGrow);
$('#input').addEventListener('keydown', e => {
  // Ctrl+Enter takes priority over every menu branch — with a slash/at
  // menu open the modifier still means "steer the raw text", not
  // "accept the highlighted completion"
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !e.isComposing && e.keyCode !== 229) { e.preventDefault(); steerSend(); return; }
  if (atEl) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); atIdx += e.key === 'ArrowDown' ? 1 : -1; atCheck(); return; }
    if ((e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229)) && atAccept()) { e.preventDefault(); return; }
    if (e.key === 'Escape') { e.preventDefault(); closeAt(); return; }
  }
  if (slashEl) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); slashIdx += e.key === 'ArrowDown' ? 1 : -1; renderSlash(); return; }
    if (e.key === 'Tab') {
      if (slashItems.length) { e.preventDefault(); const ta = $('#input'); ta.value = '/' + slashItems[((slashIdx % slashItems.length) + slashItems.length) % slashItems.length].name + ' '; slashCheck(true); ta.focus(); autoGrow(); return; }
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
      if (slashItems.length) { e.preventDefault(); const ta = $('#input'); ta.value = '/' + slashItems[((slashIdx % slashItems.length) + slashItems.length) % slashItems.length].name; slashCheck(true); send(); return; }
    }
    if (e.key === 'Escape') { e.preventDefault(); slashCheck(true); return; }
  }
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) { e.preventDefault(); send(); }
});
// one image upload path — the paste handler and the `+` picker both land
// here. The chip alone carries the draft (no [图片] text marker — the wire
// payload is the {path,mime} list); blob url is the thumb, the retained
// File re-mints it if a send-failure restore needs one.
async function uploadImageFile(file) {
  const ext = (file.type.split('/')[1] || 'png').replace('jpeg', 'jpg');
  const bytes = await file.arrayBuffer();
  const r = await api('/attachments?ext=' + ext + '&sess=' + encodeURIComponent(sessionId), { method: 'POST', body: bytes });
  pendingAtts.push({ path: r.path, mime: r.mime, name: r.name, thumb: URL.createObjectURL(file), file });
  renderAtts();
  toast(`图片已附加 → ${r.name}`, 'note');
}
$('#att-file').addEventListener('change', e => {
  for (const f of [...e.target.files]) {
    if (!/^image\//.test(f.type)) { toast(`只收图片：${f.name}`, 'alert', 'warn'); continue; }
    uploadImageFile(f).catch(err => toast(`图片上传失败：${err.message || err}`, 'alert', 'warn'));
  }
  e.target.value = ''; // same file twice in a row must re-fire change
});
$('#input').addEventListener('paste', e => {
  // images first — a copied screenshot arrives as a File, not text
  const file = e.clipboardData && [...(e.clipboardData.files || [])].find(f => /^image\//.test(f.type));
  if (file) {
    e.preventDefault();
    uploadImageFile(file).catch(err => toast(`图片上传失败：${err.message || err}`, 'alert', 'warn'));
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
// completion popups ride the same .show transition as pop() — create
// without it, rAF-add (enter), and exit through the ease-in fade before
// remove() so open/close mirror the rest of the popover family
function hideCompletion(el) { if (!el) return null; el.classList.remove('show'); setTimeout(() => el.remove(), motion.dur('fast')); return null; }
function popShow(el) { requestAnimationFrame(() => el.classList.add('show')); return el; }
function closeAt() { atEl = hideCompletion(atEl); }
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
    atEl.className = 'pop glass up slash';
    const r = $('#composer').getBoundingClientRect();
    atEl.style.left = Math.max(8, r.left + 6) + 'px';
    atEl.style.width = Math.min(motion.px('--w-at-pop', 420), r.width - 12) + 'px';
    atEl.style.bottom = (innerHeight - r.top + 8) + 'px';
    document.body.appendChild(atEl);
    atEl.addEventListener('click', e => { const b = e.target.closest('.mi'); if (b) pickAt(b.dataset.p); });
    popShow(atEl);
  }
  atEl.innerHTML = '<div class="lbl">@ 文件提及 — Enter 选中，目录可继续下钻</div>'
    + atItems.map((p, i) => `<button class="mi${i === atIdx ? ' hl' : ''}" data-p="${esc(p)}">${ic(p.endsWith('/') ? 'folder' : 'file', 'i sm')}<span class="mt mono"><span>${esc(p)}</span></span></button>`).join('');
  atEl.querySelector('.hl')?.scrollIntoView({ block: 'nearest' });
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
  if (hide || !m) { slashEl = hideCompletion(slashEl); return; }
  const q = m[1].toLowerCase();
  slashItems = slashList.filter(c => c.name.toLowerCase().includes(q));
  if (!slashItems.length) { slashEl = hideCompletion(slashEl); return; }
  slashIdx = Math.min(slashIdx, slashItems.length - 1);
  if (!slashEl) {
    slashEl = document.createElement('div');
    slashEl.className = 'pop glass up slash';
    const r = $('#composer').getBoundingClientRect();
    slashEl.style.left = Math.max(8, r.left + 6) + 'px';
    slashEl.style.width = Math.min(motion.px('--w-slash-pop', 360), r.width - 12) + 'px';
    slashEl.style.bottom = (innerHeight - r.top + 8) + 'px';
    document.body.appendChild(slashEl);
    slashEl.addEventListener('click', e => { const b = e.target.closest('.mi'); if (b) pickSlash(b.dataset.c); });
    popShow(slashEl);
  }
  renderSlash();
}
function renderSlash() {
  if (!slashEl) return;
  slashEl.innerHTML = '<div class="lbl">/ 命令 — Enter 执行，Tab 补全</div>'
    + slashItems.map((c, i) => `<button class="mi${i === slashIdx ? ' hl' : ''}" data-c="${esc(c.name)}">${ic(c.kind === 'skill' ? 'sparkles' : 'terminal', 'i sm')}<span class="mt mono"><span>/${esc(c.name)}</span>${c.desc ? `<small>${esc(c.desc)}</small>` : ''}</span></button>`).join('');
  // keyboard nav must drag the viewport — the highlight re-renders on
  // every arrow, so scroll the fresh .hl into the scroller's nearest edge
  slashEl.querySelector('.hl')?.scrollIntoView({ block: 'nearest' });
}
function pickSlash(cmd) {
  const ta = $('#input');
  const needsArg = ['resume', 'fork', 'model', 'annotate', 'mode', 'rewind'].includes(cmd);
  ta.value = '/' + cmd + (needsArg ? ' ' : '');
  slashCheck(true);
  ta.focus(); autoGrow();
}
document.addEventListener('mousedown', e => { if (slashEl && !slashEl.contains(e.target) && e.target !== $('#input')) slashCheck(true); if (atEl && !atEl.contains(e.target) && e.target !== $('#input')) closeAt(); }, true);

/* the composer zone isn't fixed — queue/attachment/top rows grow the card
   upward. Track its real top edge so hero and toasts stay above it at any
   height instead of clearing a static guess. */
new ResizeObserver(() => {
  const h = $('#composer').getBoundingClientRect().height;
  $('.stage').style.setProperty('--off-hero-bottom', (h + 16) + 'px');
}).observe($('#composer'));

