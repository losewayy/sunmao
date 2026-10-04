/* dock — the right-hand panel. The tab strip is EMPTY by default and
   per-session: + opens a launcher listing every pane kind (overview/
   agents/jobs/flow) plus browser tabs; the set lives in ui.json keyed by
   session id (S.dockTabs[sess]). Left edge drags to resize (S.dockW).
   In the Tauri shell a browser tab is a real child webview composited
   over .br-view (X-Frame-Options can't refuse a guest); in a plain
   browser it falls back to the iframe (+ host proxy for cross-origin
   annotation). Live DOM/history hangs off BR. */
'use strict';

/* S.dockTabs: {[sessionId]: [{id,kind:'pane',pane}|{id,kind:'browser',url,title,proxy}]}
   A legacy flat array (pre-session scoping) migrates onto the live session. */
const sessionTabs = () => {
  if (Array.isArray(S.dockTabs)) S.dockTabs = { __legacy: S.dockTabs };
  if (typeof S.dockTabs !== 'object' || !S.dockTabs) S.dockTabs = {};
  // a legacy flat list lands on the first real session that claims it —
  // pre-hello boots have no id, so it waits under __legacy until one does
  if (sessionId && S.dockTabs.__legacy) {
    S.dockTabs[sessionId] = S.dockTabs.__legacy;
    delete S.dockTabs.__legacy;
  }
  return S.dockTabs[sessionId] = S.dockTabs[sessionId] || [];
};
const BR = {}; // id → {hist, hi} — live-only, not persisted
let brSeq = Date.now() % 100000; // ids must not collide across sessions' webview labels

const PANE_META = {
  overview: { t: '概览', i: 'zap', num: '' },
  agents: { t: '子代理', i: 'blocks', num: 'dt-count' },
  jobs: { t: '后台', i: 'clock', num: 'dj-count' },
  flow: { t: '流向', i: 'file-code', num: '' },
};
const brHost = u => { try { return new URL(u).host || u; } catch { return u; } };
const brPane = b => $(`#dock .dock-pane[data-pane="br:${b.id}"]`);
const brTitle = b => b.title || (b.url ? brHost(b.url) : '浏览器');
const nativeBr = () => !!(TAURI && TAURI.webview);

/* ---- tab strip ---- */
function renderDockTabs() {
  const cur = $('#dock').dataset.tab;
  $('#dt-dyn').innerHTML = sessionTabs().map(t => {
    if (t.kind === 'pane') {
      const m = PANE_META[t.pane] || {};
      return `<button class="dock-tab" role="tab" data-act="dock-tab" data-tab="${esc(t.pane)}" aria-selected="${cur === t.pane}"><svg class="i"><use href="#i-${m.i}"/></svg><span>${esc(m.t)}</span>${m.num ? `<i class="dt-num" id="${m.num}"></i>` : ''}<i class="dt-x" data-bclose="${t.id}">×</i></button>`;
    }
    return `<button class="dock-tab dock-dyn" role="tab" data-act="dock-tab" data-tab="br:${t.id}" aria-selected="${cur === 'br:' + t.id}" data-tip="${esc(t.url || '新标签页')}"><svg class="i"><use href="#i-globe"/></svg><span>${esc(brTitle(t))}</span><i class="dt-x" data-bclose="${t.id}">×</i></button>`;
  }).join('');
  $('#dock-empty').hidden = sessionTabs().length > 0;
}

function dockAdd(el) {
  const have = new Set(sessionTabs().filter(t => t.kind === 'pane').map(t => t.pane));
  const items = Object.entries(PANE_META)
    .filter(([k]) => !have.has(k))
    .map(([v, m]) => ({ v, t: m.t, icon: m.i }));
  items.push({ v: 'browser', t: '浏览器标签页', icon: 'globe' });
  menuPop(el, items, v => {
    if (v === 'browser') return brNew();
    const t = { id: brSeq++, kind: 'pane', pane: v };
    sessionTabs().push(t);
    save(); renderDockTabs(); dockTab(v);
  }, { place: 'bottom', align: 'end' });
}

function brNew() {
  const b = { id: brSeq++, kind: 'browser', url: '', title: '', proxy: false };
  sessionTabs().push(b); BR[b.id] = { hist: [], hi: -1 };
  save(); mountBrowser(b); renderDockTabs(); dockTab('br:' + b.id);
  if (!dockOn) toggleDock();
  setTimeout(() => $('.br-url', brPane(b))?.focus(), 30);
}

function dockClose(id) {
  const tabs = sessionTabs(), i = tabs.findIndex(t => t.id === +id);
  if (i < 0) return;
  const t = tabs[i];
  tabs.splice(i, 1); delete BR[t.id];
  if (t.kind === 'browser') {
    brPane(t)?.remove();
    if (nativeBr()) TAURI.webview({ op: 'close', id: t.id });
  }
  save(); renderDockTabs();
  if ($('#dock').dataset.tab === (t.kind === 'browser' ? 'br:' + t.id : t.pane)) {
    const next = sessionTabs()[0];
    dockTab(next ? (next.kind === 'browser' ? 'br:' + next.id : next.pane) : '');
  }
}

/* ---- native child webview (Tauri shell) ---- */
/* the guest is window-level chrome: it must follow its pane's rect on
   every layout change and park offscreen whenever the pane isn't visible —
   a CSS-hidden iframe hides itself, a native webview does not */
function brSyncNative(b) {
  if (!nativeBr()) return;
  const bv = brPane(b) && $('.br-view', brPane(b));
  const vis = !!(b.url && bv && !brPane(b).hidden && $('#app').dataset.dock === 'on');
  if (!vis) {
    TAURI.webview({ op: 'rect', id: b.id, rect: { x: -40000, y: 0, w: 10, h: 10 } });
    return;
  }
  const r = view.getBoundingClientRect(), z = S.zoom || 1;
  const rect = { x: r.left * z, y: r.top * z, w: r.width * z, h: r.height * z };
  TAURI.webview({ op: 'create', id: b.id, url: b.url, rect });
  TAURI.webview({ op: 'rect', id: b.id, rect });
}
function brSyncAll() {
  for (const t of sessionTabs()) if (t.kind === 'browser') brSyncNative(t);
}
/* the whole grid can move under a native webview: dock width drags, window
   resizes, zoom steps, tab/dock/view flips — re-sync on all of them */
window.addEventListener('resize', brSyncAll);
new ResizeObserver(brSyncAll).observe($('#dock'));

/* ---- browser pane ---- */
function mountBrowser(b) {
  if (brPane(b)) return;
  BR[b.id] = BR[b.id] || { hist: [], hi: -1 };
  const pane = append($('#dock'), `<div class="dock-pane br" data-pane="br:${b.id}" hidden>
    <div class="br-bar">
      <button class="ib sm" data-bnav="back" data-tip="后退" aria-label="后退"><svg class="i"><use href="#i-chev-l"/></svg></button>
      <button class="ib sm" data-bnav="fwd" data-tip="前进" aria-label="前进"><svg class="i"><use href="#i-chev-r"/></svg></button>
      <button class="ib sm" data-bnav="reload" data-tip="刷新" aria-label="刷新"><svg class="i"><use href="#i-rotate"/></svg></button>
      <input class="br-url mono" placeholder="输入网址 — localhost:3000 或 https://…" spellcheck="false" autocomplete="off">
      ${nativeBr() ? '' : '<button class="ib sm br-proxy" data-bnav="proxy" data-tip="代理加载 — 跨源站点经本地代理后可元素批注（页面脚本不运行）" aria-label="代理加载"><svg class="i"><use href="#i-shield"/></svg></button>'}
      <button class="ib sm" data-act="annotate" data-tip="批注页面元素或区域" aria-label="批注"><svg class="i"><use href="#i-note"/></svg></button>
      <button class="ib sm" data-bnav="external" data-tip="在系统浏览器中打开" aria-label="外部打开"><svg class="i"><use href="#i-external"/></svg></button>
    </div>
    <div class="br-view">
      <div class="br-empty"><svg class="i"><use href="#i-globe"/></svg><p>输入网址回车加载</p><small>${nativeBr() ? '原生内核 — 任何站点都能开' : '本地 dev 服务器直接可交互；跨源站点开 <b>代理</b> 后可批注元素'}</small></div>
      <iframe hidden></iframe>
    </div>
  </div>`);
  const url = $('.br-url', pane), fr = $('iframe', pane);
  url.value = b.url || '';
  const px = $('.br-proxy', pane);
  if (px) px.classList.toggle('on', !!b.proxy);
  fr.addEventListener('load', () => {
    let t = null;
    try { t = fr.contentDocument && fr.contentDocument.title; } catch {}
    if (t && t !== b.title) { b.title = t; save(); renderDockTabs(); }
  });
  url.addEventListener('keydown', e => {
    if (e.key === 'Enter') { e.preventDefault(); brGo(b, url.value); }
    e.stopPropagation();
  });
  pane.addEventListener('click', e => {
    const nb = e.target.closest('[data-bnav]');
    if (!nb) return;
    const st = BR[b.id];
    switch (nb.dataset.bnav) {
      case 'back': if (st.hi > 0) { st.hi--; brGo(b, st.hist[st.hi], false); } break;
      case 'fwd': if (st.hi < st.hist.length - 1) { st.hi++; brGo(b, st.hist[st.hi], false); } break;
      case 'reload': {
        if (nativeBr()) TAURI.webview({ op: 'nav', id: b.id, url: b.url });
        else { const fr2 = $('iframe', pane); if (!fr2.hidden) fr2.src = fr2.src; }
        break;
      }
      case 'proxy': b.proxy = !b.proxy; nb.classList.toggle('on', b.proxy); save(); if (b.url) brGo(b, b.url, false); break;
      case 'external': {
        if (!b.url) break;
        if (TAURI && TAURI.openExternal) return Promise.resolve(TAURI.openExternal(b.url)).catch(er => toast(`打开失败：${er}`, 'alert', 'warn'));
        return open(b.url, '_blank');
      }
    }
  });
  if (b.url && !EMBED) brGo(b, b.url, false); // persisted tab restores loaded
}

function brGo(b, raw, push = true) {
  let url = String(raw || '').trim();
  if (!url) return;
  if (url.startsWith('/')) url = location.origin + url;
  if (!/^[a-z]+:\/\//i.test(url)) url = (/^[\w.-]+(:\d+)?([/:]|$)/.test(url) ? 'http://' : 'https://') + url;
  const st = BR[b.id], pane = brPane(b);
  if (!pane) return;
  if (push) { st.hist = st.hist.slice(0, st.hi + 1); st.hist.push(url); st.hi = st.hist.length - 1; }
  b.url = url; save();
  if (nativeBr()) {
    // a real guest webview — create it parked offscreen on about:blank,
    // then one navigate; no sandbox gymnastics needed
    TAURI.webview({ op: 'create', id: b.id, url: 'about:blank', rect: { x: -40000, y: 0, w: 10, h: 10 } });
    TAURI.webview({ op: 'nav', id: b.id, url });
  } else {
    const fr = $('iframe', pane);
    // direct: opaque origin + scripts — a real page, DOM unreachable so
    // annotation falls back to region/point. proxy: same-origin doc, no
    // scripts — element-level picking, zero script access to our API.
    fr.setAttribute('sandbox', b.proxy ? 'allow-same-origin' : 'allow-scripts allow-same-origin allow-forms allow-popups');
    // same-origin targets get ?embed: if the framed doc is this app it
    // boots with EMBED set and skips restoring its own browser tabs —
    // that is what stops self-nesting from recursing forever
    const same = url.startsWith(location.origin);
    fr.src = b.proxy
      ? '/browse?url=' + encodeURIComponent(url) + (same ? '&embed=1' : '')
      : same ? url + (url.includes('?') ? '&' : '?') + 'embed=1' : url;
  }
  $('.br-empty', pane).hidden = true; $('iframe', pane).hidden = nativeBr();
  $('.br-url', pane).value = url;
  pane.dataset.annLabel = url;
  b.title = b.title || brHost(url);
  renderDockTabs();
  const [bk, fw] = $$('[data-bnav="back"],[data-bnav="fwd"]', pane);
  if (bk) bk.disabled = st.hi <= 0;
  if (fw) fw.disabled = st.hi >= st.hist.length - 1;
  brSyncNative(b);
}

/* ---- session lifecycle ---- */
/* the tab set is per-session: on a switch the old session's panes/webviews
   leave with it, the new session's mount in */
function dockSessionSwap() {
  if (nativeBr()) for (const id of Object.keys(BR)) TAURI.webview({ op: 'close', id: +id });
  const keep = new Set(sessionTabs().filter(t => t.kind === 'browser').map(t => 'br:' + t.id));
  $$('#dock .dock-pane[data-pane^="br:"]').forEach(p => { if (!keep.has(p.dataset.pane)) p.remove(); });
  sessionTabs().filter(t => t.kind === 'browser').forEach(mountBrowser);
  renderDockTabs();
  const cur = $('#dock').dataset.tab, tabs = sessionTabs();
  if (cur && !tabs.some(t => (t.kind === 'browser' ? 'br:' + t.id : t.pane) === cur)) {
    const first = tabs[0];
    dockTab(first ? (first.kind === 'browser' ? 'br:' + first.id : first.pane) : '');
  }
  brSyncAll();
}

/* ---- resize edge ---- */
(function dockResize() {
  const edge = $('#dock-edge'), dock = $('#dock');
  if (S.dockW) document.getElementById('app').style.setProperty('--w-dock', S.dockW + 'px');
  edge.addEventListener('pointerdown', e => {
    e.preventDefault();
    edge.setPointerCapture(e.pointerId);
    const right = dock.getBoundingClientRect().right;
    const move = ev => {
      const w = Math.max(240, Math.min(innerWidth * 0.72, right - ev.clientX));
      document.getElementById('app').style.setProperty('--w-dock', w + 'px');
      S.dockW = Math.round(w);
    };
    const up = () => {
      edge.removeEventListener('pointermove', move);
      edge.removeEventListener('pointerup', up);
      save();
      brSyncAll();
    };
    edge.addEventListener('pointermove', move);
    edge.addEventListener('pointerup', up);
  });
})();

/* ---- boot ---- */
// #dt-dyn lives in the static nav; clicks on a × close the tab without
// tripping the dock-tab act on the row
$('#dt-dyn').addEventListener('click', e => {
  const x = e.target.closest('[data-bclose]');
  if (!x) return;
  e.preventDefault(); e.stopPropagation();
  dockClose(x.dataset.bclose);
});
// framed-as-embed: a browser pane loaded this page — mount the strip so
// the UI looks right, but EMBED suppresses the auto-load inside
// mountBrowser or the nested app would nest us again (pairs with the
// ?embed src marker in brGo)
const EMBED = new URLSearchParams(location.search).has('embed');
sessionTabs().filter(t => t.kind === 'browser').forEach(mountBrowser);
renderDockTabs();
