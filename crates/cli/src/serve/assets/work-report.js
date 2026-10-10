/* Last-turn work facts from the same durable/live events as the transcript.
 * Never infer that tests passed or files changed from the model's prose. */
'use strict';
const WorkSummary = (() => {
  const fresh = () => ({calls:0,done:0,failed:0,tests:0,testFails:0,
    modified:new Set(),pending:new Map(),outcome:'idle'});
  const argsOf = x => {
    if (x && typeof x === 'object') return x;
    if (typeof x === 'string') {try {return JSON.parse(x);} catch {}}
    return {};
  };
  const shellTest = cmd => /(?:^|[;&|\s])(?:cargo\s+test|node\s+--test|npm\s+test|pnpm\s+test|pytest|go\s+test|dotnet\s+test)(?:\s|$)/i.test(cmd);
  function apply(state,ev) {
    if (!ev || typeof ev !== 'object') return state;
    if ((ev.type === 'message' && ev.message?.role === 'user') || ev.type === 'user_message' || ev.type === 'compacted') return fresh();
    const call = ev.type === 'tool_call' ? ev.call : null;
    if (ev.type === 'tool_call' || ev.type === 'tool_start') {
      const name = String(call?.function?.name || ev.name || '');
      const args = argsOf(call?.function?.arguments ?? ev.args);
      const id = call?.id || ev.call_id || String(state.calls);
      const key = [ev.depth || 0,ev.lane || 0,id].join(':');
      state.pending.set(key,{name,args});
      state.calls++; state.outcome='running'; return state;
    }
    if (ev.type === 'tool_result' || ev.type === 'tool_done') {
      const id = ev.call_id || '';
      const prefix = [ev.depth || 0,ev.lane || 0,id].join(':');
      let key = id && state.pending.has(prefix) ? prefix : null;
      if (!key) for (const [k,item] of state.pending) if (item.name === ev.name) {key=k;break;}
      const item = key ? state.pending.get(key) : null;
      if (key) state.pending.delete(key);
      const name = item?.name || ev.name || '';
      state.done++; if (!ev.ok) state.failed++;
      if (ev.ok && (name === 'Write' || name === 'Edit')) {
        const path = item?.args?.file_path || item?.args?.path;
        if (typeof path === 'string' && path.length <= 512) state.modified.add(path);
      }
      if (name === 'Bash' || name === 'shell') {
        if (shellTest(String(item?.args?.command || ''))) {
          state.tests++; if (!ev.ok) state.testFails++;
        }
      }
      return state;
    }
    if (ev.type === 'turn_end') state.outcome = ev.outcome || 'completed';
    return state;
  }
  const view = s => ({calls:s.calls,done:s.done,failed:s.failed,
    tests:s.tests,testFails:s.testFails,modified:[...s.modified],outcome:s.outcome});
  return {fresh,apply,view};
})();
let workSummaryState = WorkSummary.fresh();
function workReportDraw() {
  const host = $('#df-report-body'); if (!host) return;
  const v = WorkSummary.view(workSummaryState);
  if (!v.calls && !v.modified.length) {
    host.innerHTML = '<div class="empty-row">' + t('本轮尚无工具操作') + '</div>'; return;
  }
  const status = v.outcome === 'running' ? t('进行中') :
    v.outcome === 'completed' ? t('已结束') : t('已结束或中断');
  const rows = [
    t('已完成工具调用') + ': ' + v.done + ' / ' + v.calls,
    t('失败的工具调用') + ': ' + v.failed,
    t('检测到的测试命令') + ': ' + v.tests + (v.testFails ? ' (' + v.testFails + ' failed)' : ''),
    t('成功修改的文件') + ': ' + v.modified.length
  ];
  host.innerHTML = '<div class="fl-t">' + esc(status) + '</div>' +
    rows.map(r => '<div class="fl-i">'+esc(r)+'</div>').join('') +
    v.modified.slice(-6).map(p => '<div class="fl-i" data-tip="'+esc(p)+'">'+esc(p)+'</div>').join('') +
    '<div class="pv-acts"><button type="button" class="btn ghost sm" data-report-rewind>' +
    t('选择回退点') + '</button>' + (v.modified.length ?
      '<button type="button" class="btn ghost sm" data-report-review>' +
      t('定位最近的文件修改') + '</button>' : '') + '</div>';
  host.querySelector('[data-report-rewind]')?.addEventListener('click', ev =>
    rewindPick(sessionId,ev.currentTarget));
  host.querySelector('[data-report-review]')?.addEventListener('click', () => {
    const edits = $('.tool',TX).filter(el => /^(Edit|Write)$/.test($('.nm',el)?.textContent?.trim() || ''));
    const last = edits[edits.length - 1];
    if (!last) return toast(t('本轮文件修改卡片不在当前记录里'),'alert','warn');
    const group = last.closest('.tools');
    if (group && group.classList.contains('fold')) foldGroup(group,false);
    last.scrollIntoView({behavior:motion.reduced() ? 'instant' : 'smooth',block:'center'});
  });
}
function workReportReset(draw=true) {workSummaryState=WorkSummary.fresh();if(draw)workReportDraw();}
function workReportEvent(ev,draw=true) {workSummaryState=WorkSummary.apply(workSummaryState,ev);if(draw)workReportDraw();}
if (typeof module !== 'undefined' && module.exports) module.exports = WorkSummary;
