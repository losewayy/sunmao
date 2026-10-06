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
const PLUGIN_DOCK = new Map();   // 'plg:<id>'  → {slot, plugin, host, mounted}
// const bindings don't reach window — builtin consumers (settings page
// dispatch) read it through this explicit handle instead
window.PLUGIN_PAGES = new Map(); // 'plg:<id>'  → {slot, plugin, host}
const PLUGIN_PAGES = window.PLUGIN_PAGES;
const PLUGIN_EVENTS = [];        // {plugin, ev, fn} — 'live' frames, 'session' switches
const PLUGIN_IDS = new Set();

const pluginHost = plugin => ({
  plugin,
  /* REST through the app's own transport, session-tagged for the viewed
     session. An enabled plugin IS trusted page code — the facade scopes
     intent, it is NOT an enforcement boundary (same-origin fetch exists);
     the pinned ledger is the trust control, isolation is the v2 iframe. */
  api: (path, opts) => api(`${path}${path.includes('?') ? '&' : '?'}sess=${encodeURIComponent(sessionId)}`, opts),
  t, esc, ic, toast,
  sess: () => sessionId,
  on: (ev, fn) => PLUGIN_EVENTS.push({ plugin, ev, fn }),
});

function pluginEmit(ev, data) {
  for (const l of PLUGIN_EVENTS.slice()) {
    if (l.ev !== ev) continue;
    try { l.fn(data); } catch (e) { console.warn('plugin', l.plugin, ev, e); }
  }
}

/* Dock panes mount lazily on first activation — connection.js's dockTab
   calls this with the pane name it just showed. */
function pluginDockActivated(name) {
  const entry = PLUGIN_DOCK.get(name);
  if (!entry || entry.mounted) return;
  try {
    entry.slot.mount($(`.dock-pane[data-pane="${name}"] .plg-body`), entry.host);
    entry.mounted = true; // only on success — a throwing mount may retry
  } catch (e) { console.warn('plugin mount failed', name, e); }
}

function pluginUnregister(id) {
  for (const [k, v] of PLUGIN_DOCK) if (v.plugin === id) PLUGIN_DOCK.delete(k);
  for (const [k, v] of PLUGIN_PAGES) if (v.plugin === id) PLUGIN_PAGES.delete(k);
  for (const [k, v] of Object.entries(PANE_META)) if (v.plg === id) delete PANE_META[k];
  for (const [k, v] of Object.entries(PAGES)) if (v.plg === id) delete PAGES[k];
  for (let i = PLUGIN_EVENTS.length - 1; i >= 0; i--) if (PLUGIN_EVENTS[i].plugin === id) PLUGIN_EVENTS.splice(i, 1);
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
  // a session switch may land on a DIFFERENT project — a plugin absent
  // from its catalog unregisters, so panes/pages/listeners from the old
  // project can't keep watching the new session's frames
  const names = new Set(list.map(p => p.name.replace(/\.js$/, '')));
  for (const id of [...PLUGIN_IDS]) if (!names.has(id)) pluginUnregister(id);
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
