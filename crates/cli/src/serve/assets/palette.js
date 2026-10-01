/* popovers, tooltip, toast, command palette */
'use strict';

/* ================= popovers / tooltip / toast ================= */
let popEl = null, popAnchor = null;
function pop(anchor, html, o = {}) {
  closePop();
  const p = document.createElement('div');
  p.className = 'pop glass' + (o.cls ? ' ' + o.cls : '');
  p.innerHTML = html; document.body.appendChild(p);
  const r = anchor.getBoundingClientRect(), pr = p.getBoundingClientRect(), gap = 6;
  let up = o.place === 'top' || (r.bottom + gap + pr.height > innerHeight - 8 && r.top - gap - pr.height > 8);
  let top = up ? r.top - gap - pr.height : r.bottom + gap;
  let left = o.align === 'end' ? r.right - pr.width : r.left;
  left = Math.max(8, Math.min(left, innerWidth - pr.width - 8)); top = Math.max(8, Math.min(top, innerHeight - pr.height - 8));
  p.style.left = left + 'px'; p.style.top = top + 'px';
  if (up) p.classList.add('up');
  requestAnimationFrame(() => p.classList.add('show'));
  popEl = p; popAnchor = anchor; anchor.classList && anchor.classList.add('pressed');
  if (o.onMount) o.onMount(p);
  return p;
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
    return `<button class="mi${it.warn ? ' warn' : ''}" data-v="${esc(it.v)}">${it.icon ? ic(it.icon) : ''}<span class="mt${it.mono ? ' mono' : ''}"><span>${esc(it.t)}</span>${it.d ? `<small>${esc(it.d)}</small>` : ''}</span>${it.k ? `<kbd>${esc(it.k)}</kbd>` : ''}${it.on ? ic('check', 'i sm ck') : ''}</button>`;
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
      top = r.bottom + 8; if (top + tr.height > innerHeight - 6) top = r.top - tr.height - 8;
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
function openPalette() { closePop(); $('#palette').hidden = false; const i = $('#pal-in'); i.value = ''; palIdx = 0; renderPal(); setTimeout(() => i.focus(), 10); }
function closePalette() { $('#palette').hidden = true; }
function palSource() {
  return [
    { g: '操作', t: '新对话', i: 'pen', k: 'Ctrl N', run: newChat },
    ...(TAURI && TAURI.win ? [{ g: '操作', t: '新窗口', i: 'monitor', run: () => TAURI.win('new') }] : []),
    { g: '操作', t: '打开设置', i: 'settings', k: 'Ctrl ,', run: () => go('settings') },
    { g: '操作', t: dockOn ? '隐藏数据面板' : '显示数据面板', i: 'panel-r', k: 'Ctrl \\', run: toggleDock },
    { g: '操作', t: '事件日志', i: 'history', run: () => pop($('[data-act="events"]'), eventsHTML(), { align: 'end', cls: 'events' }) },
    { g: '操作', t: '在对话中查找', i: 'search', k: 'Ctrl F', run: openFind },
    { g: '操作', t: '导出当前会话', i: 'download', run: () => exportSession(sessionId) },
    { g: '操作', t: '刷新会话列表', i: 'reset', run: refreshSessions },
    { g: '外观', t: '主题：深色', i: 'moon', run: () => { S.mode = 'dark'; commit(); } },
    { g: '外观', t: '主题：浅色', i: 'sun', run: () => { S.mode = 'light'; commit(); } },
    { g: '外观', t: '主题：跟随系统', i: 'monitor', run: () => { S.mode = 'system'; commit(); } },
    ...WALLS.map(w => ({ g: '外观', t: '壁纸：' + w.name, i: 'image', run: () => { S.wallpaper = w.id; commit(); } })),
    ...slashList.map(c => ({ g: '命令', t: '/' + c, i: 'terminal', run: () => { show('session'); $('#input').value = '/' + c + ' '; autoGrow(); $('#input').focus(); } })),
    ...SESSION_IDS.map(id => ({ g: '会话', t: sessTitle(id) || '新对话', d: id, i: 'note', run: () => { show('session'); resumeSession(id); } })),
  ];
}
function renderPal() {
  const q = $('#pal-in').value.trim().toLowerCase();
  palItems = palSource().filter(x => !q || `${x.t} ${x.d || ''} ${x.g}`.toLowerCase().includes(q));
  palIdx = Math.max(0, Math.min(palIdx, palItems.length - 1));
  let g = '', h = '';
  palItems.forEach((x, i) => { if (x.g !== g) { g = x.g; h += `<div class="pg">${g}</div>`; } h += `<button class="mi${i === palIdx ? ' hl' : ''}" data-pi="${i}">${ic(x.i)}<span class="mt"><span>${esc(x.t)}</span></span>${x.d ? `<kbd>${esc(x.d)}</kbd>` : ''}${x.k ? `<kbd>${esc(x.k)}</kbd>` : ''}</button>`; });
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

