/* The send queue above the composer — one row per waiting message, the row's
   left edge carrying the grip that reorders it. Split out of composer.js the
   way quote.js was: the drag gesture is a self-contained unit.
   Two queues, one column: steer items (⚡, injected at the running turn's
   next request boundary) are pinned on top — `steer_cancel` is the only op
   the kernel offers them, so their left slot holds the zap marker instead of
   a grip; the FIFO prompts below are what `input_move` reorders. The kernel
   stays the single source: rows render the `input_queue` / `steer_queue`
   broadcasts verbatim, the controls only send ops back. */
'use strict';

let steerQ = [];
let inputQ = [];
let qDrag = null;   // {id, el, pointerId} while a pointer drag is live
const qClip = s => esc(s.length > 40 ? s.slice(0, 40) + '…' : s);
const qBox = () => $('#cmp-queue');
/* FIFO rows in DOM order = queue order; the pinned steer rows are excluded */
const qRows = () => $$('.q-row[data-qid]', qBox());

function qRowHTML(o) {
  const grip = o.steer
    ? `<span class="q-grip pin" data-tip="${t('引导已入队 · 下个请求边界注入')}">${ic('zap', 'i xs')}</span>`
    : `<button class="q-grip" type="button" data-grip="${o.id}" aria-label="${t('拖动调整顺序 · Alt+↑/↓ 移动')}" data-tip="${t('拖动调整顺序 · Alt+↑/↓ 移动')}">${ic('grip')}</button>`;
  const body = o.steer
    ? `<span class="q-text">${qClip(o.text)}</span>`
    : `<button class="q-text" type="button" data-qedit="${o.id}" data-tip="${t('排队中 · 点击编辑')}">${qClip(o.text)}</button>`;
  const x = o.steer
    ? `<button class="chip-x" type="button" data-si="${o.si}" aria-label="${t('撤回')}">×</button>`
    : `<button class="chip-x" type="button" data-qx="${o.id}" aria-label="${t('移除')}">×</button>`;
  return `<div class="q-row${o.steer ? ' q-steer' : ''}"${o.steer ? '' : ` data-qid="${o.id}"`}>`
    + grip + `<span class="q-state">${t('等待发送')}</span>` + body + x + '</div>';
}

function renderQueueChips() {
  const box = qBox();
  if (!steerQ.length && !inputQ.length) { box.hidden = true; box.innerHTML = ''; return; }
  box.hidden = false;
  box.innerHTML =
    steerQ.map((s, i) => qRowHTML({ steer: true, si: i, text: s })).join('') +
    inputQ.map(q => qRowHTML({ id: q.id, text: q.text })).join('');
}

/* the DOM side of a reorder, shared by the drag and the keyboard path —
   `to` indexes the FIFO rows without `el`, which is where el ends up */
function qPlace(el, to) {
  const others = qRows().filter(r => r !== el);
  const at = others[to];
  if (at) { if (at.previousElementSibling !== el) at.before(el); }
  else if (others.length) {
    const last = others[others.length - 1];
    if (last.nextElementSibling !== el) last.after(el);
  }
}

/* one reorder, one frame. Optimistic: the array and the row move first, the
   `input_move` goes out after; a frame the wire refuses puts both back. The
   host reads `dir` as a RELATIVE step, so the position the row landed on
   travels as `to - from` (0 is a no-op nobody sends). */
function qMove(id, to) {
  const from = inputQ.findIndex(q => q.id === id);
  if (from < 0) return;
  to = Math.max(0, Math.min(inputQ.length - 1, to));
  if (to === from) return;
  const [item] = inputQ.splice(from, 1);
  inputQ.splice(to, 0, item);
  const el = qRows().find(r => +r.dataset.qid === id);
  if (el) qPlace(el, to);
  const undo = () => {
    const at = inputQ.findIndex(q => q.id === id);
    if (at >= 0) { const [it] = inputQ.splice(at, 1); inputQ.splice(from, 0, it); }
    renderQueueChips();
    toast(t('排序未生效，顺序已还原'), 'alert', 'warn');
  };
  if (!wsSend({ type: 'input_move', id, dir: to - from }, undo)) undo();
}

/* drag to reorder — pointerdown on the grip arms it, window listeners carry
   the gesture (pointer capture would too, but a synthetic pointer has no
   capture target), pointerup drops. A row only trades places once the
   pointer crosses another row's midline, so a plain click changes nothing.
   A release the page never sees (the pointer left the window) must not leave
   the gesture armed: the next stray pointerup would then commit a position
   the user never confirmed. So the button state on every move, a press
   somewhere else, a lost focus and a hidden tab all end it with a redraw and
   NO `input_move`. */
function qDragMove(e) {
  if (!qDrag || e.pointerId !== qDrag.pointerId) return;
  if (e.buttons === 0) return qDragAbort();   // released outside the window
  const others = qRows().filter(r => r !== qDrag.el);
  // the row's new index = how many of the other rows' midlines the pointer
  // has already passed
  const to = others.filter(r => {
    const b = r.getBoundingClientRect();
    return e.clientY > b.top + b.height / 2;
  }).length;
  qPlace(qDrag.el, to);
}
function qDragOff() {
  window.removeEventListener('pointermove', qDragMove);
  window.removeEventListener('pointerup', qDragEnd);
  window.removeEventListener('pointercancel', qDragCancel);
  window.removeEventListener('pointerdown', qDragBail);
  window.removeEventListener('blur', qDragBail);
  document.removeEventListener('visibilitychange', qDragBail);
  const d = qDrag;
  qDrag = null;
  if (d) { d.el.classList.remove('drag'); qBox().classList.remove('dragging'); }
  return d;
}
/* the gesture is over without a release we can trust — drop it and redraw the
   kernel's order; this path must never send */
function qDragAbort() { qDragOff(); renderQueueChips(); }
function qDragEnd(e) {
  if (!qDrag || e.pointerId !== qDrag.pointerId) return;
  if (e.button) return qDragAbort();  // only the arming button's release commits
  const d = qDragOff();
  if (!d) return;
  const to = qRows().indexOf(d.el);   // -1 if a broadcast redrew the column mid-drag
  if (to >= 0) qMove(d.id, to);
}
function qDragCancel(e) {
  if (!qDrag || e.pointerId !== qDrag.pointerId) return;
  qDragAbort();
}
/* the release that ended this drag is already behind us: a press that is not
   the grip's own, a lost focus, or a hidden tab */
function qDragBail(e) {
  if (!qDrag) return;
  if (e.type === 'visibilitychange' && !document.hidden) return;
  // the press that armed (or re-armed) the live gesture reaches window after
  // the grip's own handler, so it is not the "press somewhere else" we act on
  if (e.type === 'pointerdown' && e.target.closest && e.target.closest('.q-grip[data-grip]')) return;
  qDragAbort();
}
qBox().addEventListener('pointerdown', e => {
  const g = e.target.closest('.q-grip[data-grip]');
  if (!g || e.button) return;
  const id = +g.dataset.grip;
  // a press landing while a gesture is still armed means the previous release
  // was lost: drop that one first (it redraws), then start clean
  if (qDrag) qDragAbort();
  // resolve the row by id, not by the pressed node — the abort above may have
  // redrawn the column inside this same dispatch
  const el = qRows().find(r => +r.dataset.qid === id);
  if (!el) return;
  e.preventDefault();
  qDrag = { id, el, pointerId: e.pointerId };
  el.classList.add('drag');
  qBox().classList.add('dragging');
  window.addEventListener('pointermove', qDragMove);
  window.addEventListener('pointerup', qDragEnd);
  window.addEventListener('pointercancel', qDragCancel);
  window.addEventListener('pointerdown', qDragBail);
  window.addEventListener('blur', qDragBail);
  document.addEventListener('visibilitychange', qDragBail);
});

/* keyboard reorder — the grip is a real button, so Tab reaches it; Alt+↑/↓
   moves that row one place (the one modifier combo the composer's own
   keydown does not already claim) */
qBox().addEventListener('keydown', e => {
  if (!e.altKey || (e.key !== 'ArrowUp' && e.key !== 'ArrowDown')) return;
  const g = e.target.closest('.q-grip[data-grip]');
  if (!g) return;
  e.preventDefault(); e.stopPropagation();
  const id = +g.dataset.grip, at = inputQ.findIndex(q => q.id === id);
  if (at < 0) return;
  qMove(id, at + (e.key === 'ArrowDown' ? 1 : -1));
  const nb = qBox().querySelector(`.q-grip[data-grip="${id}"]`);
  if (nb) nb.focus();
});

qBox().addEventListener('click', e => {
  const b = e.target.closest('[data-si],[data-qx],[data-qedit]');
  if (!b) return;
  if (b.dataset.si !== undefined) return wsSend({ type: 'steer_cancel', idx: +b.dataset.si });
  if (b.dataset.qx !== undefined) return wsSend({ type: 'input_remove', id: +b.dataset.qx });
  // inline edit — swap the label for an input seeded with the FULL text (the
  // row shows a 40-char clip); Enter/blur commits, Esc discards
  const q = inputQ.find(x => x.id === +b.dataset.qedit);
  if (!q) return;
  const inp = document.createElement('input');
  inp.value = q.text; inp.spellcheck = false;
  b.replaceWith(inp);
  inp.focus(); inp.setSelectionRange(inp.value.length, inp.value.length);
  let done = false;
  const commit = save => {
    if (done) return; done = true;
    if (save && inp.value.trim() && inp.value !== q.text)
      wsSend({ type: 'input_edit', id: q.id, text: inp.value.trim() });
    else renderQueueChips();
  };
  inp.addEventListener('keydown', ev => {
    ev.stopPropagation();
    if (ev.key === 'Enter') { ev.preventDefault(); commit(true); }
    if (ev.key === 'Escape') { ev.preventDefault(); commit(false); }
  });
  inp.addEventListener('blur', () => commit(true));
});
