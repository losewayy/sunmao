// replay_parity.mjs — Node driver for the TUI↔GUI replay-equivalence test.
// Not shipped: only serve/assets/index.html is embedded into the binary.
//
//   node replay_parity.mjs <path-to-index.html> <events.jsonl>
//
// The HTML's <script> files are eval'd against a hand-rolled fake DOM, then
// `window.__sunmao.renderReplay(events)` folds the session log into it.
// The driver walks the resulting transcript plus the event log and prints
// one canonical line per entry; tui/replay_parity.rs diffs these lines
// against the TUI fold's own canonical emission.
//
// Canonical format (mirrored by the Rust emitter — keep them in lockstep):
//   user|<text>
//   assistant|<text>
//   tool|<depth>|<name>|<N>x<state>[,<state>…]   state: ok|err|interrupted
//   artifact|<name>
//   compacted
//   task_done|<id>|<ok|fail>
//   hook|<event>
//   note|<text>          (any note-line without a canonical mapping)
//   step_summary|<n>     (TUI fold-by-cap — must not appear; surfaced on purpose)

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';

const htmlPath = process.argv[2];
const eventsPath = process.argv[3];
if (!htmlPath || !eventsPath) {
  console.error('usage: node replay_parity.mjs <index.html> <events.jsonl>');
  process.exit(2);
}

/* ================= op log =================
   Canonical ordering needs hook events interleaved with transcript entries.
   Hooks only reach EVLOG (never TX), so we record DOM appends and EVLOG
   pushes into one ordered op stream and derive positions from it. */
const OPS = [];
const recAdd = (parent, nodes) => {
  // snapshot entry nodes NOW — children appended later must land at their
  // own append position, not be pulled forward to the container's
  if (connected(parent)) OPS.push({ k: 'add', nodes: nodes.flatMap(entryNodes) });
};
const recClear = el => { if (connected(el)) OPS.push({ k: 'clear', el }); };
const recRemove = el => { if (connected(el)) OPS.push({ k: 'remove', el }); };
function connected(el) {
  for (let p = el; p; p = p.parentElement) if (p === document.body) return true;
  return false;
}

/* ================= html parse ================= */
const VOID = new Set(['area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta', 'source', 'track', 'wbr']);
const decodeEnt = s => s.replace(/&(amp|lt|gt|quot|#39);/g, m => ({ '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"', '&#39;': "'" })[m]);
const TOKENS = /<!--[\s\S]*?-->|<![^>]*>|<\/[a-zA-Z][^>]*>|<[a-zA-Z][^>]*>|[^<]+/g;

function parseHTML(html) {
  const frag = { kind: 'fragment', childNodes: [] };
  const stack = [frag];
  for (const m of html.matchAll(TOKENS)) {
    const t = m[0];
    if (t.startsWith('<!--') || t.startsWith('<!')) continue;
    if (t.startsWith('</')) { if (stack.length > 1) stack.pop(); continue; }
    if (t[0] === '<') {
      const tm = /^<([a-zA-Z][\w-]*)/.exec(t);
      const el = new FakeEl(tm[1]);
      const attrRe = /([\w-]+)(?:="([^"]*)")?/g;
      const rest = t.slice(tm[0].length);
      let am;
      while ((am = attrRe.exec(rest))) {
        el.attrs[am[1].toLowerCase()] = decodeEnt(am[2] ?? '');
      }
      const parent = stack[stack.length - 1];
      parent.childNodes.push(el);
      if (parent instanceof FakeEl) el.parentElement = parent;
      for (const [k, v] of Object.entries(el.attrs)) {
        if (k === 'class') el._classes = v.split(/\s+/).filter(Boolean);
        if (k.startsWith('data-')) el.dataset[camel(k.slice(5))] = v;
      }
      if (!VOID.has(el.tag) && !t.endsWith('/>')) stack.push(el);
    } else {
      stack[stack.length - 1].childNodes.push({ kind: 'text', textContent: decodeEnt(t) });
    }
  }
  for (const kid of frag.childNodes) if (kid instanceof FakeEl) kid.parentElement = null;
  return frag.childNodes;
}

/* ================= selector matching ================= */
function parseCompound(s) {
  const c = { tag: null, id: null, classes: [], attrs: [], pseudo: [] };
  let i = 0;
  const tm = /^([a-zA-Z][\w-]*|\*)/.exec(s);
  if (tm) { if (tm[1] !== '*') c.tag = tm[1].toLowerCase(); i = tm[0].length; }
  while (i < s.length) {
    let m;
    if (s[i] === '.') { m = /^\.([\w-]+)/.exec(s.slice(i)); c.classes.push(m[1]); }
    else if (s[i] === '#') { m = /^#([\w-]+)/.exec(s.slice(i)); c.id = m[1]; }
    else if (s[i] === '[') { m = /^\[([\w-]+)(?:=["']?([^"'\]]+)["']?)?\]/.exec(s.slice(i)); c.attrs.push([m[1], m[2]]); }
    else if (s[i] === ':') { m = /^:([\w-]+)/.exec(s.slice(i)); c.pseudo.push(m[1]); }
    else break;
    if (!m) break;
    i += m[0].length;
  }
  return c;
}
function matchCompound(el, c) {
  if (c.tag && c.tag !== '*' && el.tag !== c.tag) return false;
  if (c.id && el.id !== c.id) return false;
  for (const cl of c.classes) if (!el.classList.contains(cl)) return false;
  for (const [k, v] of c.attrs) {
    const cur = k === 'class' ? el.className : el.attrs[k];
    if (v === undefined) { if (cur === undefined || cur === null) return false; }
    else if (cur !== v) return false;
  }
  for (const p of c.pseudo) {
    if (p === 'last-child') {
      const sibs = el.parentElement ? el.parentElement.children : [];
      if (sibs[sibs.length - 1] !== el) return false;
    } else if (p === 'first-child') {
      const sibs = el.parentElement ? el.parentElement.children : [];
      if (sibs[0] !== el) return false;
    } else return false;
  }
  return true;
}
function matchesSel(el, sel) {
  return sel.split(',').some(g => {
    const chain = g.trim().split(/\s+/).map(parseCompound);
    let cur = el;
    for (let i = chain.length - 1; i >= 0; i--) {
      while (cur && cur !== document && !matchCompound(cur, chain[i])) cur = cur.parentElement;
      if (!cur || cur === document) return false;
      cur = cur.parentElement;
    }
    return true;
  });
}
function qsa(root, sel) {
  const out = [];
  const walk = n => {
    for (const k of n.childNodes) {
      if (!(k instanceof FakeEl)) continue;
      if (matchesSel(k, sel)) out.push(k);
      walk(k);
    }
  };
  walk(root);
  return out;
}

/* ================= fake element ================= */
const absorb = () => new Proxy(function () {}, {
  get(t, k) {
    if (k === Symbol.toPrimitive) return () => 0;
    if (k === 'toString') return () => '';
    if (k === 'valueOf') return () => 0;
    if (k === 'then') return undefined; // not a promise — must not look awaitable
    if (k === Symbol.iterator) return undefined;
    if (k === 'length') return 0;
    return absorb();
  },
  apply() { return absorb(); },
  set() { return true; },
  has() { return true; },
});

class Frag {
  constructor() { this.childNodes = []; }
  get firstElementChild() { return this.childNodes.find(n => n instanceof FakeEl) ?? null; }
}

class FakeEl {
  constructor(tag) {
    this.tag = String(tag).toLowerCase();
    this.childNodes = [];
    this.parentElement = null;
    this.attrs = {};
    this.dataset = {};
    this.style = { setProperty() {} };
    this.value = '';
    this.hidden = false;
    this._classes = [];
    if (this.tag === 'template') this.content = new Frag();
  }
  get children() { return this.childNodes.filter(n => n instanceof FakeEl); }
  get firstElementChild() { return this.children[0] ?? null; }
  get lastElementChild() { const c = this.children; return c[c.length - 1] ?? null; }
  get nextElementSibling() {
    if (!this.parentElement) return null;
    const s = this.parentElement.children, i = s.indexOf(this);
    return s[i + 1] ?? null;
  }
  get id() { return this.attrs.id || ''; }
  get className() { return this._classes.join(' '); }
  set className(v) { this._classes = String(v).split(/\s+/).filter(Boolean); }
  get classList() {
    const el = this;
    return {
      add: (...cs) => { for (const c of cs) if (c && !el._classes.includes(c)) el._classes.push(c); },
      remove: (...cs) => { el._classes = el._classes.filter(c => !cs.includes(c)); },
      toggle: (c, force) => {
        const on = force === undefined ? !el._classes.includes(c) : !!force;
        if (on) el.classList.add(c); else el.classList.remove(c);
        return on;
      },
      contains: c => el._classes.includes(c),
    };
  }
  get innerHTML() { return serializeKids(this); }
  set innerHTML(v) {
    recClear(this);
    this.childNodes = [];
    const nodes = parseHTML(String(v));
    for (const n of nodes) if (n instanceof FakeEl) n.parentElement = this;
    this.childNodes = nodes;
    if (this.tag === 'template') this.content.childNodes = nodes;
    recAdd(this, nodes);
  }
  get textContent() {
    return this.childNodes.map(n => n instanceof FakeEl ? n.textContent : n.textContent).join('');
  }
  set textContent(v) { this.childNodes = [{ kind: 'text', textContent: String(v) }]; }
  // approximate innerText: block elements terminate lines
  get innerText() { return innerTextOf(this); }
  get outerHTML() { return serializeEl(this); }
  set outerHTML(v) {
    const nodes = parseHTML(String(v));
    const p = this.parentElement;
    if (p) {
      const i = p.childNodes.indexOf(this);
      for (const n of nodes) if (n instanceof FakeEl) n.parentElement = p;
      p.childNodes.splice(i, 1, ...nodes);
      this.parentElement = null;
    }
    recRemove(this);
    if (p) recAdd(p, nodes);
  }
  get offsetHeight() { return 0; }
  get offsetWidth() { return 0; }
  get offsetTop() { return 0; }
  setAttribute(k, v) {
    k = String(k); v = String(v);
    this.attrs[k] = v;
    if (k.startsWith('data-')) this.dataset[camel(k.slice(5))] = v;
  }
  getAttribute(k) {
    if (this.attrs[k] !== undefined) return this.attrs[k];
    if (k.startsWith('data-')) return this.dataset[camel(k.slice(5))] ?? null;
    return null;
  }
  removeAttribute(k) { delete this.attrs[k]; }
  hasAttribute(k) { return this.getAttribute(k) !== null && this.getAttribute(k) !== undefined; }
  appendChild(node) {
    if (node.childNodes && !(node instanceof FakeEl)) {
      // DocumentFragment-ish (template.content): append moves the children
      const kids = [...node.childNodes];
      for (const n of kids) this.appendChild(n);
      node.childNodes = [];
      return node;
    }
    if (node.parentElement) {
      const i = node.parentElement.childNodes.indexOf(node);
      if (i >= 0) node.parentElement.childNodes.splice(i, 1);
    }
    if (node instanceof FakeEl) node.parentElement = this;
    this.childNodes.push(node);
    recAdd(this, [node]);
    return node;
  }
  insertAdjacentHTML(pos, html) {
    const nodes = parseHTML(html);
    if (pos === 'beforeend' || pos === 'afterbegin' || pos === 'beforebegin' || pos === 'afterend') {
      for (const n of nodes) {
        if (n instanceof FakeEl) n.parentElement = this;
        this.childNodes.push(n);
      }
      recAdd(this, nodes);
    }
  }
  remove() {
    if (!this.parentElement) return;
    const i = this.parentElement.childNodes.indexOf(this);
    if (i >= 0) this.parentElement.childNodes.splice(i, 1);
    recRemove(this);
    this.parentElement = null;
  }
  contains(el) { for (let p = el; p; p = p.parentElement) if (p === this) return true; return false; }
  closest(sel) { for (let p = this; p && p !== document; p = p.parentElement) if (matchesSel(p, sel)) return p; return null; }
  matches(sel) { return matchesSel(this, sel); }
  querySelector(sel) { return qsa(this, sel)[0] ?? null; }
  querySelectorAll(sel) { return qsa(this, sel); }
  getBoundingClientRect() { return { left: 0, right: 0, top: 0, bottom: 0, width: 0, height: 0 }; }
  addEventListener() {}
  removeEventListener() {}
  focus() {}
  blur() {}
  click() {}
  animate() { return {}; }
  getContext() { return absorb(); }
  toDataURL() { return ''; }
}
const camel = s => s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());

const BLOCK_TAGS = new Set(['p', 'div', 'br', 'li', 'ul', 'ol', 'pre', 'h1', 'h2', 'h3', 'h4', 'blockquote', 'hr', 'time', 'button']);
function innerTextOf(el) {
  let out = '';
  const walk = n => {
    if (!(n instanceof FakeEl)) { out += n.textContent; return; }
    const before = out.length;
    for (const k of n.childNodes) walk(k);
    if (BLOCK_TAGS.has(n.tag) && out.length > before && !out.endsWith('\n')) out += '\n';
    else if (BLOCK_TAGS.has(n.tag) && before === 0) { /* leading empty block — nothing to end */ }
  };
  walk(el);
  return out;
}
function escAttr(s) { return String(s).replace(/&/g, '&amp;').replace(/"/g, '&quot;'); }
function escText(s) { return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
function serializeKids(el) { return el.childNodes.map(serializeEl).join(''); }
function serializeEl(n) {
  if (!(n instanceof FakeEl)) return escText(n.textContent);
  const at = Object.entries(n.attrs).map(([k, v]) => ` ${k}="${escAttr(v)}"`).join('');
  if (VOID.has(n.tag)) return `<${n.tag}${at}>`;
  return `<${n.tag}${at}>${serializeKids(n)}</${n.tag}>`;
}

/* ================= fake document / globals ================= */
const byId = new Map();
function elForId(id) {
  if (!byId.has(id)) {
    const el = new FakeEl('div');
    el.attrs.id = id;
    el.parentElement = document.body;
    byId.set(id, el);
  }
  return byId.get(id);
}
const document_ = {
  documentElement: new FakeEl('html'),
  body: new FakeEl('body'),
  createElement: t => new FakeEl(t),
  createTextNode: t => ({ kind: 'text', textContent: String(t) }),
  getElementById: id => elForId(id),
  querySelector(sel) {
    sel = String(sel).trim();
    if (sel.startsWith('#') && !/[\s.[:]/.test(sel.slice(1))) return elForId(sel.slice(1));
    const m = /^#([\w-]+)\s+(.+)$/.exec(sel);
    if (m) return elForId(m[1]).querySelector(m[2]);
    return this.body.querySelector(sel);
  },
  querySelectorAll(sel) {
    sel = String(sel).trim();
    if (sel.startsWith('#') && !/[\s.[:]/.test(sel.slice(1))) { const el = byId.get(sel.slice(1)); return el ? [el] : []; }
    const m = /^#([\w-]+)\s+(.+)$/.exec(sel);
    if (m) return elForId(m[1]).querySelectorAll(m[2]);
    return this.body.querySelectorAll(sel);
  },
  addEventListener() {},
  contains(el) { return this.body.contains(el); },
};
const document = document_;

const fakeTimers = {
  setTimeout: (fn, ms) => { const t = globalThis.setTimeout(fn, ms); t.unref?.(); return t; },
  clearTimeout: t => globalThis.clearTimeout(t),
  setInterval: () => 0,
  clearInterval: () => {},
};
const windowObj = { addEventListener() {}, removeEventListener() {} };
const RealURL = URL;
class FakeURL extends RealURL {
  static createObjectURL() { return 'blob:'; }
  static revokeObjectURL() {}
}
const globals = {
  window: windowObj,
  document: document_,
  localStorage: { _m: new Map(), getItem(k) { return this._m.get(k) ?? null; }, setItem(k, v) { this._m.set(k, String(v)); }, removeItem(k) { this._m.delete(k); } },
  matchMedia: () => ({ matches: false, media: '', addEventListener() {}, addListener() {} }),
  WebSocket: class { send() {} close() {} },
  fetch: () => Promise.resolve({ ok: true, status: 200, json: async () => ({}), text: async () => '' }),
  location: { host: '127.0.0.1:3000', href: 'http://127.0.0.1:3000/', pathname: '/' },
  performance: { now: () => 0 },
  requestAnimationFrame: () => 0,
  navigator: { clipboard: { writeText: () => Promise.resolve() } },
  Image: class { set src(v) {} },
  ResizeObserver: class { observe() {} unobserve() {} disconnect() {} },
  MutationObserver: class { observe() {} disconnect() {} },
  URL: FakeURL,
  open: () => null,
  getComputedStyle: () => ({ getPropertyValue: () => '' }),
  innerWidth: 1440,
  innerHeight: 900,
  devicePixelRatio: 1,
  addEventListener: () => {},
  ...fakeTimers,
};

/* ================= eval the page scripts =================
   index.html's scripts now live in external files (state.js … boot.js).
   Walk every <script> tag in document order: an inline body contributes
   itself, a src="x.js" contributes the file's bytes (resolved against the
   page's own directory, not the cwd). All sources concatenate into one
   Function scope — the same shared lexical surface the browser gives
   classic scripts. */
const html = readFileSync(htmlPath, 'utf8');
const scripts = [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)];
if (!scripts.length) { console.error('no <script> block found'); process.exit(2); }
const srcs = [];
for (const [, attrs, body] of scripts) {
  const m = /\bsrc\s*=\s*["']([^"']+)["']/.exec(attrs);
  srcs.push(m ? readFileSync(join(dirname(htmlPath), m[1]), 'utf8') : body);
}
const src = srcs.join('\n');

const names = Object.keys(globals);
const fn = new Function(...names, src);
fn(...names.map(n => globals[n]));

const sun = windowObj.__sunmao;
if (!sun || !sun.renderReplay) { console.error('window.__sunmao missing'); process.exit(2); }

// interleave EVLOG pushes into the op stream so hook rows land where the
// fold emitted them (EVLOG is session-scoped — compaction clears it too)
const EV = sun.EVLOG;
const realPush = Array.prototype.push;
EV.push = function (...items) {
  for (const it of items) OPS.push({ k: 'evlog', type: it[1], detail: it[2] });
  return realPush.apply(this, items);
};

/* ================= run the fold ================= */
const events = readFileSync(eventsPath, 'utf8')
  .split('\n').map(l => l.trim()).filter(Boolean).map(l => JSON.parse(l));
sun.renderReplay(events);
const TX_EL = elForId('tx');

/* ================= canonical transcript ================= */
// entry kinds: .msg.you / bare .bubble / .tool / .island / .note-line / .notice
function entryKind(el) {
  if (el.classList.contains('msg') && el.classList.contains('you')) return 'user';
  // .msg.you also has .bubble inside — the wrapper wins; a bare .bubble is
  // assistant text (live inside .msg.bot or replayed at top level)
  if (el.classList.contains('bubble')) return 'assistant';
  if (el.classList.contains('tool')) return 'tool';
  if (el.classList.contains('island')) return 'island';
  if (el.classList.contains('note-line')) return 'note';
  if (el.classList.contains('notice')) return 'notice';
  return null;
}
function entryNodes(node) {
  if (node instanceof FakeEl) {
    if (entryKind(node)) return [node];
    return node.children.flatMap(entryNodes);
  }
  return [];
}
function under(el, anc) {
  for (let p = el; p; p = p.parentElement) if (p === anc) return true;
  return false;
}

// replay the op stream into an ordered transcript. Entries and hook
// markers live in parallel lists: a hook records how many transcript
// entries preceded it (`after`), since it never becomes a DOM node. A
// TX-level clear (compaction) drops hooks together with the entries — the
// TUI's blocks.clear() wipes audit rows the same way.
const V = [];        // surviving entry nodes, append order
const HOOKS = [];    // { after, detail } — after = entries preceding it
for (const op of OPS) {
  if (op.k === 'add') {
    V.push(...op.nodes);
  } else if (op.k === 'clear') {
    const el = op.el;
    if (el === TX_EL) { V.length = 0; HOOKS.length = 0; }
    else for (let i = V.length - 1; i >= 0; i--) {
      if (under(V[i], el) || V[i] === el) V.splice(i, 1);
    }
  } else if (op.k === 'remove') {
    const el = op.el;
    for (let i = V.length - 1; i >= 0; i--) {
      if (under(V[i], el) || V[i] === el) V.splice(i, 1);
    }
  } else if (op.k === 'evlog' && op.type === 'hook') {
    HOOKS.push({ after: V.length, detail: op.detail || '' });
  }
}

// interleave: at boundary i (after i entries), emit hooks recorded there
const seq = [];
for (let i = 0; i <= V.length; i++) {
  for (const h of HOOKS.filter(h => h.after === i)) seq.push(h);
  if (i < V.length) seq.push(V[i]);
}

const ws = s => String(s).replace(/\s+/g, ' ').trim();
function textOf(el, sel) { const t = el.querySelector(sel); return t ? t.innerText : ''; }
function toolLine(el) {
  const nmRaw = textOf(el, '.nm');
  const depth = nmRaw.startsWith('↳ ') ? 1 : 0;
  let name = nmRaw.replace(/^↳ /, '');
  if (name === '!') name = 'shell';
  const stEl = el.querySelector('.st');
  let state = 'run';
  if (stEl) {
    if (stEl.classList.contains('ok')) state = 'ok';
    else if (stEl.classList.contains('err')) state = 'err';
  }
  if (ws(textOf(el, '.tm')) === '已中断') state = 'interrupted';
  return { depth, name, state };
}

const lines = [];
for (const item of seq) {
  if (item.after !== undefined) {
    lines.push('hook|' + ws(item.detail.split(' · ')[0]));
    continue;
  }
  const el = item;
  switch (entryKind(el)) {
    case 'user': lines.push('user|' + ws(textOf(el, '.bubble'))); break;
    case 'assistant': lines.push('assistant|' + ws(el.innerText)); break;
    case 'tool': {
      const t = toolLine(el);
      const last = lines[lines.length - 1];
      if (typeof last === 'object' && last.g.depth === t.depth && last.g.name === t.name) {
        last.g.count++; last.g.states.push(t.state);
      } else {
        lines.push({
          g: { depth: t.depth, name: t.name, count: 1, states: [t.state] },
          toString() {
            const g = this.g;
            const states = g.states.every(s => s === g.states[0]) ? g.states[0] : g.states.join(',');
            return `tool|${g.depth}|${g.name}|${g.count}x${states}`;
          },
        });
      }
      break;
    }
    case 'island': lines.push('artifact|' + ws(el.dataset.artifact || '')); break;
    case 'note': {
      const text = ws(el.innerText);
      if (text.startsWith('[context compacted]')) lines.push('compacted');
      else if (text.startsWith('session started')) break; // chrome, not transcript
      else if (text.startsWith('task list:')) break; // durable state, not transcript
      else lines.push('note|' + text);
      break;
    }
    case 'notice': {
      const id = ws(textOf(el, 'code'));
      const ok = /完成/.test(el.innerText);
      lines.push(`task_done|${id}|${ok ? 'ok' : 'fail'}`);
      break;
    }
    default: lines.push('note|unclassified ' + el.className); break;
  }
}

console.log(lines.map(l => String(l)).join('\n'));
