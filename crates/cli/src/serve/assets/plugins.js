/* plugins.js — the plugin host: registry, capability facade, loader.

   Trust model mirrors trusted-hooks: `.sunmao/plugins/*.js` in the viewed
   session's project, enabled by pinning the file's sha256 in
   `.sunmao/plugins.json`. The serve only hands out bytes while the file is
   enabled AND hash-identical to what was approved — an agent-written file
   can't self-enable, and a tampered file stops serving.

   Isolation v1: ES modules. Module scope means no accidental collisions
   with the shared classic-script globals; the documented reach is
   `window.sunmao` — a facade that mediates data (host.api), events
   (host.on), and slot registration instead of handing out raw internals.
   A per-plugin sandboxed iframe host is the v2 hardening path; the
   register/mount contract is shaped so a slot can opt into it later.

   v1 slots: `dock` (a dock pane) and `settings` (a settings page). Adding
   a pane/page takes a file in .sunmao/plugins + one toggle — no edits to
   index.html, serve.rs, or request.rs (they carry the *system*, once). */
const PLUGIN_DOCK = new Map();   // 'plg:<id>:<slot>'  → {slot, plugin, host, mounted}
const PLUGIN_COMPOSER = new Map(); // 'plg:<id>:<slot>' → {slot, plugin, host} — cmp-bar chips
const PLUGIN_PAL = [];           // {g,t,i,k,sub,run,plugin,host} — palette rows
const PLUGIN_MENUS = [];         // {v,t,i,warn,run,plugin,host} — session-menu items
const PLUGIN_TRANSFORM = [];     // {plugin, fn} — draft transforms (veto-able send)
/* palSource() reads a plain items array — re-project whenever the set
   changes (register + unregister) */
function syncPalItems() {
  window.PLUGIN_PAL_ITEMS = PLUGIN_PAL.map(m => ({ g: m.g, t: m.t, i: m.i, k: m.k, sub: m.sub, run: () => m.run(m.host) }));
}
// const bindings don't reach window — builtin consumers (settings page
// dispatch) read it through this explicit handle instead
window.PLUGIN_PAGES = new Map(); // 'plg:<id>'  → {slot, plugin, host}
const PLUGIN_PAGES = window.PLUGIN_PAGES;
const PLUGIN_EVENTS = [];        // {plugin, ev, fn} — 'live' frames, 'session' switches
const PLUGIN_IDS = new Set();
/* sticky last-value per live-frame type — panes mount lazily (on first tab
   open), long after replay frames already flowed. The cache lets a fresh
   mount see the CURRENT state instead of an empty world; cleared on
   session switch so another session's frames can't bleed through. */
const PLUGIN_LAST_LIVE = {};

const pluginHost = plugin => ({
  plugin,
  /* REST through the app's own transport, session-tagged for the viewed
     session. An enabled plugin IS trusted page code — the facade scopes
     intent, it is NOT an enforcement boundary (same-origin fetch exists);
     the pinned ledger is the trust control, isolation is the v2 iframe. */
  api: (path, opts) => api(`${path}${path.includes('?') ? '&' : '?'}sess=${encodeURIComponent(sessionId)}`, opts),
  t, esc, ic, toast,
  sess: () => sessionId,
  // the viewed session's shape — read-only snapshot, refreshed per call
  session: () => ({ id: sessionId, cwd, model: modelLabel, driver, busy: !!busy, goal: curGoal }),
  on: (ev, fn) => PLUGIN_EVENTS.push({ plugin, ev, fn }),
  // draft-transform: runs on the outgoing prompt before it hits the wire;
  // return the (possibly rewritten) text, or false to veto the send
  onSend: fn => PLUGIN_TRANSFORM.push({ plugin, fn }),
  // write paths — prompts steer through the same frames the composer sends
  send: text => wsSend({ type: 'prompt', text: String(text || '') }),
  steer: text => wsSend({ type: 'steer', text: String(text || '') }),
  // chrome helpers — the shared popover/menu builders, so a plugin's UI
  // lands with the app's own motion/placement instead of re-inventing it
  pop: (anchor, html, o) => pop(anchor, html, o),
  menuPop: (anchor, items, onPick, o) => menuPop(anchor, items, onPick, o),
  closePop,
  // namespaced KV — rides ui.json via save(); keys can't collide across
  // plugins, and the value must stay JSON-shaped (it's ui.json on disk)
  store: (k, v) => { S[`plg:${plugin}:${k}`] = v; save(); },
  load: k => S[`plg:${plugin}:${k}`],
  // a plugin ships its own glyphs: registers <symbol> defs and returns the
  // name to feed ic()/slot icons. Ids are namespaced the same way.
  icon: (name, innerSvg) => {
    const sym = `plg-${plugin}-${name}`;
    const defs = $('#icon-defs defs');
    if (defs && !document.getElementById(`i-${sym}`))
      defs.insertAdjacentHTML('beforeend', `<symbol id="i-${sym}" viewBox="0 0 24 24">${innerSvg}</symbol>`);
    return sym;
  },
});

function pluginEmit(ev, data) {
  if (ev === 'session') for (const k of Object.keys(PLUGIN_LAST_LIVE)) delete PLUGIN_LAST_LIVE[k];
  if (ev === 'live' && data && typeof data === 'object' && data.type) PLUGIN_LAST_LIVE[data.type] = data;
  // index-iterate instead of slicing — a live frame fires per token; a
  // listener that unregisters itself mid-emit simply gets skipped
  for (let i = 0; i < PLUGIN_EVENTS.length; i++) {
    const l = PLUGIN_EVENTS[i];
    if (l.ev !== ev) continue;
    try { l.fn(data); } catch (e) { console.warn('plugin', l.plugin, ev, e); }
  }
}

/* a freshly-mounted surface gets the sticky live cache replayed to ITS OWN
   listeners — dock panes and settings pages share the rule */
function pluginReplayLive(plugin, label) {
  for (const l of PLUGIN_EVENTS.slice()) {
    if (l.plugin === plugin && l.ev === 'live')
      for (const ev of Object.values(PLUGIN_LAST_LIVE)) { try { l.fn(ev); } catch (e) { console.warn('plugin replay', label, e); } }
  }
}

/* Dock panes mount lazily on first activation — connection.js's dockTab
   calls this with the pane name it just showed. After a successful mount
   the pane's own 'live' listeners get the cached latest frames, so a pane
   opened mid-session renders current state, not a blank. */
function pluginDockActivated(name) {
  const entry = PLUGIN_DOCK.get(name);
  if (!entry || entry.mounted) return;
  try {
    entry.slot.mount($(`.dock-pane[data-pane="${name}"] .plg-body`), entry.host);
    entry.mounted = true; // only on success — a throwing mount may retry
    pluginReplayLive(entry.plugin, name);
  } catch (e) { console.warn('plugin mount failed', name, e); }
}

/* settings.js calls this after a plugin page's render() — same sticky
   replay, so a page opened mid-session sees current frames too */
function pluginPageMounted(page) {
  const entry = PLUGIN_PAGES.get(page);
  if (entry) pluginReplayLive(entry.plugin, page);
}
window.pluginPageMounted = pluginPageMounted;

function pluginUnregister(id) {
  for (const [k, v] of PLUGIN_DOCK) if (v.plugin === id) PLUGIN_DOCK.delete(k);
  for (const [k, v] of PLUGIN_PAGES) if (v.plugin === id) PLUGIN_PAGES.delete(k);
  for (const [k, v] of PLUGIN_COMPOSER) if (v.plugin === id) PLUGIN_COMPOSER.delete(k);
  for (let i = PLUGIN_PAL.length - 1; i >= 0; i--) if (PLUGIN_PAL[i].plugin === id) PLUGIN_PAL.splice(i, 1);
  for (let i = PLUGIN_MENUS.length - 1; i >= 0; i--) if (PLUGIN_MENUS[i].plugin === id) PLUGIN_MENUS.splice(i, 1);
  for (let i = PLUGIN_TRANSFORM.length - 1; i >= 0; i--) if (PLUGIN_TRANSFORM[i].plugin === id) PLUGIN_TRANSFORM.splice(i, 1);
  syncPalItems();
  for (const [k, v] of Object.entries(PANE_META)) if (v.plg === id) delete PANE_META[k];
  for (const [k, v] of Object.entries(PAGES)) if (v.plg === id) delete PAGES[k];
  for (let i = PLUGIN_EVENTS.length - 1; i >= 0; i--) if (PLUGIN_EVENTS[i].plugin === id) PLUGIN_EVENTS.splice(i, 1);
  $$('.cmp-bar .plg-cb').forEach(el => { if (!PLUGIN_COMPOSER.has(el.dataset.pcb)) el.remove(); });
  for (let i = SET_NAV.length - 1; i >= 0; i--) if (SET_NAV[i][3] === id) SET_NAV.splice(i, 1);
  $$('#dock .dock-pane[data-plg]').forEach(el => { if (PLUGIN_DOCK.get(el.dataset.pane)?.plugin !== id && !PLUGIN_DOCK.has(el.dataset.pane)) el.remove(); });
  for (const tabs of Object.values(S.dockTabs || {})) {
    if (Array.isArray(tabs)) for (let i = tabs.length - 1; i >= 0; i--) if (tabs[i].pane?.startsWith(`plg:${id}:`)) tabs.splice(i, 1);
  }
  PLUGIN_IDS.delete(id);
  // the module URL stays cached, but the load ledger must forget it so a
  // re-enable can re-import (with a fresh nonce — see loadPlugins)
  for (const k of PLUGIN_LOADED) if (k.endsWith(`:${id}.js`)) PLUGIN_LOADED.delete(k);
  // leave no cursor pointing at a removed surface — the active dock tab
  // falls back to overview, an open plugin settings page to appearance
  if ($('#dock').dataset.tab?.startsWith(`plg:${id}:`)) dockTab('overview');
  if (typeof setPage !== 'undefined' && setPage.startsWith(`plg:${id}:`)) {
    setPage = 'appearance';
    if (view === 'settings') settingsPage('appearance');
  }
  renderDockTabs?.();
  save?.(); // persist — otherwise ghost tabs resurrect at next boot
}

/* The one door in — a plugin module calls this exactly once. `spec.id`
   must equal the served filename minus `.js`: the settings page toggles
   by filename, so the file↔plugin link has to be derivable both ways. */
let PLUGIN_IMPORTING = '';
window.sunmao = {
  version: 1,
  register(spec) {
    // an idempotent re-import (session switch re-ran the loader) is not an
    // error — the registries are project-global for v1, so just no-op
    if (spec && PLUGIN_IDS.has(spec.id)) return;
    if (!spec || spec.id !== PLUGIN_IMPORTING) {
      return console.warn('sunmao.register: rejected spec', spec && spec.id);
    }
    // slot ids interpolate into data-pane/attr selectors — keep them boring
    const slotOk = s => s && /^[\w-]+$/.test(s.id) && typeof s.title === 'string';
    const docks = (spec.slots?.dock || []).filter(slotOk);
    const pages = (spec.slots?.settings || []).filter(slotOk);
    PLUGIN_IDS.add(spec.id);
    const host = pluginHost(spec.id);
    try {
      for (const s of docks) {
        const pane = `plg:${spec.id}:${s.id}`;
        PLUGIN_DOCK.set(pane, { slot: s, plugin: spec.id, host, mounted: false });
        PANE_META[pane] = { t: s.title, i: s.icon || 'blocks', plg: spec.id };
        append($('#dock'), `<div class="dock-pane" data-pane="${pane}" data-plg hidden><div class="plg-body"></div></div>`);
        // a persisted tab may already name this pane — reveal it now that
        // the element exists, not after the user's next click
        if ($('#dock').dataset.tab === pane) dockTab(pane);
      }
      for (const s of pages) {
        const page = `plg:${spec.id}:${s.id}`;
        PLUGIN_PAGES.set(page, { slot: s, plugin: spec.id, host });
        PAGES[page] = () => `<div class="plg-host" data-plgpage="${page}"></div>`;
        PAGES[page].plg = spec.id;
        SET_NAV.push([page, s.title, s.icon || 'blocks', spec.id]);
      }
      // composer chips land in .cmp-bar ahead of the spacer — left group is
      // affordances (attach/mode), right group is pickers + send, so plugin
      // chips belong with the affordances. A chip is valid when it has a
      // boring id plus a click surface (onClick or popover) — no title req.
      const chipOk = s => s && /^[\w-]+$/.test(s.id) && (typeof s.onClick === 'function' || typeof s.popover === 'function');
      for (const s of (spec.slots?.composer || []).filter(chipOk)) {
        const key = `plg:${spec.id}:${s.id}`;
        PLUGIN_COMPOSER.set(key, { slot: s, plugin: spec.id, host });
        const sp = $('.cmp-bar .sp');
        if (sp) sp.insertAdjacentHTML('beforebegin',
          `<button class="cb plg-cb" data-act="plg-cb" data-pcb="${esc(key)}"${s.tip ? ` data-tip="${esc(s.tip)}"` : ''}>${s.icon ? ic(s.icon) : ''}${s.label ? `<span>${esc(s.label)}</span>` : ''}</button>`);
      }
      // palette rows and session-menu items are pure data — the surfaces
      // pull them in at render time, nothing mounts eagerly
      for (const s of (spec.slots?.palette || []))
        if (s && typeof s.t === 'string' && typeof s.run === 'function')
          PLUGIN_PAL.push({ g: s.g || spec.id, t: s.t, i: s.i || 'blocks', k: s.k, sub: s.sub, run: s.run, plugin: spec.id, host });
      syncPalItems();
      for (const s of (spec.slots?.sessionMenu || []))
        if (s && typeof s.v === 'string' && /^[\w-]+$/.test(s.v) && typeof s.t === 'string' && typeof s.run === 'function')
          PLUGIN_MENUS.push({ v: `${spec.id}:${s.v}`, t: s.t, i: s.i, warn: !!s.warn, run: s.run, plugin: spec.id, host });
    } catch (e) {
      // partial registration is worse than none — un-register so a
      // re-enable can try cleanly instead of finding a half-built pane
      pluginUnregister(spec.id);
      return console.warn('sunmao.register: spec failed mid-setup', spec.id, e);
    }
    renderDockTabs?.();
  },
};

/* Loader — fetch the session project's catalog, import each enabled,
   un-tampered module once per (name, session) pair. Runs serialize on a
   promise chain: PLUGIN_IMPORTING is a single slot, and overlapping calls
   (hello + replay + a toggle) would let one import claim another's name.
   Each queued run refetches the catalog, so ordering is self-healing. */
const PLUGIN_LOADED = new Set();
let PLUGIN_SEQ = 0;
let PLUGIN_LOAD_P = Promise.resolve();
function loadPlugins() {
  PLUGIN_LOAD_P = PLUGIN_LOAD_P.then(loadPluginsNow).catch(() => {});
  return PLUGIN_LOAD_P;
}
async function loadPluginsNow() {
  if (!sessionId) return;
  let list;
  try { list = (await api('/plugins?sess=' + encodeURIComponent(sessionId))).plugins || []; }
  catch { return; }
  // the LIVE set is enabled + un-tampered — a disabled or tampered plugin
  // tears down now, not at next reload (the ledger already says "stopped"),
  // and a session switch to a project lacking it unregisters likewise
  const live = new Set(list.filter(p => p.enabled && !p.tampered).map(p => p.name.replace(/\.js$/, '')));
  for (const id of [...PLUGIN_IDS]) if (!live.has(id)) {
    pluginUnregister(id);
    // a burned PLUGIN_LOADED key must go with it — else a later re-enable
    // finds the key taken and never re-imports (the surfaces stay dead)
    for (const k of [...PLUGIN_LOADED]) if (k.endsWith(`:${id}.js`)) PLUGIN_LOADED.delete(k);
  }
  for (const p of list.filter(p => p.enabled && !p.tampered)) {
    const key = `${sessionId}:${p.name}`;
    if (PLUGIN_LOADED.has(key)) continue;
    PLUGIN_IMPORTING = p.name.replace(/\.js$/, '');
    // &r= busts the module cache: a disable→enable cycle re-executes the
    // file so register() runs again (PLUGIN_IDS dedups same-session repeats)
    try {
      await import(`/plugins/${encodeURIComponent(p.name)}?sess=${encodeURIComponent(sessionId)}&r=${PLUGIN_SEQ++}`);
      PLUGIN_LOADED.add(key); // only a landed import burns the key
    } catch (e) { console.warn('plugin load failed', p.name, e); }
    finally { PLUGIN_IMPORTING = ''; }
  }
  // the persisted tab may name a plg pane that only now exists — re-run
  // the activator so its lazy mount fires at boot, not on the next click
  const cur = $('#dock').dataset.tab;
  if (cur?.startsWith('plg:')) dockTab(cur);
}
