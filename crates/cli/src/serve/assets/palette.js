/* popovers, tooltip, toast, command palette */
'use strict';

/* ================= popovers / tooltip / toast ================= */
let popEl = null, popAnchor = null;
function pop(anchor, html, o = {}) {
  closePop();
  const p = document.createElement('div');
  p.className = 'pop glass' + (o.cls ? ' ' + o.cls : '');
  p.innerHTML = html; document.body.appendChild(p);
  popEl = p; popAnchor = anchor; anchor.classList && anchor.classList.add('pressed');
  // Content first: `onMount` fills the dynamic lists (model catalog, job
  // tails), so placement measures the box the popover will really have.
  // Measured as an empty shell it landed on the composer and then grew
  // down over the send button.
  if (o.onMount) o.onMount(p);
  place(p, anchor, o);
  requestAnimationFrame(() => p.classList.add('show'));
  return p;
}
/* Popover placement — `place:'top'` (or a cramped bottom) opens upward.
   A popover opened from a composer chip is anchored to the whole composer,
   not to the chip: level with the row it would cover the send button. */
function place(p, anchor, o) {
  const gap = 6, r = anchor.getBoundingClientRect();
  const bar = anchor.closest && anchor.closest('.composer');
  const upEdge = bar ? bar.getBoundingClientRect().top : r.top;
  const w = p.offsetWidth, h = () => p.offsetHeight;
  const up = o.place === 'top' || (r.bottom + gap + h() > innerHeight - 8 && upEdge - gap - h() > 8);
  if (up) {
    // too tall for the room above? the scrolling body shrinks — the box
    // never spills back onto the composer
    const room = upEdge - gap - 8, body = p.querySelector('.mp-list, .scroll');
    if (h() > room && body) body.style.maxHeight = Math.max(72, room - (h() - body.offsetHeight)) + 'px';
  }
  const top = up ? upEdge - gap - h() : r.bottom + gap;
  const left = o.align === 'end' ? r.right - w : r.left;
  p.style.left = Math.max(8, Math.min(left, innerWidth - w - 8)) + 'px';
  p.style.top = Math.max(8, Math.min(top, innerHeight - h() - 8)) + 'px';
  if (up) p.classList.add('up');
}
function closePop() {
  if (!popEl) return;
  const p = popEl; popEl = null;
  if (popAnchor && popAnchor.classList) popAnchor.classList.remove('pressed');
  popAnchor = null; p.classList.remove('show'); setTimeout(() => p.remove(), motion.dur('fast'));
}
document.addEventListener('mousedown', e => { if (popEl && !popEl.contains(e.target) && !(popAnchor && popAnchor.contains && popAnchor.contains(e.target))) closePop(); }, true);
function menuHTML(items) {
  return items.map(it => {
    if (it === '-') return '<div class="sep"></div>';
    if (it.label) return `<div class="lbl">${it.label}</div>`;
    return `<button class="mi${it.warn ? ' warn' : ''}" data-v="${esc(it.v)}"${it.style ? ` style="${it.style}"` : ''}>${it.icon ? ic(it.icon) : ''}<span class="mt${it.mono ? ' mono' : ''}"><span>${esc(it.t)}</span>${it.d ? `<small>${esc(it.d)}</small>` : ''}</span>${it.k ? `<kbd>${esc(it.k)}</kbd>` : ''}${it.on ? ic('check', 'i sm ck') : ''}</button>`;
  }).join('');
}
function menuPop(anchor, items, onPick, o = {}) {
  if (popAnchor === anchor) return closePop();
  pop(anchor, menuHTML(items), Object.assign({}, o, { onMount(p) { p.addEventListener('click', e => { const b = e.target.closest('.mi'); if (!b) return; closePop(); onPick(b.dataset.v); }); } }));
}
const tip = $('#tip'); let tipT = 0, tipFor = null;
document.addEventListener('mouseover', e => {
  const el = e.target.closest('[data-tip]');
  if (el === tipFor) return;
  clearTimeout(tipT); tip.classList.remove('show'); tipFor = el;
  if (!el) return;
  tipT = setTimeout(() => {
    if (!document.body.contains(el) || !el.dataset.tip) return;
    const [t, k] = el.dataset.tip.split('|');
    tip.innerHTML = esc(t) + (k ? `<kbd>${esc(k)}</kbd>` : '');
    const r = el.getBoundingClientRect(), tr = tip.getBoundingClientRect();
    let top, left;
    if (el.dataset.tipSide === 'right') {
      // list rows: sit beside the rail, never on top of the next row
      const rail = $('#rail').getBoundingClientRect();
      left = rail.right + 8; top = Math.max(6, Math.min(r.top + r.height / 2 - tr.height / 2, innerHeight - tr.height - 6));
    } else {
      // centered on the anchor, and clear of what it sits on: over a
      // composer chip the bubble lifts above the whole bar — the two edges
      // touching made the capsule read as clipped
      const bar = el.closest('.composer');
      top = r.bottom + 8; if (top + tr.height > innerHeight - 6) top = r.top - tr.height - 8;
      if (bar) top = Math.min(top, bar.getBoundingClientRect().top - 8 - tr.height);
      left = Math.max(6, Math.min(r.left + r.width / 2 - tr.width / 2, innerWidth - tr.width - 6));
    }
    tip.style.transform = `translate(${Math.round(left)}px,${Math.round(top)}px)`; tip.classList.add('show');
  }, motion.delay('tip'));
});
document.addEventListener('mousedown', () => { clearTimeout(tipT); tip.classList.remove('show'); });
function toast(msg, icon = 'check', cls = '') {
  const t = document.createElement('div'); t.className = 'toast glass' + (cls ? ' ' + cls : '');
  t.innerHTML = ic(icon) + `<span>${esc(msg)}</span>`; $('#toasts').appendChild(t);
  setTimeout(() => { t.classList.add('out'); setTimeout(() => t.remove(), motion.dur('fast')); }, motion.hold('toast'));
}

/* ================= palette ================= */
let palItems = [], palIdx = 0;
function openPalette() { closePop(); const p = $('#palette'); p.classList.remove('out'); p.hidden = false; const i = $('#pal-in'); i.value = ''; palIdx = 0; renderPal(); setTimeout(() => i.focus(), 10); }
function closePalette() { const p = $('#palette'); if (p.hidden) return; p.classList.add('out'); setTimeout(() => { if (p.classList.contains('out')) { p.hidden = true; p.classList.remove('out'); } }, motion.dur('fast')); }
function palSource() {
  return [
    { g: '操作', t: '新对话', i: 'pen', k: 'Ctrl N', run: newChat },
    ...(TAURI && TAURI.win ? [{ g: '操作', t: '新窗口', i: 'monitor', run: () => TAURI.win('new') }] : []),
    { g: '操作', t: '打开设置', i: 'settings', k: 'Ctrl ,', run: () => go('settings') },
    { g: '操作', t: '定时任务', i: 'clock', run: () => go('schedules') },
    { g: '操作', t: railOn ? '隐藏会话侧栏' : '显示会话侧栏', i: 'panel-l', k: 'Ctrl B', run: toggleRail },
    { g: '操作', t: dockOn ? '隐藏数据面板' : '显示数据面板', i: 'panel-r', k: 'Ctrl \\', run: toggleDock },
    { g: '操作', t: '执行记录', i: 'history', run: () => pop($('[data-act="events"]'), eventsHTML(), { align: 'end', cls: 'events' }) },
    { g: '操作', t: '在对话中查找', i: 'search', k: 'Ctrl F', run: openFind },
    { g: '操作', t: '导出当前会话', i: 'download', run: () => exportSession(sessionId) },
    { g: '操作', t: '刷新会话列表', i: 'reset', run: refreshSessions },
    { g: '操作', t: S.railGroup === 'project' ? '会话列表：按时间排列' : '会话列表：按项目分组', i: 'blocks', run: () => { S.railGroup = S.railGroup === 'project' ? 'time' : 'project'; save(); renderRail(); } },
    { g: '外观', t: '主题：深色', i: 'moon', run: () => { S.mode = 'dark'; commit(); } },
    { g: '外观', t: '主题：浅色', i: 'sun', run: () => { S.mode = 'light'; commit(); } },
    { g: '外观', t: '主题：跟随系统', i: 'monitor', run: () => { S.mode = 'system'; commit(); } },
    ...WALLS.map(w => ({ g: '外观', t: '壁纸：' + w.name, i: 'image', run: () => { S.wallpaper = w.id; commit(); } })),
    ...slashList.map(c => ({ g: '命令', t: '/' + c.name, sub: c.desc || '', i: 'terminal', run: () => { show('session'); $('#input').value = '/' + c.name + ' '; autoGrow(); $('#input').focus(); } })),
    ...SESSION_IDS.map(id => ({ g: '会话', t: sessTitle(id) || '新对话', d: id, i: 'note', run: () => { show('session'); resumeSession(id); } })),
  ];
}
function renderPal() {
  const q = $('#pal-in').value.trim().toLowerCase();
  palItems = palSource().filter(x => !q || `${x.t} ${x.d || ''} ${x.sub || ''} ${x.g}`.toLowerCase().includes(q));
  palIdx = Math.max(0, Math.min(palIdx, palItems.length - 1));
  let g = '', h = '';
  palItems.forEach((x, i) => { if (x.g !== g) { g = x.g; h += `<div class="pg">${g}</div>`; } h += `<button class="mi${i === palIdx ? ' hl' : ''}" data-pi="${i}">${ic(x.i)}<span class="mt"><span>${esc(x.t)}</span>${x.sub ? `<small>${esc(x.sub)}</small>` : ''}</span>${x.d ? `<kbd>${esc(x.d)}</kbd>` : ''}${x.k ? `<kbd>${esc(x.k)}</kbd>` : ''}</button>`; });
  $('#pal-list').innerHTML = h || '<div class="none">没有匹配的命令</div>';
  const hl = $('#pal-list .mi.hl'), L = $('#pal-list');
  if (hl) { const t = hl.offsetTop, b = t + hl.offsetHeight; if (t < L.scrollTop) L.scrollTop = t - 6; else if (b > L.scrollTop + L.clientHeight) L.scrollTop = b - L.clientHeight + 6; }
}
function runPal(i) { const x = palItems[i]; if (!x) return; closePalette(); x.run(); }
$('#pal-in').addEventListener('input', () => { palIdx = 0; renderPal(); });
$('#pal-in').addEventListener('keydown', e => {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); if (palItems.length) palIdx = (palIdx + (e.key === 'ArrowDown' ? 1 : -1) + palItems.length) % palItems.length; renderPal(); }
  else if (e.key === 'Enter') { e.preventDefault(); runPal(palIdx); }
});
$('#pal-list').addEventListener('click', e => { const b = e.target.closest('[data-pi]'); if (b) runPal(+b.dataset.pi); });
$('#pal-list').addEventListener('mousemove', e => { const b = e.target.closest('[data-pi]'); if (b && +b.dataset.pi !== palIdx) { palIdx = +b.dataset.pi; $$('#pal-list .mi').forEach(x => x.classList.toggle('hl', x === b)); } });

