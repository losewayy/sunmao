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
    host.innerHTML = head('IM 渠道', '') + '<div class="empty-hint">正在读取渠道状态…</div>';
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

  let html = head('IM 渠道', '');

  // daemon heartbeat — status.json 是 sunmao im 落地的心跳
  html += sec('服务状态', '', card([
    row('运行状态', st ? '最近更新 ' + new Date(st.updated * 1000).toLocaleString() : '尚未连接',
        `<span class="channel-state ${st ? 'online' : 'offline'}"><i class="sd ${st ? 'done' : 'off'}"></i>${st ? '在线' : '离线'}</span>`),
  ]));

  // pairing queue — approve rides the same ledger `sunmao pairing approve` writes
  html += sec('待处理配对', '',
    card(pairing.length
      ? pairing.map(p => row(
          `<span class="mono">${esc(p.code)}</span>`,
          `${esc(p.channel)} · ${esc(p.sender)}`,
          `<button class="btn allow sm" data-pair="${esc(p.code)}">${ic('check')}允许</button>`))
      : ['<div class="channel-empty">暂无待处理请求</div>']));

  html += sec('已允许账号', '',
    card(allow.length
      ? allow.map(a => {
          const role = a.role === 'owner' ? '管理员' : a.role;
          return row(`${esc(a.channel)} · ${esc(a.sender)}`, '', `<span class="tag">${esc(role)}</span>`);
        })
      : ['<div class="channel-empty">暂无已允许账号</div>']));

  // raw editor — the file is the contract; the daemon validates on boot
  html += sec('渠道配置', '',
    `<div class="card glass cfg"><textarea id="ch-cfg" class="channel-config" aria-label="IM 渠道配置" spellcheck="false">${esc(cfg)}</textarea></div>`
    + `<div class="channel-save"><button class="btn allow sm" data-act="channels-save">${ic('check')}保存</button><span>重启 IM 服务后生效</span></div>`);

  host.innerHTML = html;
}

document.addEventListener('click', async e => {
  const ap = e.target.closest('[data-pair]');
  if (ap) {
    try {
      await api('/channels/pairing/approve', jpost({ code: ap.dataset.pair }));
      toast('已允许账号接入', 'check');
    } catch (err) { toast(`操作失败：${err.message}`, 'alert', 'warn'); }
    return refreshChannels();
  }
  const sv = e.target.closest('[data-act="channels-save"]');
  if (sv) {
    try {
      await api('/channels', jput({ config: $('#ch-cfg').value }));
      toast('配置已保存；重启 IM 服务后生效', 'check');
    } catch (err) { toast(`保存失败：${err.message}`, 'alert', 'warn'); }
    return refreshChannels();
  }
});
