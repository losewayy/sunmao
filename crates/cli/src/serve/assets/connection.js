/* ws/IPC channel + REST adapters, sessions rail, dock dataflow, view switching */
'use strict';

/* ================= websocket / IPC channel ================= */
let ws = null, wsDelay = 800, wsTimer = 0;
const projectName = p => String(p || '').split(/[\\/]/).filter(Boolean).pop() || '';
/* Under the Tauri shell the ws degrades to an IPC pair (GUI.md §8):
   session_events carries a JS Channel for host→page frames (parsed JSON
   objects on onmessage) and host_call carries page→host frames. */
function wsSend(v, onFail) {
  if (TAURI) {
    if (!connected) return false;
    // the invoke resolves as soon as the lane accepted the frame — a dead
    // IPC only surfaces here, after the caller already returned true, so
    // it can't hold the prompt. Callers that cleared a draft pass onFail
    // to restore it.
    window.__TAURI_INTERNALS__.invoke('host_call', { msg: v })
      .catch(() => { setConn(false); if (onFail) onFail(); });
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
  // A sess-scoped frame that arrives WITHOUT the tag must not be
  // attributed to whatever this tab happens to be viewing (audit-gui #13)
  // — every emitter tags, so an untagged one is already an anomaly:
  // drop it to the event log rather than mis-draw it.
  const SESS_TYPES = new Set(['live', 'note', 'approval', 'approval_done', 'steer_queue', 'input_queue', 'model', 'mode', 'effort', 'busy']);
  const sess = typeof v.sess === 'string' ? v.sess : null;
  if (SESS_TYPES.has(v.type) && sess == null) { logEv('note', `untagged ${v.type} frame dropped`); return; }
  switch (v.type) {
    case 'hello':
      sessionId = v.session || '';
      if (typeof dockSessionSwap === 'function') dockSessionSwap();
      clientId = v.client || 0;
      cwd = String(v.cwd || '').replace(/^\\\\\?\\/, '');
      slashList = v.slash || []; // [{name, desc}] — desc is the one-line zh blurb
      models = v.models || [];
      {
        const project = projectName(cwd);
        $('#df-cwd').textContent = project || '—';
        $('#df-cwd').dataset.tip = cwd;
        $('#hero-sub').textContent = project ? `在 ${project} 中开始` : '';
      }
      setApprovalMode(v.mode);
      setEffort(v.effort, v.effort_levels);
      sandboxPort = v.sandbox_port || 0;
      busySessions.clear(); (v.busy_sessions || []).forEach(id => busySessions.add(id));
      // re-attach mid-ask: rebuild the waiting badges + re-render any
      // approval card still open — renderReplay just wiped the old DOM
      waitingSessions.clear(); (v.waiting_sessions || []).forEach(id => waitingSessions.add(id));
      steerQ = v.steer || []; inputQ = v.queue || []; renderQueueChips();
      renderReplay(v.replay || []);
      // the authoritative live goal rides the hello frame — the replay
      // fold just walked the same log, so this only overwrites on drift
      if (v.goal) { curGoal = v.goal; renderGoalChip(); }
      (v.pending || []).forEach(c => approvalCard(c));
      setBusy(!!v.busy);
      driver = v.driver || '';
      refreshSessions(); refreshModels(); refreshProjects();
      refreshRoster(); refreshJobs(); refreshGrants(); HOOKS = MCPS = null;
      renderCrumb();
      break;
    case 'live':
      // roster/jobs nudges are refresh signals, not transcript content —
      // skip liveEvent (no bubble, no event-log line), just re-pull
      if (v.event && v.event.type === 'hook' && v.event.event === 'tasks.changed') {
        if (sess === sessionId) refreshRosterSoon();
        break;
      }
      if (v.event && v.event.type === 'hook' && v.event.event === 'jobs.changed') {
        if (sess === sessionId) refreshJobsSoon();
        break;
      }
      if (sess === sessionId) {
        liveEvent(v.event);
        // the first streamed row retires the hero — send() checked before
        // the echo landed (no optimistic bubble), so the empty check has
        // to ride the live path too
        updateHero();
        // output.log grows inside a running job with no further signal —
        // any live tool frame throttles a re-pull so the tail stays fresh
        if (v.event && (v.event.type === 'tool_start' || v.event.type === 'tool_done')) refreshJobsSoon();
      }
      if (v.event && v.event.type === 'turn_end' && !shellFocused()) {
        shellNotify('回合结束', sessTitle(sess) || sess);
      }
      break;
    case 'replay':
      if (v.session) { sessionId = v.session; }
      if (v.cwd) { cwd = String(v.cwd).replace(/^\\\\\?\\/, ''); $('#df-cwd').textContent = projectName(cwd) || '—'; $('#df-cwd').dataset.tip = cwd; }
      // switching sessions rebuilds the whole transcript — fadeSwap turns
      // the hard cut into a fast out/in so the swap reads as a transition
      fadeSwap($('#scroller'), () => renderReplay(v.events || []));
      setApprovalMode(v.mode);
      setEffort(v.effort, v.effort_levels);
      if (v.goal) { curGoal = v.goal; renderGoalChip(); }
      setBusy(!!v.busy);
      (v.pending || []).forEach(c => approvalCard(c));
      steerQ = v.steer || []; inputQ = v.queue || []; renderQueueChips();
      syncWait();
      driver = v.driver || '';
      refreshSessions(); refreshRoster(); refreshJobs(); refreshGrants(); HOOKS = MCPS = null; renderCrumb();
      break;
    case 'approval':
      waitingSessions.add(sess);
      if (!shellFocused()) shellNotify('需要批准', `${v.tool || ''} · ${(v.detail || '').slice(0, 80)}`);
      if (sess === sessionId) { approvalCard(v); logEv('hook', `approval requested · ${v.tool}: ${(v.detail || '').slice(0, 80)}`); }
      else {
        // a background session raised an approval — surface it as a
        // clickable toast that jumps the view to that session. One toast
        // per session at a time: a polling tool loop re-raises the same
        // approval, and stacking duplicates is pure noise
        const dup = [...$('#toasts').querySelectorAll('.toast.jump')].some(x => x.dataset.ap === sess);
        if (dup) break;
        const t = document.createElement('div'); t.className = 'toast glass jump';
        t.dataset.ap = sess;
        t.innerHTML = ic('shield-check') + `<span>待审批 · <b>${esc(v.tool || '?')}</b> · ${esc(sess)}</span>` + ic('arrow-l', 'i xs');
        t.addEventListener('click', () => { resumeSession(sess); t.classList.add('out'); setTimeout(() => t.remove(), motion.dur('fast')); });
        $('#toasts').appendChild(t);
        setTimeout(() => { t.classList.add('out'); setTimeout(() => t.remove(), motion.dur('fast')); }, motion.hold('toast-long'));
      }
      renderRail();
      break;
    case 'approval_done': {
      waitingSessions.delete(sess);
      // another tab answered first — the card here is unanswerable (the
      // kernel already forgot the id), so collapse it instead of leaving
      // a live card that would fake a verdict
      const card = pendingApprovals.get(v.id);
      if (card) {
        pendingApprovals.delete(v.id);
        const detail = $('.cmd', card) ? $('.cmd', card).textContent.trim() : '';
        const why = v.why === 'cancelled' ? '已取消' : '已由其他窗口答复';
        collapse(card, `<div class="ap-done">${stIcon('ok')}<b>已处理</b><code>${esc(detail)}</code><span>${why}</span></div>`);
        syncWait();
      }
      renderRail();
      break;
    }
    case 'note':
      if (sess === sessionId) { addNote(v.text); logEv('note', v.text); }
      break;
    case 'session': {
      // a directed switch names the issuing client — only that tab moves;
      // untagged frames (older kernels) fall back to the from-match.
      // The kernel still points `viewing` at the old session until we
      // adopt — without this a `/resume` typed in the box relabels the
      // UI but keeps prompts/approvals hitting the abandoned session.
      if (v.client != null ? v.client === clientId : (!v.from || v.from === sessionId)) {
        sessionId = v.id || sessionId;
        if (typeof dockSessionSwap === 'function') dockSessionSwap();
        // the frame itself carries no replay — clear the transcript +
        // per-session caches NOW (renderReplay([]) tears islands down and
        // wipes the DOM) so the old session's transcript / cards / chips
        // can't render against the new id in the frame → replay gap.
        // The replay repopulates everything.
        renderReplay([]);
        steerQ = []; inputQ = []; renderQueueChips();
        modelLabel = ''; $('#cmp-model').textContent = '…';
        wsSend({ type: 'view', id: sessionId });
        renderCrumb();
      }
      refreshSessions();
      break;
    }
    case 'sessions_changed': refreshSessions(); break;
    case 'schedules_changed': if (view === 'schedules') refreshSchedules(); break;
    case 'sched_fired': {
      toast(`定时任务已启动${v.name ? `：${v.name}` : ''}`, 'zap');
      if (!shellFocused()) shellNotify('定时任务已启动', v.name || v.task || '');
      break;
    }
    case 'steer_queue':
      if (sess === sessionId) { steerQ = v.items || []; renderQueueChips(); }
      break;
    case 'input_queue':
      if (sess === sessionId) { inputQ = v.items || []; renderQueueChips(); }
      break;
    case 'models_changed': refreshModels(); break;
    case 'ui_changed': loadUi(); break;
    case 'shell_changed': refreshShell(); break;
    case 'wallpaper_changed': loadCustom(true); break;
    case 'model':
      if (sess === sessionId) { modelLabel = v.label || modelLabel; $('#cmp-model').textContent = modelLabel; toast(`模型切换为 ${v.label}`, 'cpu'); }
      break;
    case 'mode':
      if (sess === sessionId) { setApprovalMode(v.mode); toast(`审批模式切换为 ${MODE_LABELS[v.mode] || v.mode}`, 'shield'); }
      break;
    case 'effort':
      if (sess === sessionId) { setEffort(v.level, v.levels); toast(`思考强度 → ${v.level || '默认'}`, 'sparkles'); }
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
    // a dead socket leaves the host adopted but this tab still viewing the
    // old session — surface it instead of silent drift
    if (!wsSend({ type: 'view', id })) toast('已接入会话，但连接断开——重连后刷新视图', 'alert', 'warn');
  }
  catch (e) { toast(`resume 失败：${e.message}`, 'alert', 'warn'); }
}
async function forkSession(id) {
  try {
    const r = await api(`/session/${encodeURIComponent(id)}/fork`, { method: 'POST' });
    if (r && r.session && !wsSend({ type: 'view', id: r.session }))
      toast('已分叉，但连接断开——重连后刷新视图', 'alert', 'warn');
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
    // loop = the session's frozen driver (ptc codemode vs standard) —
    // empty means "let --loop/manifest decide", picked in the new-chat pop
    const r = await api('/session/new', jpost({ ...(project ? { cwd: project } : {}), ...(S.loopDriver ? { loop: S.loopDriver } : {}) }));
    if (r && r.session) { sessionId = r.session; if (typeof dockSessionSwap === 'function') dockSessionSwap(); wsSend({ type: 'view', id: r.session }); }
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
/* ---- export — a GET on the session's `/md` route gives us the server's
   `commands::export::markdown` output (the SAME fold `/export-md` and the
   debug bundle ride) so the download is byte-for-byte what REPL/TUI would
   produce. The transcript the user scrolls is the folded live view; the
   download is the durable event-source. ---- */
async function exportSession(id) {
  try {
    const r = await fetch(`/session/${encodeURIComponent(id)}/md`);
    if (!r.ok) throw new Error(`${r.status}`);
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([await r.text()], { type: 'text/markdown' }));
    a.download = `sunmao-${id}.md`;
    a.click();
    // revoke on a delay — engines that hand the blob to the download
    // manager asynchronously (Firefox) read an empty file if we revoke
    // in the same task
    setTimeout(() => URL.revokeObjectURL(a.href), 10000);
    toast(`已导出 ${id}.md`, 'download');
  } catch (e) { toast(`导出失败：${e.message}`, 'alert', 'warn'); }
}
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
// `SHELL` = last GET /shell snapshot — resolved backend + which layer chose
// it; the 终端 settings page reads it, PUT /shell writes .sunmao/shell.txt.
let SHELL = null;
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
async function refreshShell() {
  try { SHELL = await api('/shell'); } catch { SHELL = null; }
  if (view === 'settings' && setPage === 'shell') renderShell();
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
  $('#df-flow').innerHTML = `<div class="df-h"><span>数据流向</span></div>` + (flowHtml || '<div class="empty-row">尚无文件/命令记录</div>');
}

/* ================= crumb / views ================= */
const SET_NAV = [['appearance', '外观', 'palette'], ['providers', '模型与提供商', 'cpu'], ['channels', 'IM 渠道', 'shield-check'], ['shell', '终端', 'terminal'], ['hooks', '钩子', 'zap'], ['mcp', 'MCP 服务器', 'blocks'], ['grants', '已授权命令', 'lock'], ['keys', '快捷键', 'keyboard'], ['about', '关于', 'info']];
function renderCrumb() {
  const c = $('#crumb');
  const interactive = view === 'session' && !!sessionId;
  let h;
  if (view === 'settings') h = `<span class="c1">设置</span><span class="cs">/</span><span class="c2">${SET_NAV.find(x => x[0] === setPage)[1]}</span>`;
  else if (view === 'schedules') h = `<span class="c1">定时任务</span>`;
  else h = `<span class="c1">${esc(cwd.split(/[\\/]/).filter(Boolean).pop() || 'sunmao')}</span><span class="cs">/</span><span class="c2">${esc(sessTitle(sessionId) || (sessionId ? '新对话' : '…'))}</span>${ic('chev-d', 'i sm')}${driver === 'ptc' ? '<span class="drv" data-tip="PTC 代码模式 — 模型经 RunCode 脚本调用工具">PTC</span>' : ''}`;
  if (interactive) {
    c.dataset.act = 'crumb';
    c.setAttribute('aria-haspopup', 'menu');
  } else {
    c.removeAttribute('data-act');
    c.removeAttribute('aria-haspopup');
  }
  c.disabled = !interactive;
  c.classList.toggle('static', !interactive);
  c.dataset.tip = interactive ? sessionId : '';
  c.innerHTML = h;
}
function show(v) {
  closePop();
  view = v; if (v !== 'settings') lastMain = v;
  app.dataset.view = v;
  $('#v-session').hidden = v !== 'session';
  $('#v-settings').hidden = v !== 'settings';
  $('#v-schedules').hidden = v !== 'schedules';
  if (v === 'schedules') refreshSchedules();
  $('#composer').hidden = v !== 'session';
  app.dataset.dock = v === 'session' && dockOn ? 'on' : 'off';
  $('#dock-btn').classList.toggle('on', dockOn && v === 'session');
  $('#rail-main').hidden = v === 'settings'; $('#rail-settings').hidden = v !== 'settings';
  renderRail(); renderCrumb();
  if (typeof brSyncAll === 'function') brSyncAll();
}
function go(v) {
  if (v === 'back') return show(lastMain);
  if (v === 'settings') { show('settings'); return settingsPage(setPage); }
  show(v);
}
function toggleRail() {
  railOn = !railOn;
  app.dataset.rail = railOn ? 'on' : 'off';
  $('#rail-btn').classList.toggle('on', railOn);
}
function toggleDock() {
  dockOn = !dockOn;
  app.dataset.dock = view === 'session' && dockOn ? 'on' : 'off';
  $('#dock-btn').classList.toggle('on', dockOn && view === 'session');
  if (typeof brSyncAll === 'function') brSyncAll();
}

/* Dock tabs: pure view swap — every pane keeps its own scroll position and
   render state; the active tab persists so the panel reopens where you left it. */
function dockTab(name) {
  const dock = $('#dock');
  if (!dock) return;
  dock.dataset.tab = name;
  $$('.dock-tab', dock).forEach(t => t.setAttribute('aria-selected', String(t.dataset.tab === name)));
  $$('.dock-pane', dock).forEach(p => { p.hidden = p.dataset.pane !== name; });
  try { localStorage.setItem('sunmao.dock.tab', name); } catch {}
  if (typeof brSyncAll === 'function') brSyncAll();
}
try { dockTab(localStorage.getItem('sunmao.dock.tab') || 'overview'); } catch {}


