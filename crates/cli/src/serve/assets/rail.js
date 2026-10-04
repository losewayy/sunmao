/* sessions rail — project picker popover, list refresh, search, and the
   two list modes (time buckets / project groups). Split off
   connection.js under the shape budget; globals ($, S, api, pop …)
   resolve through the shared script scope. */
'use strict';

/* ================= project picker (新对话 ∨) ================= */
// The nav button itself creates in the CURRENT dir — this popover is the
// opt-in "elsewhere" entry: exec mode, known projects, a 浏览 row that
// pops the OS-native folder dialog (Tauri bridge under the shell;
// /fs/pick makes the same native dialog for a plain browser), then a
// freeform path input.
let PROJECTS = null;
async function refreshProjects() {
  try { PROJECTS = (await api('/projects')).projects || []; } catch { PROJECTS = null; }
}
// host-native folder pick — the modal lives on the serve's machine
async function pickDirNative() {
  if (TAURI && TAURI.pickDir) return TAURI.pickDir(cwd);
  const v = await api('/fs/pick?dir=' + encodeURIComponent(cwd));
  return v.path || null;
}
function newChatPop(el) {
  if (popAnchor === el) return closePop();
  const list = (PROJECTS || [cwd]).filter(Boolean);
  pop(el, `<div class="lbl">在其他项目中新建</div><div class="np-modes"><span class="np-ml">执行模式</span>` +
    [['', '自动', '项目清单或启动参数决定'], ['full', '标准', '工具逐个声明给模型'], ['ptc', 'PTC', '代码模式 — 模型只拿 RunCode/SearchTools，脚本内调工具']].map(([v, t, d]) => `<button class="seg${S.loopDriver === v ? ' on' : ''}" data-drv="${v}" data-tip="${d}">${t}</button>`).join('') +
    `</div><div class="mp-list scroll" id="np-list">` +
    `<button class="mi" data-browse="1">${ic('folder')}<span class="mt"><span>浏览文件夹…</span><small>系统目录选择器</small></span></button>` +
    list.map(p => `<button class="mi" data-v="${esc(p)}">${ic('folder')}<span class="mt mono"><span>${esc(projectName(p) || p)}</span><small>${esc(p)}</small></span>${p === cwd ? ic('check', 'i sm ck') : ''}</button>`).join('') +
    `</div><div class="field"><input id="np-in" placeholder="或直接输入路径，回车创建" spellcheck="false" autocomplete="off"></div>`,
    { place: 'bottom', cls: 'models', onMount(p) {
      const inp = $('#np-in', p);
      inp.addEventListener('keydown', e => {
        if (e.key !== 'Enter') return;
        e.preventDefault();
        const v = inp.value.trim();
        closePop(); newChat(v || cwd);
      });
      p.addEventListener('click', e => {
        const seg = e.target.closest('[data-drv]');
        if (seg) { S.loopDriver = seg.dataset.drv; save(); $$('.seg', p).forEach(b => b.classList.toggle('on', b.dataset.drv === seg.dataset.drv)); return; }
        if (e.target.closest('[data-browse]')) {
          closePop();
          return pickDirNative()
            .then(path => { if (path) newChat(path); })
            .catch(er => toast(`目录选择失败：${er.message || er}`, 'alert', 'warn'));
        }
        const sel = e.target.closest('[data-v]');
        if (!sel) return;
        closePop(); newChat(sel.dataset.v);
      });
      setTimeout(() => inp.focus(), 20);
    } });
}

/* ================= session list ================= */
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
const sessRow = (id, showProj = true) => {
  const on = id === sessionId, run = busySessions.has(id) || (on && busy), wait = waitingSessions.has(id);
  const title = sessTitle(id), m = SESSION_META[id] || {};
  const proj = SESSION_PROJ[id] || '';
  // a session running in another project wears its project name — the
  // same-project majority stays clean; grouped mode suppresses the tag
  // entirely (the group header already names it)
  const foreign = showProj && proj && cwd && proj !== cwd ? `<span class="tag">${esc(proj.split(/[\\/]/).filter(Boolean).pop() || proj)}</span>` : '';
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
  if (S.railGroup === 'project') {
    // 按项目分桶：当前项目置顶，其余按组内最新会话排；组头可折叠，
    // 折叠态记在 S.railFold（ui.json 持久化）
    const groups = new Map(), fold = S.railFold || {};
    for (const id of shown) {
      const p = SESSION_PROJ[id] || cwd || '';
      if (!groups.has(p)) groups.set(p, []);
      groups.get(p).push(id);
    }
    const recent = ids => Math.max(0, ...ids.map(id => (SESSION_META[id] || {}).mtime || 0));
    const ordered = [...groups].sort((a, b) =>
      a[0] === cwd ? -1 : b[0] === cwd ? 1 : recent(b[1]) - recent(a[1]));
    for (const [p, ids] of ordered) {
      html += `<button class="grp proj${fold[p] ? ' fold' : ''}" data-fold="${esc(p)}" data-tip="${esc(p)}" data-tip-side="right">${ic('chev-d', 'i sm')}<span class="gn">${esc(projectName(p) || p || '未分组')}</span><b>${ids.length}</b></button>`;
      if (!fold[p]) html += ids.map(id => sessRow(id, false)).join('');
    }
  } else {
    for (const id of shown) {
      const b = sessBucket((SESSION_META[id] || {}).mtime);
      if (b !== last) { html += `<div class="grp"><span>${b}</span></div>`; last = b; }
      html += sessRow(id);
    }
  }
  $('#sessions').innerHTML = html || '<div class="empty-hint">暂无会话记录</div>';
  $$('.nav-i[data-go]').forEach(b => b.classList.toggle('on', b.dataset.go === view));
  syncRailMode();
}
// the rail-mode toggle's icon names where a click TAKES you: the grid
// means "group by project", the clock "back to the time list"
function syncRailMode() {
  const b = $('#rail-mode');
  if (!b) return;
  const proj = S.railGroup === 'project';
  b.innerHTML = ic(proj ? 'clock' : 'blocks');
  b.dataset.tip = proj ? '按时间排列' : '按项目分组';
  b.classList.toggle('on', proj);
}

/* rail resize — right edge drags set --w-rail on #app so .stage's calc
   follows live; S.railW persists per project (same seam as the dock's) */
(function railResize() {
  const edge = $('#rail-edge'), rail = $('#rail'), app = $('#app');
  edge.addEventListener('pointerdown', e => {
    e.preventDefault();
    edge.setPointerCapture(e.pointerId);
    app.dataset.dragging = '1';
    const move = ev => {
      const w = Math.max(180, Math.min(innerWidth * 0.4, ev.clientX - rail.getBoundingClientRect().left));
      app.style.setProperty('--w-rail', w + 'px');
      S.railW = Math.round(w);
    };
    const up = () => {
      delete app.dataset.dragging;
      edge.removeEventListener('pointermove', move);
      edge.removeEventListener('pointerup', up);
      save();
      if (typeof brSyncAll === 'function') brSyncAll();
    };
    edge.addEventListener('pointermove', move);
    edge.addEventListener('pointerup', up);
  });
})();
