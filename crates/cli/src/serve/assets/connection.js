/* ws/IPC channel + REST adapters, sessions rail, dock dataflow, view switching */
'use strict';

/* ================= websocket / IPC channel ================= */
let ws = null, wsDelay = 800, wsTimer = 0;
/* Under the Tauri shell the ws degrades to an IPC pair (GUI.md §8):
   session_events carries a JS Channel for host→page frames (parsed JSON
   objects on onmessage) and host_call carries page→host frames. */
function wsSend(v) {
  if (TAURI) {
    if (!connected) return false;
    window.__TAURI_INTERNALS__.invoke('host_call', { msg: v }).catch(() => setConn(false));
    return true;
  }
  if (ws && ws.readyState === 1) { ws.send(JSON.stringify(v)); return true; }
  return false;
}
function setConn(on) {
  connected = on;
  const d = $('#conn-dot');
  d.className = 'sd ' + (on ? 'done' : 'off');
  d.dataset.tip = on ? '已连接 ' + location.host : '未连接 · 重连中';
  $('#df-live').classList.toggle('off', !on);
}
function connect() {
  clearTimeout(wsTimer);
  if (TAURI) {
    const ch = new TAURI.Channel();
    ch.onmessage = v => route(v); // Channel delivers parsed objects
    window.__TAURI_INTERNALS__.invoke('session_events', { events: ch })
      .then(() => { const was = !connected; setConn(true); wsDelay = 800; if (was) toast('已连接 ' + location.host, 'check'); })
      .catch(() => { setConn(false); wsTimer = setTimeout(connect, wsDelay); wsDelay = Math.min(8000, wsDelay * 1.8); });
    return;
  }
  try { ws && ws.close(); } catch {}
  ws = new WebSocket(`ws://${location.host}/ws`);
  ws.onopen = () => { const was = !connected; setConn(true); wsDelay = 800; if (was) toast('已连接 ' + location.host, 'check'); };
  ws.onclose = () => { const was = connected; setConn(false); if (was) toast('与内核断开连接，重连中…', 'alert', 'warn'); wsTimer = setTimeout(connect, wsDelay); wsDelay = Math.min(8000, wsDelay * 1.8); };
  ws.onerror = () => {};
  ws.onmessage = e => {
    let v; try { v = JSON.parse(e.data); } catch { return; }
    route(v);
  };
}
function route(v) {
  // session routing: `sess` tags every host frame — transcript-shaping
  // messages (live/note/approval/replay) only apply when they name this
  // tab's viewed session; rail-facing frames (busy/approval_done/session/
  // sessions_changed) update every row.
  const sess = typeof v.sess === 'string' ? v.sess : sessionId;
  switch (v.type) {
    case 'hello':
      sessionId = v.session || '';
      clientId = v.client || 0;
      cwd = String(v.cwd || '').replace(/^\\\\\?\\/, '');
      slashList = v.slash || [];
      models = v.models || [];
      $('#df-cwd').textContent = cwd;
      $('#df-cwd').dataset.tip = cwd;
      $('#df-sess').textContent = sessionId || '—';
      $('#hero-sub').textContent = cwd;
      setApprovalMode(v.mode);
      sandboxPort = v.sandbox_port || 0;
      busySessions.clear(); (v.busy_sessions || []).forEach(id => busySessions.add(id));
      steerQ = v.steer || []; renderSteerChips();
      renderReplay(v.replay || []);
      setBusy(!!v.busy);
      refreshSessions(); refreshModels(); refreshProjects();
      renderCrumb();
      break;
    case 'live':
      if (sess === sessionId) liveEvent(v.event);
      if (v.event && v.event.type === 'turn_end' && !shellFocused()) {
        shellNotify('回合结束', sessTitle(sess) || sess);
      }
      break;
    case 'replay':
      if (v.session) { sessionId = v.session; }
      if (v.cwd) { cwd = String(v.cwd).replace(/^\\\\\?\\/, ''); $('#df-cwd').textContent = cwd; $('#df-cwd').dataset.tip = cwd; }
      $('#df-sess').textContent = sessionId || '—';
      renderReplay(v.events || []);
      setApprovalMode(v.mode);
      setBusy(!!v.busy);
      (v.pending || []).forEach(c => approvalCard(c));
      steerQ = v.steer || []; renderSteerChips();
      syncWait();
      refreshSessions(); renderCrumb();
      break;
    case 'approval':
      waitingSessions.add(sess);
      if (!shellFocused()) shellNotify('需要批准', `${v.tool || ''} · ${(v.detail || '').slice(0, 80)}`);
      if (sess === sessionId) { approvalCard(v); logEv('hook', `approval requested · ${v.tool}: ${(v.detail || '').slice(0, 80)}`); }
      else {
        // a background session raised an approval — surface it as a
        // clickable toast that jumps the view to that session
        const t = document.createElement('div'); t.className = 'toast glass jump';
        t.innerHTML = ic('shield-check') + `<span>待审批 · <b>${esc(v.tool || '?')}</b> · ${esc(sess)}</span>` + ic('arrow-l', 'i xs');
        t.addEventListener('click', () => { resumeSession(sess); t.remove(); });
        $('#toasts').appendChild(t);
        setTimeout(() => { t.classList.add('out'); setTimeout(() => t.remove(), motion.dur('fast')); }, motion.hold('toast-long'));
      }
      renderRail();
      break;
    case 'approval_done':
      waitingSessions.delete(sess);
      renderRail();
      break;
    case 'note':
      if (sess === sessionId) { addNote(v.text); logEv('note', v.text); }
      break;
    case 'session': {
      // a directed switch names the issuing client — only that tab moves;
      // untagged frames (older kernels) fall back to the from-match
      if (v.client != null ? v.client === clientId : (!v.from || v.from === sessionId)) {
        sessionId = v.id || sessionId;
        renderCrumb();
      }
      refreshSessions();
      break;
    }
    case 'sessions_changed': refreshSessions(); break;
    case 'steer_queue':
      if (sess === sessionId) { steerQ = v.items || []; renderSteerChips(); }
      break;
    case 'models_changed': refreshModels(); break;
    case 'model':
      if (sess === sessionId) { modelLabel = v.label || modelLabel; $('#cmp-model').textContent = modelLabel; toast(`模型切换为 ${v.label}`, 'cpu'); }
      break;
    case 'mode':
      if (sess === sessionId) { setApprovalMode(v.mode); toast(`审批模式切换为 ${MODE_LABELS[v.mode] || v.mode}`, 'shield'); }
      break;
    case 'busy':
      busySessions[v.busy ? 'add' : 'delete'](sess);
      if (sess === sessionId) setBusy(v.busy); else renderRail();
      break;
    // MCP Apps bridge reply — route the kernel's result back into the
    // island that asked (keyed by artifact name + rpc id)
    case 'ui_result': {
      const el = pendingUi.get(`${v.name}#${v.id}`);
      if (el) {
        pendingUi.delete(`${v.name}#${v.id}`);
        const msg = v.error != null
          ? { jsonrpc: '2.0', id: v.id, error: { code: -32000, message: String(v.error) } }
          : { jsonrpc: '2.0', id: v.id, result: v.result };
        postToIsland(el, msg);
      }
      break;
    }
  }
}

/* ================= REST ================= */
async function api(path, opts) {
  const r = await fetch(path, opts);
  if (!r.ok) throw new Error(`${r.status} ${await r.text().then(t => t.slice(0, 160)).catch(() => '')}`);
  return r.json();
}
// OS notification via the shell — no-op outside the Tauri window.
// Focus tracking: document.hasFocus() can lie in some webviews, so blur
// events demote the flag and focus restores it.
let shellFocus = true;
const shellFocused = () => shellFocus && (typeof document.hasFocus !== 'function' || document.hasFocus());
const shellNotify = (title, body) => { if (TAURI && TAURI.notify) TAURI.notify(title, body).catch(() => {}); };
if (TAURI) {
  window.addEventListener('blur', () => { shellFocus = false; });
  window.addEventListener('focus', () => { shellFocus = true; });
}
async function resumeSession(id) {
  if (!id || id === sessionId) return;
  try {
    await api(`/session/${encodeURIComponent(id)}/resume`, { method: 'POST' });
    wsSend({ type: 'view', id }); // adopt → re-point this tab (replay follows)
  }
  catch (e) { toast(`resume 失败：${e.message}`, 'alert', 'warn'); }
}
async function forkSession(id) {
  try {
    const r = await api(`/session/${encodeURIComponent(id)}/fork`, { method: 'POST' });
    if (r && r.session) wsSend({ type: 'view', id: r.session });
    toast(`已分叉 ${id}`, 'fork');
  }
  catch (e) { toast(`fork 失败：${e.message}`, 'alert', 'warn'); }
}
async function newChat(project) {
  if (view !== 'session') show('session');
  // no explicit project → the *current* session's root (Ctrl N keeps you
  // in the same project)
  project = project === undefined ? cwd : project;
  try {
    const r = await api('/session/new', jpost(project ? { cwd: project } : {}));
    if (r && r.session) { sessionId = r.session; wsSend({ type: 'view', id: r.session }); }
    renderCrumb(); refreshSessions();
  } catch (e) { toast(`新会话失败：${e.message}`, 'alert', 'warn'); }
}
async function renameSession(id, title) {
  try {
    await api(`/session/${encodeURIComponent(id)}/rename`, jpost({ title }));
    const m = SESSION_META[id] || (SESSION_META[id] = {});
    m.title = title; renderRail(); renderCrumb();
    toast('已重命名', 'pen');
  } catch (e) { toast(`重命名失败：${e.message}`, 'alert', 'warn'); }
}
async function deleteSession(id) {
  try {
    await api(`/session/${encodeURIComponent(id)}`, { method: 'DELETE' });
    delete SESSION_META[id];
    SESSION_IDS = SESSION_IDS.filter(x => x !== id);
    renderRail();
    toast('已删除会话', 'trash');
  } catch (e) { toast(`删除失败：${e.message}`, 'alert', 'warn'); }
}
/* ---- export — fold the session's durable events into Markdown and
   download it. Reads GET /session/{id}/events (not the live transcript)
   so dormant sessions export identically. ---- */
function mdToolArgs(call) {
  try { const a = JSON.parse((call.function || {}).arguments || '{}'); return JSON.stringify(a, null, 2); }
  catch { return String((call.function || {}).arguments || ''); }
}
function sessionMarkdown(id, events) {
  let title = (SESSION_META[id] && SESSION_META[id].title) || '';
  const out = [];
  for (const ev of events || []) {
    const t = ev.type;
    if (t === 'session_meta' && ev.title) title = ev.title;
    else if (t === 'message') {
      const m = ev.message || {};
      if (m.role === 'user') {
        const c = m.content || '';
        if (c.startsWith('[hook context]') || c.startsWith('<local-shell>')) continue;
        out.push(`## user\n\n${c}\n`);
      } else if (m.role === 'assistant' && m.content) {
        out.push(`## assistant\n\n${m.content}\n`);
      }
    } else if (t === 'tool_call') {
      const c = ev.call || {}, name = (c.function || {}).name || '?';
      out.push(`\`\`\`tool-call\n${name} ${mdToolArgs(c).replace(/\n+/g, ' ')}\n\`\`\`\n`);
    } else if (t === 'tool_result') {
      out.push(`\`\`\`tool-result\n${ev.ok === false ? '[error] ' : ''}${ev.output || ''}\n\`\`\`\n`);
    } else if (t === 'local_shell') {
      out.push(`\`\`\`shell\n$ ${ev.command || ''}\n${ev.output || ''}\n[exit ${ev.exit_code}]\n\`\`\`\n`);
    } else if (t === 'compacted') {
      out.push(`> [context compacted] ${ev.summary || ''}\n`);
    } else if (t === 'artifact') {
      out.push(`> artifact: \`${ev.name}.html\` (${fmtBytes(ev.bytes || 0)})\n`);
    }
  }
  return `# ${title || id}\n\n> session \`${id}\` — exported from the event log\n\n${out.join('\n')}`;
}
async function exportSession(id) {
  try {
    const v = await api(`/session/${encodeURIComponent(id)}/events`);
    const md = sessionMarkdown(id, v.events || []);
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([md], { type: 'text/markdown' }));
    a.download = `sunmao-${id}.md`;
    a.click();
    URL.revokeObjectURL(a.href);
    toast(`已导出 ${id}.md`, 'download');
  } catch (e) { toast(`导出失败：${e.message}`, 'alert', 'warn'); }
}
// project picker for 新对话 — known projects (launch dir + registry) plus
// freeform input; a path that exists on disk becomes the session's root
let PROJECTS = null;
async function refreshProjects() {
  try { PROJECTS = (await api('/projects')).projects || []; } catch { PROJECTS = null; }
}
function newChatPop(el) {
  if (popAnchor === el) return closePop();
  const list = (PROJECTS || [cwd]).filter(Boolean);
  pop(el, `<div class="lbl">新对话的项目目录</div><div class="field"><input id="np-in" placeholder="输入路径，回车创建" spellcheck="false" autocomplete="off"></div><div class="mp-list scroll">` +
    list.map(p => `<button class="mi" data-v="${esc(p)}">${ic('folder')}<span class="mt mono"><span>${esc(p.split(/[\\/]/).filter(Boolean).pop() || p)}</span><small>${esc(p)}</small></span>${p === cwd ? ic('check', 'i sm ck') : ''}</button>`).join('') +
    `</div>`, { place: 'bottom', cls: 'models', onMount(p) {
      const inp = $('#np-in', p);
      inp.addEventListener('keydown', e => {
        if (e.key !== 'Enter') return;
        e.preventDefault();
        const v = inp.value.trim();
        closePop(); newChat(v || cwd);
      });
      p.addEventListener('click', e => { const b = e.target.closest('.mi'); if (!b) return; closePop(); newChat(b.dataset.v); });
      setTimeout(() => inp.focus(), 20);
    } });
}
let SESSION_PROJ = {}; // session id → display project path
async function refreshSessions() {
  try {
    const v = await api('/sessions');
    SESSION_PROJ = {};
    SESSION_IDS = (Array.isArray(v.sessions) ? v.sessions : []).map(r => {
      const o = typeof r === 'string' ? { id: r } : r;
      SESSION_PROJ[o.id] = o.project || '';
      return o.id;
    });
    SESSION_META = v.meta && typeof v.meta === 'object' ? v.meta : {};
    renderRail(); renderCrumb();
    railSearch(); // an open rail query re-runs against the fresh list
  } catch {}
}
let railTimer = 0;
$('#rail-q').addEventListener('input', () => { clearTimeout(railTimer); railTimer = setTimeout(railSearch, DEBOUNCE_SEARCH); });
$('#rail-q').addEventListener('keydown', e => {
  if (e.key !== 'Enter') return;
  e.preventDefault();
  const first = $('#sessions [data-sess]');
  if (first) resumeSession(first.dataset.sess);
});
let dfTimer = 0;
function refreshDataflowSoon() { clearTimeout(dfTimer); dfTimer = setTimeout(refreshDataflow, DEBOUNCE_DATAFLOW); }
async function refreshDataflow() {
  try { renderDock(await api('/dataflow?sess=' + encodeURIComponent(sessionId))); }
  catch { $('#df-sub').textContent = '无可读会话日志'; }
}

/* ================= providers / models ================= */
// `MODELS` = last GET /models snapshot — providers (keys redacted),
// routes, completable selectors. The picker renders from it; the settings
// page edits it through PUT /models.
let MODELS = null;
const jput = v => ({ method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(v) });
const jpost = v => ({ method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(v) });
async function refreshModels() {
  try {
    MODELS = await api('/models');
    models = MODELS.selectors || [];
  } catch { MODELS = null; }
  if (view === 'settings' && setPage === 'providers') renderProviders();
  if (popEl && popEl.classList.contains('models')) renderModelRows(popEl, $('#mp-in') ? $('#mp-in').value.trim() : '');
}
const provNames = () => Object.keys((MODELS && MODELS.providers) || {}).sort();
// compose the PUT body: current file with `name` replaced/removed/added.
// `api_key` is only written when the form actually collected one — a blank
// field means "keep".
function modelsBody(edit) {
  const providers = {};
  for (const [n, p] of Object.entries((MODELS && MODELS.providers) || {})) {
    providers[n] = { base_url: p.base_url, dialect: p.dialect || 'openai', catalog: p.catalog || [] };
    if (p.api_key_env) providers[n].api_key_env = p.api_key_env;
  }
  if (edit) {
    if (edit.del) delete providers[edit.name];
    else {
      providers[edit.name] = { base_url: edit.base_url, dialect: edit.dialect, catalog: edit.setCatalog || (edit.keepCatalog ? (MODELS.providers[edit.name] || {}).catalog || [] : []) };
      if (edit.api_key) providers[edit.name].api_key = edit.api_key;
      else if (edit.keepKey && MODELS.providers[edit.name] && MODELS.providers[edit.name].api_key_env) providers[edit.name].api_key_env = MODELS.providers[edit.name].api_key_env;
    }
  }
  return { providers, routes: (MODELS && MODELS.routes) || {} };
}
async function saveProviders(edit, ok) {
  try {
    MODELS = await api('/models', jput(modelsBody(edit)));
    models = MODELS.selectors || [];
    if (ok) toast(ok, 'check');
    if (view === 'settings' && setPage === 'providers') renderProviders();
  } catch (e) { toast(`保存失败：${e.message}`, 'alert', 'warn'); }
}
async function fetchCatalog(name) {
  toast('正在拉取模型列表…', 'reset');
  try {
    const r = await api('/models/fetch', jpost({ provider: name }));
    const cat = r.catalog || [];
    if (!cat.length) return toast('该 provider 返回了空列表', 'alert', 'warn');
    const cur = ((MODELS.providers[name] || {}).catalog) || [];
    const merged = [...cat];
    for (const m of cur) if (!merged.some(c => c.id === m.id)) merged.push(m);
    await saveProviders({ name, base_url: MODELS.providers[name].base_url, dialect: MODELS.providers[name].dialect, keepKey: true, keepCatalog: false, setCatalog: merged }, `已拉取 ${cat.length} 个模型`);
  } catch (e) { toast(`拉取失败：${e.message}`, 'alert', 'warn'); }
}

/* ================= dock ================= */
function renderDock(d) {
  const tok = d.tokens || {}, hit = tok.cache_hit_pct || 0;
  // no model call yet → a dash, not a loud "0%" that reads as a failure
  const idle = !(tok.prompt || tok.completion);
  $('#df-hit').textContent = idle ? '–' : hit + '%';
  $('#df-hit').classList.toggle('idle', idle);
  $('#df-sub').textContent = idle ? '尚无模型调用' : `缓存读取 ${nf(tok.cache_read || 0)} tokens`;
  $('#df-bar').style.width = Math.min(100, hit) + '%';
  $('#df-prompt').textContent = nf(tok.prompt || 0);
  $('#df-comp').textContent = nf(tok.completion || 0);
  $('#df-tools').textContent = String(d.tool_calls || 0);
  $('#df-fails').textContent = String(d.tool_failures || 0);
  $('#df-compact').textContent = String(d.compactions || 0);
  const flow = d.data_flow || {};
  const grp = (icon, label, items) => items && items.length
    ? `<div class="fl-g"><div class="fl-t">${ic(icon)}${label}<b>${items.length}</b></div>${items.slice(-8).map(x => `<div class="fl-i" data-tip="${esc(x)}">${esc(x)}</div>`).join('')}</div>` : '';
  const flowHtml = grp('file', '读取', flow.files_read) + grp('file-pen', '写入', flow.files_written) + grp('terminal', '命令', flow.shell_commands);
  $('#df-flow').innerHTML = `<div class="df-h"><span>数据流向</span><span class="mono">${esc(sessionId)}.jsonl</span></div>` + (flowHtml || '<div class="empty-row">尚无文件/命令记录</div>');
}

/* ================= rail / crumb / views ================= */
// title = first typed prompt (GET /sessions meta); a session that has no
// prompt yet reads as 新对话 rather than leaking its raw id
const sessTitle = id => (SESSION_META[id] && SESSION_META[id].title) || '';
const dayStart = t => { const d = new Date(t); d.setHours(0, 0, 0, 0); return d.getTime(); };
function sessWhen(ms) {
  if (!ms) return '';
  const d = new Date(ms), today = dayStart(Date.now());
  if (ms >= today) return pad(d.getHours()) + ':' + pad(d.getMinutes());
  if (ms >= today - 6 * 864e5) return '周' + '日一二三四五六'[d.getDay()];
  return (d.getMonth() + 1) + '/' + d.getDate();
}
function sessBucket(ms) {
  const today = dayStart(Date.now());
  if (!ms || ms >= today) return '今天';
  if (ms >= today - 864e5) return '昨天';
  if (ms >= today - 6 * 864e5) return '近 7 天';
  return '更早';
}
const sessRow = id => {
  const on = id === sessionId, run = busySessions.has(id) || (on && busy), wait = waitingSessions.has(id);
  const title = sessTitle(id), m = SESSION_META[id] || {};
  const proj = SESSION_PROJ[id] || '';
  // a session running in another project wears its project name — the
  // same-project majority stays clean
  const foreign = proj && cwd && proj !== cwd ? `<span class="tag">${esc(proj.split(/[\\/]/).filter(Boolean).pop() || proj)}</span>` : '';
  const state = wait ? '<i class="sd wait" aria-label="等待批准"></i>' : run ? '<i class="sd run" aria-label="运行中"></i>' : `<span class="when">${sessWhen(m.mtime)}</span>`;
  return `<button class="row sess${on ? ' on' : ''}" data-sess="${esc(id)}" data-tip="${esc((proj ? proj + ' · ' : '') + (title ? title + '|' + id : id))}" data-tip-side="right"><span class="t${title ? '' : ' untitled'}">${esc(title || '新对话')}</span>${foreign}${state}</button>`;
};
/* ---- rail search — ≥2 chars greps every session log's message content
   server-side (GET /sessions?q=); shorter input filters the rail by
   title/id client-side. railHits: null = list mode, else the last
   endpoint payload. ---- */
let railHits = null, railQ = 0;
const DEBOUNCE_SEARCH = 250;
function railSearch() {
  const q = ($('#rail-q') && $('#rail-q').value || '').trim();
  if (q.length < 2) { railHits = null; renderRail(); return; }
  const seq = ++railQ;
  railHits = null; // keep the filtered list while the query flies
  fetch(`/sessions?q=${encodeURIComponent(q)}`)
    .then(r => r.ok ? r.json() : Promise.reject())
    .then(v => { if (seq !== railQ) return; railHits = (v.sessions || []); renderRail(); })
    .catch(() => { if (seq === railQ) { railHits = null; renderRail(); } });
}
function railRowWithHits(h) {
  const id = h.id;
  SESSION_META[id] = SESSION_META[id] || {};
  if (h.title) SESSION_META[id].title = h.title;
  const snips = (h.hits || []).map(s => `<div class="snip" data-sess="${esc(id)}">${esc(s)}</div>`).join('');
  return sessRow(id) + `<div class="snips">${snips}</div>`;
}
function renderRail() {
  let html = '', last = '';
  // a session nobody typed into is just an opened-and-left window: keep it
  // out of the rail (the palette still lists every id) unless it's the
  // current one or has something running / waiting
  const q = ($('#rail-q') && $('#rail-q').value || '').trim().toLowerCase();
  if (railHits) {
    $('#sessions').innerHTML = railHits.map(railRowWithHits).join('') ||
      `<div class="empty-hint">没有匹配 “${esc(q)}” 的会话</div>`;
    $$('.nav-i[data-go]').forEach(b => b.classList.toggle('on', b.dataset.go === view));
    return;
  }
  const shown = SESSION_IDS.filter(id => {
    if (q && !(sessTitle(id) + ' ' + id).toLowerCase().includes(q)) return false;
    return sessTitle(id) || id === sessionId || busySessions.has(id) || waitingSessions.has(id);
  });
  for (const id of shown) {
    const b = sessBucket((SESSION_META[id] || {}).mtime);
    if (b !== last) { html += `<div class="grp"><span>${b}</span></div>`; last = b; }
    html += sessRow(id);
  }
  $('#sessions').innerHTML = html || '<div class="empty-hint">暂无会话记录</div>';
  $$('.nav-i[data-go]').forEach(b => b.classList.toggle('on', b.dataset.go === view));
}
const SET_NAV = [['appearance', '外观', 'palette'], ['providers', '模型与提供商', 'cpu'], ['keys', '快捷键', 'keyboard'], ['about', '关于', 'info']];
function renderCrumb() {
  const c = $('#crumb');
  let h;
  if (view === 'settings') h = `<span class="c1">设置</span><span class="cs">/</span><span class="c2">${SET_NAV.find(x => x[0] === setPage)[1]}</span>`;
  else h = `<span class="c1">${esc(cwd.split(/[\\/]/).filter(Boolean).pop() || 'sunmao')}</span><span class="cs">/</span><span class="c2">${esc(sessTitle(sessionId) || (sessionId ? '新对话' : '…'))}</span>${ic('chev-d', 'i sm')}`;
  c.dataset.tip = view === 'settings' ? '' : sessionId;
  c.innerHTML = h;
}
function show(v) {
  closePop();
  view = v; if (v !== 'settings') lastMain = v;
  app.dataset.view = v;
  $('#v-session').hidden = v !== 'session';
  $('#v-settings').hidden = v !== 'settings';
  $('#composer').hidden = v !== 'session';
  app.dataset.dock = v === 'session' && dockOn ? 'on' : 'off';
  $('#dock-btn').classList.toggle('on', dockOn && v === 'session');
  $('#rail-main').hidden = v === 'settings'; $('#rail-settings').hidden = v !== 'settings';
  renderRail(); renderCrumb();
}
function go(v) {
  if (v === 'back') return show(lastMain);
  if (v === 'settings') { show('settings'); return settingsPage(setPage); }
  show(v);
}
function toggleDock() {
  dockOn = !dockOn;
  app.dataset.dock = view === 'session' && dockOn ? 'on' : 'off';
  $('#dock-btn').classList.toggle('on', dockOn && view === 'session');
}


