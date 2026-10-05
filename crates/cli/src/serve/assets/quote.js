/* Quote a passage of the transcript into the composer — split out of
   transcript.js, which owns rendering and was already at its file budget. */
'use strict';

/* ================= quote a passage into the composer ================= */
/* Select text in a message and a 引用 button floats next to it: one click
   drops the passage into the composer as a quote, so a reply can answer one
   paragraph instead of a whole turn. The line shape mirrors the dock's
   annotate output (`brAnnLine`) — both land as the same kind of user
   message. */
const QUOTE_MAX = 500;      // characters kept; the tail is elided
const QUOTE_MIN = 2;        // a stray click should not offer it
const QUOTE_GAP = 34;       // the button sits this far above the selection
const QUOTE_W = 96, QUOTE_HALF = 40;
let quoteBtn = null, quoteText = '';

function quoteLine(text) {
  const clipped = text.length > QUOTE_MAX ? text.slice(0, QUOTE_MAX).trimEnd() + '…' : text;
  return t('引用') + '：\n' + clipped.split('\n').map(l => '> ' + l).join('\n') + '\n\n';
}
function quoteHide() {
  if (quoteBtn) { quoteBtn.remove(); quoteBtn = null; }
  quoteText = '';
}
function quoteToComposer() {
  if (!quoteText) return;
  const ta = $('#input');
  ta.value = (ta.value.trim() ? ta.value.trimEnd() + '\n' : '') + quoteLine(quoteText);
  autoGrow();
  ta.focus();
  toast(t('已引用到输入框'), 'quote');
  quoteHide();
  const sel = window.getSelection();
  if (sel) sel.removeAllRanges();
}
function quotePick() {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || !sel.rangeCount) return quoteHide();
  const text = String(sel).trim();
  const node = sel.anchorNode;
  const el = node && (node.nodeType === 1 ? node : node.parentElement);
  // only a passage inside a message — the rail and the composer keep their
  // own gestures, and a stray double-click is not a quote
  if (!el || !el.closest('.msg') || text.length < QUOTE_MIN) return quoteHide();
  quoteText = text;
  if (!quoteBtn) {
    quoteBtn = document.createElement('button');
    quoteBtn.type = 'button';
    quoteBtn.className = 'quote-pop glass';
    quoteBtn.innerHTML = ic('quote', 'i sm') + '<span>' + t('引用') + '</span>';
    // mousedown, not click: the press would otherwise clear the selection
    // before the click ever fires
    quoteBtn.addEventListener('mousedown', e => { e.preventDefault(); e.stopPropagation(); quoteToComposer(); });
    document.body.appendChild(quoteBtn);
  }
  const r = sel.getRangeAt(0).getBoundingClientRect();
  quoteBtn.style.top = Math.max(8, r.top - QUOTE_GAP) + 'px';
  quoteBtn.style.left = Math.max(8, Math.min(r.left + r.width / 2 - QUOTE_HALF, innerWidth - QUOTE_W)) + 'px';
}
document.addEventListener('pointerup', () => setTimeout(quotePick, 0));
document.addEventListener('selectionchange', () => {
  const sel = window.getSelection();
  if (sel && sel.isCollapsed) quoteHide();
});
document.addEventListener('keydown', e => { if (e.key === 'Escape') quoteHide(); });
document.addEventListener('scroll', quoteHide, true);
window.addEventListener('resize', quoteHide);
