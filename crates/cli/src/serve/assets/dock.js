/* dock — dynamic tab strip: the four fixed pane tabs (概览/子代理/后台/
   流向), then dynamic browser tabs the + launcher spawns — each a full
   page with its own address bar/history, reusing the island annotation
   layer. Dynamic tabs persist in ui.json (S.dockTabs); their live
   DOM/history hangs off BR. */
'use strict';

const DOCK_TABS = S.dockTabs = Array.isArray(S.dockTabs) ? S.dockTabs : [];
const BR = {}; // id → {hist, hi} — live-only, not persisted
let brSeq = DOCK_TABS.reduce((m, t) => Math.max(m, t.id || 0), 0) + 1;

const brHost = u => { try { return new URL(u).host || u; } catch { return u; } };
const brPane = b => $(`#dock .dock-pane[data-pane="br:${b.id}"]`);
const brTitle = b => b.title || (b.url ? brHost(b.url) : '浏览器');

/* ---- tab strip ---- */
function renderDockTabs() {
  $('#dt-dyn').innerHTML = DOCK_TABS.map(b =>
    `<button class="dock-tab dock-dyn" role="tab" data-act="dock-tab" data-tab="br:${b.id}" aria-selected="${$('#dock').dataset.tab === 'br:' + b.id}" data-tip="${esc(b.url || '新标签页')}"><svg class="i"><use href="#i-globe"/></svg><span>${esc(brTitle(b))}</span><i class="dt-x" data-bclose="${b.id}">×</i></button>`
  ).join('');
}

function dockAdd(el) {
  menuPop(el, [{ v: 'browser', t: '新建浏览器标签页', icon: 'globe' }], () => brNew(), { place: 'bottom', align: 'end' });
}

function brNew() {
  const b = { id: brSeq++, kind: 'browser', url: '', title: '', proxy: false };
  DOCK_TABS.push(b); BR[b.id] = { hist: [], hi: -1 };
  save(); mountBrowser(b); renderDockTabs(); dockTab('br:' + b.id);
  if (!dockOn) toggleDock();
  setTimeout(() => $('.br-url', brPane(b))?.focus(), 30);
}

function dockClose(id) {
  const i = DOCK_TABS.findIndex(t => t.id === +id);
  if (i < 0) return;
  DOCK_TABS.splice(i, 1); delete BR[id];
  brPane({ id })?.remove();
  save(); renderDockTabs();
  if ($('#dock').dataset.tab === 'br:' + id) dockTab('overview');
}

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
      <button class="ib sm br-proxy" data-bnav="proxy" data-tip="代理加载 — 跨源站点经本地代理后可元素批注（页面脚本不运行）" aria-label="代理加载"><svg class="i"><use href="#i-shield"/></svg></button>
      <button class="ib sm" data-act="annotate" data-tip="批注页面元素或区域" aria-label="批注"><svg class="i"><use href="#i-note"/></svg></button>
      <button class="ib sm" data-bnav="external" data-tip="在系统浏览器中打开" aria-label="外部打开"><svg class="i"><use href="#i-external"/></svg></button>
    </div>
    <div class="br-view">
      <div class="br-empty"><svg class="i"><use href="#i-globe"/></svg><p>输入网址回车加载</p><small>本地 dev 服务器直接可交互；跨源站点开 <b>代理</b> 后可批注元素</small></div>
      <iframe hidden></iframe>
    </div>
  </div>`);
  const url = $('.br-url', pane), fr = $('iframe', pane);
  url.value = b.url || '';
  $('.br-proxy', pane).classList.toggle('on', !!b.proxy);
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
      case 'reload': { const fr2 = $('iframe', pane); if (!fr2.hidden) fr2.src = fr2.src; break; }
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
  $('.br-empty', pane).hidden = true; fr.hidden = false;
  $('.br-url', pane).value = url;
  pane.dataset.annLabel = url;
  b.title = b.title || brHost(url);
  renderDockTabs();
  const [bk, fw] = $$('[data-bnav="back"],[data-bnav="fwd"]', pane);
  if (bk) bk.disabled = st.hi <= 0;
  if (fw) fw.disabled = st.hi >= st.hist.length - 1;
}

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
DOCK_TABS.forEach(mountBrowser);
renderDockTabs();
