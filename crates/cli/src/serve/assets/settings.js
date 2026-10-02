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
  paintWall();
  syncSettingsUI();
}
function commit() { apply(); save(); }


/* ================= settings pages ================= */
function renderWallGrid() {
  const g = $('#wgrid'); if (!g || view !== 'settings' || setPage !== 'appearance') return;
  const items = WALLS.map(x => ({ id: x.id, name: x.name })).concat(customImg ? [{ id: 'custom', name: '自定义' }] : []);
  g.innerHTML = items.map(x => `<button class="wt${S.wallpaper === x.id ? ' on' : ''}" data-wall="${x.id}"><span class="th"><canvas></canvas><span class="chk">${ic('check')}</span></span><span class="nm2">${x.name}</span></button>`).join('')
    + `<button class="wt up" data-act="upload-wall"><span class="th">${ic('image-plus')}<em>上传图片</em></span><span class="nm2">JPG · PNG</span></button>`;
  const dpr = Math.min(2, devicePixelRatio || 1);
  $$('.wt[data-wall]', g).forEach(b => { const c = $('canvas', b), r = c.getBoundingClientRect(); c.width = Math.round((r.width || 112) * dpr); c.height = Math.round((r.height || 70) * dpr); const ctx = c.getContext('2d'); drawWall(ctx, c.width, c.height, b.dataset.wall); });
}
function setRange(sel, v, f) { const el = $(sel); el.value = v; el.style.setProperty('--_p', ((v - el.min) / (el.max - el.min) * 100) + '%'); const o = el.parentElement.querySelector('output'); if (o) o.textContent = f(v); }
function themeLines(o) {
  const s = v => [JSON.stringify(v), 'tk-s'], n = v => [String(v), 'tk-n'];
  const P = (k, tok, ind = '  ') => [[ind, ''], [k, 'tk-p'], [': ', 'tk-x'], tok, [',', 'tk-x']];
  return [
    [['const ', 'tk-k'], ['themePreview', 'tk-v'], [': ', 'tk-x'], ['ThemeConfig', 'tk-t'], [' = {', 'tk-x']],
    P('mode', s(o.mode)), P('accent', s(o.accent.toUpperCase())), P('background', s(o.background.toUpperCase())), P('foreground', s(o.foreground.toUpperCase())),
    P('wallpaper', s(o.wallpaper)), P('dim', n(o.dim.toFixed(2))), P('panelOpacity', n(o.panelOpacity.toFixed(2))), P('blur', n(o.blur)),
    P('translucentSidebar', n(o.translucentSidebar)), P('contrast', n(o.contrast)),
    [['  ', ''], ['fonts', 'tk-p'], [': {', 'tk-x']], P('ui', s(o.fonts.ui), '    '), P('code', s(o.fonts.code), '    '),
    [['  },', 'tk-x']], [['};', 'tk-x']],
  ];
}
function renderPreview() {
  const A = themeLines(DEFAULTS), B = themeLines(S), txt = l => l.map(t => t[0]).join('');
  const col = (L, O, cls) => L.map((l, i) => `<div class="ln${txt(l) !== txt(O[i]) ? ' ' + cls : ''}"><span class="no">${i + 1}</span><span>${l.map(([t, c]) => c ? `<span class="${c}">${esc(t)}</span>` : esc(t)).join('')}</span></div>`).join('');
  $('#jp-a').innerHTML = col(A, B, 'del'); $('#jp-b').innerHTML = col(B, A, 'add');
  const d = A.filter((l, i) => txt(l) !== txt(B[i])).length;
  $('#jp-count').textContent = d ? `${d} 处不同于默认` : '与默认一致';
}
function syncSettingsUI() {
  if (view !== 'settings' || setPage !== 'appearance') return;
  $$('.tc').forEach(b => b.classList.toggle('on', b.dataset.mode === S.mode));
  $('#sys-now').textContent = matchMedia('(prefers-color-scheme: light)').matches ? '当前浅色' : '当前深色';
  setRange('#rg-op', Math.round(S.panelOpacity * 100), v => v + '%');
  setRange('#rg-blur', S.blur, v => v + 'px');
  setRange('#rg-dim', Math.round(S.dim * 100), v => v + '%');
  setRange('#rg-con', S.contrast, v => String(v));
  for (const k of ['accent', 'background', 'foreground']) $('#pv-' + k).innerHTML = `<i class="cdot" style="background:${S[k]}"></i>${S[k].toUpperCase()}`;
  $('#pv-uif').innerHTML = esc(S.fonts.ui) + ic('chev-d');
  $('#pv-codef').innerHTML = esc(S.fonts.code) + ic('chev-d');
  $('#sw-side').setAttribute('aria-checked', String(S.translucentSidebar));
  $('#pv-motion').innerHTML = esc({ system: '跟随系统', full: '完整', reduce: '减少' }[S.motion] || S.motion) + ic('chev-d');
  $$('.wt[data-wall]').forEach(b => b.classList.toggle('on', b.dataset.wall === S.wallpaper));
  renderPreview();
}
$('#rg-op').addEventListener('input', e => { S.panelOpacity = +e.target.value / 100; commit(); });
$('#rg-blur').addEventListener('input', e => { S.blur = +e.target.value; commit(); });
$('#rg-dim').addEventListener('input', e => { S.dim = +e.target.value / 100; commit(); });
$('#rg-con').addEventListener('input', e => { S.contrast = +e.target.value; commit(); });

const SWATCH = { accent: ['#339CFF', '#2BB3A3', '#4CB782', '#5B7CFA', '#E5608A', '#A0A6B8'], background: ['#16181F', '#1A1A17', '#121A1F', '#0F1115', '#1B1822', '#202329'], foreground: ['#E8E9F0', '#F2EFE8', '#DDE3EA', '#FFFFFF', '#CDD3DE', '#E9E4D8'] };
const CLABEL = { accent: '强调色', background: '背景', foreground: '前景' };
function colorPop(anchor, key) {
  if (popAnchor === anchor) return closePop();
  const cur = S[key].toUpperCase();
  pop(anchor, `<div class="lbl">${CLABEL[key]}</div><div class="sws">${SWATCH[key].map(c => `<button class="swt${c === cur ? ' on' : ''}" data-c="${c}" style="--_c:${c}" aria-label="${c}"></button>`).join('')}</div><div class="field"><i class="cdot" style="background:${cur}"></i><input value="${cur}" maxlength="7" spellcheck="false" aria-label="十六进制色值"></div><div class="hint">输入 #RRGGBB 后按 Enter</div>`, { align: 'end', onMount(p) {
    const setC = c => { S[key] = c.toUpperCase(); commit(); $$('.swt', p).forEach(b => b.classList.toggle('on', b.dataset.c === S[key])); $('.cdot', p).style.background = S[key]; $('input', p).value = S[key]; };
    p.addEventListener('click', e => { const b = e.target.closest('.swt'); if (b) setC(b.dataset.c); });
    const inp = $('input', p);
    inp.addEventListener('keydown', e => { if (e.key !== 'Enter') return; const v = inp.value.trim(); if (/^#?[0-9a-f]{6}$/i.test(v)) setC(v[0] === '#' ? v : '#' + v); else toast('色值格式应为 #RRGGBB', 'alert', 'warn'); });
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
  menuPop(anchor, [{ label: key === 'ui' ? 'UI 字体' : '代码字体' }, ...FONTS[key].map(f => ({ v: f, t: f, on: f === S.fonts[key], d: hasFont(f) ? '' : '未安装，将回退到下一个字体', style: key === 'code' ? `font-family:"${f}",monospace` : `font-family:"${f}",sans-serif` }))], v => { S.fonts[key] = v; commit(); }, { align: 'end' });
}
function motionPop(anchor) {
  const OPTS = [{ v: 'system', t: '跟随系统' }, { v: 'full', t: '完整' }, { v: 'reduce', t: '减少' }];
  menuPop(anchor, [{ label: '动效' }, ...OPTS.map(m => Object.assign({}, m, { on: m.v === S.motion }))], v => { S.motion = v; commit(); }, { align: 'end' });
}

/* model picker — the live model first, then routes, then each provider's
   catalog. Enter picks the typed selector verbatim (provider/id or bare id
   resolve through the kernel's ModelResolver); rows carry capability
   badges. A provider with no catalog offers the pull, never a fake id. */
const ctxLen = n => typeof n === 'number' && Number.isFinite(n)
  ? (n >= 1000 ? Math.round(n / 1000) + 'k' : String(n))
  : (n ? String(n) : '');
const mbadge = m => `${m.vision ? '<span class="tag">图</span>' : ''}${m.context_length ? `<span class="tag">${esc(ctxLen(m.context_length))}</span>` : ''}${(m.thinking || []).length ? `<span class="tag">思:${esc((m.thinking || []).map(t => String(t)).join('/'))}</span>` : ''}`;
function renderModelRows(p, q) {
  q = (q || '').toLowerCase();
  const rows = [];
  // what this session is running on comes first — the picker's job is to
  // state the live model, not to ask for it again
  if (modelLabel && (!q || modelLabel.toLowerCase().includes(q)))
    rows.push(`<div class="lbl">当前</div><div class="mi cur"><span class="mt mono"><span>${esc(modelLabel)}</span><small>本会话正在使用</small></span>${ic('check', 'i sm ck')}</div>`);
  const routes = (MODELS && MODELS.routes) || {};
  for (const [r, chain] of Object.entries(routes)) {
    const sel = '@' + r, d = Array.isArray(chain) ? chain.join(' → ') : String(chain);
    if (!q || (sel + ' ' + d).toLowerCase().includes(q)) rows.push(`<button class="mi" data-v="${esc(sel)}">${ic('zap')}<span class="mt mono"><span>${esc(sel)}</span><small>${esc(d)}</small></span></button>`);
  }
  for (const n of provNames()) {
    const p0 = MODELS.providers[n], cat = p0.catalog || [];
    const shown = cat.filter(m => !q || `${n}/${m.id}`.toLowerCase().includes(q));
    const listed = shown.map(m => `<button class="mi" data-v="${esc(n + '/' + m.id)}"><span class="mt mono"><span>${esc(n + '/' + m.id)}</span></span>${mbadge(m)}</button>`).join('');
    if (listed) rows.push(`<div class="lbl">${esc(n)} · ${cat.length} 个模型</div>` + listed);
  }
  $('.mp-list', p).innerHTML = rows.join('') || `<div class="hint">无匹配 — 输入 provider/model 或模型 id，回车直接切换</div>`;
}
function modelPop(el) {
  if (popAnchor === el) return closePop();
  pop(el, `<div class="field"><input id="mp-in" placeholder="provider/model 或 @route — 回车切换" spellcheck="false" autocomplete="off"></div><div class="mp-list scroll"></div>`, { place: 'top', align: 'end', cls: 'models', onMount(p) {
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
/* dialect pick list — native <select> pops a system-drawn menu that ignores
   the page's dark theme; this rides the same pop() chrome as every other
   picker so the options stay on-brand */
const DIALECTS = [
  { v: 'openai', t: 'OpenAI 兼容', d: '/chat/completions + SSE' },
  { v: 'openai-responses', t: 'OpenAI Responses', d: '/responses + response_id 链' },
  { v: 'anthropic', t: 'Anthropic', d: '/messages + SSE' },
];
function renderProviders() {
  if (view !== 'settings' || setPage !== 'providers') return;
  const host = $('#set-generic');
  if (!MODELS) { host.innerHTML = head('模型与提供商', '') + '<div class="empty-hint">正在读取模型配置…</div>'; refreshModels(); return; }
  const names = Object.keys(MODELS.providers || {}).sort();
  let html = head('模型与提供商', '');
  for (const n of names) {
    const p = MODELS.providers[n], cat = p.catalog || [];
    let body;
    if (pvEdit && pvEdit.name === n) {
      body = provForm(n, p);
    } else {
      body = `<div class="pv-h"><b class="mono">${esc(n)}</b><span class="pv-sub">${esc((p.dialect || 'openai') + ' · ' + p.base_url)}</span>`
        + `<span class="tag">${p.api_key_set ? 'key 已配置' : '无 key'}</span>${n === MODELS.default_provider ? '<span class="tag">本会话</span>' : ''}`
        + `<div class="pv-acts"><button class="btn ghost sm" data-pv="edit" data-n="${esc(n)}" data-tip="编辑">${ic('pen', 'i sm')}</button><button class="btn ghost sm" data-pv="del" data-n="${esc(n)}" data-tip="删除">${ic('trash', 'i sm')}</button></div></div>`
        + (cat.length
          ? `<div class="pv-cat">${cat.slice(0, 8).map(m => `<span class="tag" data-tip="${esc(n + '/' + m.id)}">${esc(m.id)}${m.vision ? ' ·图' : ''}${m.context_length ? ' ·' + esc(ctxLen(m.context_length)) : ''}</span>`).join('')}${cat.length > 8 ? `<span class="tag">等 ${cat.length} 个</span>` : ''}</div>`
          : '');
    }
    html += `<div class="card glass cfg pv">${body}</div>`;
  }
  html += `<div class="card glass cfg pv">${pvEdit && pvEdit.name === null ? provForm('', null) : `<button class="btn ghost sm" data-pv="add">${ic('plus')}添加 provider</button>`}</div>`;
  host.innerHTML = html;
  const am = host.querySelector('[data-f="addmodel"]');
  if (am) am.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); providerAction('addmodel', am); } });
}
function provForm(n, p) {
  const v = pvEdit || {};
  const val = (k, d) => esc(v[k] != null ? v[k] : (p && p[k] != null ? p[k] : d || ''));
  const cands = v.cands || [];
  const list = cands.length
    ? `<div class="pv-ckl scroll">${cands.map(m => `<button class="pv-ck" data-mc="${esc(m.id)}">${ic(v.sel && v.sel.has(m.id) ? 'square-check' : 'square', 'i sm')}<span class="mono">${esc(m.id)}</span>${m.vision ? '<span class="tag">图</span>' : ''}${m.context_length ? `<span class="tag">${esc(ctxLen(m.context_length))}</span>` : ''}</button>`).join('')}</div>`
    : `<div class="pv-empty">${v.fetching ? '拉取中…' : '未拉取 — 也可在下方直接填 model id'}</div>`;
  return `<div class="pv-form">
    <label>名称<input data-f="name" value="${esc(n)}" ${n ? 'disabled' : ''} placeholder="如 default、deepseek"></label>
    <label>Base URL<input data-f="base_url" value="${val('base_url')}" placeholder="https://api.example.com/v1"></label>
    <label>协议<button class="pv-sel" type="button" data-pv="dialect" data-v="${val('dialect', 'openai')}"><span>${esc(DIALECTS.find(d => d.v === val('dialect', 'openai'))?.t || 'OpenAI 兼容')}</span>${ic('chev-d')}</button></label>
    <label>API Key<input data-f="api_key" type="password" value="${val('api_key')}" placeholder="${p && p.api_key_set ? '已配置 — 留空保持不变' : 'sk-… 或留空（本地服务）'}"></label>
    <div class="pv-mh"><span class="pv-ml">模型（勾选要用的）</span><button class="btn ghost sm" data-pv="fetch" ${v.fetching ? 'disabled' : ''}>${ic('download')}${v.fetched ? '重新拉取' : '拉取模型'}</button></div>
    ${list}
    <div class="pv-add"><input data-f="addmodel" placeholder="手写 model id" spellcheck="false"><button class="btn ghost sm" data-pv="addmodel">${ic('plus')}添加</button></div>
    <div class="pv-acts"><button class="btn allow sm" data-pv="save">${ic('check')}保存</button><button class="btn ghost sm" data-pv="cancel">取消</button></div>
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
  if (kind === 'save') {
    const name = pvEdit && pvEdit.name != null ? pvEdit.name : g('name');
    const base = g('base_url');
    if (!name || !base) return toast('名称与 Base URL 必填', 'alert', 'warn');
    const catalog = (pvEdit.cands || []).filter(m => pvEdit.sel.has(m.id));
    const dBtn = cardEl.querySelector('[data-pv="dialect"]');
    const edit = { name, base_url: base, dialect: (dBtn && dBtn.dataset.v) || 'openai', api_key: g('api_key') || null, keepKey: true, keepCatalog: false, setCatalog: catalog };
    pvEdit = null;
    return saveProviders(edit, '已保存 provider ' + name);
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
  if (kind === 'del') { return saveProviders({ name: n, del: true }, '已删除 ' + n); }
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
      for (const m of cat) if (!have.has(m.id)) { pvEdit.cands.push(m); pvEdit.sel.add(m.id); }
      pvEdit.fetched = true;
      if (!cat.length) toast('该 provider 返回了空列表', 'alert', 'warn');
    } catch (e) { toast(`拉取失败：${e.message}`, 'alert', 'warn'); }
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
const PAGES = {
  providers: () => head('模型与提供商', '') + '<div class="empty-hint">正在读取模型配置…</div>',
  keys: () => head('快捷键', '焦点不在输入框时，审批快捷键直接裁决最早的待审批卡。') + sec('', '', card([['新对话', 'Ctrl N'], ['命令面板', 'Ctrl K'], ['打开设置', 'Ctrl ,'], ['显示或隐藏数据面板', 'Ctrl \\'], ['Allow / Deny / Always', 'Y N A'], ['发送（运行中则排队）', 'Enter'], ['插队引导（不打断本轮）', 'Ctrl Enter'], ['换行', 'Shift Enter'], ['关闭弹层或返回', 'Esc']].map(([a, k]) => row(a, '', `<span class="keys">${k.split(' ').map(x => `<kbd>${esc(x)}</kbd>`).join('')}</span>`)))),
  about: () => head('关于', '') + `<div class="card glass cfg"><div class="ab-top">${$('#hero svg').outerHTML}<div><b>sunmao</b><span>Rust 编写的 agent 运行时内核</span></div></div>${row('会话', '', mono(sessionId || '—'))}${row('工作目录', '', mono(cwd || '—'))}${row('本地服务', TAURI ? '内嵌内核 · 自定义协议（无 TCP 监听）' : 'sunmao serve 只绑定本机', mono(location.host))}${row('内核', '', mono('sunmao-core'))}${row('许可', '', mono('MIT OR Apache-2.0'))}</div>`,
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
}

