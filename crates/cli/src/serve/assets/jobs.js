/* background jobs — dock card fed by GET /jobs, nudged by jobs.changed */
'use strict';

/* ================= background jobs card ================= */
/* Job state is the filesystem (.sunmao/jobs/{id}/output.log + exit.json);
   the kernel nudges with `jobs.changed` hooks at spawn and exit — and any
   live tool frame may signal progress, so those throttle a re-pull too. */
let djTimer = 0;
function refreshJobsSoon() { clearTimeout(djTimer); djTimer = setTimeout(refreshJobs, DEBOUNCE_ROSTER); }
async function refreshJobs() {
  if (!sessionId) return renderJobs([]);
  try { renderJobs((await api('/jobs?sess=' + encodeURIComponent(sessionId))).jobs || []); }
  catch { /* host gone between frames — keep the last list */ }
}
function jobWhen(ms) {
  if (!ms) return '';
  const d = new Date(ms), today = new Date(); today.setHours(0, 0, 0, 0);
  return ms >= today.getTime() ? pad(d.getHours()) + ':' + pad(d.getMinutes()) : (d.getMonth() + 1) + '/' + d.getDate();
}
function renderJobs(jobs) {
  const box = $('#dj-list');
  if (!box) return;
  const running = jobs.filter(j => j.running).length;
  // tab-badge: bare count — the running-vs-total breakdown would overflow
  // the chip; the pane body still shows each row's own 运行中/exit tag
  const dc = $('#dj-count'); if (dc) dc.textContent = jobs.length ? String(jobs.length) : '';
  box.innerHTML = jobs.map(j => {
    const st = j.running ? 'run' : j.exit === 0 ? 'done' : 'off';
    const tail = (j.preview || '').trim().split('\n').pop() || '';
    return `<button class="rt" data-job="${esc(j.id)}" data-tip="${esc(j.id + ' · 点击查看输出')}"><i class="sd ${st}"></i><span class="rt-id mono">${esc(j.id)}</span><span class="tag">${j.running ? '运行中' : 'exit ' + j.exit}</span><span class="rt-p">${esc(tail)}</span><span class="rt-when">${esc(jobWhen(j.mtime))}</span></button>`;
  }).join('') || '<div class="empty-row">没有后台任务</div>';
}
async function jobOutPop(anchor, id) {
  if (popAnchor === anchor) return closePop();
  let v;
  try { v = await api('/jobs/' + encodeURIComponent(id) + '/output?sess=' + encodeURIComponent(sessionId)); }
  catch (e) { return toast(`读取失败：${e.message}`, 'alert', 'warn'); }
  const tail = String(v.chunk || '');
  const head = `${esc(id)} · ${v.exit === null || v.exit === undefined ? '运行中' : 'exit ' + v.exit} · ${fmtBytes(v.total || 0)}`;
  pop(anchor, `<div class="lbl">${head}</div><pre class="jobview scroll">${esc(tail) || '（暂无输出）'}</pre>`, { align: 'end', cls: 'jobview-pop' });
}
document.addEventListener('click', e => {
  const b = e.target.closest('[data-job]');
  if (b) jobOutPop(b, b.dataset.job);
});
