/* Quote a passage of the transcript into the composer — split out of
   transcript.js, which owns rendering and was already at its file budget. */
'use strict';

/* ================= quote a passage into the composer ================= */
/* Select text in a message and a 引用 button floats next to it: one click
   adds a quote CARD above the input row instead of pasting the passage into
   the box, so the draft keeps only the user's own words. The card carries
   the quote icon, the passage's first line and an ×; hovering it reveals
   the whole passage. At submit `quoteBlocks()` turns the cards into the
   same `> …` block the dock's annotate output uses (`brAnnLine`) — both
   land as the same kind of user message, so the reading side is unchanged. */
const QUOTE_MAX = 500;      // characters kept; the tail is elided
const QUOTE_MIN = 2;        // a stray click should not offer it
let quoteBtn = null, quoteText = '';
let pendingQuotes = [];     // [{head, text}] in the order they were picked
let quoteFull = null;       // the hover layer, created on the first hover
/* geometry comes from the tokens (`motion.px`), never from literals here */
const quoteGeom = () => ({
  gap: motion.px('--quote-gap', 34),
  w: motion.px('--quote-w', 96),
  margin: motion.px('--quote-margin', 8),
});
const quoteFullGeom = () => ({ gap: motion.px('--s-8', 8), margin: motion.px('--s-8', 8) });

/* the outbound block — one `引用：` header per card, then its `> ` lines.
   Quote blocks lead the prompt; the user's own text follows them. */
function quoteBlocks() {
  return pendingQuotes.map(q =>
    t('引用') + '：\n' + q.text.split('\n').map(l => '> ' + l).join('\n') + '\n\n').join('');
}
function quoteSummary(text) { return text.split('\n').map(s => s.trim()).find(Boolean) || text; }
function renderQuotes() {
  const box = $('#cmp-quotes');
  quoteFullHide(); // the DOM under the pointer is about to be replaced
  box.hidden = !pendingQuotes.length;
  box.innerHTML = pendingQuotes.map((q, i) =>
    `<span class="quote-chip" data-qi="${i}">${ic('quote', 'i xs')}<span class="quote-n">${esc(q.head)}</span><button class="chip-x" data-qx="${i}" aria-label="${esc(t('移除'))}">×</button></span>`).join('');
  box.querySelectorAll('.quote-chip').forEach(c => {
    c.addEventListener('mouseenter', () => quoteFullShow(c));
    c.addEventListener('mouseleave', quoteFullHide);
  });
}
function removeQuote(i) {
  if (!pendingQuotes[i]) return;
  pendingQuotes.splice(i, 1);
  renderQuotes();
}
function clearQuotes() { pendingQuotes = []; renderQuotes(); }
/* × retracts the card only — the words already in the input stay there */
$('#cmp-quotes').addEventListener('click', e => {
  const b = e.target.closest('[data-qx]');
  if (b) removeQuote(+b.dataset.qx);
});
/* hover reveals the whole passage — a multi-line layer of its own, not the
   one-line #tip bubble; pointer-events:none keeps it from stealing the
   hover that opened it */
function quoteFullHide() { if (quoteFull) quoteFull.classList.remove('show'); }
function quoteFullShow(chip) {
  const q = pendingQuotes[+chip.dataset.qi];
  if (!q) return;
  if (!quoteFull) {
    quoteFull = document.createElement('div');
    quoteFull.className = 'quote-full glass g-float';
    document.body.appendChild(quoteFull);
  }
  quoteFull.textContent = q.text;
  const g = quoteFullGeom();
  const r = chip.getBoundingClientRect(), fr = quoteFull.getBoundingClientRect();
  const col = $('#composer').getBoundingClientRect();
  // clear of the whole card row, not just the hovered card — with two rows
  // the passage would otherwise cover its own siblings; left-aligned with
  // the card, inside the composer's reading column, clamped to the viewport
  const rowTop = $('#cmp-quotes').getBoundingClientRect().top;
  const top = Math.max(g.margin, Math.min(rowTop - fr.height - g.gap, innerHeight - fr.height - g.margin));
  let left = Math.min(r.left, innerWidth - fr.width - g.margin);
  if (fr.width <= col.width) left = Math.max(col.left, Math.min(left, col.right - fr.width));
  quoteFull.style.transform = `translate(${Math.round(Math.max(g.margin, left))}px,${Math.round(top)}px)`;
  quoteFull.classList.add('show');
}
function quoteHide() {
  if (quoteBtn) { quoteBtn.remove(); quoteBtn = null; }
  quoteText = '';
}
function quoteToComposer() {
  if (!quoteText) return;
  const text = quoteText.length > QUOTE_MAX ? quoteText.slice(0, QUOTE_MAX).trimEnd() + '…' : quoteText;
  pendingQuotes.push({ head: quoteSummary(text), text });
  renderQuotes();
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
  const g = quoteGeom();
  const r = sel.getRangeAt(0).getBoundingClientRect();
  quoteBtn.style.top = Math.max(g.margin, r.top - g.gap) + 'px';
  quoteBtn.style.left = Math.max(g.margin, Math.min(r.left + r.width / 2 - g.w / 2, innerWidth - g.w)) + 'px';
}
document.addEventListener('pointerup', () => setTimeout(quotePick, 0));
document.addEventListener('selectionchange', () => {
  const sel = window.getSelection();
  if (sel && sel.isCollapsed) quoteHide();
});
document.addEventListener('keydown', e => { if (e.key === 'Escape') quoteHide(); });
document.addEventListener('scroll', quoteHide, true);
window.addEventListener('resize', quoteHide);
