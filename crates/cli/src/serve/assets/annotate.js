/* Element picker for a dock browser tab's native guest webview — injected by
   `shell_webview {op:'annotate'}`.
   The host page cannot reach this document (separate webview, foreign
   origin), so the whole gesture lives here: hover-highlight, click an element
   or drag a region, type the note, and drop the result on `window.__smAnn`.
   The shell's `ann-poll` op reads it back one-shot and the page hands it to
   the Composer.
   Everything is inline-styled on purpose: this document carries the site's
   own CSS and CSP, and none of ours. */
(function () {
  'use strict';
  const KEY = '__smAnn', ACCENT = '#339CFF', HINT = '点击元素批注 · 拖动选区域 · Esc 取消';
  if (window.__smAnnPick) { window.__smAnnPick.start(); return; }

  let root = null, hi = null, box = null, hover = null, down = null, chosen = null;
  const mk = (tag, css) => { const e = document.createElement(tag); e.style.cssText = css; return e; };
  const px = n => Math.round(n) + 'px';
  const rect = r => ({ x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height) });
  const spot = 'position:fixed;pointer-events:none;z-index:2147483645;border:2px solid ' + ACCENT +
    ';background:rgba(51,156,255,.15);border-radius:3px;box-shadow:0 0 0 1px rgba(0,0,0,.35)';
  const place = (e, r) => { e.style.left = px(r.left); e.style.top = px(r.top); e.style.width = px(r.width); e.style.height = px(r.height); };
  const show = r => { hi.style.display = 'block'; place(hi, r); };
  const clear = () => { hi.style.display = 'none'; };
  /* the overlay itself is the topmost element at every point, so walk the
     hit stack and take the first thing that is not ours */
  const under = (x, y) => { for (const n of document.elementsFromPoint(x, y)) if (!root.contains(n)) return n; return null; };

  function cssPath(node) {
    const parts = [];
    for (let n = node; n && n.nodeType === 1 && n !== document.documentElement; n = n.parentElement) {
      let p = n.tagName.toLowerCase();
      if (n.id) { parts.unshift(p + '#' + CSS.escape(n.id)); break; }
      const cls = [...(n.classList || [])].slice(0, 2).join('.');
      if (cls) p += '.' + cls;
      const sibs = n.parentElement ? [...n.parentElement.children].filter(c => c.tagName === n.tagName) : [];
      if (sibs.length > 1) p += ':nth-of-type(' + (sibs.indexOf(n) + 1) + ')';
      parts.unshift(p);
      if (parts.length >= 4) break;
    }
    return parts.join('>');
  }
  const describe = node => ({
    kind: 'element',
    tag: node.tagName.toLowerCase(),
    id: node.id || '',
    cls: [...(node.classList || [])].slice(0, 3).join(' '),
    text: (node.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 90),
    css: cssPath(node),
    rect: rect(node.getBoundingClientRect()),
  });
  const region = r => ({ kind: 'region', rect: rect(r) });
  function done(payload) { window[KEY] = payload; stop(); }
  function ask() {
    if (box || !chosen) return;
    const anchor = chosen.rect, w = 320;
    box = mk('div', 'position:fixed;z-index:2147483646;width:' + w + 'px;display:flex;gap:6px;align-items:center;' +
      'padding:8px;border-radius:12px;background:#1b1d24;border:1px solid rgba(255,255,255,.18);' +
      'box-shadow:0 10px 34px rgba(0,0,0,.55);font:13px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif;color:#e8e9f0');
    box.style.left = px(Math.max(8, Math.min(anchor.x, innerWidth - w - 8)));
    const below = anchor.y + anchor.h + 8;
    box.style.top = px(below + 46 > innerHeight ? Math.max(8, anchor.y - 52) : below);
    const input = mk('input', 'flex:1;min-width:0;height:30px;padding:0 10px;border:0;outline:0;border-radius:8px;' +
      'background:rgba(255,255,255,.10);color:inherit;font:inherit');
    input.placeholder = '批注这条 ' + (chosen.kind === 'region' ? '区域' : chosen.css || chosen.tag) + '…';
    const ok = mk('button', 'flex:none;height:30px;padding:0 10px;border:0;border-radius:8px;background:' + ACCENT + ';color:#fff;font:inherit;cursor:pointer');
    ok.textContent = '批注';
    const no = mk('button', 'flex:none;height:30px;width:30px;border:0;border-radius:8px;background:rgba(255,255,255,.10);color:inherit;font:inherit;cursor:pointer');
    no.textContent = '×';
    box.append(input, ok, no);
    root.appendChild(box);
    /* the box owns its keyboard: the page underneath must not see the keys */
    for (const ev of ['keydown', 'keyup', 'keypress']) box.addEventListener(ev, e => e.stopPropagation());
    const rearm = () => { if (box) { box.remove(); box = null; } chosen = null; clear(); };
    const send = () => {
      const note = input.value.trim();
      if (!note) return rearm();
      done({ url: location.href, title: document.title, sel: chosen, note: note, at: new Date().toISOString() });
    };
    input.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); send(); } if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); rearm(); } });
    ok.addEventListener('click', send);
    no.addEventListener('click', rearm);
    setTimeout(() => input.focus(), 0);
  }
  function pickAt(x, y) {
    const node = under(x, y);
    if (node) { chosen = describe(node); show(node.getBoundingClientRect()); }
    else { chosen = region({ left: x, top: y, width: 0, height: 0 }); show({ left: x, top: y, width: 0, height: 0 }); }
    ask();
  }
  function onMove(e) {
    if (box) return;
    const node = under(e.clientX, e.clientY);
    hover = node;
    if (node) show(node.getBoundingClientRect()); else clear();
  }
  function onDown(e) {
    if (box) return;
    down = { x: e.clientX, y: e.clientY };
  }
  function onUp(e) {
    if (box || !down) return;
    const start = down; down = null;
    const moved = Math.abs(e.clientX - start.x) > 6 || Math.abs(e.clientY - start.y) > 6;
    if (moved) {
      const r = { left: Math.min(start.x, e.clientX), top: Math.min(start.y, e.clientY), width: Math.abs(start.x - e.clientX), height: Math.abs(start.y - e.clientY) };
      chosen = region(r); show(r); ask();
    } else pickAt(e.clientX, e.clientY);
  }
  function onKey(e) {
    if (e.key !== 'Escape') return;
    e.preventDefault(); e.stopPropagation();
    if (box) { box.remove(); box = null; chosen = null; clear(); return; }
    done({ cancel: true });
  }
  /* the overlay swallows the wheel — forward it so a page can still be
     scrolled to the element that needs annotating */
  function onWheel(e) { e.preventDefault(); scrollBy(e.deltaX, e.deltaY); }
  function stop() {
    if (!root) return;
    root.removeEventListener('pointermove', onMove, true);
    root.removeEventListener('pointerdown', onDown, true);
    root.removeEventListener('pointerup', onUp, true);
    root.removeEventListener('wheel', onWheel, { capture: true });
    document.removeEventListener('keydown', onKey, true);
    root.remove(); root = null; box = null; hi = null; hover = null; down = null; chosen = null;
  }
  function start() {
    if (root) { if (box) { box.remove(); box = null; } chosen = null; clear(); return; }
    root = mk('div', 'position:fixed;inset:0;z-index:2147483647;cursor:crosshair;background:transparent');
    hi = mk('div', spot);
    hi.style.display = 'none';
    const tip = mk('div', 'position:fixed;left:50%;top:10px;transform:translateX(-50%);z-index:2147483646;' +
      'padding:5px 12px;border-radius:999px;background:rgba(20,22,28,.92);color:#e8e9f0;' +
      'font:12px/1.4 system-ui,-apple-system,"Segoe UI",sans-serif;pointer-events:none;white-space:nowrap');
    tip.textContent = HINT;
    root.append(hi, tip);
    root.addEventListener('pointermove', onMove, true);
    root.addEventListener('pointerdown', onDown, true);
    root.addEventListener('pointerup', onUp, true);
    root.addEventListener('wheel', onWheel, { capture: true, passive: false });
    document.addEventListener('keydown', onKey, true);
    document.documentElement.appendChild(root);
  }
  window.__smAnnPick = { start: start, stop: stop };
  start();
})();
