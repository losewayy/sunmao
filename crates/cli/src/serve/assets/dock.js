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
const BR = {}; // id → {hist, hi, live, chain} — live-only, not persisted
let brSeq = Date.now() % 100000; // ids must not collide across sessions' webview labels

const PANE_META = {
  overview: { t: t('概览'), i: 'zap', num: '' },
  agents: { t: t('子代理'), i: 'blocks', num: 'dt-count' },
  jobs: { t: t('后台'), i: 'clock', num: 'dj-count' },
  flow: { t: t('流向'), i: 'file-code', num: '' },
};
const brHost = u => { try { return new URL(u).host || u; } catch { return u; } };
const brPane = b => $(`#dock .dock-pane[data-pane="br:${b.id}"]`);
const brTitle = b => b.title || (b.url ? brHost(b.url) : t('浏览器'));
const nativeBr = () => !!(TAURI && TAURI.webview);

/* ---- tab strip ---- */
function renderDockTabs() {
  const tabs = sessionTabs(), cur = $('#dock').dataset.tab;
  $('#dt-dyn').innerHTML = tabs.map(tb => {
    if (t.kind === 'pane') {
      const m = PANE_META[t.pane] || {};
      return `<button class="dock-tab" role="tab" data-act="dock-tab" data-tab="${esc(tb.pane)}" aria-selected="${cur === tb.pane}"><svg class="i"><use href="#i-${m.i}"/></svg><span>${esc(m.t)}</span>${m.num ? `<i class="dt-num" id="${m.num}"></i>` : ''}<i class="dt-x" data-bclose="${tb.id}">×</i></button>`;
    }
    return `<button class="dock-tab dock-dyn" role="tab" data-act="dock-tab" data-tab="br:${tb.id}" aria-selected="${cur === 'br:' + tb.id}" data-tip="${esc(tb.url || esc(t('新标签页')))}"><svg class="i"><use href="#i-globe"/></svg><span>${esc(brTitle(tb))}</span><i class="dt-x" data-bclose="${tb.id}">×</i></button>`;
  }).join('');
  // empty panel = the launcher itself: the strip goes away and the body
  // is a vertical menu of everything you can add (KanaMi's shape)
  $('#dock-tabs').hidden = !tabs.length;
  $('#dock-empty').hidden = tabs.length > 0;
  if (!tabs.length) {
    $('#de-rows').innerHTML = [
      ...Object.entries(PANE_META).map(([v, m]) => ({ v, t: m.t, i: m.i })),
      { v: 'browser', t: t('浏览器标签页'), i: 'globe' },
    ].map(o => `<button class="de-row" data-deadd="${o.v}"><svg class="i"><use href="#i-${o.i}"/></svg><span>${esc(o.t)}</span></button>`).join('');
  }
}

function dockAddKind(v) {
  if (v === 'browser') return brNew();
  sessionTabs().push({ id: brSeq++, kind: 'pane', pane: v });
  save(); renderDockTabs(); dockTab(v);
}

function dockAdd(el) {
  const have = new Set(sessionTabs().filter(t => t.kind === 'pane').map(t => t.pane));
  const items = Object.entries(PANE_META)
    .filter(([k]) => !have.has(k))
    .map(([v, m]) => ({ v, t: m.t, icon: m.i }));
  items.push({ v: 'browser', t: t('浏览器标签页'), icon: 'globe' });
  menuPop(el, items, dockAddKind, { place: 'bottom', align: 'end' });
}

function brNew() {
  const b = { id: brSeq++, kind: 'browser', url: '', title: '', proxy: false };
  sessionTabs().push(b); BR[b.id] = { hist: [], hi: -1, live: false };
  save(); mountBrowser(b); renderDockTabs(); dockTab('br:' + b.id);
  if (!dockOn) toggleDock();
  setTimeout(() => $('.br-url', brPane(b))?.focus(), 30);
}

function dockClose(id) {
  const tabs = sessionTabs(), i = tabs.findIndex(t => t.id === +id);
  if (i < 0) return;
  const t = tabs[i];
  tabs.splice(i, 1);
  if (t.kind === 'browser') {
    brPane(t)?.remove();
    if (nativeBr()) brSendId(t.id, { op: 'close', id: t.id });
    brAnnStop(t.id); // an armed picker on a dead tab has nowhere to report
  }
  delete BR[t.id];
  save(); renderDockTabs();
  if ($('#dock').dataset.tab === (t.kind === 'browser' ? 'br:' + t.id : t.pane)) {
    const next = sessionTabs()[0];
    dockTab(next ? (next.kind === 'browser' ? 'br:' + next.id : next.pane) : '');
  }
}

/* ---- native child webview (Tauri shell) ---- */
/* The guest is window-level chrome: it must follow its pane's rect on
   every layout change and park offscreen whenever the pane isn't visible —
   a CSS-hidden iframe hides itself, a native webview does not.

   Two shell-side facts shape this code:
   - Rust runs these ops on a worker pool, not a queue, so two invokes in
     flight can land out of order (a create overtaking its own nav, or a
     duplicate create racing the first into "already exists"). Every send
     goes through its tab's own chain, so each tab's stream stays ordered.
   - `create` is only for a guest that does not exist yet; an existing one
     is moved with `rect`, so a resize never re-navigates the page. */
const BR_PARK = { x: -40000, y: 0, w: 10, h: 10 };
/* A page overlay (menu, popover, palette, image viewer) and a native guest
   cannot share pixels: the guest is a child HWND, so it paints over — and
   eats the pointer of — whatever the page draws inside its rect. While an
   overlay is open, the guests it would cover are parked offscreen at the
   pane's own size: invisible, but no reflow, so the page comes straight back
   when the overlay closes. `true` means "the overlay is the whole window". */
let brOverlayBox = null;
/* takes a DOMRect (a popover) or `true` (the overlay is the whole window);
   both normalize to the {x,y,w,h} shape the hit test reads */
const brBox = b => b === true
  ? { x: 0, y: 0, w: innerWidth, h: innerHeight }
  : { x: b.x, y: b.y, w: b.width ?? b.w, h: b.height ?? b.h };
const brOverlayHits = r => !!brOverlayBox && !!r &&
  brOverlayBox.x < r.x + r.w && r.x < brOverlayBox.x + brOverlayBox.w &&
  brOverlayBox.y < r.y + r.h && r.y < brOverlayBox.y + brOverlayBox.h;
function brOverlay(box) {
  const next = box ? brBox(box) : null;
  const had = !!brOverlayBox;
  brOverlayBox = next;
  if (had || next) brSyncAll();
}
const brSt = id => (BR[id] = BR[id] || { hist: [], hi: -1, live: false });
function brSendId(id, op) {
  const st = brSt(id);
  const next = (st.chain || Promise.resolve()).then(() => TAURI.webview(op));
  st.chain = next.catch(() => {});
  return next;
}
const brSend = (b, op) => brSendId(b.id, op);
const brViewRect = b => {
  const pane = brPane(b), bv = pane && $('.br-view', pane);
  if (!bv) return null;
  const r = bv.getBoundingClientRect(), z = S.zoom || 1;
  return { x: r.left * z, y: r.top * z, w: r.width * z, h: r.height * z };
};
/* sync the guest to its pane; true when this call is the one creating it */
function brSyncNative(b) {
  if (!nativeBr()) return false;
  const st = brSt(b.id), pane = brPane(b);
  const rect = b.url && pane && !pane.hidden && $('#app').dataset.dock === 'on' ? brViewRect(b) : null;
  if (!rect) { brSend(b, { op: 'rect', id: b.id, rect: BR_PARK }); return false; }
  if (brOverlayHits(rect)) { brSend(b, { op: 'rect', id: b.id, rect: Object.assign({}, rect, { x: BR_PARK.x }) }); return false; }
  if (st.live || st.creating) { brSend(b, { op: 'rect', id: b.id, rect }); return false; }
  st.creating = true;
  brSend(b, { op: 'create', id: b.id, url: b.url, rect })
    .then(() => { st.live = true; }, e => {
      const em = pane && $('.br-empty small', pane);
      if (em) em.textContent = t('原生窗口创建失败：{e}', { e });
    })
    .finally(() => { st.creating = false; });
  return true;
}
/* create the guest if it is missing, then navigate — one ordered stream */
function brGoNative(b, url) {
  if (!brSyncNative(b)) brSend(b, { op: 'nav', id: b.id, url });
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
  const bid = b.id;
  BR[bid] = BR[bid] || { hist: [], hi: -1, live: false };
  /* a ui_changed frame reloads S wholesale (state.js loadUi), so the array
     object bound here goes stale — every handler re-resolves the live one
     by id instead of writing into the orphan */
  const tb = () => sessionTabs().find(t => t.id === bid) || b;
  const pane = append($('#dock'), `<div class="dock-pane br" data-pane="br:${bid}" hidden>
    <div class="br-bar">
      <button class="ib sm" data-bnav="back" data-tip="${esc(t('后退'))}" aria-label="${esc(t('后退'))}"><svg class="i"><use href="#i-chev-l"/></svg></button>
      <button class="ib sm" data-bnav="fwd" data-tip="${esc(t('前进'))}" aria-label="${esc(t('前进'))}"><svg class="i"><use href="#i-chev-r"/></svg></button>
      <button class="ib sm" data-bnav="reload" data-tip="${esc(t('刷新'))}" aria-label="${esc(t('刷新'))}"><svg class="i"><use href="#i-rotate"/></svg></button>
      <input class="br-url mono" placeholder="${esc(t('输入网址 — localhost:3000 或 https://…'))}" spellcheck="false" autocomplete="off">
      ${nativeBr() ? '' : `<button class="ib sm br-proxy" data-bnav="proxy" data-tip="${esc(t('代理加载 — 跨源站点经本地代理后可元素批注（页面脚本不运行）'))}" aria-label="${esc(t('代理加载'))}"><svg class="i"><use href="#i-shield"/></svg></button>`}
      <button class="ib sm" data-act="annotate" data-tip="${esc(t('批注页面元素或区域'))}" aria-label="${esc(t('批注'))}"><svg class="i"><use href="#i-note"/></svg></button>
      <button class="ib sm" data-bnav="external" data-tip="${esc(t('在系统浏览器中打开'))}" aria-label="${esc(t('外部打开'))}"><svg class="i"><use href="#i-external"/></svg></button>
    </div>
    <div class="br-view">
      <div class="br-empty"><svg class="i"><use href="#i-globe"/></svg><p>${t('输入网址回车加载')}</p><small>${nativeBr() ? t('原生内核 — 任何站点都能开') : t('本地 dev 服务器直接可交互；跨源站点开 {proxy} 后可批注元素', { proxy: '<b>' + t('代理') + '</b>' })}</small></div>
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
    const live = tb();
    if (t && t !== live.title) { live.title = t; save(); renderDockTabs(); }
  });
  url.addEventListener('keydown', e => {
    if (e.key === 'Enter') { e.preventDefault(); brGo(tb(), url.value); }
    e.stopPropagation();
  });
  pane.addEventListener('click', e => {
    const nb = e.target.closest('[data-bnav]');
    if (!nb) return;
    const live = tb(), st = BR[bid];
    switch (nb.dataset.bnav) {
      case 'back': if (st.hi > 0) { st.hi--; brGo(live, st.hist[st.hi], false); } break;
      case 'fwd': if (st.hi < st.hist.length - 1) { st.hi++; brGo(live, st.hist[st.hi], false); } break;
      case 'reload': {
        if (nativeBr()) brGoNative(live, live.url);
        else { const fr2 = $('iframe', pane); if (!fr2.hidden) fr2.src = fr2.src; }
        break;
      }
      case 'proxy': live.proxy = !live.proxy; nb.classList.toggle('on', live.proxy); save(); if (live.url) brGo(live, live.url, false); break;
      case 'external': {
        if (!live.url) break;
        if (TAURI && TAURI.openExternal) return Promise.resolve(TAURI.openExternal(live.url)).catch(er => toast(t('打开失败：{e}', { e: er }), 'alert', 'warn'));
        return open(live.url, '_blank');
      }
    }
  });
  const live = tb();
  if (live.url && !EMBED) brGo(live, live.url, false); // persisted tab restores loaded
}

/* ---- 元素批注（原生 guest）----
   The guest is its own document, so the page cannot pick anything in it: the
   shell injects the picker (annotate.js) into the guest and the whole
   gesture happens there. The result comes back through `ann-poll` and lands
   in the Composer — the selector plus the page URL is what an agent needs to
   go fix it. Canvas/iframe targets degrade to a region inside the guest. */
const brAnnBtn = id => $(`#dock .dock-pane[data-pane="br:${id}"] [data-act="annotate"]`);
function brAnnStop(id) {
  const st = BR[id];
  if (st && st.ann) { clearTimeout(st.ann); st.ann = 0; }
  const btn = brAnnBtn(id);
  if (btn) btn.classList.remove('on');
}
function brAnnLine(p) {
  const sel = p.sel || {};
  if (sel.kind === 'region') return t('批注 {url} 区域 {x},{y} {w}×{h}：{note}', { url: p.url, x: sel.rect.x, y: sel.rect.y, w: sel.rect.w, h: sel.rect.h, note: p.note });
  return t(sel.text ? '批注 {url} {sel}「{text}」：{note}' : '批注 {url} {sel}：{note}', { url: p.url, sel: sel.css || selLabel(sel), text: (sel.text || '').slice(0, 40), note: p.note });
}
function brAnnToComposer(p) {
  const ta = $('#input');
  ta.value = (ta.value.trim() ? ta.value.trimEnd() + '\n' : '') + brAnnLine(p);
  autoGrow(); ta.focus();
  toast(t('批注已写入输入框'), 'note');
}
/* click again to disarm; the poll is the only way back from a foreign page.
   Self-scheduling, not setInterval: a poll can sit for its full timeout when
   the guest is mid-navigation, and they must not stack up on the runtime.
   The wait is a poll interval, not motion — a named constant, same as the
   debounces in state.js. */
const ANN_POLL_MS = 300;
function brAnnToggle(pane) {
  const id = +(pane.dataset.pane || 'br:0').slice(3);
  const st = brSt(id);
  if (st.ann) { brAnnStop(id); return; }
  const btn = brAnnBtn(id);
  if (btn) btn.classList.add('on');
  brSendId(id, { op: 'annotate', id, lang: uiLang });
  // a navigation throws the injected picker away, so the poll must not
  // outlive it: `brGo` disarms, and this cap covers a guest that navigated
  // on its own (a link in the page)
  let left = 1200;
  const tick = async () => {
    if (!st.ann) return;
    if (--left < 0) { brAnnStop(id); return; }
    let raw = null;
    try { raw = await TAURI.webview({ op: 'ann-poll', id }); } catch { /* guest busy */ }
    if (!st.ann) return;
    // the payload may arrive as an object or as its JSON text — accept both
    let v = raw;
    for (let i = 0; i < 2 && typeof v === 'string'; i++) { try { v = JSON.parse(v); } catch { v = null; break; } }
    if (v && typeof v === 'object') {
      brAnnStop(id);
      if (!v.cancel) brAnnToComposer(v);
      return;
    }
    st.ann = setTimeout(tick, ANN_POLL_MS);
  };
  st.ann = setTimeout(tick, ANN_POLL_MS);
}

function brGo(b, raw, push = true) {
  let url = String(raw || '').trim();
  if (!url) return;
  // any navigation replaces the guest document, picker and all
  if (BR[b.id] && BR[b.id].ann) brAnnStop(b.id);
  if (url.startsWith('/')) url = location.origin + url;
  if (!/^[a-z]+:\/\//i.test(url)) url = (/^[\w.-]+(:\d+)?([/:]|$)/.test(url) ? 'http://' : 'https://') + url;
  const st = brSt(b.id), pane = brPane(b);
  if (!pane) return;
  if (push) { st.hist = st.hist.slice(0, st.hi + 1); st.hist.push(url); st.hi = st.hist.length - 1; }
  b.url = url; save();
  if (nativeBr()) {
    // first navigation creates the guest at its pane rect (create carries
    // the url); a guest that already exists just takes the nav. Both ride
    // the tab's chain, so the url can never land before the guest does.
    brGoNative(b, url);
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
/* the tab set is per-session: on a switch the old session's panes/guests
   leave with it, the new session's mount in. This also runs whenever a
   `ui_changed` frame reloads S (every save round-trips through it), so it
   reconciles — only a tab that left the set loses its guest — instead of
   tearing every native webview down on each save. */
function dockSessionSwap() {
  const keep = new Set(sessionTabs().filter(t => t.kind === 'browser').map(t => 'br:' + t.id));
  for (const id of Object.keys(BR)) {
    if (keep.has('br:' + id)) continue;
    if (nativeBr()) brSendId(+id, { op: 'close', id: +id });
    brAnnStop(+id);
    if (BR[id]) BR[id].live = false;
  }
  $$('#dock .dock-pane[data-pane^="br:"]').forEach(p => { if (!keep.has(p.dataset.pane)) p.remove(); });
  sessionTabs().filter(t => t.kind === 'browser').forEach(mountBrowser);
  renderDockTabs();
  const cur = $('#dock').dataset.tab, tabs = sessionTabs();
  /* panes mounted fresh above start `hidden` — the remembered tab may be
     valid for this set yet point at a pane that didn't exist when it was
     last activated; re-activate either way so something actually shows */
  const want = cur && tabs.some(t => (t.kind === 'browser' ? 'br:' + t.id : t.pane) === cur)
    ? cur : (tabs[0] ? (tabs[0].kind === 'browser' ? 'br:' + tabs[0].id : tabs[0].pane) : '');
  dockTab(want);
  brSyncAll();
}

/* ---- resize edge ---- */
(function dockResize() {
  const edge = $('#dock-edge'), dock = $('#dock');
  edge.addEventListener('pointerdown', e => {
    e.preventDefault();
    edge.setPointerCapture(e.pointerId);
    const app = document.getElementById('app');
    app.dataset.dragging = '1';
    const right = dock.getBoundingClientRect().right;
    const move = ev => {
      const w = Math.max(240, Math.min(innerWidth * 0.5, right - ev.clientX));
      app.style.setProperty('--w-dock', w + 'px');
      S.dockW = Math.round(w);
    };
    const up = () => {
      delete app.dataset.dragging;
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
$('#de-rows').addEventListener('click', e => {
  const row = e.target.closest('[data-deadd]');
  if (row) dockAddKind(row.dataset.deadd);
});
// framed-as-embed: a browser pane loaded this page — mount the strip so
// the UI looks right, but EMBED suppresses the auto-load inside
// mountBrowser or the nested app would nest us again (pairs with the
// ?embed src marker in brGo)
const EMBED = new URLSearchParams(location.search).has('embed');
sessionTabs().filter(t => t.kind === 'browser').forEach(mountBrowser);
renderDockTabs();
