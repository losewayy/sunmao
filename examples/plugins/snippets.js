/* snippets.js — second plugin example: canned prompts, exercising every
   writable surface the facade exposes.
*
*   slots.composer    a "snippets" chip in the composer bar — its popover
*                     lists the saved prompts; picking one sends it.
*   slots.palette     a command-palette row to save the current draft.
*   slots.sessionMenu a right-click row on any session to send a snippet
*                     into it.
*   host.store/load   the snippet list persists per project via ui.json.
*   host.onSend       appends nothing — shown here vetoing an empty-ish
*                     prompt as the contract example.
*/
sunmao.register({
  id: 'snippets',
  slots: {
    composer: [{
      id: 'snip',
      icon: 'quote',
      label: null,
      tip: '常用语',
      popover(el, host) {
        const list = host.load('list') || [];
        if (!list.length) return host.toast(host.t('还没有常用语——命令面板里可保存当前草稿'), 'info');
        host.menuPop(el,
          list.map((s, i) => ({ v: String(i), t: s.length > 40 ? s.slice(0, 40) + '…' : s })),
          v => host.send(list[+v]),
          { place: 'top' });
      },
    }],
    palette: [{
      g: 'snippets',
      t: '把输入框草稿存为常用语',
      i: 'quote',
      run(host) {
        const ta = document.querySelector('#input');
        const v = (ta?.value || '').trim();
        if (!v) return host.toast(host.t('输入框是空的'), 'alert', 'warn');
        const list = host.load('list') || [];
        list.push(v);
        host.store('list', list);
        ta.value = '';
        host.toast(host.t('已存为常用语'), 'check');
      },
    }],
    sessionMenu: [{
      v: 'send-snip',
      t: '向此会话发一条常用语',
      i: 'quote',
      run(id, host) {
        const list = host.load('list') || [];
        if (!list.length) return host.toast(host.t('还没有常用语'), 'info');
        // send() targets the VIEWED session — for another session's menu
        // the honest shape is a note, not a cross-session prompt
        if (id !== host.sess()) return host.toast(host.t('先切到该会话再发送'), 'info');
        host.menuPop(document.querySelector('[data-act="crumb"]'),
          list.map((s, i) => ({ v: String(i), t: s.slice(0, 40) })),
          v => host.send(list[+v]));
      },
    }],
  },
});
