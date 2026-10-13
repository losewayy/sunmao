/* popovers, tooltip, toast, command palette */
'use strict';

/* ================= popovers / tooltip / toast ================= */
let popEl = null, popAnchor = null;
let popCleanup = null;
let popUnmount = null;
const settingsFloats = new Set();
function pop(anchor, html, o = {}) {
  closePop();
  document.querySelectorAll('.pop[data-legacy-pop]').forEach(old => old.remove());
  const p = document.createElement('div');
  p.className = 'pop glass' + (o.cls ? ' ' + o.cls : '');
  p.dataset.legacyPop = '';
  p.innerHTML = html; document.body.appendChild(p);
  popEl = p; popAnchor = anchor; anchor.classList && anchor.classList.add('pressed');
  // Content first: `onMount` fills the dynamic lists (model catalog, job
  // tails), so placement measures the box the popover will really have.
  // Measured as an empty shell it landed on the composer and then grew
  // down over the send button.
  if (o.onMount) { const unmount = o.onMount(p); if (typeof unmount === 'function') popUnmount = unmount; }
  place(p, anchor, o);
  // a dock guest is a native child window: it paints over whatever the page
  // draws in its rect, so a popover that lands on the pane parks it first
  brOverlay(p.getBoundingClientRect());
  requestAnimationFrame(() => p.classList.add('show'));
  return p;
}
/* Popover placement — `place:'top'` (or a cramped bottom) opens upward from
   the anchor itself, so a composer chip's menu grows out of that chip. The
   scrolling body shrinks to the room above the anchor, which is what keeps a
   tall list (the model picker) on screen. */
function place(p, anchor, o) {
  const ui = window.SunmaoUI;
  const body = p.querySelector('.mp-list, .scroll');
  const bodyLimit = body ? parseFloat(getComputedStyle(body).maxHeight) : NaN;
  const update = () => ui.computePosition(anchor, p, {
    strategy: 'fixed', placement: `${o.place === 'top' ? 'top' : 'bottom'}-${o.align === 'end' ? 'end' : 'start'}`,
    middleware: [ui.offset(motion.px('--s-6', 6)), ui.flip({ padding: 8 }), ui.shift({ padding: 8 }), ui.size({ padding: 8, apply({ availableHeight }) {
      const room = Math.max(0, availableHeight);
      if (body) { const chrome = p.offsetHeight - body.offsetHeight; body.style.maxHeight = Math.min(Number.isFinite(bodyLimit) ? bodyLimit : Infinity, Math.max(0, room - chrome)) + 'px'; }
      else { p.style.maxHeight = room + 'px'; p.style.overflowY = 'auto'; }
    } })],
  }).then(({ x, y, placement }) => {
    if (popEl !== p) return;
    p.style.left = x + 'px'; p.style.top = y + 'px'; p.classList.toggle('up', placement.startsWith('top'));
    brOverlay(p.getBoundingClientRect());
  }).catch(e => console.error('popover placement', e));
  popCleanup = ui.autoUpdate(anchor, p, update);
}
function closePop() {
  if (!popEl) return;
  const p = popEl; popEl = null;
  if (popCleanup) popCleanup(); popCleanup = null;
  if (popAnchor && popAnchor.classList) popAnchor.classList.remove('pressed');
  popAnchor = null; p.classList.remove('show');
  const unmount = popUnmount; popUnmount = null;
  setTimeout(() => { if (unmount) unmount(); p.remove(); }, motion.dur('fast'));
  syncOverlay();
}
document.addEventListener('mousedown', e => { if (popEl && !popEl.contains(e.target) && !(popAnchor && popAnchor.contains && popAnchor.contains(e.target))) closePop(); }, true);
function menuHTML(items) {
  const rows = items.map(it => {
    if (it === '-') return '<div class="sep"></div>';
    if (it.label) return `<div class="lbl">${it.label}</div>`;
    const disabled = it.disabled ? ' disabled aria-disabled="true"' : '';
    return `<button class="mi${it.warn ? ' warn' : ''}${it.disabled ? ' disabled' : ''}" data-v="${esc(it.v)}"${disabled}${it.style ? ` style="${it.style}"` : ''}>${it.icon ? ic(it.icon) : ''}<span class="mt${it.mono ? ' mono' : ''}"><span>${esc(it.t)}</span>${it.d ? `<small>${esc(it.d)}</small>` : ''}</span>${it.k ? `<kbd>${esc(it.k)}</kbd>` : ''}${it.on ? ic('check', 'i sm ck') : ''}</button>`;
  }).join('');
  // the scrolling lane every menu gets — without it a tall list (the default
  // model picker, ~50 rows) outgrows the window with nothing to scroll
  return `<div class="mp-list scroll">${rows}</div>`;
}
function menuPop(anchor, items, onPick, o = {}) {
  if (popAnchor === anchor) return closePop();
  pop(anchor, menuHTML(items), Object.assign({}, o, { onMount(p) { p.addEventListener('click', e => { const b = e.target.closest('.mi'); if (!b || b.disabled) return; closePop(); onPick(b.dataset.v); }); } }));
}
const tip = $('#tip'); let tipT = 0, tipFor = null, tipHeld = false, tipCleanup = null, tipDescribed = null;
function hideTip() {
  clearTimeout(tipT); tip.classList.remove('show');
  if (tipCleanup) tipCleanup(); tipCleanup = null;
  if (tipDescribed) {
    const ids = (tipDescribed.getAttribute('aria-describedby') || '').split(/\s+/).filter(id => id && id !== 'tip');
    if (ids.length) tipDescribed.setAttribute('aria-describedby', ids.join(' ')); else tipDescribed.removeAttribute('aria-describedby');
    tipDescribed = null;
  }
}
function trackTip(e) {
  const el = e.target.closest('[data-tip]');
  if (el === tipFor) return;
  hideTip(); tipFor = el;
  if (!el) return;
  tipT = setTimeout(() => {
    // a gesture owns the pointer: a resize drag keeps re-entering the strip,
    // and the bubble that landed mid-drag read as a notification
    if (tipHeld || app.dataset.dragging) return;
    if (!document.body.contains(el) || !el.dataset.tip) return;
    const [t, k] = el.dataset.tip.split('|');
    tip.innerHTML = esc(t) + (k ? `<kbd>${esc(k)}</kbd>` : '');
    const bar = el.closest('.composer'), right = el.dataset.tipSide === 'right', ui = window.SunmaoUI;
    const reference = { contextElement: el, getBoundingClientRect() {
      const r = el.getBoundingClientRect(), rail = right && $('#rail').getBoundingClientRect();
      const left = rail ? rail.left : r.left, width = rail ? rail.width : r.width;
      const top = bar ? bar.getBoundingClientRect().top : r.top;
      return { x: left, y: top, left, right: left + width, top, bottom: r.bottom, width, height: r.bottom - top };
    } };
    tipDescribed = el;
    const ids = new Set((el.getAttribute('aria-describedby') || '').split(/\s+/).filter(Boolean)); ids.add('tip'); el.setAttribute('aria-describedby', [...ids].join(' '));
    const update = () => ui.computePosition(reference, tip, { strategy: 'fixed', placement: right ? 'right' : bar ? 'top' : 'bottom', middleware: [ui.offset(motion.px('--s-8', 8)), ui.flip({ padding: 6 }), ui.shift({ padding: 6 })] }).then(({ x, y }) => {
      if (tipFor !== el || tipDescribed !== el) return;
      tip.style.left = Math.round(x) + 'px'; tip.style.top = Math.round(y) + 'px'; tip.style.transform = ''; tip.classList.add('show');
    }).catch(err => console.error('tooltip placement', err));
    tipCleanup = ui.autoUpdate(reference, tip, update);
  }, e.type === 'focusin' ? 0 : motion.delay('tip'));
}
document.addEventListener('mouseover', trackTip);
document.addEventListener('focusin', trackTip);
document.addEventListener('focusout', e => { if (tipFor && tipFor.contains(e.target) && !tipFor.contains(e.relatedTarget)) { hideTip(); tipFor = null; } });
// A gesture start hides it: `pointerdown` and not just `mousedown` — the
// resize handles call preventDefault() on pointerdown, which suppresses the
// compatibility mouse events, so a mousedown-only listener never ran and the
// hover bubble stayed up for the whole drag.
document.addEventListener('pointerdown', () => { tipHeld = true; hideTip(); }, true);
document.addEventListener('mousedown', () => { tipHeld = true; hideTip(); });
document.addEventListener('mouseup', () => { tipHeld = false; });
document.addEventListener('pointerup', () => { tipHeld = false; });
document.addEventListener('pointercancel', () => { tipHeld = false; });
// a release outside the window never reaches mouseup — heal on the next free move
document.addEventListener('mousemove', e => { if (!e.buttons) tipHeld = false; });
window.addEventListener('blur', () => { tipHeld = false; hideTip(); tipFor = null; });
function toast(msg, icon = 'check', cls = '') {
  const t = document.createElement('div'); t.className = 'toast glass' + (cls ? ' ' + cls : '');
  t.innerHTML = ic(icon) + `<span>${esc(msg)}</span>`; $('#toasts').appendChild(t);
  setTimeout(() => { t.classList.add('out'); setTimeout(() => t.remove(), motion.dur('fast')); }, motion.hold('toast'));
}

/* ================= palette ================= */
function syncOverlay() {
  brOverlay(settingsFloats.size || !$('#palette').hidden || !$('#imv').hidden ? true : popEl ? popEl.getBoundingClientRect() : null);
}
function openPalette() {
  closePop();
  window.SunmaoUI.openPalette({ host: $('#palette'), items: palSource(), label: t('命令面板'), closeLabel: t('关闭'), placeholder: t('输入命令，或搜索会话与设置'), empty: t('没有匹配的命令'), hints: [t('选择'), t('执行'), t('关闭')], onClosed: syncOverlay });
  brOverlay(true);
}
function closePalette() { window.SunmaoUI.closePalette(); }
function palSource() {
  return [
    { g: t('操作'), t: t('新对话'), i: 'pen', k: 'Ctrl N', run: newChat },
    ...(TAURI && TAURI.win ? [{ g: t('操作'), t: t('新窗口'), i: 'monitor', run: () => TAURI.win('new') }] : []),
    { g: t('操作'), t: t('打开设置'), i: 'settings', k: 'Ctrl ,', run: () => go('settings') },
    { g: t('操作'), t: t('定时任务'), i: 'clock', run: () => go('schedules') },
    { g: t('操作'), t: railOn ? t('隐藏会话侧栏') : t('显示会话侧栏'), i: 'panel-l', k: 'Ctrl B', run: toggleRail },
    { g: t('操作'), t: dockOn ? t('隐藏数据面板') : t('显示数据面板'), i: 'panel-r', k: 'Ctrl \\', run: toggleDock },
    { g: t('操作'), t: t('执行记录'), i: 'history', run: () => pop($('[data-act="events"]'), eventsHTML(), { align: 'end', cls: 'events' }) },
    { g: t('操作'), t: t('在对话中查找'), i: 'search', k: 'Ctrl F', run: openFind },
    { g: t('操作'), t: t('导出当前会话'), i: 'download', run: () => exportSession(sessionId) },
    { g: t('操作'), t: t('导出调试包'), i: 'file', run: () => exportZipSession(sessionId) },
    { g: t('操作'), t: t('设置会话目标'), i: 'zap', run: () => goalPop(curGoal ? $('#cmp-goal') : $('[data-act="crumb"]')) },
    { g: t('操作'), t: t('刷新会话列表'), i: 'reset', run: refreshSessions },
    { g: t('操作'), t: S.railGroup === 'project' ? t('会话列表：按时间排列') : t('会话列表：按项目分组'), i: 'blocks', run: () => { S.railGroup = S.railGroup === 'project' ? 'time' : 'project'; save(); renderRail(); } },
    { g: t('外观'), t: t('主题：深色'), i: 'moon', run: () => { S.mode = 'dark'; commit(); } },
    { g: t('外观'), t: t('主题：浅色'), i: 'sun', run: () => { S.mode = 'light'; commit(); } },
    { g: t('外观'), t: t('主题：跟随系统'), i: 'monitor', run: () => { S.mode = 'system'; commit(); } },
    ...WALLS.map(w => ({ g: t('外观'), t: t('壁纸：{name}', { name: w.name }), i: 'image', run: () => { S.wallpaper = w.id; commit(); } })),
    ...slashList.map(c => ({ g: t('命令'), t: '/' + c.name, sub: c.desc || '', i: 'terminal', run: () => { show('session'); $('#input').value = '/' + c.name + ' '; autoGrow(); $('#input').focus(); } })),
    ...SESSION_IDS.map(id => ({ g: t('会话'), t: sessTitle(id) || t('新对话'), d: id, i: 'note', run: () => { show('session'); resumeSession(id); } })),
    // plugin rows trail everything — a plugin gets a real action surface
    // without needing a dock pane
    ...(window.PLUGIN_PAL_ITEMS || []),
  ];
}

