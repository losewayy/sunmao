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
  let meta;
  try {
    const r = await fetch(`/artifacts/${encodeURIComponent(name)}/ui`);
    if (!r.ok) return;
    meta = await r.json();
  } catch { return; }
  let html = '';
  try { html = await (await fetch(`/artifacts/${encodeURIComponent(name)}`)).text(); } catch {}
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
        '--color-background-inverse': dark ? '#E8E9F0' : '#1a1d2e',
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
      if (h && sb) sb.style.height = Math.min(Math.max(Math.round(h), 80), 1400) + 'px';
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

// island → host: messages come from the sandbox iframe's contentWindow
window.addEventListener('message', ev => {
  const d = ev.data;
  if (!d || d.jsonrpc !== '2.0') return;
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
    const r = await fetch('/artifacts/' + encodeURIComponent(name) + '/revs');
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
  $('iframe', isl).src = cur === max
    ? `/artifacts/${encodeURIComponent(name)}`
    : `/artifacts/${encodeURIComponent(name)}?rev=${cur}`;
}
async function refreshNotes(el, name) {
  let anns = [];
  try {
    const r = await fetch('/artifacts/' + encodeURIComponent(name) + '/notes');
    const v = await r.json();
    anns = v && Array.isArray(v.annotations) ? v.annotations : [];
  } catch {}
  const pill = $('.notes-pill', el), list = $('.notes', el);
  $('span:last-child', pill).textContent = anns.length ? `+${anns.length} notes` : '批注';
  list.innerHTML = anns.map(a => `<div class="note-r"><span>${esc(a.note || '')}</span><time>${esc(a.at || '')}</time></div>`).join('')
    + `<div class="note-add"><input data-name="${esc(name)}" placeholder="添加批注，写入 ${esc(name)}.state.json" aria-label="批注"><button class="btn ghost sm" data-act="note-add" data-name="${esc(name)}">添加</button></div>`;
}

