/* todos.js — the plugin system's first customer: a dock pane listing the
   session's live task list. `/todos` only ever rendered as a one-off note;
   this pane keeps the latest `todos` frame pinned where you can see it.

   Contract (v1): the file runs as an ES module, reaches the app via
   `window.sunmao`, and calls `register` exactly once DURING evaluation —
   `spec.id` must equal the filename minus `.js` (the import window is how
   the host binds file to plugin; deferred setTimeout-register is refused).
   Dock slots mount lazily on first activation with (bodyEl, host); the
   host is the capability facade — session-scoped api(), live frames via
   on('live'), session switches via on('session'), i18n/toast helpers. */
sunmao.register({
  id: 'todos',
  slots: {
    dock: [{
      id: 'list',
      title: t('待办'),
      icon: 'square-check',
      mount(el, host) {
        // null = TodoWrite never ran this session; [] = it ran and cleared.
        // Collapsing the two hides "I emptied the list" from the agent's own
        // audit trail.
        let items = null;
        const MARK = { done: '☑', in_progress: '▶', pending: '☐' };
        const render = () => {
          const done = (items || []).filter(i => i.status === 'done').length;
          el.innerHTML = `<section class="df"><div class="df-h"><span>${host.t('待办')}</span>${items ? `<span class="df-cnt">${done}/${items.length}</span>` : ''}</div>` +
            (items === null
              ? `<div class="empty-hint">${host.t('会话还没有任务列表')}</div>`
              : items.length === 0
                ? `<div class="empty-hint">${host.t('任务列表已清空')}</div>`
                : `<div class="plg-todos">${items.map(i => `<div class="plg-todo ${host.esc(i.status || 'pending')}"><span class="plg-mark">${MARK[i.status] || '☐'}</span><span>${host.esc(i.content || '')}</span></div>`).join('')}</div>`) +
            `</section>`;
        };
        host.on('live', ev => { if (ev.type === 'todos') { items = ev.items || []; render(); } });
        host.on('session', () => { items = null; render(); });
        render();
      },
    }],
  },
});
