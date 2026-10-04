/* MCP Apps host bridge (SEP-1865 double-iframe), island revs/notes */
'use strict';

/* ============ MCP Apps host (SEP-1865 / GUI.md §5) ============
 * An artifact with a `{name}.ui.json` sidecar is an MCP App island — it
 * renders through the spec's double-iframe sandbox: outer sandbox.html on
 * a DIFFERENT origin (its own listener port), inner View iframe opaque.
 * This page is the Host end of the ui/* JSON-RPC bridge; server traffic
 * (tools/call, resources/read) proxies over the ws into the kernel's gate.
 */
let sandboxPort = 0; // from hello — 0 means "no sandbox listener" (older kernel)
const pendingUi = new Map(); // `${artifact}#${id}` → island state
const islandApps = new WeakMap(); // island el → { meta, html, inited }

/* Under the shell there is no sandbox listener — the proxy page rides its
   own scheme (`http://sunmao-sandbox.localhost/`), still a distinct origin.
   hello.sandbox_port stays the browser path's hint (0 under the shell). */
const SANDBOX_URL = TAURI ? 'http://sunmao-sandbox.localhost/sandbox.html' : null;
async function probeApp(el, name) {
  if (!TAURI && !sandboxPort) return;
  // artifact fetches need the tab's session — the server falls back to the
  // FIRST live session without `sess`, so a non-first project's islands
  // would read the wrong dir (audit-gui #9)
  const sq = `?sess=${encodeURIComponent(sessionId)}`;
  let meta;
  try {
    const r = await fetch(`/artifacts/${encodeURIComponent(name)}/ui${sq}`);
    if (!r.ok) return;
    meta = await r.json();
  } catch { return; }
  let html = '';
  try { html = await (await fetch(`/artifacts/${encodeURIComponent(name)}${sq}`)).text(); } catch {}
  const sb = document.createElement('iframe');
  sb.setAttribute('sandbox', 'allow-scripts allow-same-origin');
  sb.src = SANDBOX_URL || `http://127.0.0.1:${sandboxPort}/sandbox.html`;
  sb.title = name;
  const old = $('iframe', el);
  if (old) { teardownIsland(el); old.replaceWith(sb); } else el.appendChild(sb);
  islandApps.set(el, { name, meta, html, inited: false, frame: sb });
}

function postToIsland(el, msg) {
  const sb = $('iframe', el);
  if (sb && sb.contentWindow) sb.contentWindow.postMessage(msg, '*');
}

let appUiSeq = 0;
// SEP-1865 teardown: before an app island's iframe leaves the DOM, ask the
// View to release its side (it may hold server-side resource state); the
// reply or a short timeout lets the swap proceed — never block render.
function teardownIsland(el) {
  // release in-flight RPC bookkeeping first — a torn-down island must not
  // pin its DOM subtree in pendingUi for the session's lifetime
  // (audit-gui #8): only ui_result deletes normally, and the reply may
  // never come once the frame is gone
  for (const [k, v] of pendingUi) if (v === el) pendingUi.delete(k);
  const it = islandApps.get(el);
  if (!it || !it.inited) return;
  const sb = $('iframe', el);
  if (!sb) return;
  const id = `teardown-${++appUiSeq}`;
  postToIsland(el, { jsonrpc: '2.0', id, method: 'ui/resource-teardown', params: {} });
}

function uiWs(el, id, msg) {
  const it = islandApps.get(el);
  const key = `${it.name}#${id}`;
  pendingUi.set(key, el);
  // a ui_result that never lands mustn't pin the island DOM forever —
  // 30s is generous for a local kernel round-trip (audit-gui #8)
  setTimeout(() => pendingUi.delete(key), 30000);
  wsSend(Object.assign({ id, name: it.name }, msg));
}

function hostContextFor(it) {
  const dark = document.documentElement.dataset.theme !== 'light';
  // SEP-1865 theming bridge: the View reads host styles from
  // hostContext.styles.variables — resolve our theme tokens live so light/
  // dark flips always hand the island the CURRENT palette, not a snapshot.
  const cs = getComputedStyle(document.documentElement);
  const css = n => cs.getPropertyValue(n).trim();
  return {
    toolInfo: { tool: { name: it.meta.tool } },
    theme: dark ? 'dark' : 'light',
    displayMode: 'inline',
    availableDisplayModes: ['inline', 'fullscreen'],
    platform: 'desktop',
    deviceCapabilities: {
      touch: matchMedia('(pointer:coarse)').matches,
      hover: matchMedia('(hover:hover)').matches,
    },
    locale: navigator.language || 'en',
    userAgent: 'sunmao-gui',
    styles: {
      variables: {
        '--color-background-primary': css('--island-bg') || `rgb(${css('--island-rgb') || '13 14 19'})`,
        '--color-background-secondary': css('--c-fill-2'),
        '--color-background-inverse': css('--c-text'),
        '--color-text-primary': css('--c-text'),
        '--color-text-secondary': css('--c-text-2'),
        '--color-text-ghost': css('--c-text-3'),
        '--color-text-info': css('--c-tool'),
        '--color-text-success': css('--c-ok'),
        '--color-text-warning': css('--c-warn'),
        '--color-text-danger': css('--c-err'),
        '--color-border-primary': css('--c-stroke'),
        '--color-border-secondary': css('--c-stroke-2'),
        '--color-ring-primary': css('--c-accent'),
        '--font-sans': css('--font-ui'),
        '--font-mono': css('--font-mono'),
      },
    },
  };
}

function handleAppMsg(el, it, d) {
  const { method, id, params } = d;
  switch (method) {
    case 'ui/notifications/sandbox-proxy-ready':
      postToIsland(el, { jsonrpc: '2.0', method: 'ui/notifications/sandbox-resource-ready',
        params: { html: it.html, csp: it.meta.csp || null } });
      break;
    case 'ui/initialize':
      postToIsland(el, { jsonrpc: '2.0', id, result: {
        protocolVersion: '2026-01-26',
        hostCapabilities: { openLinks: {}, serverTools: { listChanged: false }, serverResources: {}, logging: {} },
        hostInfo: { name: 'sunmao', version: '0.2.0' },
        hostContext: hostContextFor(it),
      }});
      break;
    case 'ui/notifications/initialized':
      it.inited = true;
      postToIsland(el, { jsonrpc: '2.0', method: 'ui/notifications/tool-input',
        params: { arguments: it.meta.arguments || {} } });
      postToIsland(el, { jsonrpc: '2.0', method: 'ui/notifications/tool-result',
        params: it.meta.result || {} });
      break;
    case 'tools/call':
      uiWs(el, id, { type: 'ui_call', name: `mcp__${it.meta.server}__${params?.name}`, args: params?.arguments || {} });
      break;
    case 'resources/read':
      uiWs(el, id, { type: 'ui_read', server: it.meta.server, uri: params?.uri });
      break;
    case 'ui/message':
      uiWs(el, id, { type: 'ui_message', text: params?.content?.text || '' });
      break;
    case 'ui/open-link': {
      const url = params?.url || '';
      if (/^https?:/i.test(url)) {
        window.open(url, '_blank', 'noopener');
        wsSend({ type: 'ui_audit', event: 'mcp.ui_open_link', detail: `${it.name}: ${url}` });
        postToIsland(el, { jsonrpc: '2.0', id, result: {} });
      } else {
        postToIsland(el, { jsonrpc: '2.0', id, error: { code: -32000, message: 'Invalid URL' } });
      }
      break;
    }
    case 'ui/request-display-mode': {
      const on = params?.mode === 'fullscreen';
      el.classList.toggle('tall', on);
      postToIsland(el, { jsonrpc: '2.0', id, result: { mode: on ? 'fullscreen' : 'inline' } });
      postToIsland(el, { jsonrpc: '2.0', method: 'ui/notifications/host-context-changed',
        params: { displayMode: on ? 'fullscreen' : 'inline' } });
      break;
    }
    case 'ui/notifications/size-changed': {
      const h = params?.height;
      const sb = $('iframe', el);
      if (h && sb) sb.style.height = Math.min(Math.max(Math.round(h), motion.px('--h-island-min', 80)), motion.px('--h-island-max', 1400)) + 'px';
      break;
    }
    case 'notifications/message': case 'ui/notifications/message':
      wsSend({ type: 'ui_audit', event: 'mcp.ui_log',
        detail: `${it.name}: ${params?.data?.message || params?.message || 'log'}` });
      break;
    case 'ui/update-model-context':
      uiWs(el, id, { type: 'ui_audit', event: 'mcp.ui_context',
        detail: `${it.name}: ${JSON.stringify(params || {}).slice(0, 400)}` });
      break;
    case 'ping':
      postToIsland(el, { jsonrpc: '2.0', id, result: {} });
      break;
    default:
      if (id != null) {
        postToIsland(el, { jsonrpc: '2.0', id, error: { code: -32601, message: `unsupported: ${method}` } });
      }
  }
}

// island → host: messages come from the sandbox iframe's contentWindow.
// The sandbox is `sandbox=""` — opaque origin — so `postMessage('*')` on
// our side is forced (there's no origin to target), and on receipt the
// origin string is literally 'null'. Validate BOTH: source has to be a
// live island frame AND origin has to be the opaque 'null' — anything else
// means the frame dropped sandbox or something else is talking.
window.addEventListener('message', ev => {
  const d = ev.data;
  if (!d || d.jsonrpc !== '2.0') return;
  if (ev.origin !== 'null') return;
  for (const isl of $$('.island')) {
    const it = islandApps.get(isl);
    const sb = isl && $('iframe', isl);
    if (it && sb && ev.source === sb.contentWindow) {
      handleAppMsg(isl, it, d);
      return;
    }
  }
});

async function refreshRevs(el, name, known) {
  let max = known | 0;
  try {
    const r = await fetch('/artifacts/' + encodeURIComponent(name) + '/revs?sess=' + encodeURIComponent(sessionId));
    const v = await r.json();
    if (v && v.rev) max = v.rev;
  } catch {}
  el.dataset.revMax = max;
  el.dataset.revCur = Math.min(Math.max(known | 0, 1), Math.max(max, 1)) || 1;
  if (!known) el.dataset.revCur = max || 1;
  const revs = $('.revs', el);
  revs.hidden = max < 2;
  setIslandRev(el);
}
function setIslandRev(isl) {
  if (islandApps.has(isl)) return; // app islands render via the sandbox bridge, not ?rev=
  const name = isl.dataset.artifact;
  const cur = +isl.dataset.revCur || 1, max = +isl.dataset.revMax || 1;
  $('.rev-n', isl).textContent = `v${cur}/${max}`;
  // ?sess too — islands from a non-first session must not read the first
  // live session's artifact dir (audit-gui #9)
  const sq = '?sess=' + encodeURIComponent(sessionId);
  $('iframe', isl).src = cur === max
    ? `/artifacts/${encodeURIComponent(name)}${sq}`
    : `/artifacts/${encodeURIComponent(name)}?rev=${cur}&sess=${encodeURIComponent(sessionId)}`;
}
async function refreshNotes(el, name) {
  let anns = [];
  try {
    const r = await fetch('/artifacts/' + encodeURIComponent(name) + '/notes?sess=' + encodeURIComponent(sessionId));
    const v = await r.json();
    anns = v && Array.isArray(v.annotations) ? v.annotations : [];
  } catch {}
  const pill = $('.notes-pill', el), list = $('.notes', el);
  $('span:last-child', pill).textContent = anns.length ? `批注 +${anns.length}` : '批注';
  el._anns = anns;
  list.innerHTML = anns.map(a => `<div class="note-r"><span>${a.sel ? `<b class="ns">${esc(selLabel(a.sel))}</b> ` : ''}${esc(a.note || '')}</span><time>${esc(a.at || '')}</time></div>`).join('')
    + `<div class="note-add"><input data-name="${esc(name)}" placeholder="添加批注，写入 ${esc(name)}.state.json" aria-label="批注"><button class="btn ghost sm" data-act="note-add" data-name="${esc(name)}">添加</button></div>`;
  renderAnnPins();
}

/* ============ 元素批注（Codex 式点选） ============
 * artifact iframe 挂 sandbox="allow-same-origin"——脚本依旧双杀
 *（sandbox 不给 allow-scripts + 响应 CSP script-src 'none'），但同源
 * 让宿主能 elementFromPoint：悬停高亮、点击取元素、拖拽取区域。批注
 * 落两处——state.json 的 sel（agent 下次 Read 见到结构化锚点）+
 * Composer（元素标签进输入框当上下文）。MCP Apps island 的跨源 frame
 * 拿不到 DOM，自动退化为区域/点批注。 */
let AN = null; // { isl, fr, veil, hl, box, hover, drag, ro }

function selLabel(sel) {
  if (!sel || sel.kind === 'region') return '区域';
  if (sel.kind === 'point') return '位置';
  return `<${sel.tag}${sel.id ? '#' + sel.id : ''}${sel.cls ? '.' + sel.cls.split(' ').filter(Boolean).join('.') : ''}>`;
}
function cssPath(el) {
  const parts = [];
  for (let n = el; n && n !== n.ownerDocument.documentElement; n = n.parentElement) {
    let p = n.tagName.toLowerCase();
    if (n.id) { parts.unshift(p + '#' + CSS.escape(n.id)); break; }
    const cls = [...(n.classList || [])].slice(0, 2).join('.');
    if (cls) p += '.' + cls;
    const sib = n.parentElement ? [...n.parentElement.children].filter(c => c.tagName === n.tagName) : [];
    if (sib.length > 1) p += `:nth-of-type(${sib.indexOf(n) + 1})`;
    parts.unshift(p);
    if (parts.length >= 4) break;
  }
  return parts.join('>');
}
const anPoint = e => { const r = AN.fr.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top }; };
const anPct = r => { const w = AN.veil.clientWidth || 1, h = AN.veil.clientHeight || 1; return [r.left / w * 100, r.top / h * 100, r.width / w * 100, r.height / h * 100].map(v => +v.toFixed(1)); };
function anPlace(el, r) { Object.assign(el.style, { left: r.left + 'px', top: r.top + 'px', width: r.width + 'px', height: r.height + 'px' }); }
const anRect = (a, b) => ({ left: Math.min(a.x, b.x), top: Math.min(a.y, b.y), width: Math.abs(a.x - b.x), height: Math.abs(a.y - b.y) });

function annToggle(isl) {
  if (!isl) return;
  if (AN && AN.isl === isl) return annOff();
  if (AN) annOff();
  const fr = $('iframe', isl);
  if (!fr) return;
  isl.classList.add('annotating', 'show-notes');
  const veil = document.createElement('div');
  veil.className = 'an-veil';
  veil.innerHTML = '<div class="an-hint">点击元素批注 · 拖动选取区域 · Esc 退出</div><div class="an-hl" hidden></div>';
  veil.addEventListener('pointerdown', annDown);
  veil.addEventListener('pointermove', annMove);
  veil.addEventListener('pointerup', annUp);
  isl.appendChild(veil);
  AN = { isl, fr, veil, hl: veil.querySelector('.an-hl'), box: null, hover: null, drag: null, ro: null };
  AN.ro = new ResizeObserver(annSync);
  AN.ro.observe(fr);
  annSync();
}
function annOff() {
  if (!AN) return;
  if (AN.ro) AN.ro.disconnect();
  AN.isl.classList.remove('annotating', 'show-notes');
  AN.veil.remove();
  AN = null;
}
function annSync() {
  if (!AN) return;
  const ir = AN.isl.getBoundingClientRect(), r = AN.fr.getBoundingClientRect();
  Object.assign(AN.veil.style, { left: r.left - ir.left + 'px', top: r.top - ir.top + 'px', width: r.width + 'px', height: r.height + 'px' });
  renderAnnPins();
}
function annMove(e) {
  if (!AN) return;
  if (AN.drag) {
    const r = anRect(AN.drag, anPoint(e));
    AN.drag.moved = AN.drag.moved || r.width > 6 || r.height > 6;
    AN.hl.classList.add('region');
    anPlace(AN.hl, r); AN.hl.hidden = false;
    return;
  }
  const doc = AN.fr.contentDocument; // null → 跨源 app island：无元素拾取
  if (!doc) { AN.hl.hidden = true; AN.hover = null; return; }
  const p = anPoint(e);
  const el = doc.elementFromPoint(p.x, p.y);
  AN.hover = el;
  if (!el) { AN.hl.hidden = true; return; }
  AN.hl.classList.remove('region');
  anPlace(AN.hl, el.getBoundingClientRect()); // veil 与 iframe 视口同坐标系
  AN.hl.hidden = false;
}
function annDown(e) {
  if (!AN) return;
  if (AN.box) { const b = AN.box; AN.box = null; AN.hl.hidden = true; b.classList.add('out'); setTimeout(() => b.remove(), motion.dur('fast')); return; }
  try { AN.veil.setPointerCapture(e.pointerId); } catch {} // pointer may die mid-gesture
  AN.drag = anPoint(e); AN.drag.moved = false;
}
function annUp(e) {
  if (!AN || !AN.drag) return;
  const start = AN.drag; AN.drag = null;
  const p = anPoint(e);
  let sel, anchor;
  if (start.moved) {
    anchor = anRect(start, p);
    sel = { kind: 'region', rect: anPct(anchor) };
  } else if (AN.hover) {
    const r = AN.hover.getBoundingClientRect();
    anchor = { left: r.left, top: r.top, width: r.width, height: r.height };
    const el = AN.hover;
    sel = { tag: el.tagName.toLowerCase(), id: el.id || '', cls: [...el.classList].slice(0, 3).join(' '), text: (el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 60), css: cssPath(el), rect: anPct(r) };
  } else {
    anchor = { left: p.x, top: p.y, width: 0, height: 0 };
    sel = { kind: 'point', rect: anPct(anchor) };
  }
  annBox(sel, anchor);
}
function annBox(sel, anchor) {
  const box = document.createElement('div');
  box.className = 'an-box';
  box.innerHTML = `<input placeholder="添加批注…" aria-label="批注"><button class="btn allow sm" aria-label="添加">${ic('check')}</button><button class="ib sm" aria-label="取消">${ic('x')}</button>`;
  const vw = AN.veil.clientWidth, vh = AN.veil.clientHeight, bw = motion.px('--w-an-box', 300);
  box.style.left = Math.max(0, Math.min(anchor.left, vw - bw)) + 'px';
  const below = anchor.top + anchor.height + 8;
  box.style.top = (below + 44 > vh ? Math.max(0, anchor.top - 52) : below) + 'px';
  AN.veil.appendChild(box); AN.box = box;
  const input = $('input', box);
  input.focus();
  const close = () => { box.classList.add('out'); AN.box = null; AN.hl.hidden = true; setTimeout(() => box.remove(), motion.dur('fast')); };
  box.addEventListener('pointerdown', e => e.stopPropagation());
  input.addEventListener('keydown', e => {
    e.stopPropagation();
    if (e.key === 'Enter') submit();
    if (e.key === 'Escape') close();
  });
  $('button.ib', box).addEventListener('click', close);
  $('button.btn', box).addEventListener('click', submit);
  async function submit() {
    const note = input.value.trim();
    if (!note) return close();
    const name = AN.isl.dataset.artifact;
    const ta = $('#input');
    // 元素标签进 Composer —— artifact 另落 state.json 的结构化 sel；
    // 浏览器标签页没有 state.json，锚点直接随提示词走
    const label = name ? `${name}.html` : (AN.isl.dataset.annLabel || '页面');
    if (name) {
      try {
        await api(`/artifacts/${encodeURIComponent(name)}/annotate?sess=${encodeURIComponent(sessionId)}`,
          { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ note, sel }) });
        ta.value = (ta.value.trim() ? ta.value.trimEnd() + '\n' : '') + `批注 ${label} ${selLabel(sel)}：${note}`;
        autoGrow(); ta.focus();
        refreshNotes(AN.isl, name);
        close();
        toast('批注已写入 state.json', 'note');
      } catch (err) { toast(`批注失败：${err.message || err}`, 'alert', 'warn'); }
      return;
    }
    ta.value = (ta.value.trim() ? ta.value.trimEnd() + '\n' : '') + `批注 ${label} ${selLabel(sel)}：${note}`;
    autoGrow(); ta.focus(); close();
  }
}
function renderAnnPins() {
  if (!AN) return;
  AN.veil.querySelectorAll('.an-pin').forEach(n => n.remove());
  const doc = AN.fr.contentDocument, w = AN.veil.clientWidth || 1, h = AN.veil.clientHeight || 1;
  (AN.isl._anns || []).forEach((a, i) => {
    const sel = a.sel;
    if (!sel || !sel.rect) return;
    let x, y;
    const live = doc && sel.css ? doc.querySelector(sel.css) : null;
    if (live) { const r = live.getBoundingClientRect(); x = r.left + r.width / 2; y = r.top + r.height / 2; }
    else { x = (sel.rect[0] + sel.rect[2] / 2) / 100 * w; y = (sel.rect[1] + sel.rect[3] / 2) / 100 * h; }
    const pin = document.createElement('span');
    pin.className = 'an-pin'; pin.textContent = i + 1;
    pin.style.left = x + 'px'; pin.style.top = y + 'px';
    AN.veil.appendChild(pin);
  });
}
// Esc：先关批注框，再退批注模式
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape' || !AN) return;
  if (AN.box) { const b = AN.box; AN.box = null; AN.hl.hidden = true; b.classList.add('out'); setTimeout(() => b.remove(), motion.dur('fast')); }
  else annOff();
});
// island 被 replay/清空拆掉时 veil 随之走 —— AN 里的死引用在下个手势清理
document.addEventListener('mousedown', () => { if (AN && !AN.isl.isConnected) AN = null; }, true);

