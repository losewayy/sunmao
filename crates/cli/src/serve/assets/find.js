/* transcript find — Ctrl+F in-page search over #tx: match count,
   Enter/Shift-Enter jump, <mark> highlight */
'use strict';

let findBar = null, findMarks = [], findIdx = -1;

function openFind() {
  if (findBar) { $('#find-in', findBar).focus(); $('#find-in', findBar).select(); return; }
  findBar = document.createElement('div');
  findBar.className = 'findbar glass';
  findBar.innerHTML = `${ic('search')}<input id="find-in" placeholder="${esc(t('在对话中查找'))}" spellcheck="false" autocomplete="off"><span class="cnt" id="find-cnt"></span><button class="ib" data-tip="${esc(t('上一个|Shift+Enter'))}" aria-label="${esc(t('上一个'))}">${ic('chev-u')}</button><button class="ib" data-tip="${esc(t('下一个|Enter'))}" aria-label="${esc(t('下一个'))}">${ic('chev-d')}</button><button class="ib" data-tip="${esc(t('关闭|Esc'))}" aria-label="${esc(t('关闭'))}">${ic('x')}</button>`;
  $('#v-session').appendChild(findBar);
  const inp = $('#find-in', findBar);
  const [up, down, close] = $$('.ib', findBar);
  // every keystroke used to TreeWalker-scan the whole transcript — debounce
  // like the rail search does (250ms is already the house interval)
  let findT = 0;
  inp.addEventListener('input', () => { findIdx = -1; clearTimeout(findT); findT = setTimeout(findApply, 150); });
  inp.addEventListener('keydown', e => {
    if (e.key === 'Escape') { e.preventDefault(); closeFind(); return; }
    if (e.key === 'Enter') { e.preventDefault(); findJump(e.shiftKey ? -1 : 1); }
  });
  up.addEventListener('click', () => findJump(-1));
  down.addEventListener('click', () => findJump(1));
  close.addEventListener('click', closeFind);
  inp.focus();
}
function closeFind() {
  if (!findBar) return;
  findClear();
  const bar = findBar; findBar = null;
  bar.classList.add('out'); setTimeout(() => bar.remove(), motion.dur('fast'));
  $('#input').focus();
}
function findClear() {
  for (const m of findMarks) {
    const p = m.parentNode;
    if (!p) continue;
    while (m.firstChild) p.insertBefore(m.firstChild, m);
    p.removeChild(m);
  }
  findMarks = []; findIdx = -1;
  TX.normalize();
}
function findApply() {
  findClear();
  const q = $('#find-in', findBar).value;
  if (!q) { $('#find-cnt', findBar).textContent = ''; return; }
  const ql = q.toLowerCase(), qlen = ql.length;
  // collect first — mutating the tree mid-walk makes nextNode() unstable.
  // tool output bodies (.tool-o) stay out: 12k-char pre dumps would drown
  // a query meant to find *conversation*.
  const walker = document.createTreeWalker(TX, 4, { acceptNode: n => n.parentElement && n.parentElement.closest('.tool-o') ? 2 : 1 });
  const nodes = [];
  for (let n = walker.nextNode(); n; n = walker.nextNode()) nodes.push(n);
  for (const n of nodes) {
    let cur = n;
    for (;;) {
      const at = cur.nodeValue.toLowerCase().indexOf(ql);
      if (at < 0) break;
      const mid = cur.splitText(at);      // cur="before", mid="match+rest"
      const rest = mid.splitText(qlen);   // mid="match", rest="rest"
      const mark = document.createElement('mark');
      mid.parentNode.insertBefore(mark, rest);
      mark.appendChild(mid);
      findMarks.push(mark);
      cur = rest;
    }
  }
  $('#find-cnt', findBar).textContent = findMarks.length ? String(findMarks.length) : t('无匹配');
  if (findMarks.length) findJump(1);
}
function findJump(d) {
  if (!findMarks.length) return;
  findIdx = ((findIdx + d) % findMarks.length + findMarks.length) % findMarks.length;
  findMarks.forEach((m, i) => m.classList.toggle('cur', i === findIdx));
  findMarks[findIdx].scrollIntoView({ block: 'center', behavior: motion.reduced() ? 'auto' : 'smooth' });
  $('#find-cnt', findBar).textContent = `${findIdx + 1} / ${findMarks.length}`;
}
// live deltas rewrite bubble innerHTML — marks inside them die with the
// node; a turn ending is the natural moment to re-apply an open query
function findRefresh() { if (findBar) { findIdx = -1; findApply(); } }
