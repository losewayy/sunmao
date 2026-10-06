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
  /* REST through the app's own transport, scoped to the viewed session —
     the plugin sees its session's project, never the launch dir by
     accident. Raw fetch is deliberately not handed out. */
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
  entry.mounted = true;
  try { entry.slot.mount($(`.dock-pane[data-pane="${name}"] .plg-body`), entry.host); }
  catch (e) { console.warn('plugin mount failed', name, e); }
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
  renderDockTabs?.();
}

/* The one door in — a plugin module calls this exactly once. `spec.id`
   must equal the served filename minus `.js`: the settings page toggles
   by filename, so the file↔plugin link has to be derivable both ways. */
let PLUGIN_IMPORTING = '';
window.sunmao = {
  version: 1,
  register(spec) {
    if (!spec || spec.id !== PLUGIN_IMPORTING || PLUGIN_IDS.has(spec.id)) {
      return console.warn('sunmao.register: rejected spec', spec && spec.id);
    }
    PLUGIN_IDS.add(spec.id);
    const host = pluginHost(spec.id);
    for (const s of spec.slots?.dock || []) {
      const pane = `plg:${spec.id}:${s.id}`;
      PLUGIN_DOCK.set(pane, { slot: s, plugin: spec.id, host, mounted: false });
      PANE_META[pane] = { t: s.title, i: s.icon || 'blocks', plg: spec.id };
      append($('#dock'), `<div class="dock-pane" data-pane="${pane}" data-plg hidden><div class="plg-body"></div></div>`);
    }
    for (const s of spec.slots?.settings || []) {
      const page = `plg:${spec.id}:${s.id}`;
      PLUGIN_PAGES.set(page, { slot: s, plugin: spec.id, host });
      PAGES[page] = () => `<div class="plg-host" data-plgpage="${page}"></div>`;
      PAGES[page].plg = spec.id;
      SET_NAV.push([page, s.title, s.icon || 'blocks', spec.id]);
    }
    renderDockTabs?.();
  },
};

/* Loader — fetch the session project's catalog, import each enabled,
   un-tampered module once per (name, session) pair. */
const PLUGIN_LOADED = new Set();
let PLUGIN_SEQ = 0;
async function loadPlugins() {
  if (!sessionId) return;
  let list;
  try { list = (await api('/plugins?sess=' + encodeURIComponent(sessionId))).plugins || []; }
  catch { return; }
  for (const p of list.filter(p => p.enabled && !p.tampered)) {
    const key = `${sessionId}:${p.name}`;
    if (PLUGIN_LOADED.has(key)) continue;
    PLUGIN_LOADED.add(key);
    PLUGIN_IMPORTING = p.name.replace(/\.js$/, '');
    // &r= busts the module cache: a disable→enable cycle re-executes the
    // file so register() runs again (PLUGIN_IDS dedups same-session repeats)
    try { await import(`/plugins/${encodeURIComponent(p.name)}?sess=${encodeURIComponent(sessionId)}&r=${PLUGIN_SEQ++}`); }
    catch (e) { console.warn('plugin load failed', p.name, e); }
    finally { PLUGIN_IMPORTING = ''; }
  }
}
