/* settings → CSS bridge and settings pages (appearance/providers/keys/about) */
'use strict';

/* ================= settings → CSS ================= */
const UI_FONTS = { 'Segoe UI': '"Segoe UI Variable Text", "Segoe UI"' };
const UI_FALLBACK = ', "PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", system-ui, sans-serif';
const CODE_FALLBACK = ', "Cascadia Mono", ui-monospace, "SFMono-Regular", Consolas, "Microsoft YaHei UI", monospace';
const effMode = () => S.mode === 'system' ? (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark') : S.mode;
function apply() {
  const light = effMode() === 'light', st = root.style;
  root.dataset.theme = light ? 'light' : 'dark';
  const bg = light ? '#F4F5F8' : S.background, fg = light ? '#1C1E28' : S.foreground;
  const b2 = light ? '#5D6278' : '#8B8FA5', b3 = light ? '#9095A8' : '#565A70', k = (S.contrast - 50) / 50;
  st.setProperty('--c-text', fg);
  st.setProperty('--c-text-2', k >= 0 ? mix(b2, fg, k * .45) : mix(b2, bg, -k * .4));
  st.setProperty('--c-text-3', k >= 0 ? mix(b3, fg, k * .35) : mix(b3, bg, -k * .35));
  st.setProperty('--c-accent', S.accent);
  st.setProperty('--glass-rgb', hex2rgb(bg).join(' '));
  st.setProperty('--glass-a', Math.min(.95, S.panelOpacity + (light ? .14 : 0)).toFixed(3));
  st.setProperty('--blur', S.blur + 'px');
  st.setProperty('--veil', light ? `rgba(238,240,245,${(.34 + S.dim * .6).toFixed(3)})` : `rgba(6,8,12,${S.dim.toFixed(3)})`);
  const sa = .075 + k * .045;
  st.setProperty('--c-stroke', light ? `rgba(20,24,40,${(sa - .005).toFixed(3)})` : `rgba(255,255,255,${sa.toFixed(3)})`);
  st.setProperty('--font-ui', `${UI_FONTS[S.fonts.ui] || `"${S.fonts.ui}"`}, -apple-system, BlinkMacSystemFont${UI_FALLBACK}`);
  st.setProperty('--font-mono', `"${S.fonts.code}"${CODE_FALLBACK}`);
  root.dataset.motion = S.motion;
  root.dataset.motionEff = S.motion === 'system' ? (matchMedia('(prefers-reduced-motion: reduce)').matches ? 'reduce' : 'full') : S.motion;
  $('#rail').classList.toggle('solid', !S.translucentSidebar);
  /* panel geometry belongs here too — a cached/bogus S.dockW written at
     script-init would otherwise poison --w-dock: width:var() falls back to
     auto (dock shrinks wide) and calc() dies (stage right:0 → composer
     slides under the dock). sanitize heals it on every apply */
  const app = $('#app');
  const dw = Math.max(240, Math.min(innerWidth * 0.5, +S.dockW || 0));
  const rw = Math.max(180, Math.min(innerWidth * 0.4, +S.railW || 0));
  if (dw) app.style.setProperty('--w-dock', Math.round(dw) + 'px');
  if (rw) app.style.setProperty('--w-rail', Math.round(rw) + 'px');
  paintWall();
  syncSettingsUI();
}
function commit() { apply(); save(); }


/* ================= settings pages ================= */
function renderWallGrid() {
  const g = $('#wgrid'); if (!g || view !== 'settings' || setPage !== 'appearance') return;
  const items = WALLS.map(x => ({ id: x.id, name: x.name })).concat(customImg ? [{ id: 'custom', name: t('自定义') }] : []);
  g.innerHTML = items.map(x => `<button class="wt${S.wallpaper === x.id ? ' on' : ''}" data-wall="${x.id}"><span class="th"><canvas></canvas>${x.id === 'custom' ? `<i class="wt-x" data-wx data-tip="${t('移除自定义壁纸')}">${ic('x')}</i>` : ''}<span class="chk">${ic('check')}</span></span><span class="nm2">${x.name}</span></button>`).join('')
    + `<button class="wt up" data-act="upload-wall"><span class="th">${ic('image-plus')}<em>${t('上传图片')}</em></span><span class="nm2">JPG · PNG</span></button>`;
  const dpr = Math.min(2, devicePixelRatio || 1);
  $$('.wt[data-wall]', g).forEach(b => { const c = $('canvas', b), r = c.getBoundingClientRect(); c.width = Math.round((r.width || 112) * dpr); c.height = Math.round((r.height || 70) * dpr); const ctx = c.getContext('2d'); drawWall(ctx, c.width, c.height, b.dataset.wall); });
}
function setRange(sel, v, f) { const el = $(sel); el.value = v; el.style.setProperty('--_p', ((v - el.min) / (el.max - el.min) * 100) + '%'); const o = el.parentElement.querySelector('output'); if (o) o.textContent = f(v); }
function syncSettingsUI() {
  if (view !== 'settings' || setPage !== 'appearance') return;
  $$('.tc').forEach(b => b.classList.toggle('on', b.dataset.mode === S.mode));
  $('#sys-now').textContent = matchMedia('(prefers-color-scheme: light)').matches ? t('当前浅色') : t('当前深色');
  setRange('#rg-op', Math.round(S.panelOpacity * 100), v => v + '%');
  setRange('#rg-blur', S.blur, v => v + 'px');
  setRange('#rg-dim', Math.round(S.dim * 100), v => v + '%');
  setRange('#rg-con', S.contrast, v => String(v));
  for (const k of ['accent', 'background', 'foreground']) $('#pv-' + k).innerHTML = `<i class="cdot" style="background:${S[k]}"></i>${S[k].toUpperCase()}`;
  $('#pv-uif').innerHTML = esc(S.fonts.ui) + ic('chev-d');
  $('#pv-codef').innerHTML = esc(S.fonts.code) + ic('chev-d');
  $('#sw-side').setAttribute('aria-checked', String(S.translucentSidebar));
  $('#pv-motion').innerHTML = esc(t({ system: '跟随系统', full: '完整', reduce: '减少' }[S.motion] || S.motion)) + ic('chev-d');
  $('#pv-lang').innerHTML = esc(langLabel()) + ic('chev-d');
  $$('.wt[data-wall]').forEach(b => b.classList.toggle('on', b.dataset.wall === S.wallpaper));
}
$('#rg-op').addEventListener('input', e => { S.panelOpacity = +e.target.value / 100; commit(); });
$('#rg-blur').addEventListener('input', e => { S.blur = +e.target.value; commit(); });
$('#rg-dim').addEventListener('input', e => { S.dim = +e.target.value / 100; commit(); });
$('#rg-con').addEventListener('input', e => { S.contrast = +e.target.value; commit(); });

const SWATCH = { accent: ['#339CFF', '#2BB3A3', '#4CB782', '#5B7CFA', '#E5608A', '#A0A6B8'], background: ['#16181F', '#1A1A17', '#121A1F', '#0F1115', '#1B1822', '#202329'], foreground: ['#E8E9F0', '#F2EFE8', '#DDE3EA', '#FFFFFF', '#CDD3DE', '#E9E4D8'] };
const CLABEL = { accent: t('强调色'), background: t('背景'), foreground: t('前景') };
function colorPop(anchor, key) {
  if (popAnchor === anchor) return closePop();
  const cur = S[key].toUpperCase();
  pop(anchor, `<div class="lbl">${CLABEL[key]}</div><div class="sws">${SWATCH[key].map(c => `<button class="swt${c === cur ? ' on' : ''}" data-c="${c}" style="--_c:${c}" aria-label="${c}"></button>`).join('')}</div><div class="field"><i class="cdot" style="background:${cur}"></i><input value="${cur}" maxlength="7" spellcheck="false" aria-label="${t('十六进制色值')}"></div><div class="hint">${t('输入 #RRGGBB 后按 Enter')}</div>`, { align: 'end', onMount(p) {
    const setC = c => { S[key] = c.toUpperCase(); commit(); $$('.swt', p).forEach(b => b.classList.toggle('on', b.dataset.c === S[key])); $('.cdot', p).style.background = S[key]; $('input', p).value = S[key]; };
    p.addEventListener('click', e => { const b = e.target.closest('.swt'); if (b) setC(b.dataset.c); });
    const inp = $('input', p);
    inp.addEventListener('keydown', e => { if (e.key !== 'Enter') return; const v = inp.value.trim(); if (/^#?[0-9a-f]{6}$/i.test(v)) setC(v[0] === '#' ? v : '#' + v); else toast(t('色值格式应为 #RRGGBB'), 'alert', 'warn'); });
  } });
}
const fontCache = {};
function hasFont(f) {
  if (f in fontCache) return fontCache[f];
  const c = document.createElement('canvas').getContext('2d'), s = 'mmmmmmmmmmlli1WW0O 汉字测试';
  const m = b => { c.font = `72px ${b}`; return c.measureText(s).width; };
  return fontCache[f] = f === 'system-ui' || ['monospace', 'serif', 'sans-serif'].some(b => m(`"${f}", ${b}`) !== m(b));
}
const FONTS = { ui: ['HarmonyOS Sans SC', 'Segoe UI', 'Microsoft YaHei UI', 'system-ui'], code: ['Maple Mono CN', 'Maple Mono', 'JetBrains Mono', 'Cascadia Mono', 'Consolas', 'SF Mono'] };
function fontPop(anchor, key) {
  menuPop(anchor, [{ label: key === 'ui' ? t('UI 字体') : t('代码字体') }, ...FONTS[key].map(f => ({ v: f, t: f, on: f === S.fonts[key], d: hasFont(f) ? '' : t('未安装，将回退到下一个字体'), style: key === 'code' ? `font-family:'${f}',monospace` : `font-family:'${f}',sans-serif` }))], v => { S.fonts[key] = v; commit(); }, { align: 'end' });
}
function motionPop(anchor) {
  const OPTS = [{ v: 'system', t: t('跟随系统') }, { v: 'full', t: t('完整') }, { v: 'reduce', t: t('减少') }];
  menuPop(anchor, [{ label: t('动效') }, ...OPTS.map(m => Object.assign({}, m, { on: m.v === S.motion }))], v => { S.motion = v; commit(); }, { align: 'end' });
}

/* model picker — the live model first, then routes, then each provider's
   catalog. Enter picks the typed selector verbatim (provider/id or bare id
   resolve through the kernel's ModelResolver); rows carry capability
   badges. A provider with no catalog offers the pull, never a fake id. */
const ctxLen = n => typeof n === 'number' && Number.isFinite(n)
  ? (n >= 1000 ? Math.round(n / 1000) + 'k' : String(n))
  : (n ? String(n) : '');
const MOD_MARK = { image: t('图'), audio: t('音'), video: t('视'), file: t('档') };
const modMarks = m => ((m.input_modalities && m.input_modalities.length) ? m.input_modalities : (m.vision ? ['image'] : [])).map(k => MOD_MARK[k] || k).join('');
function renderModelRows(p, q) {
  q = (q || '').toLowerCase();
  const rows = [];
  // what this session is running on comes first — the picker's job is to
  // state the live model, not to ask for it again
  if (modelLabel && (!q || modelLabel.toLowerCase().includes(q)))
    rows.push(`<div class="lbl">${t('当前')}</div><div class="mi cur"><span class="mt mono"><span>${esc(modelLabel)}</span><small>${t('本会话正在使用')}</small></span>${ic('check', 'i sm ck')}</div>`);
  const routes = (MODELS && MODELS.routes) || {};
  for (const [r, chain] of Object.entries(routes)) {
    const sel = '@' + r, d = Array.isArray(chain) ? chain.join(' → ') : String(chain);
    if (!q || (sel + ' ' + d).toLowerCase().includes(q)) rows.push(`<button class="mi" data-v="${esc(sel)}">${ic('zap')}<span class="mt mono"><span>${esc(sel)}</span><small>${esc(d)}</small></span></button>`);
  }
  for (const n of provNames()) {
    const p0 = MODELS.providers[n], cat = p0.catalog || [];
    const shown = cat.filter(m => !q || `${n}/${m.id}`.toLowerCase().includes(q));
    // name only: the picker's job is to say which model, and the capability
    // fields (window, output cap, thinking ladder, tool flag) turn a list you
    // scan into one you have to read — they live in 设置 › 模型, where there
    // is room to explain them
    const listed = shown.map(m => `<button class="mi" data-v="${esc(n + '/' + m.id)}"><span class="mt mono"><span>${esc(n + '/' + m.id)}</span></span></button>`).join('');
    if (listed) rows.push(`<div class="lbl">${esc(n)} · ${t('{n} 个模型', { n: cat.length })}</div>` + listed);
  }
  $('.mp-list', p).innerHTML = rows.join('') || `<div class="hint">${t('无匹配 — 输入 provider/model 或模型 id，回车直接切换')}</div>`;
}
function modelPop(el) {
  if (popAnchor === el) return closePop();
  pop(el, `<div class="field"><input id="mp-in" placeholder="${t('provider/model 或 @route — 回车切换')}" spellcheck="false" autocomplete="off"></div><div class="mp-list scroll"></div>`, { place: 'top', align: 'end', cls: 'models', onMount(p) {
    const inp = $('#mp-in', p);
    renderModelRows(p, '');
    inp.addEventListener('input', () => renderModelRows(p, inp.value.trim()));
    inp.addEventListener('keydown', e => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const v = inp.value.trim(); if (!v) return;
      closePop(); wsSend({ type: 'model', sel: v });
    });
    p.addEventListener('click', e => {
      const b = e.target.closest('.mi'); if (!b || !b.dataset.v) return;
      closePop(); wsSend({ type: 'model', sel: b.dataset.v });
    });
    setTimeout(() => inp.focus(), 20);
  } });
}

/* effort picker — the level the ACTIVE session runs at. Rows are exactly
   what the model's catalog advertises, and the checked one is the level in
   force: there is no "default" row, because a session always has one (the
   kernel starts a model at the second-strongest rung it declares). A
   catalog that declares nothing gets a hint instead of a choice; freeform
   stays a slash affair (`/effort <anything>` passes through verbatim). */
function effortPop(el) {
  const items = [
    { label: t('思考强度') },
    ...effortLevels.map(l => ({ v: l, t: l, mono: true, on: l === effortLevel })),
  ];
  if (!effortLevels.length) items.push({ label: t('catalog 未声明思考档位 — 可用 /effort <level> 直接设置') });
  menuPop(el, items, v => wsSend({ type: 'effort', level: v }), { place: 'top', align: 'end' });
}

const mono = t => `<span class="pill plain">${esc(t)}</span>`;
const row = (b, s, ctl) => `<div class="cr"><div class="l"><b>${b}</b>${s ? `<span>${s}</span>` : ''}</div>${ctl}</div>`;
const card = rows => `<div class="card glass cfg">${rows.join('')}</div>`;
const sec = (h, p, body) => `<div class="sec">${h ? `<div class="sec-h"><h2>${h}</h2>${p ? `<p>${p}</p>` : ''}</div>` : ''}${body}</div>`;
const head = (t, d) => `<h1>${t}</h1>${d ? `<p class="lead">${d}</p>` : ''}`;

/* providers page — edits ride PUT /models (writes .sunmao/models.json,
   every live host reloads). `pvEdit` is the open editor state; keys are
   write-only (the page shows 已配置, a blank field means "keep").
   Model selection lives INSIDE the editor: pvEdit.cands is the fetched
   + hand-added candidate pool, pvEdit.sel the checked subset — only the
   checked ids ever land in catalog. Fetching never writes until 保存. */
let pvEdit = null; // { name|null for add, sel:Set, cands:[ModelEntry], fetched:bool }
let pvListH = ''; // dragged list height — outlives the per-form pvEdit
/* dialect pick list — native <select> pops a system-drawn menu that ignores
   the page's dark theme; this rides the same pop() chrome as every other
   picker so the options stay on-brand */
// protocol names are the vendors' own (Chat Completions, Responses,
// Anthropic Messages): a wire dialect is a proper noun, not a translated
// label, so these stay English in every UI language
const DIALECTS = [
  { v: 'openai', t: 'Chat Completions', d: '/chat/completions + SSE' },
  { v: 'openai-responses', t: 'Responses', d: '/responses + response_id chaining' },
  { v: 'anthropic', t: 'Anthropic Messages', d: '/messages + SSE' },
];
function renderProviders() {
  if (view !== 'settings' || setPage !== 'providers') return;
  const host = $('#set-generic');
  if (!MODELS) { host.innerHTML = head(t('模型与提供商'), '') + '<div class="empty-hint">' + t('正在读取模型配置…') + '</div>'; refreshModels(); return; }
  const names = Object.keys(MODELS.providers || {}).sort();
  let html = head(t('模型与提供商'), '');
  for (const n of names) {
    const p = MODELS.providers[n], cat = p.catalog || [];
    let body;
    if (pvEdit && pvEdit.name === n) {
      body = provForm(n, p);
    } else {
      body = `<div class="pv-h"><b class="mono">${esc(n)}</b><span class="pv-sub">${esc((p.dialect || 'openai') + ' · ' + p.base_url)}</span>`
        + `<span class="tag">${p.api_key_set ? t('key 已配置') : t('无 key')}</span>${n === MODELS.default_provider ? '<span class="tag">' + t('本会话') + '</span>' : ''}`
        + `<div class="pv-acts"><button class="btn ghost sm" data-pv="edit" data-n="${esc(n)}" data-tip="${t('编辑')}">${ic('pen', 'i sm')}</button><button class="btn ghost sm" data-pv="del" data-n="${esc(n)}" data-tip="${t('删除')}">${ic('trash', 'i sm')}</button></div></div>`
        + (cat.length
          ? `<div class="pv-cat">${cat.slice(0, 8).map(m => `<span class="tag" data-tip="${esc(n + '/' + m.id)}">${esc(m.id)}${modMarks(m) ? ' ·' + modMarks(m) : ''}${m.context_length ? ' ·' + esc(ctxLen(m.context_length)) : ''}${(m.thinking || []).length || m.reasoning ? ' ·' + t('思') : ''}</span>`).join('')}${cat.length > 8 ? `<span class="tag">${t('等 {n} 个', { n: cat.length })}</span>` : ''}</div>`
          : '');
    }
    html += `<div class="card glass cfg pv">${body}</div>`;
  }
  html += `<div class="card glass cfg pv">${pvEdit && pvEdit.name === null ? provForm('', null) : `<button class="btn ghost sm" data-pv="add">${ic('plus')}${t('添加 provider')}</button>`}</div>`;
  // the rebuild below replaces every node — capture the list's scroll
  // position and any drag-set height first, restore them right after,
  // or each chip toggle snaps the view back to the top
  const oldCkl = host.querySelector('.pv-ckl');
  if (oldCkl && pvEdit) {
    pvEdit.cklTop = oldCkl.scrollTop;
    const h = oldCkl.style.getPropertyValue('--pv-ckl-h');
    if (h) pvListH = h;
  }
  host.innerHTML = html;
  const newCkl = host.querySelector('.pv-ckl');
  if (newCkl) {
    if (pvListH) newCkl.style.setProperty('--pv-ckl-h', pvListH);
    if (pvEdit && pvEdit.cklTop) newCkl.scrollTop = pvEdit.cklTop;
  }
  // capability inputs write straight into pvEdit.cands — the save path
  // serializes them verbatim into catalog entries. The provider's own fields
  // (name, base url, key) go into pvEdit too: a re-render — the one 拉取模型
  // does before it awaits — must not throw away what was just typed.
  host.oninput = e => {
    if (!pvEdit) return;
    const f = e.target.closest('[data-f]');
    if (f) {
      const k = f.dataset.f;
      if (k === 'name') pvEdit.formName = f.value;
      else if (k) pvEdit[k] = f.value;
      return;
    }
    const num = e.target.closest('.pv-num');
    if (!num) return;
    const c = pvEdit.cands.find(m => m.id === (num.dataset.cx || num.dataset.mo));
    if (!c) return;
    const n = parseInt(num.value.replace(/[^\d]/g, ''), 10);
    const key = num.dataset.cx ? 'context_length' : 'max_output';
    if (Number.isFinite(n) && n > 0) c[key] = n; else delete c[key];
  };
  const am = host.querySelector('[data-f="addmodel"]');
  if (am) am.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); providerAction('addmodel', am); } });
  const cklDrag = host.querySelector('.pv-ckl-drag');
  if (cklDrag) cklDrag.addEventListener('pointerdown', e => {
    const ckl = host.querySelector('.pv-ckl');
    if (!ckl) return;
    e.preventDefault();
    cklDrag.setPointerCapture(e.pointerId);
    const y0 = e.clientY, h0 = ckl.getBoundingClientRect().height;
    const mv = ev => ckl.style.setProperty('--pv-ckl-h', Math.max(120, Math.min(innerHeight * 0.7, h0 + ev.clientY - y0)) + 'px');
    const up = () => {
      pvListH = ckl.style.getPropertyValue('--pv-ckl-h');
      cklDrag.removeEventListener('pointermove', mv);
      cklDrag.removeEventListener('pointerup', up);
    };
    cklDrag.addEventListener('pointermove', mv);
    cklDrag.addEventListener('pointerup', up);
  });
}
function provForm(n, p) {
  const v = pvEdit || {};
  const val = (k, d) => esc(v[k] != null ? v[k] : (p && p[k] != null ? p[k] : d || ''));
  const cands = v.cands || [];
  // capability editor — every field writes back into pvEdit.cands and
  // rides `setCatalog` on save: numeric ctx/out inputs, modality + wire
  // capability toggle chips. Fetched entries arrive knowledge-filled;
  // the user edits whatever the guess got wrong.
  const mchip = (m, kind, label) => {
    const mods = m.input_modalities || [];
    return `<button class="pv-chip${mods.includes(kind) ? ' on' : ''}" data-mf="mod" data-mk="${kind}" data-mid="${esc(m.id)}" data-tip="${t('输入模态 · {kind}', { kind })}">${label}</button>`;
  };
  const mflag = (m, key, label, tip) => `<button class="pv-chip${m[key] ? ' on' : ''}" data-mf="flag" data-mk="${key}" data-mid="${esc(m.id)}" data-tip="${tip}">${label}</button>`;
  // each model = one card: identity row, then labeled sections that wrap —
  // numbers in fixed fields, the effort vocabulary as toggle chips (the
  // catalog's `thinking` IS the declared level set — low|medium|high|…),
  // `仅开关` for toggle-only reasoning models, modality + wire flags last
  const mrow = m => {
    const lv = new Set(m.thinking || []);
    const tgl = !!m.reasoning && !lv.size;
    const chips = THINK_LADDER.map(l => `<button class="pv-chip${lv.has(l) ? ' on' : ''}" data-mf="lvl" data-mk="${l}" data-mid="${esc(m.id)}" data-tip="${t('思考档位 · {l}', { l })}">${l}</button>`).join('');
    return `<div class="pv-ckr${v.sel && v.sel.has(m.id) ? ' on' : ''}"><button class="pv-ck${v.sel && v.sel.has(m.id) ? ' on' : ''}" data-mc="${esc(m.id)}">${ic(v.sel && v.sel.has(m.id) ? 'square-check' : 'square', 'i sm')}<span class="mono">${esc(m.id)}</span></button>`
      + `<div class="pv-cf"><label class="pv-f">${t('上下文')}<input class="pv-num" data-cx="${esc(m.id)}" value="${m.context_length || ''}" placeholder="—" spellcheck="false" data-tip="${t('上下文窗口（tokens）')}"></label><label class="pv-f">${t('输出')}<input class="pv-num" data-mo="${esc(m.id)}" value="${m.max_output || ''}" placeholder="—" spellcheck="false" data-tip="${t('单次输出上限（tokens）')}"></label></div>`
      + `<div class="pv-cf"><span class="pv-fl">${t('思考')}</span><span class="pv-chips">${chips}<button class="pv-chip${tgl ? ' on' : ''}" data-mf="tgl" data-mid="${esc(m.id)}" data-tip="${t('模型只提供思考开关，没有档位')}">${t('仅开关')}</button></span></div>`
      + `<div class="pv-cf"><span class="pv-fl">${t('输入')}</span><span class="pv-chips"><button class="pv-chip on" data-tip="${t('文本输入 · 所有模型的基线')}" aria-disabled="true">${t('文')}</button>${mchip(m, 'image', t('图'))}${mchip(m, 'video', t('视'))}${mchip(m, 'audio', t('音'))}${mchip(m, 'file', t('档'))}<i class="pv-sep"></i>${mflag(m, 'supports_tools', t('具'), t('支持工具调用'))}${mflag(m, 'structured_outputs', t('构'), t('支持结构化输出'))}</span></div>`
      // removing one candidate is how a hand-typed id gets taken back out:
      // it never reaches the catalog, so save simply stops carrying it
      + `<button class="btn ghost sm pv-x" data-pv="delmodel" data-n="${esc(m.id)}" data-tip="${esc(t('从列表移除 {id}', { id: m.id }))}">${ic('x', 'i sm')}</button>`
      + `</div>`;
  };
  const list = cands.length
    ? `<div class="pv-ckl scroll">${cands.map(mrow).join('')}</div><div class="pv-ckl-drag" data-tip="${t('拖动调整列表高度')}"></div>`
    : `<div class="pv-empty">${v.fetching ? t('拉取中…') : t('未拉取 — 也可在下方直接填 model id')}</div>`;
  return `<div class="pv-form">
    <label>${t('名称')}<input data-f="name" value="${esc(v.formName != null ? v.formName : n)}" placeholder="${t('如 default、deepseek')}"></label>
    <label>Base URL<input data-f="base_url" value="${val('base_url')}" placeholder="https://api.example.com/v1"></label>
    <label>${t('协议')}<button class="pv-sel" type="button" data-pv="dialect" data-v="${val('dialect', 'openai')}"><span>${esc(DIALECTS.find(d => d.v === val('dialect', 'openai'))?.t || 'Chat Completions')}</span>${ic('chev-d')}</button></label>
    <label>API Key<input data-f="api_key" type="password" value="${val('api_key')}" placeholder="${p && p.api_key_set ? t('已配置 — 留空保持不变') : t('sk-… 或留空（本地服务）')}"></label>
    <div class="pv-mh"><span class="pv-ml">${t('模型（勾选要用的）')}</span><span class="pv-mr"><button class="btn ghost sm" data-pv="fetch" ${v.fetching ? 'disabled' : ''}>${ic('download')}${v.fetched ? t('重新拉取') : t('拉取模型')}</button></span></div>
    ${list}
    <div class="pv-add"><input data-f="addmodel" placeholder="${t('手写 model id')}" spellcheck="false"><button class="btn ghost sm" data-pv="addmodel">${ic('plus')}${t('添加')}</button></div>
    <div class="pv-acts"><button class="btn allow sm" data-pv="save">${ic('check')}${t('保存')}</button><button class="btn ghost sm" data-pv="cancel">${t('取消')}</button></div>
  </div>`;
}
async function providerAction(kind, el) {
  const cardEl = el.closest('.pv');
  const g = f => { const i = cardEl && cardEl.querySelector(`[data-f="${f}"]`); return i ? i.value.trim() : ''; };
  if (kind === 'add') { pvEdit = { name: null, sel: new Set(), cands: [] }; return renderProviders(); }
  if (kind === 'cancel') { pvEdit = null; return renderProviders(); }
  if (kind === 'addmodel') {
    const id = g('addmodel');
    if (!id || !pvEdit) return;
    if (!pvEdit.cands.some(m => m.id === id)) pvEdit.cands.push({ id });
    pvEdit.sel.add(id);
    return renderProviders();
  }
  if (kind === 'delmodel') {
    if (!pvEdit) return;
    const id = el.dataset.n;
    pvEdit.cands = pvEdit.cands.filter(m => m.id !== id);
    pvEdit.sel.delete(id);
    return renderProviders();
  }
  if (kind === 'save') {
    // the name is editable now: a typed value that differs from the entry we
    // opened is a rename, which the PUT body spells as remove-old + add-new
    const typed = (pvEdit && pvEdit.formName != null ? pvEdit.formName : g('name')).trim();
    const name = typed || (pvEdit && pvEdit.name) || '';
    const base = g('base_url');
    if (!name || !base) return toast(t('名称与 Base URL 必填'), 'alert', 'warn');
    const catalog = (pvEdit.cands || []).filter(m => pvEdit.sel.has(m.id));
    const dBtn = cardEl.querySelector('[data-pv="dialect"]');
    const edit = { name, base_url: base, dialect: (dBtn && dBtn.dataset.v) || 'openai', api_key: g('api_key') || null, keepKey: true, keepCatalog: false, setCatalog: catalog };
    if (pvEdit && pvEdit.name && pvEdit.name !== name) edit.renameFrom = pvEdit.name;
    pvEdit = null;
    return saveProviders(edit, t('已保存 provider {n}', { n: name }));
  }
  const n = el.dataset.n;
  if (kind === 'dialect') {
    return pop(el, menuHTML(DIALECTS.map(d => ({ v: d.v, t: d.t, d: d.d, on: d.v === el.dataset.v }))), { align: 'end', cls: 'models', onMount(p) {
      p.addEventListener('click', e => {
        const b = e.target.closest('.mi'); if (!b) return;
        el.dataset.v = b.dataset.v;
        el.querySelector('span').textContent = DIALECTS.find(d => d.v === b.dataset.v)?.t || b.dataset.v;
        closePop();
      });
    } });
  }
  if (kind === 'edit') {
    const cat = (MODELS.providers[n].catalog) || [];
    pvEdit = { name: n, sel: new Set(cat.map(m => m.id)), cands: cat.slice(), fetched: false };
    return renderProviders();
  }
  if (kind === 'del') { return saveProviders({ name: n, del: true }, t('已删除 {n}', { n })); }
  if (kind === 'fetch') {
    if (!pvEdit) return;
    pvEdit.fetching = true;
    renderProviders();
    // typed-but-unsaved providers probe inline; a blank key on a provider
    // that already has one falls back to the named path (server resolves
    // the saved key). Inline fields win only when the form changed them.
    const base = g('base_url'), key = g('api_key'), dialect = cardEl.querySelector('[data-pv="dialect"]')?.dataset.v || 'openai';
    const inline = base && (pvEdit.name == null || key || base !== MODELS.providers[pvEdit.name]?.base_url || dialect !== MODELS.providers[pvEdit.name]?.dialect);
    const body = inline ? { base_url: base, api_key: key || undefined, dialect } : { provider: pvEdit.name };
    try {
      const r = await api('/models/fetch', jpost(body));
      const cat = r.catalog || [];
      const have = new Set(pvEdit.cands.map(m => m.id));
      // the listing is a menu, not a decision: everything lands in the list
      // unchecked, and the user ticks what this provider should actually serve
      for (const m of cat) if (!have.has(m.id)) pvEdit.cands.push(m);
      pvEdit.fetched = true;
      if (!cat.length) toast(t('该 provider 返回了空列表'), 'alert', 'warn');
    } catch (e) { toast(t('拉取失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
    if (pvEdit) pvEdit.fetching = false;
    renderProviders();
  }
}
/* checklist toggle — `data-mc` rows flip membership in pvEdit.sel */
function toggleCand(id) {
  if (!pvEdit) return;
  if (pvEdit.sel.has(id)) pvEdit.sel.delete(id); else pvEdit.sel.add(id);
  renderProviders();
}
/* the canonical effort vocabulary — chips toggle membership in
   `thinking`; `仅开关` declares toggle-only reasoning instead */
const THINK_LADDER = ['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];
/* capability chips — `data-mf="mod|flag|lvl|tgl"` toggles write into
   pvEdit.cands: mod flips a kind inside input_modalities, flag flips a
   boolean field, lvl edits the declared effort set, tgl marks the
   toggle-only shape */
function toggleField(kind, key, id) {
  if (!pvEdit) return;
  const c = pvEdit.cands.find(m => m.id === id);
  if (!c) return;
  if (kind === 'mod') {
    const mods = c.input_modalities || (c.input_modalities = []);
    const i = mods.indexOf(key);
    if (i >= 0) mods.splice(i, 1); else mods.push(key);
  } else if (kind === 'lvl') {
    const t = new Set(c.thinking || []);
    if (t.has(key)) t.delete(key); else t.add(key);
    c.thinking = THINK_LADDER.filter(l => t.has(l));
    if (c.thinking.length) c.reasoning = false;
  } else if (kind === 'tgl') {
    c.reasoning = !c.reasoning;
    if (c.reasoning) c.thinking = [];
  } else {
    c[key] = !c[key];
  }
  renderProviders();
}
const SHELL_OPTS = [
  { v: 'auto', t: t('自动选择'), d: t('Windows 有 PowerShell 7 时使用它；否则使用内置 POSIX') },
  { v: 'pwsh', t: 'PowerShell 7', d: t('以 PowerShell 语法运行命令') },
  { v: 'posix', t: t('内置 POSIX'), d: t('使用跨平台一致的 Bash 语法') },
];
function shellCur() { return SHELL && SHELL.source === 'auto-detect' ? 'auto' : SHELL.backend; }
function renderShell() {
  if (view !== 'settings' || setPage !== 'shell') return;
  const host = $('#set-generic');
  if (!SHELL) { host.innerHTML = head(t('终端'), '') + '<div class="empty-hint">' + t('正在读取设置…') + '</div>'; refreshShell(); return; }
  const name = { pwsh: 'PowerShell 7', posix: t('内置 POSIX') }[SHELL.backend] || SHELL.backend;
  const source = ({
    'auto-detect': t('自动选择'),
    SUNMAO_SHELL: t('环境变量'),
    '.sunmao/shell.txt': t('当前项目'),
    '~/.sunmao/shell.txt': t('用户设置'),
  })[SHELL.source] || t('系统设置');
  const warn = SHELL.pwsh_requested_but_missing ? t('未找到 PowerShell 7，当前使用内置 POSIX。')
    : SHELL.unrecognized ? t('检测到无法识别的终端设置，已忽略。')
    : !SHELL.pwsh_on_path ? t('未找到 PowerShell 7，选择该项后将使用内置 POSIX。') : '';
  host.innerHTML = head(t('终端'), '')
    + sec('', '', card([
      row(t('命令执行方式'), t('仅对新建的会话生效'), `<button class="pill plain" data-act="shell-pick" id="pv-shell"></button>`),
      row(t('新会话使用'), `${name} · ${source}`, ''),
    ])) + (warn ? `<div class="empty-hint">${warn}</div>` : '');
  const b = $('#pv-shell');
  b.innerHTML = esc(SHELL_OPTS.find(o => o.v === shellCur()).t) + ic('chev-d');
}
function shellPick(el) {
  const cur = shellCur();
  menuPop(el, [{ label: t('命令执行方式') }, ...SHELL_OPTS.map(o => Object.assign({}, o, { on: o.v === cur }))], async v => {
    try { SHELL = await api('/shell', jput({ backend: v })); toast(t('设置已保存；新会话生效'), 'check'); }
    catch (e) { toast(t('设置失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
    renderShell();
  }, { align: 'end' });
}

/* grants page — the `Approval::Session` ledger ("本会话都别问了" verdicts).
   GET /session reports it read-only; DELETE /session/{id}/grants revokes —
   per-row or the whole table. `GRANTS` caches the last pull so renders stay
   synchronous like MODELS/SHELL. */
let GRANTS = null;
async function refreshGrants() {
  try { GRANTS = (await api('/session?id=' + encodeURIComponent(sessionId))).grants || []; } catch { GRANTS = null; }
  if (view === 'settings' && setPage === 'grants') renderGrants();
}
async function revokeGrant(key) {
  try {
    const v = await api(`/session/${encodeURIComponent(sessionId)}/grants`, { method: 'DELETE', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ key }) });
    GRANTS = v.grants || [];
    toast(key === '*' ? t('已撤销全部授权') : t('已撤销 · 下次仍会询问'), 'shield');
  } catch (e) { toast(t('撤销失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
  if (view === 'settings' && setPage === 'grants') renderGrants();
}
function renderGrants() {
  if (view !== 'settings' || setPage !== 'grants') return;
  const host = $('#set-generic');
  if (!GRANTS) { host.innerHTML = head(t('已授权命令'), '') + '<div class="empty-hint">' + t('正在读取授权…') + '</div>'; refreshGrants(); return; }
  host.innerHTML = head(t('已授权命令'), t('审批卡上点了「本会话都别问了」留下的许可 —— 只放行完全相同的一条调用，撤销后下一次仍会询问。'))
    + (GRANTS.length
      ? sec('', '', card([
        ...GRANTS.map(g => `<div class="cr"><code class="mono" style="flex:1;min-width:0;overflow-wrap:anywhere;text-align:left">${esc(g)}</code><button class="btn ghost sm" data-gv="${esc(g)}" data-tip="${t('撤销这条授权')}">${ic('trash')}${t('撤销')}</button></div>`),
        ...(GRANTS.length > 1 ? [`<div class="cr"><div class="l"><b>${t('全部撤销')}</b><span>${t('清掉本页列出的所有授权')}</span></div><button class="btn ghost sm warn" data-act="grants-clear">${ic('trash')}${t('全部撤销')}</button></div>`] : []),
      ]))
      : '<div class="empty-hint">' + t('本会话还没有授权 — 审批时点 A 会把那条调用记到这里') + '</div>');
}

/* hooks page — trust roster for the viewed session (`GET /hooks`, the
   same rows `/hooks` prints in text). `user` rows are user-layer config —
   implicitly trusted, nothing to toggle; `pinned`/`untrusted` ride the
   trusted-hooks.json ledger, flips go through `PUT /hooks` exactly like
   `/hooks trust|untrust <n>`. */
let HOOKS = null;
const HK_STATUS = { user: t('用户层'), pinned: t('已信任'), untrusted: t('未信任') };
async function refreshHooks() {
  try { HOOKS = (await api('/hooks?sess=' + encodeURIComponent(sessionId))).hooks || []; } catch { HOOKS = null; }
  if (view === 'settings' && setPage === 'hooks') renderHooks();
}
async function setHookTrust(index, trusted) {
  try {
    await api('/hooks?sess=' + encodeURIComponent(sessionId), jput({ index, trusted }));
    toast(trusted ? t('已信任此钩子') : t('已撤销信任'), 'shield');
  } catch (e) { toast(t('操作失败：{msg}', { msg: e.message }), 'alert', 'warn'); }
  refreshHooks();
}
function renderHooks() {
  if (view !== 'settings' || setPage !== 'hooks') return;
  const host = $('#set-generic');
  if (!HOOKS) { host.innerHTML = head(t('钩子'), '') + '<div class="empty-hint">' + t('正在读取钩子…') + '</div>'; refreshHooks(); return; }
  const KIND = { hook: t('钩子'), mcp: 'MCP', ext: t('扩展'), perm: t('权限') };
  host.innerHTML = head(t('钩子'), t('会话加载的命令钩子与子进程规约 —— 未信任的不会执行，信任记录在 .sunmao/trusted-hooks.json。'))
    + (HOOKS.length
      ? sec('', '', card(HOOKS.map((h, i) => {
          const act = h.status === 'untrusted' ? `<button class="btn ghost sm" data-ht="${i + 1}:1">${t('信任')}</button>`
            : h.status === 'pinned' ? `<button class="btn ghost sm warn" data-ht="${i + 1}:0">${t('撤销')}</button>`
            : `<span class="tag">${esc(HK_STATUS[h.status] || h.status)}</span>`;
          return `<div class="cr"><div class="l" style="flex:1;min-width:0"><b>${esc(KIND[h.kind] || h.kind)} · ${esc(h.event)}${h.matcher ? ' · ' + esc(h.matcher) : ''}</b><span class="mono" style="overflow-wrap:anywhere">${esc(h.command)}</span></div>${act}</div>`;
        })))
      : '<div class="empty-hint">' + t('当前会话没有加载任何钩子 — 项目 .sunmao/hooks.json 或插件清单会出现在这里') + '</div>');
}

/* mcp page — the session's MCP server roster (`GET /mcp`; `/mcp` text is
   the terminal rendering of the same structs). Read-only: servers are
   configured in .sunmao/mcp.json / plugin manifests. */
let MCPS = null;
async function refreshMcp() {
  try { MCPS = (await api('/mcp?sess=' + encodeURIComponent(sessionId))).servers || []; } catch { MCPS = null; }
  if (view === 'settings' && setPage === 'mcp') renderMcp();
}
function renderMcp() {
  if (view !== 'settings' || setPage !== 'mcp') return;
  const host = $('#set-generic');
  if (!MCPS) { host.innerHTML = head(t('MCP 服务器'), '') + '<div class="empty-hint">' + t('正在读取 MCP 服务器…') + '</div>'; refreshMcp(); return; }
  host.innerHTML = head(t('MCP 服务器'), t('本会话挂载的 MCP 服务器 — 在 .sunmao/mcp.json 或插件清单中配置。'))
    + (MCPS.length
      ? sec('', '', card(MCPS.map(m =>
          `<div class="cr"><div class="l" style="flex:1;min-width:0"><b>${esc(m.name)}<span class="tag" style="margin-left:var(--s-6)">${esc(m.transport)}</span></b><span>${t('{tools} 工具 · {prompts} 提示词 · {resources} 资源', { tools: m.tools, prompts: m.prompts, resources: m.resources })}</span></div><span class="sd ${m.connected ? 'run' : 'off'}"></span></div>`)))
      : '<div class="empty-hint">' + t('当前会话没有连接 MCP 服务器') + '</div>');
}

const PAGES = {
  providers: () => head(t('模型与提供商'), '') + '<div class="empty-hint">' + t('正在读取模型配置…') + '</div>',
  channels: () => head(t('IM 渠道'), '') + '<div class="empty-hint">' + t('正在读取渠道状态…') + '</div>',
  grants: () => head(t('已授权命令'), '') + '<div class="empty-hint">' + t('正在读取授权…') + '</div>',
  hooks: () => head(t('钩子'), '') + '<div class="empty-hint">' + t('正在读取钩子…') + '</div>',
  mcp: () => head(t('MCP 服务器'), '') + '<div class="empty-hint">' + t('正在读取 MCP 服务器…') + '</div>',
  shell: () => head(t('终端'), '') + '<div class="empty-hint">' + t('正在读取设置…') + '</div>',
  keys: () => head(t('快捷键'), '') + sec('', '', card([[t('新对话'), 'Ctrl N'], [t('命令面板'), 'Ctrl K'], [t('打开设置'), 'Ctrl ,'], [t('显示或隐藏数据面板'), 'Ctrl \\'], [t('允许一次 / 拒绝 / 本会话允许'), 'Y N A'], [t('发送'), 'Enter'], [t('追加指示'), 'Ctrl Enter'], [t('换行'), 'Shift Enter'], [t('关闭弹层或返回'), 'Esc']].map(([a, k]) => row(a, '', `<span class="keys">${k.split(' ').map(x => `<kbd>${esc(x)}</kbd>`).join('')}</span>`)))),
  about: () => head(t('关于'), '') + `<div class="card glass cfg"><div class="ab-top">${$('#hero svg').outerHTML}<div><b>${brand()}</b><span>${t('本地 AI 开发助手')}</span></div></div>${row(t('许可证'), '', mono('MIT OR Apache-2.0'))}</div>`,
};
function settingsPage(p) {
  setPage = p;
  $('#snav').innerHTML = SET_NAV.map(([k, t, i]) => `<button class="nav-i${k === p ? ' on' : ''}" data-page="${k}">${ic(i)}<span>${t}</span></button>`).join('');
  const isA = p === 'appearance';
  $('#set-appearance').hidden = !isA; $('#set-generic').hidden = isA;
  if (!isA) $('#set-generic').innerHTML = PAGES[p]();
  $('#set-scroll').scrollTop = 0;
  renderCrumb();
  if (isA) { renderWallGrid(); syncSettingsUI(); }
  else if (p === 'providers') { renderProviders(); }
  else if (p === 'channels') { refreshChannels(); }
  else if (p === 'grants') { renderGrants(); }
  else if (p === 'hooks') { renderHooks(); }
  else if (p === 'mcp') { renderMcp(); }
  else if (p === 'shell') { renderShell(); }
}

