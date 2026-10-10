/* sunmao Native UI: bounded, declarative JSONL patch stream in ```sunmao-ui.
 * The source stays in the assistant message for session replay. No model code,
 * unsafe HTML, remote resources or implicit tool calls execute in this renderer.
 */
'use strict';
const NativeUI = (() => {
  const MAX_BODY = 131072, MAX_PATCHES = 256, MAX_NODES = 96;
  const TYPES = new Set(['card', 'row', 'column', 'text', 'heading', 'metric', 'badge',
    'progress', 'divider', 'slider', 'toggle', 'select', 'input', 'button', 'chart', 'table']);
  const CONTAINERS = new Set(['card', 'row', 'column']);
  const KEY = /^[A-Za-z][A-Za-z0-9_-]{0,63}$/;
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const panels = new WeakMap();
  const surfaces = new Map(); // stable per-session panels; reset before replay
  const pendingValues = new Map(); // events may precede the finished assistant message
  const safeNumber = x => typeof x === 'number' && Number.isFinite(x);
  const safeScalar = x => typeof x === 'boolean' || safeNumber(x) || (typeof x === 'string' && x.length <= 256);

  // Delimiters must occupy a full line, so incomplete fences are still text.
  function parts(source) {
    const chunks = String(source).match(/[^\n]*\n|[^\n]+$/g) || [];
    const result = [];
    let kind = 'md', body = '', fence = '```';
    for (const chunk of chunks) {
      const full = chunk.endsWith('\n');
      const line = chunk.replace(/\r?\n$/, '');
      if (kind === 'md' && full && /^(?:```|~~~)sunmao-ui[ \t]*$/.test(line)) {
        fence = line.startsWith('~~~') ? '~~~' : '```';
        if (body) result.push({ kind, body, closed: true });
        kind = 'ui'; body = '';
      } else if (kind === 'ui' && line.trim() === fence) {
        result.push({ kind, body, closed: true });
        kind = 'md'; body = '';
      } else body += chunk;
    }
    if (body || kind === 'ui') result.push({ kind, body, closed: kind !== 'ui' });
    return result;
  }

  // Deliberately tiny expression language: no eval, property access or calls.
  function value(expr, state, depth = 0) {
    if (depth > 10) return null;
    if (expr === null || typeof expr === 'string' || typeof expr === 'boolean') return expr;
    if (safeNumber(expr)) return expr;
    if (!expr || typeof expr !== 'object' || Array.isArray(expr)) return null;
    if (typeof expr.var === 'string' && KEY.test(expr.var)) return own(state, expr.var) ? state[expr.var] : null;
    if (!Array.isArray(expr.args) || expr.args.length > 12 || !expr.args.length) return null;
    const args = expr.args.map(v => value(v, state, depth + 1));
    const numeric = () => args.every(v => v !== null && v !== '' && Number.isFinite(Number(v)));
    let result;
    switch (expr.op) {
      case 'eq': return args.length === 2 ? args[0] === args[1] : null;
      case 'neq': return args.length === 2 ? args[0] !== args[1] : null;
      case 'and': return args.every(Boolean);
      case 'or': return args.some(Boolean);
      case 'not': return args.length === 1 ? !args[0] : null;
      case 'if': return args.length === 3 ? args[args[0] ? 1 : 2] : null;
      default: break;
    }
    if (!numeric()) return null;
    const a = args.map(Number);
    switch (expr.op) {
      case 'add': result = a.reduce((x, y) => x + y, 0); break;
      case 'mul': result = a.reduce((x, y) => x * y, 1); break;
      case 'sub': result = a.slice(1).reduce((x, y) => x - y, a[0]); break;
      case 'div': result = a.slice(1).reduce((x, y) => x / y, a[0]); break;
      case 'min': result = Math.min(...a); break;
      case 'max': result = Math.max(...a); break;
      case 'round': result = Math.round(a[0]); break;
      case 'gt': return a.length === 2 ? a[0] > a[1] : null;
      case 'gte': return a.length === 2 ? a[0] >= a[1] : null;
      case 'lt': return a.length === 2 ? a[0] < a[1] : null;
      case 'lte': return a.length === 2 ? a[0] <= a[1] : null;
      default: return null;
    }
    return Number.isFinite(result) ? result : null;
  }

  function cleanProps(src) {
    if (!src || typeof src !== 'object' || Array.isArray(src)) return null;
    const dst = Object.create(null);
    for (const [k, v] of Object.entries(src)) {
      if (!KEY.test(k) || k === 'constructor' || k === 'prototype') return null;
      dst[k] = v;
    }
    return dst;
  }

  class Compiler {
    constructor() { this.reset(); }
    reset() {
      this.nodes = new Map(); this.state = Object.create(null);
      this.defaults = Object.create(null); this.touched = new Set();
      this.lineCount = 0; this.invalid = 0; this.body = ''; this.pending = '';
      this.byteCount = 0; this.overLimit = false;
      this.revision = (this.revision || 0) + 1;
    }
    patch(p) {
      if (!p || typeof p !== 'object' || Array.isArray(p)) return false;
      if (p.op === 'state' && KEY.test(p.key || '') && safeScalar(p.value)) {
        if (!own(this.defaults, p.key)) this.defaults[p.key] = p.value;
        // Once the user adjusts a control, later model patches never steal it.
        if (!this.touched.has(p.key)) this.state[p.key] = p.value;
        return true;
      }
      if (p.op === 'remove' && KEY.test(p.id || '') && this.nodes.has(p.id)) {
        const drop = id => { for (const [k, n] of this.nodes) if (n.parent === id) drop(k); this.nodes.delete(id); };
        drop(p.id); return true;
      }
      if (!KEY.test(p.id || '')) return false;
      if (p.op === 'upsert' && TYPES.has(p.type) &&
          (p.parent === undefined || p.parent === 'root' || KEY.test(p.parent))) {
        if (!this.nodes.has(p.id) && this.nodes.size >= MAX_NODES) return false;
        const props = cleanProps(p.props || {});
        if (!props) return false;
        const prev = this.nodes.get(p.id);
        if (p.parent === p.id) return false;
        const parent = p.parent === undefined && prev ? prev.parent : (p.parent || 'root');
        if (parent !== 'root' && this.nodes.has(parent) && !CONTAINERS.has(this.nodes.get(parent).type)) return false;
        if (!CONTAINERS.has(p.type) && [...this.nodes.values()].some(n => n.parent === p.id)) return false;
        for (let ancestor = parent, hops = 0; ancestor !== 'root'; hops++) {
          if (ancestor === p.id || hops >= MAX_NODES) return false;
          ancestor = this.nodes.get(ancestor)?.parent || 'root';
        }
        this.nodes.set(p.id, { id: p.id, type: p.type, parent,
          props: Object.assign(Object.create(null), prev?.type === p.type ? prev.props : null, props) });
        return true;
      }
      if (p.op === 'set' && this.nodes.has(p.id)) {
        const props = cleanProps(p.props);
        if (!props) return false;
        Object.assign(this.nodes.get(p.id).props, props); return true;
      }
      if (p.op === 'append_text' && this.nodes.has(p.id) && typeof p.text === 'string' && p.text.length <= 4096) {
        const n = this.nodes.get(p.id);
        const next = String(n.props.text || '') + p.text;
        if (next.length > 16384) return false;
        n.props.text = next; return true;
      }
      return false;
    }
    push(body, closed = false) {
      body = String(body);
      if (!body.startsWith(this.body)) this.reset(); // retry/rewrite/rewind
      const suffix = body.slice(this.body.length);
      const prev = this.body;
      // UTF-8 byte budget, accounting for a surrogate pair split across deltas.
      const splitPair = prev.length && suffix.length &&
        prev.charCodeAt(prev.length - 1) >= 0xD800 && prev.charCodeAt(prev.length - 1) <= 0xDBFF &&
        suffix.charCodeAt(0) >= 0xDC00 && suffix.charCodeAt(0) <= 0xDFFF;
      this.byteCount += new TextEncoder().encode(suffix).length - (splitPair ? 2 : 0);
      this.body = body;
      if (this.overLimit) return false;
      if (body.length > MAX_BODY || this.byteCount > MAX_BODY) {
        this.invalid++; this.overLimit = true; return false;
      }
      const lines = (this.pending + suffix).split('\n');
      this.pending = lines.pop();
      if (closed && this.pending) { lines.push(this.pending); this.pending = ''; }
      let changed = false;
      for (const raw of lines) {
        if (++this.lineCount > MAX_PATCHES) {
          this.invalid++; this.overLimit = true; break;
        }
        const line = raw.trim();
        if (!line) continue;
        if (line.length > 8192 || new TextEncoder().encode(line).length > 8192) {
          this.invalid++; continue;
        }
        try { if (this.patch(JSON.parse(line))) changed = true; else this.invalid++; }
        catch { this.invalid++; }
      }
      return changed;
    }
    touch(key, next) {
      if (!KEY.test(key) || !safeScalar(next)) return false;
      this.touched.add(key); this.state[key] = next; return true;
    }
    resetControls() {
      this.touched.clear();
      // Preserve the state object captured by stable DOM control listeners.
      for (const key of Object.keys(this.state)) delete this.state[key];
      Object.assign(this.state, this.defaults);
    }
  }

  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = String(text);
    return n;
  };
  const labelText = x => String(x ?? '').slice(0, 2048);
  const numeric = x => { const n = Number(x); return Number.isFinite(n) ? n : null; };

  function chart(container, props, state) {
    const raw = Array.isArray(props.data) ? props.data.slice(0, 64) : [];
    const series = Array.isArray(props.series) ? props.series.slice(0, 4).filter(x => KEY.test(x?.key || '')) : [];
    const keys = series.length ? series.map(s => s.key) : ['y'];
    const data = raw.map(d => ({ x: labelText(d?.x).slice(0, 40),
      ys: keys.map(k => numeric(value(d?.[k], state))) }));
    const vals = data.flatMap(p => p.ys).filter(n => n !== null);
    if (!vals.length) { container.textContent = 'No chart data'; return; }
    const ns = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('viewBox', '0 0 340 155'); svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', labelText(props.label || 'Data chart').slice(0, 120));
    const min = Math.min(0, ...vals), max = Math.max(0, ...vals);
    const y = v => 126 - (v - min) / (max - min || 1) * 106;
    const COLORS = ['var(--c-accent,#598eff)', 'var(--c-ok,#159e82)',
      'var(--c-warn,#ce933d)', 'var(--c-tool,#9775cf)'];
    const shape = (tag, attrs) => {
      const n = document.createElementNS(ns, tag);
      for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
      svg.appendChild(n); return n;
    };
    shape('line', { x1: 24, x2: 325, y1: y(0), y2: y(0), stroke: 'currentColor', opacity: '.3' });
    if (props.kind === 'bar') {
      const slot = 300 / Math.max(1, data.length), bw = Math.max(1, (slot - 4) / keys.length);
      for (let i = 0; i < data.length; i++) for (let j = 0; j < keys.length; j++) {
        const v = data[i].ys[j]; if (v === null) continue;
        shape('rect', { x: 25 + slot * i + j * bw, y: Math.min(y(0), y(v)),
          width: Math.max(1,bw - 1), height: Math.max(1, Math.abs(y(0) - y(v))), fill: COLORS[j % COLORS.length] });
      }
    } else {
      for (let j = 0; j < keys.length; j++) {
        let points = [];
        const flush = () => { if (points.length) shape('polyline', {points:points.join(' '),fill:'none',
          stroke:COLORS[j % COLORS.length], 'stroke-width':2.5, 'stroke-linecap':'round'}); points=[]; };
        for (let i = 0; i < data.length; i++) {
          const v = data[i].ys[j];
          if (v === null) flush();
          else points.push(`${25 + i * 300 / Math.max(1, data.length-1)},${y(v)}`);
        }
        flush();
      }
    }
    container.replaceChildren(svg);
    if (series.length) {
      const legend = el('div','native-legend');
      series.forEach((s,i) => { const chip = el('span','native-legend-item',labelText(s.label || s.key));
        chip.style.setProperty('--series-color',COLORS[i]); legend.appendChild(chip); });
      container.appendChild(legend);
    }
  }

  function table(container, props, state) {
    const columns = Array.isArray(props.columns) ? props.columns.slice(0, 8)
      .filter(c => KEY.test(c?.key || '')) : [];
    const rows = Array.isArray(props.rows) ? props.rows.slice(0, 64) : [];
    const table = el('table','native-table-data');
    const thead = el('thead'), header = el('tr');
    columns.forEach(c => header.appendChild(el('th','',labelText(c.label || c.key))));
    thead.appendChild(header); table.appendChild(thead);
    const tbody = el('tbody');
    for (const row of rows) {
      const tr = el('tr');
      for (const col of columns) tr.appendChild(el('td','',labelText(value(row?.[col.key], state))));
      tbody.appendChild(tr);
    }
    table.appendChild(tbody); container.replaceChildren(table);
  }

  class Panel {
    constructor(host) { this.host = host; this.compiler = new Compiler(); this.els = new Map(); this.saveTimers = new Map(); this.surfaceId = null; this.host.classList.add('native-panel'); this.renderedRevision = 0; }
    saveControl(key,next) {
      if (!this.surfaceId || typeof wsSend !== 'function') return;
      clearTimeout(this.saveTimers.get(key));
      const surface = this.surfaceId;
      this.saveTimers.set(key,setTimeout(() => {
        this.saveTimers.delete(key);
        wsSend({type:'ui_state',surface,key,value:next});
      },350));
    }
    update(body, closed) {
      const changed = this.compiler.push(body, closed);
      if (this.compiler.revision !== this.renderedRevision) {
        this.host.replaceChildren(); this.els.clear(); this.renderedRevision = this.compiler.revision;
      }
      if (changed) this.draw();
      this.host.dataset.invalid = this.compiler.invalid ? String(this.compiler.invalid) : '';
    }
    draw() {
      const { nodes, state } = this.compiler;
      for (const [id, dom] of this.els) if (!nodes.has(id)) { dom.remove(); this.els.delete(id); }
      const attaching = new Set();
      const attach = id => {
        const n = nodes.get(id);
        if (!n || attaching.has(id)) return;
        attaching.add(id);
        if (n.parent !== 'root' && !nodes.has(n.parent)) return; // forward parent reference
        if (n.parent !== 'root') attach(n.parent);
        let dom = this.els.get(id);
        if (dom?.dataset.type !== n.type) { dom?.remove(); dom = el('div', `native-${n.type}`);
          dom.dataset.type = n.type; this.els.set(id, dom); }
        const parent = n.parent === 'root' ? this.host : this.els.get(n.parent);
        if (parent && dom.parentElement !== parent && !dom.contains(parent)) parent.appendChild(dom);
        this.paint(dom, n, state);
      };
      for (const id of nodes.keys()) attach(id);
    }
    paint(dom, node, state) {
      const p = node.props;
      dom.hidden = own(p,'when') ? !Boolean(value(p.when,state)) : false;
      if (CONTAINERS.has(node.type) || node.type === 'divider') return;
      const val = own(p, 'value') ? value(p.value, state) : '';
      if (['text','heading','badge'].includes(node.type)) {
        dom.textContent = labelText(own(p,'value') ? val : (p.text ?? '')).slice(0, 16384); return;
      }
      if (node.type === 'metric') {
        const num = numeric(val); const digits = Math.min(6,Math.max(0,Number(p.digits ?? 2) || 0));
        const fmt = num === null ? '—' : num.toLocaleString(undefined,{maximumFractionDigits:digits});
        dom.replaceChildren(el('span','native-label',labelText(p.label)),
          el('strong','native-value',fmt + (p.unit ? ` ${labelText(p.unit)}` : ''))); return;
      }
      if (node.type === 'progress') {
        const n = numeric(val); dom.replaceChildren(el('span','native-label',labelText(p.label)));
        const bar = el('progress'); bar.max = 100; bar.value = n === null ? 0 : Math.max(0,Math.min(100,n));
        dom.appendChild(bar); return;
      }
      if (node.type === 'chart') { chart(dom,p,state); return; }
      if (node.type === 'table') { table(dom,p,state); return; }
      if (['slider','toggle','select','input'].includes(node.type)) {
        if (!KEY.test(p.bind || '')) { dom.textContent = 'Invalid binding'; return; }
        const key = p.bind, kind = node.type;
        let input = dom.querySelector('[data-native-control]');
        if (!input || input.dataset.nativeControl !== kind) {
          input = el(kind === 'select' ? 'select' : 'input'); input.dataset.nativeControl = kind;
          if (kind !== 'select') input.type = kind === 'toggle' ? 'checkbox' : kind === 'slider' ? 'range' : 'text';
          input.addEventListener('input', () => {
            const next = kind === 'slider' ? Number(input.value) : kind === 'toggle' ? input.checked : input.value;
            if (this.compiler.touch(key, next)) { this.draw(); if (kind !== 'input') this.saveControl(key,next); }
          });
          dom.replaceChildren(el('label','native-label',labelText(p.label)),input,el('span','native-control-value'));
        }
        dom.firstElementChild.textContent = labelText(p.label);
        if (kind === 'slider') {
          const min = numeric(p.min), max = numeric(p.max), step = numeric(p.step);
          input.min = String(min ?? 0); input.max = String(max ?? 100); input.step = String(step && step > 0 ? step : 1);
          input.value = own(state,key) ? String(state[key]) : input.min;
          dom.lastElementChild.textContent = input.value;
        } else if (kind === 'toggle') input.checked = Boolean(state[key]);
        else if (kind === 'select') {
          const options = Array.isArray(p.options) ? p.options.slice(0, 32) : [];
          const signature = JSON.stringify(options);
          if (input.dataset.options !== signature) {
            input.replaceChildren(...options.map(o => {
              const opt = el('option','',labelText(o?.label ?? o?.value ?? o)); opt.value = labelText(o?.value ?? o); return opt;
            }));
            input.dataset.options = signature;
          }
          input.value = own(state,key) ? String(state[key]) : '';
        } else {
          input.maxLength = 256;
          if (document.activeElement !== input) input.value = own(state,key) ? String(state[key]) : '';
        }
        return;
      }
      if (node.type === 'button') {
        let button = dom.firstElementChild;
        if (!button) { button = el('button'); button.type = 'button'; dom.appendChild(button); }
        button.textContent = labelText(p.label || 'Action');
        button.onclick = () => {
          if (p.action === 'reset') {
            this.compiler.resetControls(); this.draw();
            // Reset is itself a user gesture. Persist reset defaults for
            // structured controls, otherwise replay would restore stale values.
            for (const n of this.compiler.nodes.values()) {
              if (!['slider','toggle','select'].includes(n.type)) continue;
              const key = n.props.bind;
              if (KEY.test(key || '') && safeScalar(state[key]))
                this.saveControl(key,state[key]);
            }
          } else if (p.action === 'copy' && typeof p.text === 'string') {
            navigator.clipboard?.writeText(p.text.slice(0,8192)).catch(() => {});
          } else if (p.action === 'ask' && typeof p.text === 'string' && typeof wsSend === 'function') {
            const prompt = p.text.slice(0,4096).replace(/\{([A-Za-z][A-Za-z0-9_-]{0,63})\}/g,
              (_, key) => own(state,key) ? String(state[key]) : '');
            if (prompt.trim()) wsSend({type:'ui_message',text:prompt});
          }
        };
      }
    }
  }

  // A stable surface header is the first complete JSONL line of a UI fence.
  // Subsequent assistant messages may append updates to the same panel.
  function surfaceHeader(body) {
    const end = body.indexOf('\n');
    if (end < 0 || end > 1024) return null;
    try {
      const value = JSON.parse(body.slice(0,end));
      if (value?.op === 'surface' && KEY.test(value.id || ''))
        return {id:value.id,body:body.slice(end+1)};
    } catch {}
    return null;
  }
  const scopedDelta = (previous,next) => next.startsWith(previous) ? next.slice(previous.length) : null;
  function applyState(surface,key,next) {
    if (!KEY.test(surface || '') || !KEY.test(key || '') || !safeScalar(next)) return false;
    const panel = surfaces.get(surface);
    if (panel) {
      if (!panel.compiler.touch(key,next)) return false;
      panel.draw();
    } else {
      if (!pendingValues.has(surface)) pendingValues.set(surface,new Map());
      const controls = pendingValues.get(surface);
      if (controls.size < 128 || controls.has(key)) controls.set(key,next);
    }
    return true;
  }
  function resetSurfaces() {
    for (const panel of surfaces.values()) {
      for (const timer of panel.saveTimers.values()) clearTimeout(timer);
      panel.saveTimers.clear();
    }
    surfaces.clear();
    pendingValues.clear();
  }


  function render(host, source, markdown) {
    source = String(source);
    if (!/(?:```|~~~)sunmao-ui\r?\n/.test(source)) {
      panels.delete(host); host.innerHTML = markdown(source); return;
    }
    // At most 32 islands per message; excess content displays as ordinary text.
    const segs = parts(source);
    if (segs.length > 65 || source.length > 1048576) {
      panels.delete(host); host.textContent = source.slice(0,65536); return;
    }
    let slots = panels.get(host);
    if (!slots) { host.replaceChildren(); slots = []; panels.set(host,slots); }
    for (let i = 0; i < segs.length; i++) {
      const seg = segs[i]; let slot = slots[i];
      if (!slot || slot.kind !== seg.kind) {
        slot?.element.remove();
        const element = el('div',seg.kind === 'md' ? 'native-md' : 'native-ui');
        slot = {kind:seg.kind,element,body:null,panel:seg.kind==='ui'?new Panel(element):null};
        slots[i] = slot;
      }
      if (slot.element.parentElement !== host) host.appendChild(slot.element);
      if (slot.kind === 'md' && slot.body !== seg.body) slot.element.innerHTML = markdown(seg.body);
      if (slot.kind === 'ui') {
        const scoped = surfaceHeader(seg.body);
        if (scoped) {
          if (slot.surfaceKey && slot.surfaceKey !== scoped.id) {
            slot.element.textContent = 'Native UI surface cannot change during streaming';
          } else {
            if (!slot.surfaceKey) {
              slot.surfaceKey = scoped.id;
              const existing = surfaces.get(scoped.id);
              if (existing) {
                slot.panel = existing;
                slot.element.hidden = true; // updates paint the original panel
              } else {
                slot.panel.surfaceId = scoped.id;
                surfaces.set(scoped.id,slot.panel);
                for (const [key,value] of pendingValues.get(scoped.id) || [])
                  slot.panel.compiler.touch(key,value);
                pendingValues.delete(scoped.id);
              }
              slot.applied = '';
            }
            const delta = scopedDelta(slot.applied,scoped.body);
            if (delta === null) {
              slot.element.textContent = 'Native UI patch stream changed; reopen session to replay';
            } else if (delta || (seg.closed && !slot.finished)) {
              slot.applied = scoped.body;
              slot.panel.update(slot.panel.compiler.body + delta,seg.closed);
              slot.finished = seg.closed;
            }
          }
        } else if (!slot.surfaceKey) {
          // Delay a likely incomplete surface header rather than treating it
          // as a malformed patch during token-by-token rendering.
          if (!seg.body.startsWith('{"op":"surface"') || seg.body.includes('\n'))
            slot.panel.update(seg.body,seg.closed);
        }
      }
      slot.body = seg.body;
    }
    while (slots.length > segs.length) slots.pop().element.remove();
  }
  return { parts, value, Compiler, render, Panel, surfaceHeader, scopedDelta, resetSurfaces, applyState };
})();
if (typeof module !== 'undefined' && module.exports) module.exports = NativeUI;
