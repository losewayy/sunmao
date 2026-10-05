/* IM channels page — `~/.sunmao/channels.json` is a policy header plus one
   block per platform adapter. GET /channels hands over the raw text, so this
   page parses it, edits the keys it knows and writes the same document back:
   an entry the form doesn't cover (a newer kind, a channel-scoped override)
   survives the round trip untouched.
 *
 * One card per platform, always, in a fixed order — the file decides what a
 * block holds, never which platforms exist. `impl` marks the adapter this
 * version actually ships; the cards without one say so on their face instead
 * of passing for working channels.
 *
 * Status is a verdict, not "the file exists": a declared channel, a
 * credential and a fresh status.json heartbeat all have to agree before
 * anything here says 在线.
 */
'use strict';

/* status.json is rewritten at startup and on every reconnect-worthy event;
   silence for this long means the daemon is not running */
const CHANNEL_STALE_SECS = 90;
/* an untouched field writes nothing, so the daemon's own default applies —
   this is the schema's value for it */
const CHANNEL_DEFAULT_POLL_SECS = 30;
/* feishu and lark are one protocol on two hosts; the schema names the choice
   `region` and defaults to the China one */
const CHANNEL_REGION_DEFAULT = 'feishu_cn';
const CHANNEL_REGIONS = [
  { v: 'feishu_cn', t: '飞书（中国）', d: 'open.feishu.cn' },
  { v: 'lark_global', t: 'Lark（国际）', d: 'open.larksuite.com' },
];

/* The five platforms, in the order the page renders them. Each field carries
   what the form needs: `req` is what the adapter cannot run without (the
   verdict reads these, so "has a credential" is per-platform rather than one
   key), `src` is the [env, file] pair the secret's source may live in (the box shows the source, never the secret)
   under — the config names where to read it instead of carrying it, with
   `env`/`file` the example each source shows — `pick` renders a menu and `ph`
   is the placeholder that shows the schema default. */
const CHANNEL_PLATFORMS = [
  {
    kind: 'wechat', t: '微信', d: '个人号 iLink 协议，扫码绑定，只能被动回复',
    impl: false, ownerId: 'wxid',
    creds: [{
      k: 'cred', t: 'Bot Token', d: '扫码登录后由服务自动写入，不要手填',
      req: true, src: ['bot_token_env', 'bot_token_file'],
      env: 'SUNMAO_WECHAT_BOT_TOKEN', file: 'D:/secrets/wechat.txt',
    }],
  },
  {
    kind: 'qq', t: 'QQ', d: 'QQ 机器人开放平台，WebSocket 接入',
    impl: false, ownerId: 'openid',
    creds: [
      { k: 'app_id', t: 'App ID', d: '开放平台应用的 App ID', req: true },
      {
        k: 'cred', t: 'App Secret', d: '开放平台应用的 App Secret',
        req: true, src: ['app_secret_env', 'app_secret_file'],
        env: 'SUNMAO_QQ_APP_SECRET', file: 'D:/secrets/qq.txt',
      },
    ],
  },
  {
    kind: 'feishu', t: '飞书', d: 'App ID + Secret，长连接接收事件，不需要回调地址',
    impl: false, ownerId: 'open_id',
    creds: [
      { k: 'app_id', t: 'App ID', d: '开放平台应用的 App ID', req: true },
      {
        k: 'cred', t: 'App Secret', d: '开放平台应用的 App Secret',
        req: true, src: ['app_secret_env', 'app_secret_file'],
        env: 'SUNMAO_FEISHU_APP_SECRET', file: 'D:/secrets/feishu.txt',
      },
      { k: 'region', t: 'Region', pick: true },
    ],
  },
  {
    kind: 'dingtalk', t: '钉钉', d: '企业内部机器人，Stream 模式接入',
    impl: false, ownerId: 'userId',
    creds: [
      { k: 'corp_id', t: 'Corp ID', req: true },
      { k: 'client_id', t: 'Client ID', d: '应用的 AppKey（Client ID）', req: true },
      {
        k: 'cred', t: 'Client Secret', d: '应用的 AppSecret（Client Secret）',
        req: true, src: ['client_secret_env', 'client_secret_file'],
        env: 'SUNMAO_DINGTALK_CLIENT_SECRET', file: 'D:/secrets/dingtalk.txt',
      },
      { k: 'robot_code', t: 'Robot Code', req: true },
      { k: 'api_base_url', t: 'API Base URL', ph: 'https://api.dingtalk.com/v1.0' },
    ],
  },
  {
    kind: 'telegram', t: 'Telegram', d: 'Bot Token，长轮询',
    impl: true, ownerId: 'chat_id',
    creds: [
      {
        k: 'cred', t: '凭据', req: true, src: ['token_env', 'token_file'],
        env: 'SUNMAO_TG_TOKEN', file: 'D:/secrets/tg.txt',
      },
      { k: 'poll_timeout_secs', t: '轮询超时', d: '长轮询秒数，默认 30', ph: String(CHANNEL_DEFAULT_POLL_SECS) },
    ],
  },
];

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

let CHANNELS = null, chCfg = null, chCfgErr = '', chRaw = '', chDirty = false, chChans = null;

async function refreshChannels() {
  try {
    CHANNELS = await api('/channels');
  } catch (e) {
    CHANNELS = { error: e.message };
  }
  chCfg = null;
  chChans = null;
  chDirty = false;
  renderChannels();
}

/* the JSON text is the contract: parse it into the editable shape, and keep
   the original string. When the file does not parse, that string is the only
   copy the user can repair — a JSON.stringify of the fallback would be the
   empty document this page would then save over the file. */
function chParse(text) {
  chCfgErr = '';
  /* whatever the endpoint hands over is kept as the text this page shows and
     may write back: a string verbatim, anything else as its JSON form */
  chRaw = typeof text === 'string' ? text : (text == null ? '' : JSON.stringify(text));
  let v;
  try {
    v = chRaw.trim() ? JSON.parse(chRaw) : {};
  } catch (e) {
    chCfgErr = e.message;
    return {};
  }
  if (v && typeof v === 'object' && !Array.isArray(v)) return v;
  chCfgErr = t('顶层不是对象（读到 {type}）', { type: chType(v) });
  return {};
}
const chCfgOf = () => chCfg || (chCfg = chParse((CHANNELS && CHANNELS.config) || ''));
/* the schema types these as arrays, but a hand edit can put anything in
   either. The form still renders and leaves the value alone unless the user
   actually edits that field — clearing it here is how a save used to write
   `{"channels": []}` over the file. */
const chType = v => Array.isArray(v) ? 'array' : v === null ? 'null' : typeof v;
const chArr = v => Array.isArray(v) ? v : [];
/* the allowlist box shows whatever the file holds: the joined array, or the
   value's own text form when a hand edit put something else there */
const chListText = v => {
  if (Array.isArray(v)) return v.join(', ');
  if (v == null) return '';
  return typeof v === 'object' ? JSON.stringify(v) : String(v);
};
const chChannels = () => {
  const c = chCfgOf();
  if (Array.isArray(c.channels)) return c.channels;
  return chChans || (chChans = []);
};
const chPlatform = kind => CHANNEL_PLATFORMS.find(p => p.kind === kind);

/* Every card renders whether or not the file declares the platform, so the
   first edit is what brings a block into existence. A created block is
   disabled until the switch says otherwise — that is the schema's own
   default for a missing `enabled`, so the switch and the daemon agree. */
function chEntry(kind, create) {
  const list = chChannels();
  const hit = list.find(x => x && x.kind === kind);
  if (hit || !create) return hit || null;
  const c = { kind, enabled: false };
  list.push(c);
  // the first real edit is what replaces a hand-edited `channels` value
  const cfg = chCfgOf();
  if (cfg.channels !== list) cfg.channels = list;
  return c;
}

/* one source at a time: a secret is never written inline, the config names
   the key it lives under instead. `f.src` is the [env, file] pair; with
   neither key present the env var wins, the resolver's first choice. */
const chCredKey = (c, f) => {
  const [env, file] = f.src;
  return c && c[file] != null && c[env] == null ? file : env;
};
function chWriteCred(c, f, val) {
  const key = chCredKey(c, f);
  if (val) c[key] = val; else delete c[key];
}
const chField = (kind, k) => {
  const p = chPlatform(kind);
  return (p && p.creds.find(f => f.k === k)) || null;
};

/* "has a credential" is per-platform: every required field has to carry a
   value, and a split secret counts when either source does */
function chArmed(c) {
  const p = chPlatform(c && c.kind);
  if (!p) return false;
  return p.creds.filter(f => f.req).every(f => f.src
    ? f.src.some(k => !!(c[k] != null && String(c[k]).trim()))
    : !!(c[f.k] != null && String(c[f.k]).trim()));
}
const chImpl = c => { const p = chPlatform(c && c.kind); return !!(p && p.impl); };
const chLabel = c => { const p = chPlatform(c && c.kind); return p ? t(p.t) : String((c && c.kind) || ''); };

/* What the page is allowed to claim. Each verdict names the one thing that
   is missing, because "在线" over an unconfigured file is exactly the lie
   this replaced. Only a shipped adapter can be missing a credential — a card
   whose adapter does not exist yet is reported, never nagged about. */
function channelVerdict(cfg, st) {
  const all = Array.isArray(cfg.channels) ? cfg.channels : [];
  const on = all.filter(c => c && c.enabled === true);
  const running = (st && Array.isArray(st.channels)) ? st.channels : [];
  const fresh = !!(st && (Date.now() / 1000 - Number(st.updated || 0)) < CHANNEL_STALE_SECS);
  const live = on.filter(c => chImpl(c) && chArmed(c));
  const uncred = on.filter(c => chImpl(c) && !chArmed(c));
  const unimpl = on.filter(c => !chImpl(c));
  if (!all.length) return { key: 'none', cls: '' };
  if (!on.length) return { key: 'off', cls: '' };
  if (uncred.length) return { key: 'nocred', cls: 'warn', uncred, unimpl };
  if (!fresh) return { key: 'down', cls: 'off', unimpl };
  // a heartbeat alone is not 在线: something this build can actually run has
  // to be enabled, armed and named by status.json
  if (!live.length || !running.length) return { key: 'idle', cls: 'warn', unimpl };
  return { key: 'on', cls: 'online', running, unimpl };
}

/* one field control — the same .ch-in box every other settings input here
   uses; a value the form does not hold stays absent from the file and the
   placeholder shows what the daemon would assume */
const chIn = (kind, k, val, ph, type) =>
  `<input class="ch-in mono"${type ? ` type="${type}"` : ''} data-chin="${kind}.${k}"` +
  ` value="${esc(val == null ? '' : val)}" placeholder="${esc(ph || '')}" spellcheck="false">`;

/* one settings row per field: label, description, control. A field with no
   description of its own still marks 必填, and a value the form does not
   hold stays absent from the file — the placeholder shows the default */
function chRow(p, c, f) {
  const v = c ? c[f.k] : undefined;
  const parts = [];
  if (f.d) parts.push(esc(t(f.d)));
  if (f.req) parts.push(esc(t('必填')));
  // a split secret is never in the file: the row picks where to read it
  if (f.src) parts.push(esc(t('配置里不写密钥明文，只写来源')));
  const desc = parts.join(' · ');
  if (f.pick) {
    const hit = CHANNEL_REGIONS.find(x => x.v === (v || CHANNEL_REGION_DEFAULT)) || CHANNEL_REGIONS[0];
    return row(t(f.t), desc, `<button class="pill plain" data-chpick="${p.kind}.${f.k}"><span>${esc(t(hit.t))}</span>${ic('chev-d')}</button>`);
  }
  if (f.src) {
    const [env, file] = f.src;
    const key = chCredKey(c, f);
    const cred = key === file
      ? { v: (c && c[file]) || '', ph: f.file }
      : { v: (c && c[env]) || '', ph: f.env };
    return row(t(f.t), desc, `<div class="ch-acts"><button class="pill plain" data-chpick="${p.kind}.${f.k}"><span>${t(key === file ? '文件' : '环境变量')}</span>${ic('chev-d')}</button>${chIn(p.kind, f.k, cred.v, cred.ph)}</div>`);
  }
  return row(t(f.t), desc, chIn(p.kind, f.k, v, f.ph));
}

/* one card per platform: the platform row carries the switch, then the
   credential fields, then the optional owner id */
function chCard(p, c) {
  const desc = esc(t(p.d)) + (p.impl ? '' : ` · ${esc(t('适配器尚未实现'))}`);
  let rows = row(esc(t(p.t)), desc,
    `<button class="sw" role="switch" data-ch="${p.kind}.enabled" aria-checked="${!!(c && c.enabled === true)}" aria-label="${esc(t('启用'))}"></button>`);
  for (const f of p.creds) rows += chRow(p, c, f);
  rows += row(t('管理员'), t('这个平台的用户 ID（{id}），可选', { id: p.ownerId }),
    chIn(p.kind, 'owner', c && c.owner, p.ownerId));
  return card([rows]);
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
  /* the markup is built before it is committed: a throw in here (a shape the
     form did not expect) used to land before host.innerHTML, leaving the
     page on its loading hint with the error going nowhere */
  let html;
  try {
    html = chMarkup();
  } catch (e) {
    html = head(t('IM 渠道'), '') + `<div class="empty-hint">${esc(t('渲染失败：{msg}', { msg: (e && e.message) || e }))}</div>`;
  }
  host.innerHTML = html;
}

/* the page's markup, from the parsed config. Anything the schema types as an
   array may arrive as something else after a hand edit: it is reported and
   shown as-is, never rewritten and never fatal. */
function chMarkup() {
  const cfg = chCfgOf();
  const st = CHANNELS.status;
  const pairing = chArr(CHANNELS.pairing);
  const allow = chArr(CHANNELS.allowlist);
  const v = channelVerdict(cfg, st);
  const chans = chChannels();
  const notes = [];
  const wantArr = (key, val) => {
    if (val != null && !Array.isArray(val))
      notes.push(t('字段 {key} 不是{want}（读到 {type}），按原样显示。', { key, want: t('数组'), type: chType(val) }));
  };
  wantArr('channels', cfg.channels);
  wantArr('allowlist', cfg.allowlist);
  wantArr('pairing', CHANNELS.pairing);
  wantArr('allowlist', CHANNELS.allowlist);

  let html = head(t('IM 渠道'), t('每个平台一张卡，固定顺序，不用挑选也不用增删；改完保存，重启 IM 服务后生效。'));
  if (notes.length) html += `<div class="empty-hint">${notes.map(n => esc(n)).join('<br>')}</div>`;

  // ── status: the verdict plus what it was read from ──
  const heartbeat = st
    ? t('最近心跳 {when}', { when: new Date(Number(st.updated || 0) * 1000).toLocaleString() })
    : t('没有心跳：IM 服务没在这台机器上跑过');
  // every arm of this map is evaluated, so the running and pending lists are
  // joined once up front — `on` is not the only key this object gets built for
  const runningText = (v.running || []).join('、');
  const uncredText = (v.uncred || []).map(chLabel).join('、');
  const unimplText = (v.unimpl || []).map(chLabel).join('、');
  const detail = {
    none: t('配置文件里还没有声明渠道；打开下面某个平台的开关就会写入。'),
    off: t('渠道都已停用。'),
    nocred: t('渠道缺凭据：{kinds}。', { kinds: uncredText }),
    down: t('服务未运行。在部署机上执行 sunmao im。'),
    idle: t('服务在跑，但没有渠道在监听。'),
    on: t('服务在跑：{chans}', { chans: runningText }),
  }[v.key];
  const note = unimplText ? ` · ${t('适配器尚未实现')}：${unimplText}` : '';
  html += sec(t('服务状态'), '', card([
    row(t('运行状态'), `${detail}${note} · ${heartbeat}`,
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
    row(t('配置白名单'), t('channel:sender，逗号分隔；留空即只认配对结果'), `<input class="ch-in mono" data-chin="allowlist" value="${esc(chListText(cfg.allowlist))}" placeholder="telegram:12345" spellcheck="false">`),
    row(t('管理员'), t('可用 /pairing 的 sender；首个批准者自动成为管理员'), `<span class="tag mono">${esc(cfg.owner || t('未指定'))}</span>`),
  ]));

  // ── the five cards, in table order, one .cr row per field; a block whose
  //    kind has no card is not rendered, and the JSON editor below is where
  //    it stays visible (and preserved) ──
  const cards = CHANNEL_PLATFORMS.map(p => chCard(p, chans.find(c => c && c.kind === p.kind))).join('');
  html += sec(t('渠道'), '', cards);

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
    `<div class="card glass cfg"><textarea id="ch-cfg" class="channel-config" aria-label="${esc(t('IM 渠道配置'))}" spellcheck="false">${esc(chCfgText())}</textarea></div>
     ${chCfgErr ? `<div class="empty-hint">${esc(t('当前文件不是合法 JSON：{msg}', { msg: chCfgErr }))}<br>${esc(t('下面是文件原文，修好后点保存；解析不通过不会写回。'))}</div>` : ''}
     <div class="channel-save"><button class="btn ghost sm" data-chraw>${t('用这段 JSON 覆盖表单')}</button></div>`);

  return html;
}

/* The advanced editor's text. When the file does not parse, its own bytes are
   what the user has to repair — the "save" button below picks this same
   textarea on that path, so a stringify of the empty fallback would end up
   replacing the file with `{"channels": []}` (the audit's repro). */
const chCfgText = () => chCfgErr ? chRaw : JSON.stringify(chCfgOf(), null, 2);

/* write the parsed shape back as text; the daemon re-reads it at startup */
async function chSave(text) {
  try {
    await api('/channels', jput({ config: text }));
    toast(t('配置已保存；重启 IM 服务后生效'), 'check');
  } catch (err) { toast(t('保存失败：{msg}', { msg: err.message }), 'alert', 'warn'); }
  return refreshChannels();
}

/* the repair path for a file that does not parse: only text that parses again
   may go back to the server, so the fallback can never be written over the
   user's bytes */
async function chSaveText(text) {
  let v;
  try {
    v = text.trim() ? JSON.parse(text) : {};
  } catch (err) {
    return toast(t('JSON 有问题：{msg}', { msg: err.message }), 'alert', 'warn');
  }
  if (!v || typeof v !== 'object' || Array.isArray(v))
    return toast(t('顶层不是对象（读到 {type}）', { type: chType(v) }), 'alert', 'warn');
  return chSave(JSON.stringify(v, null, 2));
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
  const sw = e.target.closest('[data-ch]');
  if (sw) {
    const [kind, key] = sw.dataset.ch.split('.');
    if (key !== 'enabled') return;
    // flip the model, not the button: menus.js has a global .sw handler that
    // rewrites aria-checked first (it is registered earlier), so reading the
    // DOM here would invert the switch
    const c = chEntry(kind, true);
    c.enabled = c.enabled !== true;
    return chTouch();
  }
  const pick = e.target.closest('[data-chpick]');
  if (pick) {
    const cfg = chCfgOf(), k = pick.dataset.chpick;
    const setTop = (key, v) => { cfg[key] = v; };
    if (k === 'dm_policy') return menuPop(pick, CHANNEL_POLICIES.map(o => Object.assign({}, o, { t: t(o.t), d: t(o.d), on: o.v === (cfg.dm_policy || 'pairing') })), x => { setTop('dm_policy', x); chTouch(); }, { place: 'top', align: 'end' });
    if (k === 'dm_scope') return menuPop(pick, CHANNEL_SCOPES.map(o => Object.assign({}, o, { t: t(o.t), d: t(o.d), on: o.v === (cfg.dm_scope || 'main') })), x => { setTop('dm_scope', x); chTouch(); }, { place: 'top', align: 'end' });
    if (k === 'unauthorized') return menuPop(pick, CHANNEL_UNAUTH.map(o => Object.assign({}, o, { t: t(o.t), d: t(o.d), on: o.v === (cfg.unauthorized_dm_behavior || 'pair') })), x => { setTop('unauthorized_dm_behavior', x); chTouch(); }, { place: 'top', align: 'end' });
    const dot = k.indexOf('.');           // "<kind>.<field>"
    const c = chEntry(k.slice(0, dot), true);
    const f = k.slice(dot + 1);
    if (!c || !f) return;
    if (f === 'region') {
      return menuPop(pick, CHANNEL_REGIONS.map(o => ({ v: o.v, t: t(o.t), d: o.d, on: o.v === (c.region || CHANNEL_REGION_DEFAULT) })),
        x => { c.region = x; chTouch(); }, { place: 'top', align: 'end' });
    }
    if (f === 'cred') {
      const field = chField(c.kind, 'cred');
      if (!field) return;
      const [env, file] = field.src;
      const cur = chCredKey(c, field);
      return menuPop(pick, [
        { v: env, t: t('环境变量'), d: field.env, on: cur === env },
        { v: file, t: t('文件'), d: field.file, on: cur === file },
      ], x => {
        // one source at a time: the schema has no plaintext secret field
        if (x === file) { delete c[env]; c[file] = ''; } else { delete c[file]; c[env] = ''; }
        chTouch();
      }, { place: 'top', align: 'end' });
    }
    return;
  }
  if (e.target.closest('[data-chsave]')) {
    const ta = $('#ch-cfg');
    // a broken file can only be repaired by hand — and only text that parses
    // again is allowed back to the server, never the in-memory fallback
    if (ta && chCfgErr) return chSaveText(ta.value);
    return chSave(JSON.stringify(chCfgOf(), null, 2));
  }
  if (e.target.closest('[data-chraw]')) {
    // adopt the textarea verbatim: this is the only path that touches keys
    // the form does not know about
    const v = chParse($('#ch-cfg').value);
    if (chCfgErr) return toast(t('JSON 有问题：{msg}', { msg: chCfgErr }), 'alert', 'warn');
    chCfg = v;
    chChans = null;
    return chTouch();
  }
});

document.addEventListener('input', e => {
  const inp = e.target.closest('[data-chin]');
  if (!inp) return;
  const key = inp.dataset.chin, val = inp.value.trim();
  if (key === 'allowlist') {
    chCfgOf().allowlist = val ? val.split(',').map(s => s.trim()).filter(Boolean) : [];
  } else {
    const dot = key.indexOf('.');         // "<kind>.<field>"
    const c = chEntry(key.slice(0, dot), true);
    const k = key.slice(dot + 1);
    if (!c || !k) return;
    if (k === 'cred') {
      const field = chField(c.kind, 'cred');
      if (field) chWriteCred(c, field, val);
    } else if (k === 'poll_timeout_secs') {
      const n = parseInt(val.replace(/[^\d]/g, ''), 10);
      if (Number.isFinite(n) && n > 0) c[k] = n; else delete c[k];
    } else if (val) c[k] = val;
    else delete c[k];
  }
  chDirty = true; // the re-render would steal focus; the save bar reads this on the next pass
});
