/* line-level diff for Edit/Write tool cards — a small LCS over split lines,
   no deps. transcript.js calls diffHTML()/newFileHTML() when a ToolStart's
   args (or a replayed tool_call) carry the file payloads. */
'use strict';

// Row types: ' ' ctx, '-' del, '+' add, '…' folded context.
function lineDiff(oldText, newText) {
  const a = oldText.split('\n'), b = newText.split('\n');
  const n = a.length, m = b.length;
  // LCS is O(n·m) — past a few thousand lines the honest rendering is
  // "everything changed", not a quadratic freeze.
  if (n * m > 400_000) {
    return a.map(l => ({ t: '-', l })).concat(b.map(l => ({ t: '+', l })));
  }
  const len = new Int32Array((n + 1) * (m + 1));
  const at = (i, j) => len[i * (m + 1) + j];
  const set = (i, j, v) => { len[i * (m + 1) + j] = v; };
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--)
      set(i, j, a[i] === b[j] ? at(i + 1, j + 1) + 1 : Math.max(at(i + 1, j), at(i, j + 1)));
  const rows = [];
  let i = 0, j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) { rows.push({ t: ' ', l: a[i] }); i++; j++; }
    else if (at(i + 1, j) >= at(i, j + 1)) { rows.push({ t: '-', l: a[i] }); i++; }
    else { rows.push({ t: '+', l: b[j] }); j++; }
  }
  while (i < n) rows.push({ t: '-', l: a[i++] });
  while (j < m) rows.push({ t: '+', l: b[j++] });
  return rows;
}

// Fold context runs past ±CTX lines around an edit into a single '…' row.
const DIFF_CTX = 3, DIFF_MAX = 400;
function foldContext(rows) {
  const near = new Uint8Array(rows.length);
  for (let i = 0; i < rows.length; i++)
    if (rows[i].t !== ' ')
      for (let k = Math.max(0, i - DIFF_CTX); k <= Math.min(rows.length - 1, i + DIFF_CTX); k++)
        near[k] = 1;
  const out = [];
  for (let i = 0; i < rows.length; i++) {
    if (!near[i]) {
      if (!out.length || out[out.length - 1].t !== '…') out.push({ t: '…', l: '' });
      continue;
    }
    out.push(rows[i]);
    if (out.length > DIFF_MAX) { out.length = DIFF_MAX; out.push({ t: '…', l: '' }); break; }
  }
  return out;
}

function diffHTML(oldText, newText) {
  const rows = foldContext(lineDiff(oldText, newText));
  return `<div class="diff">${rows.map(r =>
    `<div class="dl dl-${r.t === ' ' ? 'c' : r.t === '…' ? 'f' : r.t}"><span class="dm">${r.t}</span><span class="dt">${esc(r.l) || '&#8203;'}</span></div>`
  ).join('')}</div>`;
}

// Write's content is all-new — a preview, not a diff against anything.
function newFileHTML(text) {
  const lines = text.split('\n');
  const rows = lines.slice(0, DIFF_MAX).map(l => `<div class="dl dl-+"><span class="dm">+</span><span class="dt">${esc(l) || '&#8203;'}</span></div>`).join('');
  const more = lines.length > DIFF_MAX ? `<div class="dl dl-f"><span class="dm">…</span><span class="dt">(${lines.length - DIFF_MAX} more lines)</span></div>` : '';
  return `<div class="diff">${rows}${more}</div>`;
}

// Card body for Edit/Write args — the expandable preview under the tool row.
// Returns '' when the args don't carry a renderable payload.
function editPreviewHTML(name, args) {
  if (!args || typeof args !== 'object') return '';
  if (name === 'Edit') {
    const o = args.old_string, nw = args.new_string;
    if (typeof o !== 'string' || typeof nw !== 'string') return '';
    return diffHTML(o, nw);
  }
  if (name === 'Write' && typeof args.content === 'string') return newFileHTML(args.content);
  return '';
}
