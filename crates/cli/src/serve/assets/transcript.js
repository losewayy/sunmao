/* markdown render, transcript blocks, live fold, approvals, event log, replay */
'use strict';

/* ================= markdown (streaming-tolerant, no deps) ================= */
const HASH_LANGS = /^(?:python|py|bash|sh|shell|zsh|powershell|ps1|yaml|yml|toml|dockerfile|make|makefile|cmake|ruby|rb|perl|pl|r|julia|jl|ini|conf|config|docker|text|txt)$/i;

function highlight(code, lang) {
  const hash = !lang || HASH_LANGS.test(lang);
  const re = /(\/\/[^\n]*|\/\*[\s\S]*?(?:\*\/|$)|"""[\s\S]*?(?:"""|$)|'''[\s\S]*?(?:'''|$)|`(?:\\.|[^`\\])*`|"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|\b\d[\d_]*(?:\.[\d_]+)?(?:[eEpPxX][\dA-Fa-f_+-]*)?|\b(?:const|let|var|function|fn|struct|enum|impl|trait|pub|use|mod|crate|where|match|if|else|elif|for|while|loop|break|continue|return|class|extends|new|this|self|super|import|from|export|default|async|await|try|catch|finally|throw|raise|except|def|print|pass|None|True|False|null|true|false|nil|Some|Ok|Err|type|interface|package|func|go|chan|select|defer|in|of|do|end|then|local|require|void|int|float|double|long|short|unsigned|signed|char|bool|auto|virtual|static|final|abstract|private|protected|public|readonly|mut|ref|move|dyn|unsafe|extern|sizeof|with|yield|assert|global|nonlocal|del|is|not|and|or)\b|#[^\n]*)/g;
  let out = '', last = 0, m;
  while ((m = re.exec(code))) {
    const tok = m[0];
    if (tok.startsWith('#') && !hash) { continue; } // don't treat # in non-hash langs
    out += esc(code.slice(last, m.index));
    let cls = 'tk-x';
    if (/^(?:\/\/|\/\*)/.test(tok) || tok.startsWith('#')) cls = 'tk-x';
    else if (/^["'`]/.test(tok)) cls = 'tk-s';
    else if (/^\d/.test(tok)) cls = 'tk-n';
    else cls = 'tk-k';
    out += `<span class="${cls}">${esc(tok)}</span>`;
    last = m.index + tok.length;
    if (m.index === re.lastIndex) re.lastIndex++;
  }
  out += esc(code.slice(last));
  return out;
}

function mdInline(s) {
  s = esc(s);
  s = s.replace(/`([^`\n]+)`/g, (_, c) => `<code>${c}</code>`);
  s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  s = s.replace(/\*\*([^*\n]+)\*\*|__([^_\n]+)__/g, '<b>$1$2</b>');
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, '$1<i>$2</i>');
  s = s.replace(/~~([^~\n]+)~~/g, '<del>$1</del>');
  return s;
}

function mdRender(src) {
  const lines = String(src).split('\n');
  let html = '', para = [], list = null, code = null, codeLang = '', quote = '';
  const flushPara = () => { if (para.length) { html += `<p>${para.map(mdInline).join('<br>')}</p>`; para = []; } };
  const flushList = () => { if (list) { html += list.html + `</${list.tag}>`; list = null; } };
  const flushQuote = () => { if (quote) { html += `<blockquote>${quote}</blockquote>`; quote = ''; } };
  const flushAll = () => { flushPara(); flushList(); flushQuote(); };
  for (const line of lines) {
    if (code !== null) {
      if (/^\s*```/.test(line)) { html += `<pre><code>${highlight(code, codeLang)}</code></pre>`; code = null; }
      else code += line + '\n';
      continue;
    }
    const fence = line.match(/^\s*```\s*([\w+.#-]*)/);
    if (fence) { flushAll(); code = ''; codeLang = fence[1] || ''; continue; }
    const h = line.match(/^\s{0,3}(#{1,4})\s+(.+)/);
    if (h) { flushAll(); const n = h[1].length; html += `<h${n}>${mdInline(h[2])}</h${n}>`; continue; }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) { flushAll(); html += '<hr>'; continue; }
    const q = line.match(/^\s*>\s?(.*)/);
    if (q) { flushPara(); flushList(); quote += `<p>${mdInline(q[1])}</p>`; continue; }
    const li = line.match(/^\s*(?:([-*+])|(\d+)[.)])\s+(.+)/);
    if (li) {
      flushPara(); flushQuote();
      const tag = li[1] ? 'ul' : 'ol';
      if (!list || list.tag !== tag) { flushList(); list = { tag, html: `<${tag}>` }; }
      list.html += `<li>${mdInline(li[3])}</li>`;
      continue;
    }
    if (/^\s*$/.test(line)) { flushAll(); continue; }
    flushList(); flushQuote();
    para.push(line);
  }
  if (code !== null) html += `<pre><code>${highlight(code, codeLang)}</code></pre>`;
  flushAll();
  return html;
}

/* ================= transcript blocks ================= */
const stIcon = st => `<span class="st ${st}">${ic(st === 'ok' ? 'check' : st === 'err' ? 'x' : 'rotate')}</span>`;
function toolHTML([st, nm, sm, tm, out], o = {}) {
  const time = st === 'run' ? `<span class="tm live" data-start="${o.start ?? performance.now()}">0.0s</span>` : st === 'wait' ? '<span class="tm w">等待批准</span>' : `<span class="tm${st === 'err' && tm && !/\d/.test(tm) ? ' e' : ''}">${tm || ''}</span>`;
  return `<div class="tool${st === 'err' ? ' bad' : ''}${o.cls ? ' ' + o.cls : ''}"><button class="tool-h">${stIcon(st)}<span class="nm">${esc(nm)}</span><span class="sum">${esc(sm)}</span>${time}${out ? ic('chev-r', 'i xs chev') : ''}</button>${out ? `<div class="tool-o"><pre>${esc(out)}</pre></div>` : ''}</div>`;
}
// replayed prompts carry no timestamp in the log — only a live send shows
// the wall clock, rather than stamping history with "now"
const youHTML = (text, anim, at) => `<div class="msg you"><div class="msg-h${anim ? ' enter' : ''}"><i class="dot"></i><span>你</span>${at ? `<time>${at}</time>` : ''}</div><div class="bubble glass${anim ? ' enter' : ''}">${esc(text)}</div></div>`;
const botHead = anim => `<div class="msg-h${anim ? ' enter' : ''}"><i class="dot"></i><span>sunmao</span>${modelLabel ? `<span class="model">${esc(modelLabel.replace(/^global:/, ''))}</span>` : ''}</div>`;
const TX = $('#tx');
function append(parent, html) { const t = document.createElement('template'); t.innerHTML = html.trim(); const first = t.content.firstElementChild; parent.appendChild(t.content); keepBottom(true); return first; }
function keepBottom(force) { const sc = $('#scroller'); if (force || sc.scrollHeight - sc.scrollTop - sc.clientHeight < 240) sc.scrollTop = sc.scrollHeight; }
/* ---- work cards: a run of tool calls (+ the short narration between
   them) shares one card; 3+ calls earn a summary header and fold once
   the run settles. Failed calls stay visible through the fold. ---- */
const TG_MANY = 3, STEP_MAX = 400;
function toolGroup(host) {
  const last = host.lastElementChild;
  if (last && last.classList.contains('tools')) return last;
  return append(host, `<div class="tools glass"><button class="tg-h" data-act="tg" aria-expanded="true">${ic('chev-r', 'i xs chev')}<span class="tg-s"></span><span class="tg-bad"></span><span class="tg-n"></span></button></div>`);
}
const TG_VERBS = [
  [/^(Read)$/, n => `读取 ${n} 个文件`],
  [/^(Grep|Glob)$/, n => `搜索 ${n} 次`],
  [/^(Bash|shell|!)$/, n => `运行 ${n} 条命令`],
  [/^(Edit|Write)$/, n => `修改 ${n} 个文件`],
  [/^(WebFetch)$/, n => `抓取 ${n} 个网页`],
  [/^(Task)$/, n => `派出 ${n} 个子代理`],
  [/^(TodoWrite)$/, () => '更新待办'],
  [/^(HtmlArtifact)$/, n => `生成 ${n} 个工件`],
];
// A short assistant line sitting right before a tool call is narration
// ("I'll read both files.") — pull it into the card as a step so the run
// reads as one unit. Remove-then-append (never a bare move) keeps the
// node order the replay-parity op log observes identical to the DOM's.
function absorbStep(host) {
  const b = host.lastElementChild;
  if (!b || !b.classList.contains('bubble') || b.textContent.length > STEP_MAX || $('pre', b)) return;
  b.remove();
  const g = toolGroup(host); // the previous card if the bubble followed one
  b.classList.remove('glass', 'enter'); b.classList.add('step');
  g.appendChild(b);
}
function refreshGroup(g) {
  if (!g || !g.classList.contains('tools')) return;
  const tools = [...g.children].filter(el => el.classList.contains('tool'));
  const counts = new Map(); let bad = 0, live = false;
  for (const t of tools) {
    const nm = ($('.nm', t) || {}).textContent || '';
    const name = nm.replace(/^↳ /, '');
    const hit = TG_VERBS.find(([re]) => re.test(name));
    const key = hit ? hit[0].source : name;
    const c = counts.get(key) || { n: 0, fmt: hit ? hit[1] : n => `${name} ×${n}` };
    c.n++; counts.set(key, c);
    const st = $('.st', t);
    if (st && st.classList.contains('err')) bad++;
    if (st && (st.classList.contains('run') || st.classList.contains('wait'))) live = true;
  }
  const many = tools.length >= TG_MANY;
  g.classList.toggle('many', many);
  // folded, a card previews only its last few failures; the badge has the count
  const fails = tools.filter(t => t.classList.contains('bad'));
  fails.forEach((t, i) => t.classList.toggle('xs', i < fails.length - TG_MANY));
  if (!many) return;
  $('.tg-s', g).textContent = [...counts.values()].map(c => c.fmt(c.n)).join(' · ');
  $('.tg-bad', g).textContent = bad ? `${bad} 失败` : '';
  $('.tg-n', g).textContent = live ? '进行中' : `${tools.length} 步`;
}
function foldGroup(g, on) {
  g.classList.toggle('fold', on);
  const h = $('.tg-h', g); if (h) h.setAttribute('aria-expanded', String(!on));
}
// settle every live card that the user hasn't opened/closed by hand
function foldSettled(root, except) {
  for (const g of $$('.tools', root)) {
    refreshGroup(g);
    if (g !== except && g.classList.contains('many') && !g.dataset.user && !$('.st.run', g) && !$('.st.wait', g)) foldGroup(g, true);
  }
}
function setTool(el, st, tm, out) {
  el.classList.toggle('bad', st === 'err');
  const h = $('.tool-h', el);
  $('.st', h).outerHTML = stIcon(st);
  const t = $('.tm', h);
  t.className = 'tm';
  if (st === 'run') { t.classList.add('live'); t.dataset.start = performance.now(); t.textContent = '0.0s'; }
  else { delete t.dataset.start; if (st === 'err' && tm && !/\d/.test(tm)) t.classList.add('e'); t.textContent = tm || ''; }
  if (out && !$('.tool-o', el)) { h.insertAdjacentHTML('beforeend', ic('chev-r', 'i xs chev')); el.insertAdjacentHTML('beforeend', `<div class="tool-o"><pre>${esc(out)}</pre></div>`); }
}
setInterval(() => { for (const t of $$('.tm.live')) t.textContent = ((performance.now() - +t.dataset.start) / 1000).toFixed(1) + 's'; }, 100);

/* ================= transcript live state ================= */
let curMsg = null;      // open .msg.bot element
let curBubble = null;   // streaming bubble element inside curMsg
let curText = '';       // accumulated markdown for curBubble
let curThink = null, curThinkText = '';
const runningTools = [];   // {el, name, lane, depth, t0}
const pendingApprovals = new Map();  // id -> card element

function msgHost() {
  if (!curMsg) { curMsg = append(TX, `<div class="msg bot">${botHead(true)}</div>`); curMsg.classList.add('enter'); }
  return curMsg;
}
function closeMsg() { curMsg = curBubble = curThink = null; curText = curThinkText = ''; }
function bubble() {
  const host = msgHost();
  if (!curBubble) { curBubble = append(host, '<div class="bubble glass enter"></div>'); curText = ''; }
  return curBubble;
}
function contentDelta(t) {
  if (!t) return;
  const b = bubble();
  curText += t;
  b.innerHTML = mdRender(curText);
  keepBottom();
}
function reasoningDelta(t) {
  if (!t) return;
  const host = msgHost();
  if (!curThink) {
    curThink = append(host, `<button class="think glass live enter" data-act="think">${ic('chev-r', 'i xs')}思考</button><div class="think-o glass"></div>`).nextElementSibling;
    curThinkText = '';
  }
  curThinkText += t;
  curThink.textContent = curThinkText;
  keepBottom();
}
const capOut = s => s.length > 12000 ? s.slice(0, 12000) + `\n…(${s.length - 12000} chars truncated)` : s;
function toolStart(ev) {
  const host = msgHost();
  absorbStep(host);
  const g = toolGroup(host);
  foldSettled(host, g); // an earlier card in this reply is done — tuck it away
  const name = (ev.depth ? '↳ ' : '') + (ev.name || '?');
  const el = append(g, toolHTML(['run', name, ev.summary || '', '', ''], { cls: 'enter', start: performance.now() }));
  refreshGroup(g);
  runningTools.push({ el, name: ev.name, lane: ev.lane || 0, depth: ev.depth || 0, call_id: ev.call_id || null, t0: performance.now() });
  curBubble = null; // text after a tool call starts a new bubble
}
function toolDone(ev) {
  // exact join key first — same-name calls in one turn mispair without it;
  // (name, lane, depth) FIFO stays the fallback for id-less synthetic events
  let i = ev.call_id ? runningTools.findIndex(t => t.call_id === ev.call_id) : -1;
  if (i < 0) i = runningTools.findIndex(t => t.name === ev.name && t.lane === (ev.lane || 0) && t.depth === (ev.depth || 0));
  if (i < 0) i = runningTools.findIndex(t => t.name === ev.name);
  if (i < 0) i = 0;
  const t = runningTools[i];
  if (!t) {
    const host = msgHost();
    let g = toolGroup(host);
    append(g, toolHTML([ev.ok ? 'ok' : 'err', ev.name || '?', '', '', capOut(ev.output || '')]));
    return;
  }
  runningTools.splice(i, 1);
  // kernel's elapsed_ms is authoritative; local t0 is the replay/orphan fallback
  const secs = (typeof ev.elapsed_ms === 'number' ? ev.elapsed_ms / 1000 : (performance.now() - t.t0) / 1000).toFixed(1) + 's';
  setTool(t.el, ev.ok ? 'ok' : 'err', secs, capOut(ev.output || ''));
  refreshGroup(t.el.parentElement);
}
function islandHTML(ev) {
  const nm = ev.name || '';
  return `<div class="island glass" data-artifact="${esc(nm)}">
    <div class="island-h">
      ${ic('file-code', 'i fi')}
      <span class="nm">HtmlArtifact</span>
      <span class="fn">${esc(nm)}.html</span>
      <button class="notes-pill" data-act="notes" data-tip="批注">${ic('note', 'i xs')}<span>批注</span></button>
      <span class="revs" hidden>
        <button class="ib" data-act="rev-prev" data-tip="上一版本" aria-label="上一版本">${ic('chev-l')}</button>
        <span class="rev-n">v1/1</span>
        <button class="ib" data-act="rev-next" data-tip="下一版本" aria-label="下一版本">${ic('chev-r')}</button>
      </span>
      <span class="sp"></span>
      <span class="meta">${fmtBytes(ev.bytes || 0)}</span>
      <button class="ib" data-act="sandbox" data-tip="沙箱：禁网 · 无同源 · 无脚本桥出" aria-label="沙箱">${ic('lock')}</button>
      <button class="ib" data-act="island-tall" data-tip="展开" aria-label="展开">${ic('expand')}</button>
      <button class="ib" data-act="island-open" data-tip="在浏览器中打开" aria-label="在浏览器中打开">${ic('external')}</button>
    </div>
    <iframe title="${esc(nm)}" sandbox="" loading="lazy" src="/artifacts/${encodeURIComponent(nm)}"></iframe>
    <div class="notes"></div>
  </div>`;
}
function addArtifact(ev) {
  const host = msgHost();
  const el = append(host, islandHTML(ev));
  el.classList.add('enter');
  curBubble = null;
  refreshNotes(el, ev.name);
  refreshRevs(el, ev.name, ev.rev || 0);
  probeApp(el, ev.name);
}


function addNote(text) {
  append(TX, `<div class="note-line glass enter">${esc(text)}</div>`);
}
function approvalCard(ev) {
  const host = msgHost();
  const card = append(host, `<div class="approve glass enter" data-apid="${ev.id}">
    <div class="ap-h"><span class="ai">${ic('alert')}</span><span>需要你的批准</span><span class="why">${esc(ev.tool || '')} · ${esc(ev.why || '')}</span></div>
    <div class="cmd"><span class="pr">$</span>${esc(ev.detail || '')}</div>
    <div class="acts">
      <button class="btn allow" data-ap="once">${ic('check')}Allow<kbd>Y</kbd></button>
      <button class="btn ghost" data-ap="deny">Deny<kbd>N</kbd></button>
      <button class="btn ghost" data-ap="session">Always<kbd>A</kbd></button>
      <span class="hint">Always：本会话内相同命令不再询问</span>
    </div>
  </div>`);
  pendingApprovals.set(ev.id, card);
  syncWait();
  return card;
}
function collapse(card, html) {
  const h0 = card.offsetHeight; card.style.overflow = 'hidden';
  card.innerHTML = html; card.classList.add('done');
  const h1 = card.offsetHeight;
  motion.height(card, h0, h1);
}
function syncWait() {
  $('#cmp-wait').hidden = pendingApprovals.size === 0;
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy;
  renderRail();
}
function decide(verdict, id) {
  let card, aid;
  if (id != null) { card = pendingApprovals.get(id); aid = id; }
  else { aid = [...pendingApprovals.keys()][0]; card = pendingApprovals.get(aid); }
  if (!card) return;
  pendingApprovals.delete(aid);
  wsSend({ type: 'approval', id: aid, sess: sessionId, verdict });
  const [st, title, note] = { once: ['ok', '已允许', '仅本次'], session: ['ok', '已允许', '本会话内相同命令不再询问'], deny: ['err', '已拒绝', '操作未执行'] }[verdict] || ['err', '已拒绝', ''];
  const detail = $('.cmd', card) ? $('.cmd', card).textContent.trim() : '';
  collapse(card, `<div class="ap-done">${stIcon(st)}<b>${title}</b><code>${esc(detail)}</code><span>${note}</span></div>`);
  logEv('approval', `verdict ${verdict} · ${aid}`);
  syncWait();
}
function abortTurn() {
  closeMsg();
  for (const t of runningTools.splice(0)) setTool(t.el, 'err', '已中断');
}
function setBusy(on) {
  busy = !!on;
  $('#cmp-busy').hidden = !busy;
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy;
  // send and stop are separate verbs — busy Enter steers (queues a message
  // for the running turn), the square button cancels
  $('#stop-btn').hidden = !busy;
  renderRail();
}

/* ================= event log ================= */
function logEv(type, detail) {
  EVLOG.push([clock(true), type, detail]);
  if (EVLOG.length > 500) EVLOG.splice(0, EVLOG.length - 500);
  if (popEl && popEl.classList.contains('events')) popEl.innerHTML = eventsHTML();
}
function eventsHTML() {
  return `<div class="ev-h">事件日志<span>${esc(sessionId)}.jsonl</span></div><div class="ev-list scroll">${EVLOG.map(e => `<div class="ev-l"><span class="t">${e[0]}</span><span class="e ${e[1]}">${e[1]}</span><span class="d">${esc(e[2])}</span></div>`).join('') || '<div class="empty-row">尚无事件</div>'}</div><div class="ev-f"><span>共 ${EVLOG.length} 条 · 事件源日志的实时镜像</span><span class="mono">${esc(cwd)}/.sunmao/sessions</span></div>`;
}

/* ================= replay ================= */
function renderReplay(events, anim) {
  // replay must not re-run entrance motion — §8.1.5: flag on, replay, flag off
  root.dataset.replaying = '';
  // app islands about to be dropped get a resource-teardown first —
  // SEP-1865 says the View releases its side on this request
  for (const isl of $$('.island')) teardownIsland(isl);
  TX.innerHTML = ''; EVLOG.length = 0;
  pendingApprovals.clear(); runningTools.length = 0;
  closeMsg(); syncWait();
  const pendingCalls = new Map(); // call_id -> tool element
  for (const ev of events || []) {
    const t = ev.type;
    if (t === 'started') {
      modelLabel = ev.model || modelLabel;
      $('#cmp-model').textContent = modelLabel || '…';
      // chrome, not transcript — model + cwd already live in the composer;
      // the event log keeps the fact. An empty session shows the hero.
      logEv('started', `${ev.model || '?'} · cwd=${String(ev.cwd || '').replace(/^\\\\\?\\/, '')}`);
    } else if (t === 'message') {
      const m = ev.message || {};
      if (m.role === 'system') { logEv('message', 'system · (identity, hidden)'); continue; }
      if (m.role === 'user') {
        const c = m.content || '';
        if (c.startsWith('[hook context]')) {
          // folded hook evidence — the TUI renders this as an audit row,
          // not a prompt the user typed; the event log carries the detail
          logEv('hook', 'hook injected context · ' + c.slice(15).slice(0, 80));
          continue;
        }
        if (c.startsWith('<local-shell>')) {
          // folded evidence — the local_shell event renders the real row
          logEv('tool_call', '! (folded message)');
          continue;
        }
        closeMsg();
        append(TX, youHTML(c, anim));
        logEv('message', 'user · ' + c.slice(0, 60));
      } else if (m.role === 'assistant') {
        if (m.content) { const h = msgHost(); append(h, `<div class="bubble glass">${mdRender(m.content)}</div>`); }
        logEv('message', 'assistant · ' + (m.content || '').slice(0, 60) + (m.tool_calls ? ` (+${m.tool_calls.length} calls)` : ''));
      } else if (m.role === 'tool') {
        continue; // paired tool_result covers it
      }
    } else if (t === 'tool_call') {
      const c = ev.call || {};
      const fn = (c.function || {});
      let sum = '';
      try { const a = JSON.parse(fn.arguments || '{}'); sum = a.command || a.path || a.pattern || a.name || a.prompt || fn.arguments.slice(0, 120); } catch { sum = String(fn.arguments || '').slice(0, 120); }
      const host = msgHost();
      absorbStep(host);
      let g = toolGroup(host);
      const el = append(g, toolHTML(['run', (ev.depth ? '↳ ' : '') + (fn.name || '?'), sum, '', ''], {}));
      // key on (call.id, depth, lane) — relayed sub-agent calls can share a
      // call id namespace across lanes; missing id falls back to a unique key
      pendingCalls.set(c.id ? `${ev.depth || 0}:${ev.lane || 0}:${c.id}` : Symbol(),
        { el, name: fn.name || '?', depth: ev.depth || 0, lane: ev.lane || 0 });
      logEv('tool_call', `${fn.name || '?'} ${sum}`.slice(0, 140));
    } else if (t === 'tool_result') {
      const key = `${ev.depth || 0}:${ev.lane || 0}:${ev.call_id}`;
      let hit = pendingCalls.get(key);
      if (hit) pendingCalls.delete(key);
      else {
        // id-less fallback (mirrors the TUI fold): newest still-running call
        // with the same name/depth/lane — never steal an unrelated row
        let k;
        for (const kk of [...pendingCalls.keys()].reverse()) {
          const p = pendingCalls.get(kk);
          if (p.name === ev.name && p.depth === (ev.depth || 0) && p.lane === (ev.lane || 0)) { k = kk; break; }
        }
        if (k !== undefined) { hit = pendingCalls.get(k); pendingCalls.delete(k); }
      }
      if (hit) setTool(hit.el, ev.ok ? 'ok' : 'err', '', capOut(ev.output || ''));
      else addNote(`${ev.ok ? '✓' : '✗'} ${ev.name || '?'}`); // result without a call — surface, don't fabricate a row
      logEv('tool_result', `${ev.name || '?'} ${ev.ok ? 'ok' : 'err'}`);
    } else if (t === 'compacted') {
      // compaction boundary — earlier history was summarized away, so the
      // transcript clears exactly like the TUI's blocks.clear()
      TX.innerHTML = ''; closeMsg(); runningTools.length = 0; pendingCalls.clear();
      addNote(`[context compacted]${ev.summary ? '\n' + ev.summary : ''}`);
      logEv('note', 'compacted');
    } else if (t === 'artifact') {
      addArtifact(ev);
      logEv('artifact', `${ev.name} · ${fmtBytes(ev.bytes || 0)}`);
    } else if (t === 'usage') {
      const u = ev.usage || ev;
      logEv('usage', `prompt ${nf(u.prompt_tokens || 0)} · cache_read ${nf(u.cache_read_input_tokens || 0)}`);
    } else if (t === 'hook') {
      logEv('hook', `${ev.event} · ${ev.detail}`);
    } else if (t === 'local_shell') {
      const host = msgHost();
      let g = toolGroup(host);
      append(g, toolHTML([ev.exit_code === 0 ? 'ok' : 'err', 'shell', '$ ' + (ev.command || ''), 'exit ' + ev.exit_code, capOut(ev.output || '')]));
      logEv('tool_call', '! ' + (ev.command || ''));
    } else if (t === 'mode_change') {
      // audit row in the TUI — on this side the event log is the audit
      // surface, so the fold only re-syncs the chip + logs the fact
      setApprovalMode(ev.mode);
      logEv('hook', `approval.mode · ${ev.mode}`);
    } else if (t === 'task_done') {
      const host = msgHost();
      append(host, `<div class="notice glass">${ic(ev.ok ? 'check' : 'x', 'i sm')}<span>子代理 <code>${esc(ev.id)}</code> ${ev.ok ? '完成' : '失败'}</span></div>`);
      logEv('tool_result', `task ${ev.id} ${ev.ok ? 'ok' : 'err'}`);
    } else if (t === 'todos') {
      // durable state, not transcript — a replay shows it once, as a note
      const items = ev.items || [];
      if (items.length) {
        const mark = { done: 'x', in_progress: '>', pending: ' ' };
        addNote('task list:\n' + items.map(i => `- [${mark[i.status] || ' '}] ${i.content}`).join('\n'));
      }
    }
    // other event types have no transcript footprint
  }
  // orphans: tool calls with no result = interrupted
  for (const p of pendingCalls.values()) setTool(p.el, 'err', '已中断');
  foldSettled(TX);
  updateHero();
  refreshDataflow();
  delete root.dataset.replaying;
  requestAnimationFrame(() => { $('#scroller').scrollTop = $('#scroller').scrollHeight; });
}
function updateHero() {
  const empty = !TX.children.length;
  $('#hero').hidden = !empty;
  if (empty) { const h = $('#hero'); h.classList.remove('play'); void h.offsetWidth; h.classList.add('play'); }
}

/* ================= live events ================= */
function liveEvent(raw) {
  // LiveEvent is internally tagged; tuple variants (Content/Reasoning) may
  // serialize as a bare string payload or arrive in a future struct shape —
  // accept every plausible form.
  let ev = raw;
  if (typeof ev === 'string') ev = { type: 'content', text: ev };
  if (!ev || typeof ev !== 'object') return;
  const t = ev.type;
  if (!t) return; // {} — unserializable tuple variant placeholder
  const text = ev.text ?? ev.content ?? ev.delta ?? (typeof ev[0] === 'string' ? ev[0] : null);
  if (t === 'content') { if (text == null) return; if (!curText) logEv('message', 'assistant · …'); contentDelta(text); }
  else if (t === 'reasoning') { if (text == null) return; reasoningDelta(text); }
  else if (t === 'tool_start') { toolStart(ev); logEv('tool_call', `${ev.name} ${ev.summary || ''}`.slice(0, 140)); }
  else if (t === 'tool_done') { toolDone(ev); logEv('tool_result', `${ev.name} ${ev.ok ? 'ok' : 'err'}`); refreshDataflowSoon(); }
  else if (t === 'artifact') { addArtifact(ev); logEv('artifact', `${ev.name} · ${fmtBytes(ev.bytes || 0)}`); }
  else if (t === 'usage') { logEv('usage', `prompt ${nf(ev.prompt_tokens || 0)} · cache_read ${nf(ev.cache_read_input_tokens || 0)}`); refreshDataflowSoon(); }
  else if (t === 'hook') {
    if (ev.event === 'steer') {
      // a queued message just folded into the turn — its durable Message
      // renders this same user bubble on replay
      closeMsg();
      append(TX, youHTML(ev.detail || '', true, clock()));
      steerQ.shift(); renderSteerChips();
      logEv('message', 'user · ' + String(ev.detail || '').slice(0, 60));
    } else logEv('hook', `${ev.event} · ${ev.detail}`);
  }
  else if (t === 'turn_end') {
    abortTurn();
    foldSettled(TX);
    $$('.think.live').forEach(t => t.classList.remove('live'));
    refreshDataflowSoon();
    if (ev.outcome && ev.outcome !== 'completed') logEv('note', `turn_end ${ev.outcome}`);
  }
}

