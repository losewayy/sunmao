/* IM channels page — ~/.sunmao/channels.json editor + pairing/allowlist
   ledger. Rides GET/PUT /channels and POST /channels/pairing/approve on
   the shared route table; the daemon is cold-plug — edits apply at the
   next `sunmao im` launch. */
'use strict';

let CHANNELS = null;

async function refreshChannels() {
  try {
    CHANNELS = await api('/channels');
  } catch (e) {
    CHANNELS = { error: e.message };
  }
  renderChannels();
}

function renderChannels() {
  if (view !== 'settings' || setPage !== 'channels') return;
  const host = $('#set-generic');
  if (!CHANNELS) {
    host.innerHTML = head('IM 渠道', 'sunmao im 的接入配置与准入账本。') + '<div class="empty-hint">正在读取渠道状态…</div>';
    refreshChannels();
    return;
  }
  if (CHANNELS.error) {
    host.innerHTML = head('IM 渠道', '') + `<div class="empty-hint">读取失败：${esc(CHANNELS.error)}</div>`;
    return;
  }
  const cfg = CHANNELS.config || '';
  const st = CHANNELS.status;
  const pairing = CHANNELS.pairing || [];
  const allow = CHANNELS.allowlist || [];

  let html = head('IM 渠道', '配置写入 ~/.sunmao/channels.json — 下次 sunmao im 启动生效（cold-plug）。');

  // daemon heartbeat — status.json 是 sunmao im 落地的心跳
  html += sec('守护进程', '', card([
    row('状态', st ? '最近更新 ' + new Date(st.updated * 1000).toLocaleString() : '未运行或未上报',
        mono(st ? '已连接 ' + (st.channels || []).join(', ') : 'offline')),
  ]));

  // pairing queue — approve rides the same ledger `sunmao pairing approve` writes
  html += sec('配对请求', '陌生 DM 拿到的 8 位配对码；批准后 sender 进入 allowlist（首个批准者成为 owner）。',
    card(pairing.length
      ? pairing.map(p => row(
          `<span class="mono">${esc(p.code)}</span>`,
          `${esc(p.channel)} · sender ${esc(p.sender)}`,
          `<button class="btn allow sm" data-pair="${esc(p.code)}">${ic('check')}批准</button>`))
      : [row('无待批准请求', '', '')]));

  html += sec('Allowlist', '',
    card(allow.length
      ? allow.map(a => row(`<span class="mono">${esc(a.channel)}:${esc(a.sender)}</span>`, '', mono(a.role)))
      : [row('空 — 首个批准的 pairing 即 owner', '', '')]));

  // raw editor — the file is the contract; the daemon validates on boot
  html += sec('channels.json', '',
    `<div class="card glass cfg"><textarea id="ch-cfg" spellcheck="false" style="width:100%;min-height:14em;font-family:var(--font-mono);font-size:12px;background:transparent;border:0;color:var(--c-text);resize:vertical;padding:12px">${esc(cfg)}</textarea></div>`
    + `<div class="pv-acts" style="margin-top:8px"><button class="btn allow sm" data-act="channels-save">${ic('check')}保存</button></div>`);

  host.innerHTML = html;
}

document.addEventListener('click', async e => {
  const ap = e.target.closest('[data-pair]');
  if (ap) {
    try {
      await api('/channels/pairing/approve', jpost({ code: ap.dataset.pair }));
      toast('已批准', 'check');
    } catch (err) { toast(`批准失败：${err.message}`, 'alert', 'warn'); }
    return refreshChannels();
  }
  const sv = e.target.closest('[data-act="channels-save"]');
  if (sv) {
    try {
      await api('/channels', jput({ config: $('#ch-cfg').value }));
      toast('已写入 channels.json — 重启 sunmao im 生效', 'check');
    } catch (err) { toast(`保存失败：${err.message}`, 'alert', 'warn'); }
    return refreshChannels();
  }
});
