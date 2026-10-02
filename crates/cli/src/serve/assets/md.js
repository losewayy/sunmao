/* markdown extensions — GFM pipe tables + TeX→MathML math.
   Loads before transcript.js: mdRender/mdInline call into these; nothing
   here touches the DOM at load time, so the replay harness evals it free. */
'use strict';

/* ================= GFM tables ================= */
// Cell split for one table row: a | inside a `code` span or written \| is
// literal text, not a separator (the escape resolves to | even in code,
// per the GFM table spec).
function mdCells(line) {
  const cells = []; let cur = '', code = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === '`') { code = !code; cur += c; continue; }
    if (c === '\\' && line[i + 1] === '|') { cur += '|'; i++; continue; }
    if (c === '|' && !code) { cells.push(cur); cur = ''; continue; }
    cur += c;
  }
  cells.push(cur);
  return cells.map(c => c.trim());
}
const TBL_DELIM = /^\s*\|?(?:\s*:?-+:?\s*\|)+\s*:?-+:?\s*\|?\s*$/;
// digits-only cells right-align in tabular-nums — everything else stays left
const TBL_NUM = /^[+\-−]?[$€£¥]?[\d,.:]+%?$/;
function mdTable(lines, i) {
  if (!lines[i].includes('|') || !TBL_DELIM.test(lines[i + 1] || '')) return null;
  const head = mdCells(lines[i]), delim = mdCells(lines[i + 1]);
  // leading/trailing empty cells are just the row's outer pipes
  for (const c of [head, delim]) { if (c[0] === '') c.shift(); if (c[c.length - 1] === '') c.pop(); }
  const cols = delim.length;
  if (!cols || head.length !== cols) return null;
  const rows = []; let j = i + 2;
  while (j < lines.length && /\S/.test(lines[j]) && lines[j].includes('|')) {
    const c = mdCells(lines[j]);
    if (c[0] === '') c.shift();
    if (c[c.length - 1] === '') c.pop();
    rows.push(c.slice(0, cols).concat(Array(Math.max(0, cols - c.length)).fill('')));
    j++;
  }
  const cell = (t, c) => `<${t}${TBL_NUM.test(c) ? ' class="num"' : ''}>${mdInline(c)}</${t}>`;
  return {
    html: `<table><thead><tr>${head.map(c => cell('th', c)).join('')}</tr></thead><tbody>`
      + rows.map(r => `<tr>${r.map(c => cell('td', c)).join('')}</tr>`).join('')
      + '</tbody></table>',
    next: j,
  };
}

/* ================= TeX → MathML =================
   Hand-rolled subset — Chromium renders MathML natively, so a converter is
   all the page needs (KaTeX is far too heavy to embed). Unknown commands
   degrade visibly (.mk-unk); a structural failure throws and texMath's
   caller-facing wrapper falls back to the literal source. */
const mml = (t, inner = '', a = '') => `<${t}${a}>${inner}</${t}>`;
const mo = (v, a = '') => mml('mo', esc(v), a);
const TEX_GREEK = {
  alpha: 'α', beta: 'β', gamma: 'γ', delta: 'δ', epsilon: 'ϵ', varepsilon: 'ε',
  zeta: 'ζ', eta: 'η', theta: 'θ', vartheta: 'ϑ', iota: 'ι', kappa: 'κ',
  lambda: 'λ', mu: 'μ', nu: 'ν', xi: 'ξ', pi: 'π', varpi: 'ϖ', rho: 'ρ',
  varrho: 'ϱ', sigma: 'σ', varsigma: 'ς', tau: 'τ', upsilon: 'υ', phi: 'ϕ',
  varphi: 'φ', chi: 'χ', psi: 'ψ', omega: 'ω',
  Gamma: 'Γ', Delta: 'Δ', Theta: 'Θ', Lambda: 'Λ', Xi: 'Ξ', Pi: 'Π',
  Sigma: 'Σ', Upsilon: 'Υ', Phi: 'Φ', Psi: 'Ψ', Omega: 'Ω',
};
const TEX_FN = {
  sin: 'sin', cos: 'cos', tan: 'tan', sec: 'sec', csc: 'csc', cot: 'cot',
  arcsin: 'arcsin', arccos: 'arccos', arctan: 'arctan',
  sinh: 'sinh', cosh: 'cosh', tanh: 'tanh',
  log: 'log', lg: 'lg', ln: 'ln', exp: 'exp', deg: 'deg', det: 'det',
  dim: 'dim', gcd: 'gcd', ker: 'ker', mod: 'mod', arg: 'arg', hom: 'hom', Pr: 'Pr',
};
// limits-style operators — scripts stack under/over, not msub/msup
const TEX_BIG = {
  sum: '∑', prod: '∏', coprod: '∐', int: '∫', iint: '∬', iiint: '∭', oint: '∮',
  bigcup: '⋃', bigcap: '⋂', bigvee: '⋁', bigwedge: '⋀',
  bigoplus: '⨁', bigotimes: '⨂', bigodot: '⨀', biguplus: '⨄',
  lim: 'lim', liminf: 'lim inf', limsup: 'lim sup',
  max: 'max', min: 'min', sup: 'sup', inf: 'inf', argmin: 'arg min', argmax: 'arg max',
};
const TEX_OP = {
  cdot: '·', times: '×', div: '÷', ast: '∗', star: '⋆', circ: '∘', bullet: '∙',
  oplus: '⊕', ominus: '⊖', otimes: '⊗', oslash: '⊘', odot: '⊙',
  le: '≤', leq: '≤', ge: '≥', geq: '≥', ne: '≠', neq: '≠', equiv: '≡',
  approx: '≈', simeq: '≃', cong: '≅', sim: '∼', propto: '∝', asymp: '≍',
  ll: '≪', gg: '≫', pm: '±', mp: '∓', doteq: '≐', models: '⊨', vdash: '⊢',
  in: '∈', ni: '∋', notin: '∉', subset: '⊂', supset: '⊃',
  subseteq: '⊆', supseteq: '⊇', nsubseteq: '⊄', nsupseteq: '⊅',
  cup: '∪', cap: '∩', setminus: '∖', emptyset: '∅', varnothing: '∅',
  to: '→', gets: '←', mapsto: '↦', leftarrow: '←', rightarrow: '→',
  uparrow: '↑', downarrow: '↓', Leftarrow: '⇐', Rightarrow: '⇒', Leftrightarrow: '⇔',
  longrightarrow: '⟶', longleftarrow: '⟵', implies: '⟹', impliedby: '⟸',
  iff: '⟺', hookrightarrow: '↪',
  infty: '∞', partial: '∂', nabla: '∇', forall: '∀', exists: '∃', nexists: '∄',
  neg: '¬', lnot: '¬', land: '∧', wedge: '∧', lor: '∨', vee: '∨',
  top: '⊤', bot: '⊥', therefore: '∴', because: '∵',
  cdots: '⋯', ldots: '…', vdots: '⋮', ddots: '⋱',
  prime: '′', angle: '∠', perp: '⊥', parallel: '∥', nparallel: '∦', degree: '°',
  ell: 'ℓ', Re: 'ℜ', Im: 'ℑ', aleph: 'ℵ', hbar: 'ℏ', wp: '℘', middot: '·',
  mid: '∣', vert: '|', Vert: '‖', lvert: '|', rvert: '|', lVert: '‖', rVert: '‖',
  lfloor: '⌊', rfloor: '⌋', lceil: '⌈', rceil: '⌉', langle: '⟨', rangle: '⟩',
  lbrace: '{', rbrace: '}', backslash: '\\',
};
const TEX_SPACE = {
  ',': '.167em', ':': '.222em', ';': '.278em', '!': '-.167em', ' ': '.25em',
  quad: '1em', qquad: '2em', enspace: '.5em',
  thinspace: '.167em', medspace: '.222em', thickspace: '.278em', negthinspace: '-.167em',
};
const TEX_DELIM = {
  '(': '(', ')': ')', '[': '[', ']': ']', '|': '|', '.': '',
  lbrace: '{', rbrace: '}', '{': '{', '}': '}',
  langle: '⟨', rangle: '⟩', lvert: '|', rvert: '|',
  Vert: '‖', lVert: '‖', rVert: '‖', lfloor: '⌊', rfloor: '⌋', lceil: '⌈', rceil: '⌉',
};
const ENV_DELIM = {
  matrix: ['', ''], pmatrix: ['(', ')'], bmatrix: ['[', ']'], Bmatrix: ['{', '}'],
  vmatrix: ['|', '|'], Vmatrix: ['‖', '‖'], cases: ['{', ''], array: ['', ''], smallmatrix: ['', ''],
};
function texTokens(src) {
  const t = [];
  for (let i = 0; i < src.length; i++) {
    const c = src[i];
    if (/\s/.test(c)) { t.push({ k: 'ws' }); continue; }
    if (c === '\\') {
      if (src[i + 1] === '\\') { t.push({ k: 'brk' }); i++; continue; }
      const m = /^[A-Za-z]+|^./.exec(src.slice(i + 1));
      if (!m) { t.push({ k: 'mo', v: '\\' }); continue; }
      t.push({ k: 'cmd', v: m[0] }); i += m[0].length;
      continue;
    }
    if (/[0-9]/.test(c)) { const m = /^[\d.]+/.exec(src.slice(i)); t.push({ k: 'num', v: m[0] }); i += m[0].length - 1; continue; }
    if (/[A-Za-z]/.test(c)) { t.push({ k: 'mi', v: c }); continue; }
    if (c === '{') { t.push({ k: 'l' }); continue; }
    if (c === '}') { t.push({ k: 'r' }); continue; }
    if (c === '^') { t.push({ k: 'sup' }); continue; }
    if (c === '_') { t.push({ k: 'sub' }); continue; }
    if (c === '[') { t.push({ k: 'lb', v: '[' }); continue; }
    if (c === ']') { t.push({ k: 'rb', v: ']' }); continue; }
    if (c === '&') { t.push({ k: 'amp' }); continue; }
    if (c === '~') { t.push({ k: 'sp', w: '.25em' }); continue; }
    t.push({ k: 'mo', v: c });
  }
  return t;
}
function texParse(src) {
  const toks = texTokens(src);
  let p = 0;
  const skipWs = () => { while (p < toks.length && toks[p].k === 'ws') p++; };
  const peek = () => { skipWs(); return toks[p]; };
  // stop predicates for seq(): end of input, group, \right/\end scope, cell
  const stopTop = t => !t;
  const stopGroup = t => !t || t.k === 'r' || (t.k === 'cmd' && (t.v === 'right' || t.v === 'end'));
  const stopInner = t => !t || t.k === 'r' || (t.k === 'cmd' && (t.v === 'right' || t.v === 'end'));
  const stopCell = t => !t || t.k === 'amp' || t.k === 'brk' || (t.k === 'cmd' && t.v === 'end');
  const stopRb = t => !t || t.k === 'rb';
  const stretchy = d => (d ? mo(d, ' stretchy="true"') : '');
  function tokenText(t) {
    switch (t.k) {
      case 'ws': return ' ';
      case 'sp': return '~';
      case 'cmd': return '\\' + t.v;
      case 'brk': return '\\\\';
      case 'l': return '{'; case 'r': return '}';
      case 'lb': return '['; case 'rb': return ']';
      case 'sub': return '_'; case 'sup': return '^';
      case 'amp': return '&';
      default: return t.v || '';
    }
  }
  // literal text of a {…} group — \text needs words, not tokenized atoms
  function rawGroup() {
    if (!peek() || peek().k !== 'l') { const t = peek(); if (t) p++; return t ? tokenText(t) : ''; }
    p++;
    let s = '', depth = 1;
    for (;;) {
      const t = toks[p++];
      if (!t) break;
      if (t.k === 'l') { depth++; s += '{'; continue; }
      if (t.k === 'r') { if (--depth === 0) break; s += '}'; continue; }
      s += tokenText(t);
    }
    return s;
  }
  // delimiter argument after \left/\right/\middle — a raw char or \{ \| …
  function delimArg() {
    const t = peek(); if (!t) return '';
    p++;
    const v = t.k === 'cmd' ? t.v : t.k === 'l' ? '{' : t.k === 'r' ? '}' : t.v;
    return TEX_DELIM[v] ?? (typeof v === 'string' && v.length === 1 ? v : '');
  }
  function node(t) {
    switch (t.k) {
      case 'mi': return { h: mml('mi', esc(t.v)) };
      case 'num': return { h: mml('mn', esc(t.v)) };
      case 'mo': return t.v === "'" ? { h: '', prime: true } : { h: mo(t.v) };
      case 'lb': case 'rb': return { h: mo(t.v) };
      case 'sp': return { h: mml('mspace', '', ` width="${t.w}"`) };
      case 'l': {
        const inner = seq(stopGroup);
        if (peek() && peek().k === 'r') p++;
        return { h: mml('mrow', inner) };
      }
      case 'cmd': return cmdNode(t.v);
      default: return null; // stray ^ _ } & \\ — nothing to hang them on
    }
  }
  // one script argument — a {…} group or a single token
  function groupish() {
    const t = peek();
    if (!t) return { h: mml('mrow') };
    p++;
    return node(t) || { h: mml('mrow') };
  }
  function matrix(env) {
    if (env === 'array') rawGroup(); // column spec — parsed and dropped
    const [open, close] = ENV_DELIM[env] || ['(', ')'];
    const rows = [];
    for (;;) {
      const t = peek();
      if (!t || (t.k === 'cmd' && t.v === 'end')) break;
      const cells = [];
      for (;;) {
        cells.push(mml('mtd', seq(stopCell)));
        const t2 = peek();
        if (t2 && t2.k === 'amp') { p++; continue; }
        break;
      }
      rows.push(mml('mtr', cells.join('')));
      const t3 = peek();
      if (t3 && t3.k === 'brk') { p++; continue; }
      break;
    }
    if (peek() && peek().k === 'cmd' && peek().v === 'end') { p++; rawGroup(); }
    return { h: mml('mrow', stretchy(open) + mml('mtable', rows.join('')) + stretchy(close)) };
  }
  function cmdNode(v) {
    if (v === 'left') {
      const open = delimArg();
      const inner = seq(stopInner);
      let close = '';
      if (peek() && peek().k === 'cmd' && peek().v === 'right') { p++; close = delimArg(); }
      else if (peek() && peek().k === 'r') p++;
      return { h: mml('mrow', stretchy(open) + inner + stretchy(close)) };
    }
    if (v === 'middle') return { h: stretchy(delimArg()) };
    if (v === 'begin') return matrix(rawGroup().trim());
    if (v === 'frac' || v === 'dfrac' || v === 'tfrac')
      return { h: mml('mfrac', groupish().h + groupish().h) };
    if (v === 'binom')
      return { h: mml('mrow', stretchy('(') + mml('mfrac', groupish().h + groupish().h, ' linethickness="0"') + stretchy(')')) };
    if (v === 'sqrt') {
      if (peek() && peek().k === 'lb') {
        p++;
        const n = seq(stopRb);
        if (peek() && peek().k === 'rb') p++;
        return { h: mml('mroot', groupish().h + n) };
      }
      return { h: mml('msqrt', groupish().h) };
    }
    if (v === 'overset' || v === 'stackrel') { const top = groupish().h, base = groupish().h; return { h: mml('mover', base + top) }; }
    if (v === 'underset') { const un = groupish().h, base = groupish().h; return { h: mml('munder', base + un) }; }
    if (v === 'overline' || v === 'bar') return { h: mml('mover', groupish().h + mo('¯', ' accent="true"')) };
    if (v === 'underline') return { h: mml('munder', groupish().h + mo('_', ' accent="true"')) };
    if (v === 'vec' || v === 'overrightarrow') return { h: mml('mover', groupish().h + mo('→', ' accent="true"')) };
    if (v === 'overleftarrow') return { h: mml('mover', groupish().h + mo('←', ' accent="true"')) };
    if (v === 'hat' || v === 'widehat') return { h: mml('mover', groupish().h + mo('^', ' accent="true"')) };
    if (v === 'tilde' || v === 'widetilde') return { h: mml('mover', groupish().h + mo('~', ' accent="true"')) };
    if (v === 'dot') return { h: mml('mover', groupish().h + mo('.', ' accent="true"')) };
    if (v === 'ddot') return { h: mml('mover', groupish().h + mo('¨', ' accent="true"')) };
    if (v === 'boxed') return { h: mml('menclose', groupish().h, ' notation="box"') };
    if (v === 'cancel') return { h: mml('menclose', groupish().h, ' notation="updiagonalstrike"') };
    if (v === 'text' || v === 'textrm' || v === 'mathrm' || v === 'operatorname')
      return { h: mml('mtext', esc(rawGroup())) };
    if (v === 'mathbf' || v === 'bm' || v === 'boldsymbol' || v === 'textbf' || v === 'pmb')
      return { h: mml('mtext', esc(rawGroup()), ' style="font-weight:bold"') };
    if (v === 'mathit' || v === 'textit' || v === 'emph')
      return { h: mml('mtext', esc(rawGroup()), ' style="font-style:italic"') };
    if (v === 'mathsf' || v === 'textsf') return { h: mml('mtext', esc(rawGroup()), ' style="font-family:sans-serif"') };
    if (v === 'mathtt' || v === 'texttt') return { h: mml('mtext', esc(rawGroup()), ' style="font-family:monospace"') };
    // sizing/phantom hints — the structure already decides; consume args, emit nothing
    if (v === 'limits' || v === 'nolimits' || v === 'displaystyle' || v === 'medskip' || v === 'smallskip' || v === 'bigskip') return null;
    if (/^(?:big|Big|bigg|Bigg)[lrm]?$/.test(v)) return { h: stretchy(delimArg()) };
    if (v === 'hspace' || v === 'kern' || v === 'mkern' || v === 'mspace' || v === 'hskip' || v === 'mskip') { rawGroup(); return { h: mml('mspace', '', ' width=".167em"') }; }
    if (v === 'phantom' || v === 'hphantom' || v === 'vphantom') { groupish(); return null; }
    if (v === 'color' || v === 'textcolor' || v === 'colorbox') { rawGroup(); return groupish(); } // colour is chrome — keep the content
    if (TEX_SPACE[v] !== undefined) return { h: mml('mspace', '', ` width="${TEX_SPACE[v]}"`) };
    if (TEX_GREEK[v]) return { h: mml('mi', esc(TEX_GREEK[v])) };
    if (TEX_FN[v]) return { h: mo(TEX_FN[v]) };
    if (TEX_BIG[v]) return { h: mo(TEX_BIG[v]), big: true };
    if (TEX_OP[v]) return { h: mo(TEX_OP[v]) };
    if ('$%&#_'.includes(v)) return { h: mo(v) };
    if (v === '{' || v === '}') return { h: mo(v) };
    if (v === '|') return { h: mo('‖') };
    return { h: mo('\\' + v, ' class="mk-unk"') }; // unknown cmd — shown, not dropped
  }
  function seq(stop) {
    const out = [];
    let base = null, sub = null, sup = null;
    const flush = () => {
      if (!base) { sub = sup = null; return; }
      let h = base.h;
      if (sub !== null && sup !== null) h = mml(base.big ? 'munderover' : 'msubsup', base.h + sub + sup);
      else if (sub !== null) h = mml(base.big ? 'munder' : 'msub', base.h + sub);
      else if (sup !== null) h = mml(base.big ? 'mover' : 'msup', base.h + sup);
      out.push({ h });
      base = null; sub = sup = null;
    };
    for (;;) {
      const t = peek();
      if (stop(t)) break;
      p++;
      if (t.k === 'sub' || t.k === 'sup') {
        if (!base) continue; // stray script marker — TeX would error; we drop it
        const g = groupish().h;
        if (t.k === 'sub') sub = sub === null ? g : mml('mrow', sub + g);
        else sup = sup === null ? g : mml('mrow', sup + g);
        continue;
      }
      const n = node(t);
      if (!n) continue;
      if (n.prime) { if (base) sup = sup === null ? mo('′') : mml('mrow', sup + mo('′')); continue; }
      flush();
      base = n;
    }
    flush();
    return out.map(n => n.h).join('');
  }
  return seq(stopTop);
}
// TeX source → <math> HTML. `block` selects display mode. Any structural
// failure returns the literal delimited source — the user keeps their text.
function texMath(src, block) {
  try {
    // mdInline hands us escaped text — restore entities the tokenizer needs
    src = String(src).replace(/&(amp|lt|gt|quot|#39);/g,
      m => ({ '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"', '&#39;': "'" }[m]));
    return mml('math', texParse(src), block ? ' display="block"' : '');
  } catch {
    return esc(block ? '$$' + src + '$$' : '$' + src + '$');
  }
}

/* ================= markdown render core (streaming-tolerant, no deps) =================
/* ================= markdown (streaming-tolerant, no deps) ================= */
const HASH_LANGS = /^(?:python|py|bash|sh|shell|zsh|powershell|ps1|yaml|yml|toml|dockerfile|make|makefile|cmake|ruby|rb|perl|pl|r|julia|jl|ini|conf|config|docker|text|txt)$/i;

function highlight(code, lang) {
  const hash = !lang || HASH_LANGS.test(lang);
  const re = /(\/\/[^\n]*|\/\*[\s\S]*?(?:\*\/|$)|"""[\s\S]*?(?:"""|$)|'''[\s\S]*?(?:'''|$)|`(?:\\.|[^`\\])*`|"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*'|\b\d[\d_]*(?:\.[\d_]+)?(?:[eEpPxX][\dA-Fa-f_+-]*)?|\b(?:const|let|var|function|fn|struct|enum|impl|trait|pub|use|mod|crate|where|match|if|else|elif|for|while|loop|break|continue|return|class|extends|new|this|self|super|import|from|export|default|async|await|try|catch|finally|throw|raise|except|def|print|pass|None|True|False|null|true|false|nil|Some|Ok|Err|type|interface|package|func|go|chan|select|defer|in|of|do|end|then|local|require|void|int|float|double|long|short|unsigned|signed|char|bool|auto|virtual|static|final|abstract|private|protected|public|readonly|mut|ref|move|dyn|unsafe|extern|sizeof|with|yield|assert|global|nonlocal|del|is|not|and|or)\b|#[^\n]*)/g;
  let out = '', last = 0, m;
  while ((m = re.exec(code))) {
    const tok = m[0];
    if (tok.startsWith('#') && !hash) { continue; } // don't treat # in non-hash langs
    out += esc(code.slice(last, m.index));
    let cls = 'tk-x';
    if (/^(?:\/\/|\/\*)/.test(tok) || tok.startsWith('#')) cls = 'tk-x';
    else if (/^["'`]/.test(tok)) cls = 'tk-s';
    else if (/^\d/.test(tok)) cls = 'tk-n';
    else cls = 'tk-k';
    out += `<span class="${cls}">${esc(tok)}</span>`;
    last = m.index + tok.length;
    if (m.index === re.lastIndex) re.lastIndex++;
  }
  out += esc(code.slice(last));
  return out;
}

function mdInline(s) {
  s = esc(s);
  // pull code spans out first — nothing below may touch their contents;
  // restored last so emphasis can't chew the emitted markup either
  const spans = [];
  s = s.replace(/`([^`\n]+)`/g, (_, c) => { spans.push(`<code>${c}</code>`); return `${spans.length - 1}`; });
  // inline math before links/emphasis — texMath re-escapes and yields a
  // self-contained <math> tree the other inline rules can't break.
  // \(…\) lands first — non-greedy stops at the first `\)` so an inner
  // unmatched `(` like \(f(x)\) is kept whole.
  s = s.replace(/\\\(([\s\S]*?)\\\)/g, (_, tex) => texMath(tex, false));
  s = s.replace(/\$([^\s$](?:[^$\n]*[^\s$])?)\$(?!\d)/g, (_, tex) => texMath(tex, false));
  s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  s = s.replace(/\*\*([^*\n]+)\*\*|__([^_\n]+)__/g, '<b>$1$2</b>');
  s = s.replace(/(^|[^*\w])\*([^*\n]+)\*/g, '$1<i>$2</i>');
  s = s.replace(/~~([^~\n]+)~~/g, '<del>$1</del>');
  return s.replace(/(\d+)/g, (_, i) => spans[i]);
}

function mdRender(src) {
  const lines = String(src).split('\n');
  let html = '', para = [], list = null, code = null, codeLang = '', quote = '', math = null;
  const flushPara = () => { if (para.length) { html += `<p>${para.map(mdInline).join('<br>')}</p>`; para = []; } };
  const flushList = () => { if (list) { html += list.html + `</${list.tag}>`; list = null; } };
  const flushQuote = () => { if (quote) { html += `<blockquote>${quote}</blockquote>`; quote = ''; } };
  const flushAll = () => { flushPara(); flushList(); flushQuote(); };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (code !== null) {
      if (/^\s*```/.test(line)) { html += `<pre><code>${highlight(code, codeLang)}</code></pre>`; code = null; }
      else code += line + '\n';
      continue;
    }
    if (math !== null) {
      // mid-math — the close string lives on the state; $$ and \[…\]
      // blocks share this loop
      const e = line.indexOf(math.close);
      if (e >= 0) {
        html += `<div class="math-block">${texMath(math.buf + line.slice(0, e), true)}</div>`;
        if (line.slice(e + math.close.length).trim()) para.push(line.slice(e + math.close.length)); // rare trailing text — keep it, not drop
        math = null;
      } else math.buf += line + '\n';
      continue;
    }
    const fence = line.match(/^\s*```\s*([\w+.#-]*)/);
    if (fence) { flushAll(); code = ''; codeLang = fence[1] || ''; continue; }
    // display math — $$…$$ and \[…\] are the two block delimiters the
    // model actually emits; both resolve through the same texMath path
    const mblock = line.match(/^\s*\$\$([\s\S]*?)(\$\$)?\s*$/) || line.match(/^\s*\\\[([\s\S]*?)(\\\])?\s*$/);
    if (mblock) {
      flushAll();
      const close = line.trimStart().startsWith('\\[') ? '\\]' : '$$';
      if (mblock[2]) html += `<div class="math-block">${texMath(mblock[1], true)}</div>`;
      else math = { buf: mblock[1], close }; // mid-stream — open until the closer lands
      continue;
    }
    const h = line.match(/^\s{0,3}(#{1,4})\s+(.+)/);
    if (h) { flushAll(); const n = h[1].length; html += `<h${n}>${mdInline(h[2])}</h${n}>`; continue; }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) { flushAll(); html += '<hr>'; continue; }
    const q = line.match(/^\s*>\s?(.*)/);
    if (q) { flushPara(); flushList(); quote += `<p>${mdInline(q[1])}</p>`; continue; }
    const li = line.match(/^\s*(?:([-*+])|(\d+)[.)])\s+(.+)/);
    if (li) {
      flushPara(); flushQuote();
      const tag = li[1] ? 'ul' : 'ol';
      if (!list || list.tag !== tag) { flushList(); list = { tag, html: `<${tag}>` }; }
      list.html += `<li>${mdInline(li[3])}</li>`;
      continue;
    }
    if (/^\s*$/.test(line)) { flushAll(); continue; }
    // GFM pipe table — header + --- delimiter rows; a | alone stays text
    const tbl = mdTable(lines, i);
    if (tbl) { flushAll(); html += tbl.html; i = tbl.next - 1; continue; }
    flushList(); flushQuote();
    para.push(line);
  }
  if (code !== null) html += `<pre><code>${highlight(code, codeLang)}</code></pre>`;
  if (math !== null) html += `<div class="math-block">${texMath(math.buf, true)}</div>`; // unterminated math block — render what arrived
  flushAll();
  return html;
}

