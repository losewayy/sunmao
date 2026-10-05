/* transcript blocks, live fold, approvals, event log, replay — markdown
   render lives in md.js (highlight/mdInline/mdRender/mdTable/texMath) */
'use strict';

/* ================= transcript blocks ================= */
const stIcon = st => `<span class="st ${st}">${ic(st === 'ok' ? 'check' : st === 'err' ? 'x' : 'rotate')}</span>`;
// `o.body` is pre-rendered HTML (Edit/Write diff preview from diff.js) —
// unlike `out` it lands unescaped; only ever pass what editPreviewHTML built.
function toolHTML([st, nm, sm, tm, out], o = {}) {
  const time = st === 'run' ? `<span class="tm live" data-start="${o.start ?? performance.now()}">0.0s</span>` : st === 'wait' ? `<span class="tm w">${t('等待批准')}</span>` : `<span class="tm${st === 'err' && tm && !/\d/.test(tm) ? ' e' : ''}">${tm || ''}</span>`;
  const body = o.body || '';
  const chev = (out || body) ? ic('chev-r', 'i xs chev') : '';
  return `<div class="tool${st === 'err' ? ' bad' : ''}${o.cls ? ' ' + o.cls : ''}"><button class="tool-h">${stIcon(st)}<span class="nm">${esc(nm)}</span><span class="sum">${esc(sm)}</span>${time}${chev}</button>${body || out ? `<div class="tool-o">${body}${out ? `<pre>${esc(out)}</pre>` : ''}</div>` : ''}</div>`;
}
// replayed prompts carry no timestamp in the log — only a live send shows
// the wall clock, rather than stamping history with "now"
// turnSeq mirrors the log's turn_boundaries scan — the Nth stamped
// .msg.you is what `POST /rewind {turn:N}` trims to
let turnSeq = 0;
const rwBtn = `<button class="ib rw" data-act="rewind-turn" data-tip="${esc(t('回退到这条之前 · 同时还原改过的文件'))}" aria-label="${esc(t('回退到这条之前'))}">${ic('reset', 'i sm')}</button>`;
// turn: number → stamped + rewind button · 'pending' → awaits the kernel's
// turn_boundary event (echoes of slash commands/vetoed prompts never get
// one — no boundary, no rewind) · falsy → plain bubble
const youHTML = (text, anim, at, turn) => `<div class="msg you"${turn === 'pending' ? ' data-pending' : turn ? ` data-turn="${turn}"` : ''}><div class="msg-h${anim ? ' enter' : ''}"><i class="dot"></i><span>${t('你')}</span>${at ? `<time>${at}</time>` : ''}${turn && turn !== 'pending' ? rwBtn : ''}</div><div class="bubble glass${anim ? ' enter' : ''}">${esc(text)}</div></div>`;
// the kernel committed a user-message boundary: stamp the oldest pending
// you-bubble with its ordinal (and mount the rewind control), else the
// ordinal is bookkeeping only — turnSeq still tracks for the replay path
const stampBoundary = (n) => {
  turnSeq = Math.max(turnSeq, n);
  const p = TX.querySelector('.msg.you[data-pending]');
  if (!p) return;
  p.dataset.turn = n;
  p.removeAttribute('data-pending');
  p.querySelector('.msg-h')?.insertAdjacentHTML('beforeend', rwBtn);
};
// message.content may be a bare string (old logs) or a block array
// [{type:'text'|'image',…}] — the same normalization the kernel's
// content_text() applies. msgText flattens for text surfaces; msgParts
// keeps the image blocks so the transcript can render real thumbs.
function msgParts(m) {
  const c = m && m.content;
  if (typeof c === 'string') return c ? [{ type: 'text', text: c }] : [];
  if (Array.isArray(c)) return c.filter(b => b && (b.type === 'text' || b.type === 'image'));
  return [];
}
const attBase = p => String(p || '').split(/[\\/]/).pop();
function msgText(m) { return msgParts(m).map(b => b.type === 'text' ? b.text : `[image: ${attBase(b.path)}]`).join('\n'); }
// a stored attachment's serving URL — same dir POST /attachments wrote into
function attURL(p) { return '/attachments/' + encodeURIComponent(attBase(p)) + '?sess=' + encodeURIComponent(sessionId); }
const attImgs = m => msgParts(m).filter(b => b.type === 'image')
  .map(b => `<img class="att" src="${attURL(b.path)}" alt="${esc(attBase(b.path))}" title="${esc(b.path)}">`).join('');
const botHead = anim => `<div class="msg-h${anim ? ' enter' : ''}"><i class="dot"></i><span>${brand()}</span>${modelLabel ? `<span class="model">${esc(modelLabel.replace(/^global:/, ''))}</span>` : ''}</div>`;
const TX = $('#tx');
// force=false by default — streamed output must not yank the scroller
// while the user reads earlier output; only own bubbles/settle force it
function append(parent, html, force) { const t = document.createElement('template'); t.innerHTML = html.trim(); const first = t.content.firstElementChild; parent.appendChild(t.content); keepBottom(!!force); return first; }
function keepBottom(force) { const sc = $('#scroller'); if (force || sc.scrollHeight - sc.scrollTop - sc.clientHeight < 240) sc.scrollTop = sc.scrollHeight; }
/* ---- work cards: a run of tool calls (+ the short narration between
   them) shares one card; 3+ calls earn a summary header and fold once
   the run settles. Failed calls stay visible. ---- */
const TG_MANY = 3, STEP_MAX = 400;
function toolGroup(host) {
  const last = host.lastElementChild;
  if (last && last.classList.contains('tools')) return last;
  return append(host, `<div class="tools glass"><button class="tg-h" data-act="tg" aria-expanded="true">${ic('chev-r', 'i xs chev')}<span class="tg-s"></span><span class="tg-bad"></span><span class="tg-n"></span></button></div>`);
}
const TG_VERBS = [
  [/^(Read)$/, n => t('读取 {n} 个文件', { n })],
  [/^(Grep|Glob)$/, n => t('搜索 {n} 次', { n })],
  [/^(Bash|shell|!)$/, n => t('运行 {n} 条命令', { n })],
  [/^(Edit|Write)$/, n => t('修改 {n} 个文件', { n })],
  [/^(WebFetch)$/, n => t('抓取 {n} 个网页', { n })],
  [/^(Task)$/, n => t('派出 {n} 个子代理', { n })],
  [/^(TodoWrite)$/, () => t('更新待办')],
  [/^(HtmlArtifact)$/, n => t('生成 {n} 个工件', { n })],
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
  $('.tg-bad', g).textContent = bad ? t('{n} 失败', { n: bad }) : '';
  $('.tg-n', g).textContent = live ? t('进行中') : t('{n} 步', { n: tools.length });
}
function foldGroup(g, on) {
  if (g.classList.contains('fold') === on) return;
  const h0 = g.offsetHeight;
  g.classList.toggle('fold', on);
  const h = $('.tg-h', g); if (h) h.setAttribute('aria-expanded', String(!on));
  motion.height(g, h0, g.offsetHeight, 'base');
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
  if (out) {
    const o = $('.tool-o', el);
    if (o) { const p = $('pre', o); p ? p.textContent = out : o.insertAdjacentHTML('beforeend', `<pre>${esc(out)}</pre>`); } // a diff body keeps the result under it
    else { h.insertAdjacentHTML('beforeend', ic('chev-r', 'i xs chev')); el.insertAdjacentHTML('beforeend', `<div class="tool-o"><pre>${esc(out)}</pre></div>`); }
  }
}
setInterval(() => { for (const t of $$('.tm.live')) t.textContent = ((performance.now() - +t.dataset.start) / 1000).toFixed(1) + 's'; }, 100);

/* ================= transcript live state ================= */
let curMsg = null;      // open .msg.bot element
let curBubble = null;   // streaming bubble element inside curMsg
let curText = '';       // accumulated markdown for curBubble
let curThink = null, curThinkText = '';
const runningTools = [];   // {el, name, lane, depth, t0}
// pendingApprovals + the card lifecycle live in approvals.js
// the session's standing goal — SessionEvent::Goal (replay) and
// LiveEvent::goal frames both fold into this; a goal keeps the top strip
// open even while idle so its round counter stays visible
let curGoal = null;

function msgHost() {
  if (!curMsg) { curMsg = append(TX, `<div class="msg bot">${botHead(true)}</div>`); curMsg.classList.add('enter'); }
  return curMsg;
}
function closeMsg() { sealThink(); curMsg = curBubble = null; curText = ''; }
// one fold = one contiguous reasoning stream — any later event seals it;
// the next reasoning delta opens a NEW fold in timeline position
function sealThink() {
  if (!curThink) return;
  curThink.previousElementSibling?.classList.remove('live');
  curThink = null; curThinkText = '';
}
function bubble() {
  const host = msgHost();
  if (!curBubble) { curBubble = append(host, '<div class="bubble glass enter"></div>'); curText = ''; }
  return curBubble;
}
function contentDelta(t) {
  if (!t) return;
  sealThink(); // reply text seals the reasoning stream that preceded it
  const b = bubble();
  curText += t;
  b.innerHTML = mdRender(curText);
  keepBottom();
}
function reasoningDelta(txt) {
  if (!txt) return;
  const host = msgHost();
  if (!curThink) curThink = append(host, `<button class="think glass live enter" data-act="think">${ic('chev-r', 'i xs')}${t('思考')}</button><div class="think-o glass"></div>`).nextElementSibling, curThinkText = '';
  curThinkText += txt;
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
  // Edit/Write cards get a live diff/preview from the call's args payload
  const el = append(g, toolHTML(['run', name, ev.summary || '', '', ''], { cls: 'enter', start: performance.now(), body: editPreviewHTML(ev.name, ev.args) }));
  refreshGroup(g);
  runningTools.push({ el, name: ev.name, lane: ev.lane || 0, depth: ev.depth || 0, call_id: ev.call_id || null, t0: performance.now() });
  curBubble = null; sealThink(); // next text/reasoning opens fresh blocks in timeline position
}
function toolDone(ev) {
  // exact join key first — same-name calls in one turn mispair without it;
  // (name, lane, depth) FIFO stays the fallback for id-less synthetic events
  let i = ev.call_id ? runningTools.findIndex(t => t.call_id === ev.call_id) : -1;
  if (i < 0) i = runningTools.findIndex(t => t.name === ev.name && t.lane === (ev.lane || 0) && t.depth === (ev.depth || 0));
  if (i < 0) i = runningTools.findIndex(t => t.name === ev.name);
  // NO unconditional `i = 0` — an unmatched tool_done must not splice the
  // oldest card (the real finisher would spin until turn_end); let it
  // fall to the synthetic-done branch below
  const t = i < 0 ? undefined : runningTools[i];
  if (!t) {
    const host = msgHost();
    let g = toolGroup(host);
    append(g, toolHTML([ev.ok ? 'ok' : 'err', ev.name || '?', '', '', capOut(ev.output || '')], { cls: 'enter' }));
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
      <button class="notes-pill" data-act="annotate" data-tip="${esc(t('批注|点击页面元素或拖拽选区域'))}">${ic('note', 'i xs')}<span>${t('批注')}</span></button>
      <span class="revs" hidden>
        <button class="ib" data-act="rev-prev" data-tip="${esc(t('上一版本'))}" aria-label="${esc(t('上一版本'))}">${ic('chev-l')}</button>
        <span class="rev-n">v1/1</span>
        <button class="ib" data-act="rev-next" data-tip="${esc(t('下一版本'))}" aria-label="${esc(t('下一版本'))}">${ic('chev-r')}</button>
      </span>
      <span class="sp"></span>
      <span class="meta">${fmtBytes(ev.bytes || 0)}</span>
      <button class="ib" data-act="island-tall" data-tip="${esc(t('展开'))}" aria-label="${esc(t('展开'))}">${ic('expand')}</button>
      <button class="ib" data-act="island-open" data-tip="${esc(t('在浏览器中打开|沙箱内脚本禁用 · 禁网'))}" aria-label="${esc(t('在浏览器中打开'))}">${ic('external')}</button>
    </div>
    <iframe title="${esc(nm)}" sandbox="allow-same-origin" loading="lazy" src="/artifacts/${encodeURIComponent(nm)}?sess=${encodeURIComponent(sessionId)}"></iframe>
    <div class="notes"></div>
  </div>`;
}
function addArtifact(ev) {
  const host = msgHost();
  const el = append(host, islandHTML(ev));
  el.classList.add('enter');
  curBubble = null; sealThink();
  refreshNotes(el, ev.name);
  refreshRevs(el, ev.name, ev.rev || 0);
  probeApp(el, ev.name);
}

function addNote(text) {
  sealThink(); append(TX, `<div class="note-line glass enter">${esc(text)}</div>`);
}
function setBusy(on) {
  busy = !!on;
  if (!busy) clearStopping();
  $('#cmp-busy').hidden = !busy;
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy && !curGoal;
  // one button morphs instead of swapping two: busy Enter queues the
  // message for the next turn while Ctrl+Enter steers mid-turn, so the
  // click target flips to cancel
  const btn = $('#send-btn');
  btn.dataset.act = busy ? 'stop' : 'send';
  btn.classList.toggle('stop', busy);
  btn.disabled = false;
  btn.setAttribute('data-tip', busy ? t('停止生成 · 引导仍可用 Ctrl+Enter') : t('发送|Enter · 引导|Ctrl+Enter'));
  btn.setAttribute('aria-label', busy ? t('停止生成') : t('发送'));
  $('use', btn).setAttribute('href', busy ? '#i-square' : '#i-arrow-up');
  renderRail();
}

/* ---- goal chip — mirrors the TUI footer chip: objective (truncated),
   status, round counter. `complete`/`abandoned` still pin it until a
   later event clears curGoal. */
const GOAL_STATUS = { in_progress: t('进行中'), complete: t('已完成'), blocked: t('受阻'), abandoned: t('已放弃') };
function renderGoalChip() {
  const el = $('#cmp-goal');
  if (!curGoal) { el.hidden = true; el.textContent = ''; return; }
  const obj = curGoal.objective || '';
  const cut = [...obj].slice(0, 18).join('') + ([...obj].length > 18 ? '…' : '');
  el.innerHTML = `<b>◎</b>${esc(cut)} · ${GOAL_STATUS[curGoal.status] || curGoal.status} · ${t('轮 {n}/{m}', { n: curGoal.rounds, m: curGoal.max_rounds })}`;
  el.hidden = false;
}
// one fold for both surfaces — a live `goal` event and a `goal` session
// event carry the same {goal:{...}} payload and land identically
function applyGoalEvent(g, log) {
  const prev = curGoal;
  curGoal = g;
  renderGoalChip();
  $('#cmp-top').hidden = pendingApprovals.size === 0 && !busy && !curGoal;
  if (log) logEv('goal', `${g.status} · ${t('轮 {n}/{m}', { n: g.rounds, m: g.max_rounds })}`);
  return !prev || prev.status !== g.status || prev.objective !== g.objective;
}

/* ================= event log ================= */
function logEv(type, detail) {
  EVLOG.push([clock(true), type, detail]);
  if (EVLOG.length > 500) EVLOG.splice(0, EVLOG.length - 500);
  if (popEl && popEl.classList.contains('events')) popEl.innerHTML = eventsHTML();
}
const EV_LABELS = { started: t('开始'), message: t('对话'), tool_call: t('工具调用'), tool_result: t('工具结果'), approval: t('审批'), artifact: t('生成内容'), usage: t('用量'), hook: t('状态'), note: t('记录'), goal: t('目标') };
function eventsHTML() {
  return `<div class="ev-h">${t('执行记录')}<span>${t('{n} 条', { n: EVLOG.length })}</span></div><div class="ev-list scroll">${EVLOG.map(e => `<div class="ev-l"><span class="t">${e[0]}</span><span class="e ${e[1]}">${esc(EV_LABELS[e[1]] || e[1])}</span><span class="d">${esc(e[2])}</span></div>`).join('') || '<div class="empty-row">' + t('暂无记录') + '</div>'}</div>`;
}

/* ================= replay ================= */
// `/clear` + replay teardown share this — wiping TX alone leaves
// pendingApprovals (the 等待批准 bar), runningTools timers and curMsg
// alive, so a later tool_done splices a stale entry
function clearTranscript() {
  // same teardown a replay runs — /clear must release island resources
  // and drop bookkeeping, not just the pixels
  for (const isl of $$('.island')) teardownIsland(isl);
  TX.innerHTML = '';
  pendingApprovals.clear(); runningTools.length = 0;
  closeMsg(); syncWait(); updateHero();
}

function renderReplay(events, anim) {
  // replay must not re-run entrance motion — §8.1.5: flag on, replay, flag off
  root.dataset.replaying = '';
  // app islands about to be dropped get a resource-teardown first —
  // SEP-1865 says the View releases its side on this request
  for (const isl of $$('.island')) teardownIsland(isl);
  TX.innerHTML = ''; EVLOG.length = 0; turnSeq = 0;
  pendingApprovals.clear(); runningTools.length = 0;
  closeMsg(); syncWait();
  // the goal is session state the replay re-derives — wipe it here so a
  // goal-less log can't inherit the previous view's chip
  curGoal = null; renderGoalChip();
  const pendingCalls = new Map(); // call_id -> tool element
  for (const ev of events || []) {
    const ty = ev.type;
    if (ty === 'started') {
      modelLabel = ev.model || modelLabel;
      $('#cmp-model').textContent = modelLabel || '…';
      // chrome, not transcript — model + cwd already live in the composer;
      // the event log keeps the fact. An empty session shows the hero.
      logEv('started', t('会话已开始'));
    } else if (ty === 'message') {
      const m = ev.message || {};
      if (m.role === 'system') continue;
      if (m.role === 'user') {
        const c = msgText(m);
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
        append(TX, youHTML(c, anim, undefined, c ? ++turnSeq : 0), true);
        const imgs = attImgs(m);
        if (imgs) append(TX, `<div class="msg you enter"><div class="att-row">${imgs}</div></div>`);
        logEv('message', t('你：') + c.slice(0, 60));
      } else if (m.role === 'assistant') {
        const c = msgText(m);
        if (c) { const h = msgHost(); append(h, `<div class="bubble glass">${mdRender(c)}</div>`); }
        logEv('message', t('{b}：', { b: brand() }) + c.slice(0, 60) + (m.tool_calls ? ` (+${m.tool_calls.length} calls)` : ''));
      } else if (m.role === 'tool') {
        continue; // paired tool_result covers it
      }
    } else if (ty === 'tool_call') {
      const c = ev.call || {};
      const fn = (c.function || {});
      let sum = '', a = null;
      try { a = JSON.parse(fn.arguments || '{}'); sum = a.command || a.path || a.pattern || a.name || a.prompt || fn.arguments.slice(0, 120); } catch { sum = String(fn.arguments || '').slice(0, 120); }
      const host = msgHost();
      absorbStep(host);
      let g = toolGroup(host);
      const el = append(g, toolHTML(['run', (ev.depth ? '↳ ' : '') + (fn.name || '?'), sum, '', ''], { body: editPreviewHTML(fn.name, a) }));
      // key on (call.id, depth, lane) — relayed sub-agent calls can share a
      // call id namespace across lanes; missing id falls back to a unique key
      pendingCalls.set(c.id ? `${ev.depth || 0}:${ev.lane || 0}:${c.id}` : Symbol(),
        { el, name: fn.name || '?', depth: ev.depth || 0, lane: ev.lane || 0 });
      logEv('tool_call', `${fn.name || '?'} ${sum}`.slice(0, 140));
    } else if (ty === 'tool_result') {
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
    } else if (ty === 'compacted') {
      // compaction boundary — earlier history was summarized away, so the
      // transcript clears exactly like the TUI's blocks.clear()
      TX.innerHTML = ''; closeMsg(); runningTools.length = 0; pendingCalls.clear();
      addNote(`[context compacted]${ev.summary ? '\n' + ev.summary : ''}`);
      logEv('note', 'compacted');
    } else if (ty === 'artifact') {
      addArtifact(ev);
      logEv('artifact', `${ev.name} · ${fmtBytes(ev.bytes || 0)}`);
    } else if (ty === 'usage') {
      const u = ev.usage || ev;
      logEv('usage', `prompt ${nf(u.prompt_tokens || 0)} · cache_read ${nf(u.cache_read_input_tokens || 0)}`);
    } else if (ty === 'hook') {
      logEv('hook', `${ev.event} · ${ev.detail}`);
    } else if (ty === 'local_shell') {
      const host = msgHost();
      let g = toolGroup(host);
      append(g, toolHTML([ev.exit_code === 0 ? 'ok' : 'err', 'shell', '$ ' + (ev.command || ''), 'exit ' + ev.exit_code, capOut(ev.output || '')]));
      logEv('tool_call', '! ' + (ev.command || ''));
    } else if (ty === 'ptc_call') {
      // a RunCode script's nested call — durable start+done in one fact;
      // renders as a done-state row one ↳ depth in from the RunCode card
      const host = msgHost();
      const g = toolGroup(host);
      let a = null, sum = '';
      try { a = JSON.parse(ev.args || '{}'); sum = a.command || a.path || a.pattern || a.name || a.prompt || String(ev.args).slice(0, 120); } catch { sum = String(ev.args || '').slice(0, 120); }
      append(g, toolHTML([ev.ok ? 'ok' : 'err', '↳ ' + (ev.name || '?'), sum, '', capOut(ev.output || '')], { body: editPreviewHTML(ev.name, a) }));
      logEv('tool_result', `↳ ${ev.name || '?'} ${ev.ok ? 'ok' : 'err'}`);
    } else if (ty === 'session_meta') {
      // rename fact — rail title override; audit-visible like mode_change,
      // no transcript row
      logEv('hook', `session.rename · ${ev.title || ''}`);
    } else if (ty === 'mode_change') {
      // audit row in the TUI — on this side the event log is the audit
      // surface, so the fold only re-syncs the chip + logs the fact
      setApprovalMode(ev.mode);
      logEv('hook', `approval.mode · ${ev.mode}`);
    } else if (ty === 'turn_mode_change') {
      // standard ↔ fusion — audit row, same posture as mode_change
      logEv('hook', `turn.mode · ${ev.mode}`);
    } else if (ty === 'fusion_spec') {
      // the delegation spec is the audit spine — surface it as a note so
      // "what the Lead asked" is readable without expanding the evlog
      const goal = (ev.spec && (ev.spec.goal || ev.spec.objective)) || '(delegation)';
      addNote(`fusion spec #${ev.seq} → ${ev.sidekick || '?'}\n${goal}`);
      logEv('hook', `fusion.spec #${ev.seq} · ${ev.sidekick || ''}`);
    } else if (ty === 'fusion_accepted') {
      logEv('hook', `fusion.accepted #${ev.spec_seq} · ${ev.sidekick || ''}`);
    } else if (ty === 'fusion_escalated') {
      addNote(`fusion escalated #${ev.spec_seq} — ${ev.reason || ''}`);
      logEv('hook', `fusion.escalated #${ev.spec_seq} · ${ev.reason || ''}`);
    } else if (ty === 'task_done') {
      const host = msgHost();
      append(host, `<div class="notice glass enter">${ic(ev.ok ? 'check' : 'x', 'i sm')}<span>${t(ev.ok ? '子代理 {id} 完成' : '子代理 {id} 失败', { id: `<code>${esc(ev.id)}</code>` })}</span></div>`);
      logEv('tool_result', `task ${ev.id} ${ev.ok ? 'ok' : 'err'}`);
    } else if (ty === 'todos') {
      // durable state, not transcript — a replay shows it once, as a note
      const items = ev.items || [];
      if (items.length) {
        const mark = { done: 'x', in_progress: '>', pending: ' ' };
        addNote('task list:\n' + items.map(i => `- [${mark[i.status] || ' '}] ${i.content}`).join('\n'));
      }
    } else if (ty === 'goal' && ev.goal) {
      // status/objective flips are transcript-worthy; round bumps just
      // re-render the chip — otherwise a long run's replay is all noise
      if (applyGoalEvent(ev.goal, true)) {
        addNote(`goal · ${GOAL_STATUS[ev.goal.status] || ev.goal.status}: ${ev.goal.objective || ''}` +
          (ev.goal.status === 'blocked' && ev.goal.blocker ? ` — ${ev.goal.blocker}` : ''));
      }
    }
    // other event types have no transcript footprint
  }
  // orphans: tool calls with no result = interrupted
  for (const p of pendingCalls.values()) setTool(p.el, 'err', t('已中断'));
  foldSettled(TX);
  updateHero();
  refreshDataflow();
  findRefresh();
  delete root.dataset.replaying;
  requestAnimationFrame(() => { $('#scroller').scrollTop = $('#scroller').scrollHeight; });
}
function updateHero() {
  // transition-only: live frames arrive per-token while a session is still
  // empty — re-running the play scene on each would strobe the mark
  const h = $('#hero'), empty = !TX.children.length;
  if (h.hidden === !empty) return;
  if (empty) { h.hidden = false; h.classList.remove('out', 'play'); void h.offsetWidth; h.classList.add('play'); return; }
  h.classList.add('out'); // the 560ms staged entrance earns a fade, not a cut
  setTimeout(() => { if (!h.classList.contains('play')) h.hidden = true; }, motion.dur('fast'));
}

/* ================= live events ================= */
function liveEvent(raw) {
  // LiveEvent is internally tagged; tuple variants (Content/Reasoning) may
  // serialize as a bare string payload or arrive in a future struct shape —
  // accept every plausible form.
  let ev = raw;
  if (typeof ev === 'string') ev = { type: 'content', text: ev };
  if (!ev || typeof ev !== 'object') return;
  const ty = ev.type;
  if (!ty) return; // {} — unserializable tuple variant placeholder
  const text = ev.text ?? ev.content ?? ev.delta ?? (typeof ev[0] === 'string' ? ev[0] : null);
  if (ty === 'content') { if (text == null) return; if (!curText) logEv('message', t('{b}：…', { b: brand() })); contentDelta(text); }
  else if (ty === 'reasoning') { if (text == null) return; reasoningDelta(text); }
  else if (ty === 'tool_start') { toolStart(ev); logEv('tool_call', `${ev.name} ${ev.summary || ''}`.slice(0, 140)); }
  else if (ty === 'tool_done') { toolDone(ev); logEv('tool_result', `${ev.name} ${ev.ok ? 'ok' : 'err'}`); refreshDataflowSoon(); }
  else if (ty === 'artifact') { addArtifact(ev); logEv('artifact', `${ev.name} · ${fmtBytes(ev.bytes || 0)}`); }
  // the durable-fact mirrors — liveEvent must render the same row a replay
  // of this exact log would (audit-gui #11): compacted clears, todos and
  // task_done land as their note/notice rows
  else if (ty === 'compacted') {
    TX.innerHTML = ''; pendingApprovals.clear(); runningTools.length = 0;
    closeMsg(); syncWait();
    addNote(`[context compacted]${ev.summary ? '\n' + ev.summary : ''}`);
    logEv('note', 'compacted');
  }
  else if (ty === 'task_done') {
    const host = msgHost();
    sealThink(); append(host, `<div class="notice glass enter">${ic(ev.ok ? 'check' : 'x', 'i sm')}<span>${t(ev.ok ? '子代理 {id} 完成' : '子代理 {id} 失败', { id: `<code>${esc(ev.id)}</code>` })}</span></div>`);
    logEv('tool_result', `task ${ev.id} ${ev.ok ? 'ok' : 'err'}`);
  }
  // the kernel's live mirror of the durable user message — emitted when a
  // prompt is accepted as a queued/started turn. The composer no longer
  // optimistically appends, so THIS row is the only bubble a fresh turn
  // gets; a busy kernel still lands it via hook:steer.
  else if (ty === 'user_message') {
    // same fold order as replay's `message` arm — text bubble first,
    // thumbs row after
    const m = { role: 'user', content: ev.content || [] };
    const c = msgText(m);
    if (c) { closeMsg(); append(TX, youHTML(c, true, clock(), 'pending'), true); }
    const imgs = attImgs(m);
    if (imgs) append(TX, `<div class="msg you enter"><div class="att-row">${imgs}</div></div>`);
    logEv('message', t('你：') + c.slice(0, 60));
  }
  else if (ty === 'turn_boundary') { stampBoundary(ev.ordinal || 0); }
  else if (ty === 'todos') {
    const items = ev.items || [];
    if (items.length) {
      const mark = { done: 'x', in_progress: '>', pending: ' ' };
      addNote('task list:\n' + items.map(i => `- [${mark[i.status] || ' '}] ${i.content}`).join('\n'));
    }
  }
  else if (ty === 'goal' && ev.goal) {
    if (applyGoalEvent(ev.goal, true)) {
      addNote(`goal · ${GOAL_STATUS[ev.goal.status] || ev.goal.status}: ${ev.goal.objective || ''}` +
        (ev.goal.status === 'blocked' && ev.goal.blocker ? ` — ${ev.goal.blocker}` : ''));
    }
  }
  else if (ty === 'usage') { logEv('usage', `prompt ${nf(ev.prompt_tokens || 0)} · cache_read ${nf(ev.cache_read_input_tokens || 0)}`); refreshDataflowSoon(); }
  else if (ty === 'hook') {
    if (ev.event === 'steer') {
      // a queued message just folded into the turn — its durable Message
      // renders this same user bubble on replay
      closeMsg();
      append(TX, youHTML(ev.detail || '', true, clock(), ev.detail ? 'pending' : 0), true);
      steerQ.shift(); renderQueueChips();
      logEv('message', t('你：') + String(ev.detail || '').slice(0, 60));
    } else if (ev.event === 'force_stop') {
      // the kernel gave up waiting for a cooperative exit and dropped the
      // round — never let a hard stop read as an ordinary stop
      closeMsg();
      addNote('⛔ ' + t('已强制停止') + (ev.detail ? ` · ${ev.detail}` : ''));
      toast(t('已强制停止'), 'alert', 'warn');
      logEv('hook', `force_stop · ${ev.detail}`);
    } else logEv('hook', `${ev.event} · ${ev.detail}`);
  }
  else if (ty === 'turn_end') {
    abortTurn();
    foldSettled(TX);
    $$('.think.live').forEach(t => t.classList.remove('live'));
    refreshDataflowSoon();
    findRefresh();
    if (ev.outcome && ev.outcome !== 'completed') logEv('note', `turn_end ${ev.outcome}`);
  }
}
