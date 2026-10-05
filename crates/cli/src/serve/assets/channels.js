/* IM channels page — `~/.sunmao/channels.json` is a policy header plus a
   list of channel adapters. GET /channels hands over the raw text, so this
   page parses it, edits the keys it knows and writes the same document back:
   an entry the form doesn't cover (a newer kind, a channel-scoped override)
   survives the round trip untouched.
 *
 * Status is a verdict, not "the file exists": a declared channel, a
   credential and a fresh status.json heartbeat all have to agree before
   anything here says 在线. Telegram is the only kind this version ships
   (docs/IM.md §非目标 covers the rest), so the channel list is driven by
   CHANNEL_KINDS — a new kind is a row there, not a new page.
 */
'use strict';

const CHANNEL_KINDS = [
  { kind: 'telegram', t: 'Telegram', d: 'Bot API 长轮询，不需要公网入口' },
];
/* status.json is rewritten at startup and on every reconnect-worthy event;
   silence for this long means the daemon is not running */
const CHANNEL_STALE_SECS = 90;
/* a fresh channel starts on the schema's own default, so an untouched card
   writes what the daemon would have assumed anyway */
const CHANNEL_DEFAULT_POLL_SECS = 30;
const CHANNEL_POLICIES = [
  { v: 'pairing', t: '配对', d: '陌生人拿到配对码，由你在这里批准' },
  { v: 'allowlist', t: '白名单', d: '只回应名单里的 sender' },
  { v: 'open', t: '开放', d: '谁都能和这个 bot 说话' },
  { v: 'disabled', t: '停用', d: '直接拒绝所有私聊' },
];
const CHANNEL_SCOPES = [
  { v: 'main', t: '同一会话', d: '所有 DM 汇入 im:main（一个脑子）' },
  { v: 'per_channel_peer', t: '按聊天分开', d: '每个渠道 + 聊天一个会话' },
];
const CHANNEL_UNAUTH = [
  { v: 'pair', t: '回配对码', d: '陌生人可以申请接入' },
  { v: 'ignore', t: '完全静默', d: '回复本身就暴露了 bot 的存在' },
];

let CHANNELS = null, chCfg = null, chCfgErr = '', chDirty = false;

async function refreshChannels() {
  try {
    CHANNELS = await api('/channels');
  } catch (e) {
    CHANNELS = { error: e.message };
  }
  chCfg = null;
  chDirty = false;
  renderChannels();
}

/* the JSON text is the contract: parse it into the editable shape, and keep
   the original string only for the advanced editor */
function chParse(text) {
  chCfgErr = '';
  try {
    const v = text && text.trim() ? JSON.parse(text) : {};
    return (v && typeof v === 'object' && !Array.isArray(v)) ? v : {};
  } catch (e) {
    chCfgErr = e.message;
    return {};
  }
}
const chCfgOf = () => chCfg || (chCfg = chParse((CHANNELS && CHANNELS.config) || ''));
const chChannels = () => { const c = chCfgOf(); return Array.isArray(c.channels) ? c.channels : (c.channels = []); };
const chCred = c => (c.token_env ? 'token_env' : (c.token_file ? 'token_file' : ''));

/* What the page is allowed to claim. Each verdict names the one thing that
   is missing, because "在线" over an unconfigured file is exactly the lie
   this replaced. */
function channelVerdict(cfg, st) {
  const all = Array.isArray(cfg.channels) ? cfg.channels : [];
  const on = all.filter(c => c && c.enabled !== false);
  const running = (st && Array.isArray(st.channels)) ? st.channels : [];
  const fresh = !!(st && (Date.now() / 1000 - Number(st.updated || 0)) < CHANNEL_STALE_SECS);
  const armed = on.filter(c => c.token_env || c.token_file);
  if (!all.length) return { key: 'none', cls: '' };
  if (!on.length) return { key: 'off', cls: '' };
  if (!armed.length) return { key: 'nocred', cls: 'warn' };
  if (!fresh) return { key: 'down', cls: 'off' };
  if (!running.length) return { key: 'idle', cls: 'warn' };
  return { key: 'on', cls: 'online', running };
}

function renderChannels() {
  if (view !== 'settings' || setPage !== 'channels') return;
  const host = $('#set-generic');
  if (!CHANNELS) {
    host.innerHTML = head(t('IM 渠道'), '') + `<div class="empty-hint">${t('正在读取渠道状态…')}</div>`;
    refreshChannels();
    return;
  }
  if (CHANNELS.error) {
    host.innerHTML = head(t('IM 渠道'), '') + `<div class="empty-hint">${esc(t('读取失败：{msg}', { msg: CHANNELS.error }))}</div>`;
    return;
  }
  const cfg = chCfgOf();
  const st = CHANNELS.status;
  const pairing = CHANNELS.pairing || [];
  const allow = CHANNELS.allowlist || [];
  const v = channelVerdict(cfg, st);

  let html = head(t('IM 渠道'), t('每个渠道是一份独立配置；改完保存，重启 IM 服务后生效。'));

  // ── status: the verdict plus what it was read from ──
  const heartbeat = st
    ? t('最近心跳 {when}', { when: new Date(Number(st.updated || 0) * 1000).toLocaleString() })
    : t('没有心跳：IM 服务没在这台机器上跑过');
  // every arm of this map is evaluated, so the running list is joined once
  // up front — `on` is not the only key this object gets built for
  const runningText = (v.running || []).join('、');
  const detail = {
    none: t('还没有渠道。先在下面添加一个，再启动服务。'),
    off: t('渠道都已停用。'),
    nocred: t('渠道缺凭据：填 token_env 或 token_file。'),
    down: t('服务未运行。在部署机上执行 sunmao im。'),
    idle: t('服务在跑，但没有渠道在监听。'),
    on: t('服务在跑：{chans}', { chans: runningText }),
  }[v.key];
  html += sec(t('服务状态'), '', card([
    row(t(v.key === 'on' ? '运行状态' : '运行状态'), `${detail} · ${heartbeat}`,
      `<span class="channel-state ${v.cls}"><i class="sd ${v.key === 'on' ? 'done' : 'off'}"></i>${t({
        none: '未配置', off: '已停用', nocred: '缺凭据', down: '未运行', idle: '空转', on: '在线',
      }[v.key])}</span>`),
  ]));

  // ── policy header: the top-level keys of the same file ──
  const pill = (key, label, val) => `<button class="pill plain" data-chpick="${key}"><span>${esc(label)}</span>${ic('chev-d')}</button>`;
  const lab = (list, cur) => (list.find(x => x.v === cur) || list[0]).t;
  html += sec(t('接入策略'), '', card([
    row(t('陌生人私聊'), t('未配对的人发消息时怎么处理'), pill('dm_policy', t(lab(CHANNEL_POLICIES, cfg.dm_policy || 'pairing')), cfg.dm_policy)),
    row(t('会话归并'), t('DM 落到哪个会话里'), pill('dm_scope', t(lab(CHANNEL_SCOPES, cfg.dm_scope || 'main')), cfg.dm_scope)),
    row(t('未授权回复'), t('配对策略下对陌生人的回应'), pill('unauthorized', t(lab(CHANNEL_UNAUTH, cfg.unauthorized_dm_behavior || 'pair')), cfg.unauthorized_dm_behavior)),
    row(t('配置白名单'), t('channel:sender，逗号分隔；留空即只认配对结果'), `<input class="ch-in mono" data-chin="allowlist" value="${esc((cfg.allowlist || []).join(', '))}" placeholder="telegram:12345" spellcheck="false">`),
    row(t('管理员'), t('可用 /pairing 的 sender；首个批准者自动成为管理员'), `<span class="tag mono">${esc(cfg.owner || t('未指定'))}</span>`),
  ]));

  // ── one card per channel entry ──
  // rows, not a bespoke grid: the header is the card's title (the kind) with
  // its actions in flow beside it, then one .cr row per field — the same
  // shape every other settings card uses, so nothing overlaps anything.
  const chans = chChannels();
  const cards = chans.map((c, i) => {
    const kind = CHANNEL_KINDS.find(k => k.kind === c.kind);
    const src = chCred(c);
    const cred = src === 'token_file'
      ? { v: c.token_file || '', ph: 'D:/secrets/tg.txt' }
      : { v: c.token_env || '', ph: 'SUNMAO_TG_TOKEN' };
    const title = kind ? kind.t : (c.kind || t('未知渠道'));
    const desc = kind
      ? t(kind.d)
      : t('这个 kind 不是本版认识的渠道；保存时会原样保留，服务会跳过它。');
    // a kind picker only earns its place once there is more than one kind
    const picker = CHANNEL_KINDS.length > 1
      ? `<button class="pill plain" data-chpick="kind:${i}"><span>${t('渠道类型')}</span>${ic('chev-d')}</button>`
      : '';
    return `<div class="card glass cfg">
      ${row(esc(title), esc(desc), `<div class="ch-acts">${picker}<button class="sw" role="switch" data-chtog="${i}" aria-checked="${c.enabled !== false}" aria-label="${t('启用')}"></button><button class="btn ghost sm" data-chdel="${i}" data-tip="${t('移除这个渠道')}">${ic('x', 'i sm')}</button></div>`)}
      ${row(t('凭据'), t('配置里不写 token 明文，只写来源'), `<div class="ch-acts"><button class="pill plain" data-chpick="cred:${i}"><span>${t(src === 'token_file' ? '文件' : '环境变量')}</span>${ic('chev-d')}</button><input class="ch-in mono" data-chin="cred:${i}" value="${esc(cred.v)}" placeholder="${cred.ph}" spellcheck="false"></div>`)}
      ${row(t('轮询超时'), t('长轮询秒数，默认 30'), `<input class="ch-in mono" data-chin="timeout:${i}" value="${esc(c.poll_timeout_secs != null ? c.poll_timeout_secs : '')}" placeholder="30" spellcheck="false">`)}
    </div>`;
  }).join('');
  html += sec(t('渠道'), '', cards + `<div class="channel-save"><button class="btn ghost sm" data-chadd>${ic('plus')}${t('添加渠道')}</button></div>`);

  // ── admission ledger ──
  html += sec(t('待处理配对'), '',
    card(pairing.length
      ? pairing.map(p => row(`<span class="mono">${esc(p.code)}</span>`, `${esc(p.channel)} · ${esc(p.sender)}`,
          `<button class="btn allow sm" data-pair="${esc(p.code)}">${ic('check')}${t('允许')}</button>`))
      : [`<div class="channel-empty">${t('暂无待处理请求')}</div>`]));

  html += sec(t('已允许账号'), '',
    card(allow.length
      ? allow.map(a => {
          const role = a.role === 'owner' ? t('管理员') : a.role;
          return row(`${esc(a.channel)} · ${esc(a.sender)}`, '', `<span class="tag">${esc(role)}</span>`);
        })
      : [`<div class="channel-empty">${t('暂无已允许账号')}</div>`]));

  // ── save + the raw escape hatch ──
  html += `<div class="channel-save">
    <button class="btn allow sm" data-chsave>${ic('check')}${t('保存')}</button>
    <span>${chDirty ? t('有未保存的改动') : t('重启 IM 服务后生效')}</span>
  </div>`;
  html += sec(t('高级'), t('表单没覆盖到的字段（渠道级覆盖、新 kind）可以直接改 JSON。'),
    `<div class="card glass cfg"><textarea id="ch-cfg" class="channel-config" aria-label="${t('IM 渠道配置')}" spellcheck="false">${esc(JSON.stringify(cfg, null, 2))}</textarea></div>
     ${chCfgErr ? `<div class="ch-hint warn">${esc(t('当前文件不是合法 JSON：{msg}', { msg: chCfgErr }))}</div>` : ''}
     <div class="channel-save"><button class="btn ghost sm" data-chraw>${t('用这段 JSON 覆盖表单')}</button></div>`);

  host.innerHTML = html;
}

/* write the parsed shape back as text; the daemon re-reads it at startup */
async function chSave(text) {
  try {
    await api('/channels', jput({ config: text }));
    toast(t('配置已保存；重启 IM 服务后生效'), 'check');
  } catch (err) { toast(t('保存失败：{msg}', { msg: err.message }), 'alert', 'warn'); }
  return refreshChannels();
}
const chTouch = () => { chDirty = true; if (view === 'settings' && setPage === 'channels') renderChannels(); };

document.addEventListener('click', async e => {
  const ap = e.target.closest('[data-pair]');
  if (ap) {
    try {
      await api('/channels/pairing/approve', jpost({ code: ap.dataset.pair }));
      toast(t('已允许账号接入'), 'check');
    } catch (err) { toast(t('操作失败：{msg}', { msg: err.message }), 'alert', 'warn'); }
    return refreshChannels();
  }
  if (e.target.closest('[data-chadd]')) {
    chChannels().push({ kind: CHANNEL_KINDS[0].kind, enabled: true, token_env: '', poll_timeout_secs: CHANNEL_DEFAULT_POLL_SECS });
    return chTouch();
  }
  const del = e.target.closest('[data-chdel]');
  if (del) {
    chChannels().splice(+del.dataset.chdel, 1);
    return chTouch();
  }
  const tog = e.target.closest('[data-chtog]');
  if (tog) {
    const c = chChannels()[+tog.dataset.chtog];
    if (c) c.enabled = c.enabled === false;
    return chTouch();
  }
  const pick = e.target.closest('[data-chpick]');
  if (pick) {
    const cfg = chCfgOf(), k = pick.dataset.chpick;
    const setTop = (key, v) => { cfg[key] = v; };
    if (k === 'dm_policy') return menuPop(pick, CHANNEL_POLICIES.map(o => Object.assign({}, o, { on: o.v === (cfg.dm_policy || 'pairing') })), x => { setTop('dm_policy', x); chTouch(); }, { place: 'top', align: 'end' });
    if (k === 'dm_scope') return menuPop(pick, CHANNEL_SCOPES.map(o => Object.assign({}, o, { on: o.v === (cfg.dm_scope || 'main') })), x => { setTop('dm_scope', x); chTouch(); }, { place: 'top', align: 'end' });
    if (k === 'unauthorized') return menuPop(pick, CHANNEL_UNAUTH.map(o => Object.assign({}, o, { on: o.v === (cfg.unauthorized_dm_behavior || 'pair') })), x => { setTop('unauthorized_dm_behavior', x); chTouch(); }, { place: 'top', align: 'end' });
    const [what, idx] = k.split(':');
    const c = chChannels()[+idx];
    if (!c) return;
    if (what === 'kind') {
      return menuPop(pick, CHANNEL_KINDS.map(o => ({ v: o.kind, t: o.t, d: o.d, on: o.kind === c.kind })), x => { c.kind = x; chTouch(); }, { place: 'top', align: 'end' });
    }
    if (what === 'cred') {
      const cur = chCred(c) || 'token_env';
      return menuPop(pick, [
        { v: 'token_env', t: t('环境变量'), d: 'SUNMAO_TG_TOKEN', on: cur === 'token_env' },
        { v: 'token_file', t: t('文件'), d: 'D:/secrets/tg.txt', on: cur === 'token_file' },
      ], x => {
        // one source at a time: the schema has no plaintext token field
        if (x === 'token_file') { delete c.token_env; c.token_file = ''; } else { delete c.token_file; c.token_env = ''; }
        chTouch();
      }, { place: 'top', align: 'end' });
    }
    return;
  }
  if (e.target.closest('[data-chsave]')) {
    const ta = $('#ch-cfg');
    if (ta && chCfgErr) return chSave(ta.value); // a broken file can only be fixed by hand
    return chSave(JSON.stringify(chCfgOf(), null, 2));
  }
  if (e.target.closest('[data-chraw]')) {
    // adopt the textarea verbatim: this is the only path that touches keys
    // the form does not know about
    const v = chParse($('#ch-cfg').value);
    if (chCfgErr) return toast(t('JSON 有问题：{msg}', { msg: chCfgErr }), 'alert', 'warn');
    chCfg = v;
    return chTouch();
  }
});

document.addEventListener('input', e => {
  const inp = e.target.closest('[data-chin]');
  if (!inp) return;
  const cfg = chCfgOf(), k = inp.dataset.chin, val = inp.value.trim();
  if (k === 'allowlist') {
    cfg.allowlist = val ? val.split(',').map(s => s.trim()).filter(Boolean) : [];
  } else if (k.startsWith('cred:')) {
    const c = chChannels()[+k.slice(5)];
    if (!c) return;
    if (c.token_file != null) { if (val) c.token_file = val; else delete c.token_file; }
    else if (val) c.token_env = val; else delete c.token_env;
  } else if (k.startsWith('timeout:')) {
    const c = chChannels()[+k.slice(8)];
    if (!c) return;
    const n = parseInt(val.replace(/[^\d]/g, ''), 10);
    if (Number.isFinite(n) && n > 0) c.poll_timeout_secs = n; else delete c.poll_timeout_secs;
  } else return;
  chDirty = true; // the re-render would steal focus; the save bar reads this on the next pass
});
