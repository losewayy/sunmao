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
  // frameless drag — the titlebar is the drag surface; interactive
  // controls (buttons) opt out so clicks still reach them.
  // Double-press toggles maximize (the Windows caption convention) — it is
  // read off the second mousedown's `detail`, because the first press's
  // native drag loop swallows the dblclick event.
  $('.titlebar').addEventListener('mousedown', e => {
    if (e.button !== 0 || e.target.closest('button,input,a,[contenteditable]')) return;
    e.preventDefault();
    if (e.detail === 2) TAURI.win('max'); else TAURI.drag();
  });
  // the titlebar now catches pointer input — hand wheel scrolls through
  // to whatever scroll surface sits under it
  $('.titlebar').addEventListener('wheel', e => {
    const sc = view === 'settings' ? $('#set-scroll') : $('#scroller');
    sc.scrollBy({ top: e.deltaY, left: 0 });
  }, { passive: true });
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
const MODE_LABELS = { always_ask: '请求批准', auto: '自动', read_only: '只读', full_access: '完全访问' };
const MODE_ICONS = { always_ask: 'shield-check', auto: 'shield', read_only: 'eye', full_access: 'lock' };
function setApprovalMode(m) {
  if (!m) return;
  approvalMode = m;
  $('#cmp-mode').textContent = MODE_LABELS[m] || m;
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
  $('#cmp-effort').textContent = effortLevel || '默认';
  $('#effort-btn').classList.toggle('lit', !!effortLevel);
}
let SESSION_IDS = [], SESSION_META = {};
const EVLOG = [];

const DEFAULTS = { mode: 'dark', motion: 'system', accent: '#339CFF', background: '#16181F', foreground: '#E8E9F0', wallpaper: 'graphite', dim: 0.16, panelOpacity: 0.72, blur: 24, translucentSidebar: false, contrast: 50, railGroup: 'time', railFold: {}, loopDriver: '', fonts: { ui: 'HarmonyOS Sans SC', code: 'Maple Mono CN' } };
const INITIAL = Object.assign(clone(DEFAULTS), { wallpaper: 'dusk-ridge', dim: 0.08, panelOpacity: 0.56, translucentSidebar: true });
/* state source of truth: `<project>/.sunmao/ui.json` via GET/PUT /ui;
   localStorage is only a first-frame cache (prevents a flash of defaults
   while the fetch is in flight). A failed fetch keeps the cached state. */
const mergeUi = v => (v && typeof v === 'object') ? Object.assign(clone(INITIAL), v, { fonts: Object.assign({}, INITIAL.fonts, v.fonts || {}) }) : clone(INITIAL);
let S = (() => { try { return mergeUi(JSON.parse(localStorage.getItem('sunmao.ui'))); } catch { return clone(INITIAL); } })();
async function loadUi() { try { const v = await api('/ui'); if (v && v.ui) { S = mergeUi(v.ui); apply(); if (S.wallpaper === 'custom') loadCustom(); if (TAURI && TAURI.setZoom) TAURI.setZoom(S.zoom || 1); if (typeof dockSessionSwap === 'function') dockSessionSwap(); } } catch {} }
let uiSaveT = 0;
const save = () => {
  try { localStorage.setItem('sunmao.ui', JSON.stringify(S)); } catch {}
  clearTimeout(uiSaveT);
  uiSaveT = setTimeout(async () => {
    try { await api('/ui', jput(S)); } catch {}
  }, DEBOUNCE_UI_SAVE);
};


