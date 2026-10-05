/* dom helpers, motion bridge, Tauri shell detection, ui state */
'use strict';

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const ic = (n, c = 'i') => `<svg class="${c}" aria-hidden="true"><use href="#i-${n}"/></svg>`;
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const nf = n => Math.round(n).toLocaleString('en-US');
const clone = o => JSON.parse(JSON.stringify(o));
const pad = n => String(n).padStart(2, '0');
const root = document.documentElement, app = $('#app');
/* motion — the only way JS touches timing; values come from tokens.css */
const motion = (() => {
  const FALLBACK = { instant: 80, fast: 140, base: 200, slow: 320, scene: 560 };
  const css = n => getComputedStyle(root).getPropertyValue(n).trim();
  const ms = (n, d) => { const v = css(n); return v ? parseFloat(v) * (v.endsWith('ms') ? 1 : 1000) : d; };
  return {
    dur: k => ms(`--dur-${k}`, FALLBACK[k]),
    ease: k => css(`--ease-${k}`) || 'ease-out',
    hold: k => ms(`--hold-${k}`, 2600),
    delay: k => ms(`--delay-${k}`, 400),
    // length tokens JS needs verbatim (input heights, pop widths)
    px: (n, d = 0) => { const v = parseFloat(css(n)); return isNaN(v) ? d : v; },
    reduced: () => root.dataset.motionEff === 'reduce',
    wait: k => new Promise(r => setTimeout(r, motion.dur(k))),
    // one-shot WAAPI height tween (overflow clipped for the run); resolves
    // when finished — callers can chain the display flip off it
    async height(el, from, to, k = 'slow') {
      if (motion.reduced() || !el.animate) return;
      const prev = el.style.overflow; el.style.overflow = 'hidden';
      try { await el.animate([{ height: from + 'px' }, { height: to + 'px' }], { duration: motion.dur(k), easing: motion.ease('out') }).finished; } catch {}
      el.style.overflow = prev;
    },
    // expand/collapse a class-toggled body (tool rows, think blocks, tool
    // groups): open = add class then tween 0→h; close = tween h→0 then the
    // class comes off (which is what display:none's the body)
    async fold(container, body, open, cls = 'open') {
      if (container.classList.contains(cls) === open) return;
      if (motion.reduced()) { container.classList.toggle(cls, open); return; }
      if (open) { container.classList.add(cls); await motion.height(body, 0, body.offsetHeight); }
      else { await motion.height(body, body.offsetHeight, 0); container.classList.remove(cls); }
    },
  };
})();
// fade an element out, swap its contents, fade back — session/view rebuilds
async function fadeSwap(el, fn) {
  if (!el || motion.reduced()) return fn();
  el.style.transition = `opacity ${motion.dur('fast')}ms ${motion.ease('in')}`;
  el.style.opacity = '0';
  await motion.wait('fast');
  fn();
  el.style.transition = `opacity ${motion.dur('base')}ms ${motion.ease('out')}`;
  el.style.opacity = '';
  el.addEventListener('transitionend', () => { el.style.transition = ''; }, { once: true });
}
/* debounces aren't motion — named constants, gate exempts the references */
const DEBOUNCE_DATAFLOW = 400, DEBOUNCE_RESIZE = 160, DEBOUNCE_ROSTER = 700, DEBOUNCE_UI_SAVE = 400;
const clock = sec => { const d = new Date(); return pad(d.getHours()) + ':' + pad(d.getMinutes()) + (sec ? ':' + pad(d.getSeconds()) : ''); };
const fmtBytes = n => n >= 1048576 ? (n / 1048576).toFixed(1) + ' MB' : n >= 1024 ? (n / 1024).toFixed(1) + ' KB' : n + ' B';
const hex2rgb = h => { h = h.replace('#', ''); if (h.length === 3) h = [...h].map(c => c + c).join(''); const n = parseInt(h, 16); return [n >> 16 & 255, n >> 8 & 255, n & 255]; };
const rgb2hex = a => '#' + a.map(v => Math.round(Math.max(0, Math.min(255, v))).toString(16).padStart(2, '0')).join('').toUpperCase();
const mix = (a, b, t) => { const A = hex2rgb(a), B = hex2rgb(b); return rgb2hex(A.map((v, i) => v + (B[i] - v) * t)); };

/* Tauri shell — the desktop app injects `__sunmaoShell` (loopback pages
   never get Tauri's own globals), so the page detects it that way and
   unlocks the window chrome: caption buttons + titlebar drag. In a plain
   browser nothing here fires. */
const TAURI = window.__sunmaoShell || null;
const shellWin = () => TAURI;
if (TAURI) {
  document.body.classList.add('shell');
  // frameless drag — the whole 48px band is the drag surface; interactive
  // controls (buttons) opt out so clicks still reach them.
  // Double-press toggles maximize (the Windows caption convention) — it is
  // read off the second mousedown's `detail`, because the first press's
  // native drag loop swallows the dblclick event.
  // Two elements, one handler: `.titlebar` only spans the area right of the
  // rail, so the gaps left of it belong to `.drag-l`. Tauri's own
  // `data-tauri-drag-region` is dead here — it invokes
  // `plugin:window|start_dragging`, and this app deliberately grants no
  // core:window capability (the shell seam is the authority), so the page
  // owns the gesture.
  const bandDrag = e => {
    if (e.button !== 0 || e.target.closest('button,input,a,[contenteditable]')) return;
    e.preventDefault();
    if (e.detail === 2) TAURI.win('max'); else TAURI.drag();
  };
  $$('.titlebar, .drag-l').forEach(el => el.addEventListener('mousedown', bandDrag));
  // the titlebar now catches pointer input — hand wheel scrolls through
  // to whatever scroll surface sits under it
  $('.titlebar').addEventListener('wheel', e => {
    const sc = view === 'settings' ? $('#set-scroll') : $('#scroller');
    sc.scrollBy({ top: e.deltaY, left: 0 });
  }, { passive: true });
  /* The caption's maximize/restore glyph follows the WINDOW, not the last
     click: maximize, Aero snap, and un-snapping by dragging a maximized
     window all have to land on the right icon, so the page asks the shell
     for the real state after every resize (and once at boot, in case the
     window-state plugin restored the window maximized). */
  const capMax = $('#cap-max');
  let winStateT = 0;
  const readWinState = async () => {
    if (!capMax) return;
    let on = false;
    try {
      const s = await TAURI.win('state');
      on = !!(s && s.maximized);
    } catch { return; }
    $('use', capMax).setAttribute('href', on ? '#c-restore' : '#c-max');
    const label = on ? t('还原') : t('最大化');
    capMax.dataset.tip = label;
    capMax.setAttribute('aria-label', label);
  };
  readWinState();
  window.addEventListener('resize', () => {
    clearTimeout(winStateT);
    winStateT = setTimeout(readWinState, DEBOUNCE_RESIZE);
  });
}

/* Zoom — the shell's Ctrl/Cmd+=/-/0 ladder lives on `S.zoom` like every
   other appearance pref: the factor lands in `.sunmao/ui.json` via PUT /ui
   and is re-applied on load (the shim's keydown listener calls into here,
   `setZoom` is the Rust-side absolute setter). Plain browsers keep their
   own page zoom — nothing to persist there. */
const sunmaoZoom = v => {
  if (!TAURI || !TAURI.setZoom) return;
  const cur = S.zoom || 1;
  const f = v === 'in' ? cur * 1.2 : v === 'out' ? cur / 1.2 : v === 'reset' ? 1 : (+v || 1);
  S.zoom = Math.min(5, Math.max(0.2, f)); save();
  TAURI.setZoom(S.zoom);
};
if (TAURI) window.sunmaoZoom = sunmaoZoom;

/* ================= state ================= */
let view = 'session', lastMain = 'session', dockOn = true, railOn = true, setPage = 'appearance';
let sessionId = '', cwd = '', slashList = [], models = [], modelLabel = '', busy = false, connected = false;
let clientId = 0; // hello assigns this tab's id — directed frames name it
// per-session rail state — every host frame carries `sess`; the transcript
// only renders the viewed session, the rail tracks them all
const busySessions = new Set(), waitingSessions = new Set();
let driver = ''; // loop driver the viewed session froze at creation ('' = not a live view yet)
let approvalMode = 'auto'; // kernel-reported stance — the only source of truth
/* approval-mode labels — resolved through t() at render time: `uiLang` only
   settles further down this file, so a load-time map would freeze whichever
   language the previous render already had */
const modeLabel = m => ({
  always_ask: t('请求批准'), auto: t('自动'), read_only: t('只读'), full_access: t('完全访问'),
}[m] || m);
const MODE_ICONS = { always_ask: 'shield-check', auto: 'shield', read_only: 'eye', full_access: 'lock' };
function setApprovalMode(m) {
  if (!m) return;
  approvalMode = m;
  $('#cmp-mode').textContent = modeLabel(m);
  $('#mode-ic').setAttribute('href', '#i-' + (MODE_ICONS[m] || 'shield'));
}
/* reasoning-effort override — kernel truth arrives on hello/replay/effort
   frames; null means "follow the provider's own". effortLevels is the
   active model's advertised vocabulary (empty = catalog never learned it;
   the picker still offers 默认). */
let effortLevel = null, effortLevels = [];
function setEffort(level, levels) {
  effortLevel = level || null;
  if (Array.isArray(levels)) effortLevels = levels;
  $('#cmp-effort').textContent = effortLevel || t('默认');
  $('#effort-btn').classList.toggle('lit', !!effortLevel);
}
let SESSION_IDS = [], SESSION_META = {};
const EVLOG = [];

const DEFAULTS = { mode: 'dark', motion: 'system', accent: '#339CFF', background: '#16181F', foreground: '#E8E9F0', wallpaper: 'graphite', dim: 0.16, panelOpacity: 0.72, blur: 24, translucentSidebar: false, contrast: 50, railGroup: 'time', railFold: {}, loopDriver: '', fonts: { ui: 'HarmonyOS Sans SC', code: 'Maple Mono CN' } };
const INITIAL = Object.assign(clone(DEFAULTS), { wallpaper: 'dusk-ridge', dim: 0.08, panelOpacity: 0.56, translucentSidebar: true, lang: 'auto' });
/* state source of truth: `<project>/.sunmao/ui.json` via GET/PUT /ui;
   localStorage is only a first-frame cache (prevents a flash of defaults
   while the fetch is in flight). A failed fetch keeps the cached state.
 *
 * Both copies are hand-editable text, and one wrong type in either used to
 * throw inside apply() (settings.js does `S.dim.toFixed(3)` and
 * `hex2rgb(S.background)`): at boot.js that took i18n, the shell and
 * connect() down with it, and nothing repaired the cache. healUi() coerces
 * every key the render path reads; a value it cannot repair is dropped so
 * the default takes over, and the caller writes the healed object back so
 * the bad value can never bite twice. */
const UI_NUM = { dim: [0, 1], panelOpacity: [0, 1], blur: [0, 64], contrast: [0, 100], zoom: [0.2, 5] };
const UI_STR = ['mode', 'motion', 'accent', 'background', 'foreground', 'wallpaper', 'lang', 'railGroup', 'loopDriver'];
const uiNum = v => {
  const n = typeof v === 'number' ? v : (typeof v === 'string' && v.trim() !== '' ? Number(v) : NaN);
  return Number.isFinite(n) ? n : null;
};
/* one stored ui object -> a shape the render path can trust, plus the names
   of the keys that had to be coerced or dropped */
function healUi(v) {
  const o = (v && typeof v === 'object' && !Array.isArray(v)) ? Object.assign(clone(INITIAL), v) : clone(INITIAL);
  const bad = [];
  for (const [k, [lo, hi]] of Object.entries(UI_NUM)) {
    if (!(k in o)) continue;
    const n = uiNum(o[k]);
    const fixed = n === null ? null : Math.min(hi, Math.max(lo, n));
    if (fixed === null) { delete o[k]; bad.push(k); }
    else if (fixed !== o[k]) { o[k] = fixed; bad.push(k); }
  }
  for (const k of UI_STR) if (k in o && !(typeof o[k] === 'string' && o[k])) { delete o[k]; bad.push(k); }
  if ('translucentSidebar' in o && typeof o.translucentSidebar !== 'boolean') { delete o.translucentSidebar; bad.push('translucentSidebar'); }
  if ('railFold' in o && (!o.railFold || typeof o.railFold !== 'object' || Array.isArray(o.railFold))) { delete o.railFold; bad.push('railFold'); }
  const fonts = (o.fonts && typeof o.fonts === 'object' && !Array.isArray(o.fonts)) ? o.fonts : {};
  o.fonts = Object.assign({}, INITIAL.fonts);
  for (const k of ['ui', 'code']) {
    if (typeof fonts[k] === 'string' && fonts[k]) o.fonts[k] = fonts[k];
    else if (k in fonts) bad.push('fonts.' + k);
  }
  return { state: o, bad };
}
let S = (() => {
  let cached = null;
  try { cached = JSON.parse(localStorage.getItem('sunmao.ui')); } catch { cached = null; }
  const { state, bad } = healUi(cached);
  const usable = !!(cached && typeof cached === 'object' && !Array.isArray(cached));
  if (cached !== null && cached !== undefined) {
    try {
      if (!usable) localStorage.removeItem('sunmao.ui'); /* not a store at all: ignore the key */
      else if (bad.length) localStorage.setItem('sunmao.ui', JSON.stringify(state));
    } catch {}
    if (bad.length) console.warn('sunmao: repaired ui cache: ' + bad.join(', '));
  }
  return state;
})();
/* the language is resolved here, before anything renders: `auto` follows the
   system locale, and the brand (榫卯 / sunmao) follows the language */
uiLang = detectLang(S.lang);
async function loadUi() {
  try {
    const v = await api('/ui');
    if (v && v.ui) {
      const { state, bad } = healUi(v.ui);
      S = state;
      apply();
      /* the server copy was hand-edited too: put the repaired shape back so
         every later load starts clean, and so the cache and the file agree */
      if (bad.length) {
        console.warn('sunmao: repaired /ui payload: ' + bad.join(', '));
        save();
      }
      if (S.wallpaper === 'custom') loadCustom();
      if (TAURI && TAURI.setZoom) TAURI.setZoom(S.zoom || 1);
      if (typeof dockSessionSwap === 'function') dockSessionSwap();
      // the server copy is authoritative for a client whose cache is empty
      // (or stale): one reload aligns it, and the flag keeps that from
      // looping when the two disagree for another reason
      if (detectLang(S.lang) !== uiLang) {
        if (!sessionStorage.getItem('lang-sync')) {
          sessionStorage.setItem('lang-sync', '1');
          location.reload();
        }
      } else sessionStorage.removeItem('lang-sync');
    }
  } catch {}
}
let uiSaveT = 0;
const save = () => {
  try { localStorage.setItem('sunmao.ui', JSON.stringify(S)); } catch {}
  clearTimeout(uiSaveT);
  uiSaveT = setTimeout(async () => {
    try { await api('/ui', jput(S)); } catch {}
  }, DEBOUNCE_UI_SAVE);
};


