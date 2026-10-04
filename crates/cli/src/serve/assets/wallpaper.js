/* procedural wallpapers — canvas painters + wallpaper switching */
'use strict';

/* ================= procedural wallpapers ================= */
function rng(seed) { let a = seed >>> 0; return () => { a = a + 0x6D2B79F5 | 0; let t = Math.imul(a ^ a >>> 15, 1 | a); t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t; return ((t ^ t >>> 14) >>> 0) / 4294967296; }; }
function vnoise(seed) { const r = rng(seed), v = new Float32Array(1024); for (let i = 0; i < 1024; i++) v[i] = r(); return x => { const i = Math.floor(x), f = x - i, u = f * f * (3 - 2 * f), a = v[i & 1023], b = v[(i + 1) & 1023]; return a + (b - a) * u; }; }
function fbm(n, x, o = 5) { let s = 0, a = .5, f = 1, t = 0; for (let k = 0; k < o; k++) { s += a * n(x * f + k * 17.3); t += a; a *= .5; f *= 2.03; } return s / t; }
function lin(ctx, y0, y1, stops) { const g = ctx.createLinearGradient(0, y0, 0, y1); for (const [o, c] of stops) g.addColorStop(o, c); return g; }
function glow(ctx, x, y, r, stops) { const g = ctx.createRadialGradient(x, y, 0, x, y, r); for (const [o, c] of stops) g.addColorStop(o, c); ctx.fillStyle = g; ctx.fillRect(0, 0, ctx.canvas.width, ctx.canvas.height); }
function ridge(ctx, w, h, base, amp, step, shape) { ctx.beginPath(); ctx.moveTo(0, h); for (let x = 0; x <= w + step; x += step) ctx.lineTo(x, base - amp * shape(x / w)); ctx.lineTo(w, h); ctx.closePath(); }

function paintRidge(ctx, w, h) {
  const u = w / 1440, R = rng(7), M = Math.max(w, h), step = Math.max(2, 3 * u);
  ctx.fillStyle = lin(ctx, 0, h, [[0, '#060911'], [.22, '#0b1526'], [.4, '#172a46'], [.5, '#2b3b58'], [.56, '#4a4d63'], [.61, '#735a55'], [.66, '#3a3139'], [.8, '#15141a'], [1, '#07080b']]);
  ctx.fillRect(0, 0, w, h);
  glow(ctx, w * .66, h * .6, M * .5, [[0, 'rgba(246,176,112,.50)'], [.28, 'rgba(206,128,96,.20)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .14, h * .56, M * .45, [[0, 'rgba(96,142,214,.30)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .42, h * .12, M * .5, [[0, 'rgba(56,96,168,.24)'], [1, 'rgba(0,0,0,0)']]);
  const stars = Math.round(900 * h / w);
  for (let i = 0; i < stars; i++) {
    const x = R() * w, y = Math.pow(R(), 1.7) * h * .6, a = (R() * .62 + .08) * (1 - y / (h * .62));
    const s = Math.max(.6, (R() < .07 ? 2 : 1.15) * u * 1.2);
    ctx.fillStyle = `rgba(226,232,255,${a.toFixed(3)})`; ctx.fillRect(x, y, s, s);
  }
  const mx = w * .87, my = h * .13, mr = Math.min(w, h) * .022;
  glow(ctx, mx, my, mr * 20, [[0, 'rgba(238,232,214,.30)'], [.12, 'rgba(238,232,214,.09)'], [1, 'rgba(0,0,0,0)']]);
  ctx.fillStyle = '#efeadb'; ctx.beginPath(); ctx.arc(mx, my, mr, 0, Math.PI * 2); ctx.fill();
  [[.665, .09, 1.1, '#5b5a6a', '#46485a', .16], [.705, .10, 1.6, '#3f4356', '#303446', .13], [.75, .11, 2.2, '#2a2f40', '#1f2331', .10], [.81, .12, 2.9, '#1a1e2a', '#12151e', .07], [.88, .12, 3.5, '#0f1218', '#090b0f', .04], [.96, .10, 4.3, '#07080b', '#040506', 0]].forEach(([b, a, f, c1, c2, m], i) => {
    const n = vnoise(101 + i * 37), base = h * b, amp = h * a;
    ridge(ctx, w, h, base, amp, step, t => { const v = fbm(n, t * f * 3 + i * 7, 5), r = 1 - Math.abs(fbm(n, t * f * 1.6 + 40, 4) * 2 - 1); return v * .7 + r * r * .6; });
    ctx.fillStyle = lin(ctx, base - amp * 1.3, h, [[0, c1], [1, c2]]); ctx.fill();
    if (m) { ctx.fillStyle = lin(ctx, base - amp * .35, base + amp * .8, [[0, 'rgba(160,176,210,0)'], [.5, `rgba(160,176,210,${m})`], [1, 'rgba(160,176,210,0)']]); ctx.fillRect(0, base - amp * .35, w, amp * 1.15); }
  });
}
function paintInk(ctx, w, h) {
  const u = w / 1440, M = Math.max(w, h), step = Math.max(2, 3 * u);
  ctx.fillStyle = lin(ctx, 0, h, [[0, '#0b0d10'], [.55, '#16191d'], [1, '#08090b']]); ctx.fillRect(0, 0, w, h);
  glow(ctx, w * .3, h * .32, M * .6, [[0, 'rgba(196,202,210,.11)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .78, h * .18, M * .3, [[0, 'rgba(210,196,170,.06)'], [1, 'rgba(0,0,0,0)']]);
  [[.50, .24, 2.1, 'rgba(118,126,136,.30)', 10], [.59, .27, 2.7, 'rgba(82,88,98,.48)', 5], [.69, .25, 3.3, 'rgba(44,48,54,.78)', 2], [.81, .21, 4.1, 'rgba(22,24,27,.96)', 0], [.93, .15, 5, 'rgba(10,11,12,1)', 0]].forEach(([b, a, f, c, bl], i) => {
    const n = vnoise(300 + i * 53), base = h * b, amp = h * a;
    ctx.save(); if (bl) ctx.filter = `blur(${(bl * u).toFixed(1)}px)`;
    ridge(ctx, w, h, base, amp, step, t => Math.pow(1 - Math.abs(fbm(n, t * f + i * 3, 5) * 2 - 1), 2.4));
    ctx.fillStyle = c; ctx.fill(); ctx.restore();
    ctx.fillStyle = lin(ctx, base - amp * .28, base + amp * .3, [[0, 'rgba(200,205,212,0)'], [.5, 'rgba(200,205,212,.07)'], [1, 'rgba(200,205,212,0)']]);
    ctx.fillRect(0, base - amp * .28, w, amp * .58);
  });
}
function paintTide(ctx, w, h) {
  const u = w / 1440, n = vnoise(77), M = Math.max(w, h);
  ctx.fillStyle = lin(ctx, 0, h, [[0, '#04070d'], [1, '#081322']]); ctx.fillRect(0, 0, w, h);
  glow(ctx, w * .18, h * .78, M * .5, [[0, 'rgba(24,120,132,.34)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .72, h * .3, M * .55, [[0, 'rgba(36,78,168,.32)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .9, h * .9, M * .4, [[0, 'rgba(18,96,128,.26)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .46, h * .08, M * .35, [[0, 'rgba(160,110,76,.12)'], [1, 'rgba(0,0,0,0)']]);
  ctx.lineWidth = Math.max(1, 1.1 * u);
  for (let k = 0; k < 70; k++) {
    const y0 = h * (k / 70) * 1.2 - h * .1, ph = k * .21;
    ctx.beginPath();
    for (let x = 0; x <= w; x += 6 * u) { const t = x / w, y = y0 + Math.sin(t * 5.2 + ph) * h * .05 + (fbm(n, t * 2.2 + k * .07, 4) - .5) * h * .16; x ? ctx.lineTo(x, y) : ctx.moveTo(x, y); }
    ctx.strokeStyle = `rgba(120,196,226,${(.025 + .035 * Math.sin(k * .5) ** 2).toFixed(3)})`; ctx.stroke();
  }
}
function paintWalnut(ctx, w, h) {
  const u = w / 1440, n = vnoise(19), n2 = vnoise(23), step = Math.max(1.5, 2 * u);
  ctx.fillStyle = '#1a120c'; ctx.fillRect(0, 0, w, h);
  ctx.lineWidth = step * 1.1;
  for (let y = -30 * u; y < h + 30 * u; y += step) {
    const tone = fbm(n, y / h * 7, 4);
    ctx.strokeStyle = `rgba(${70 + tone * 60 | 0},${44 + tone * 36 | 0},${26 + tone * 20 | 0},${(.12 + tone * .22).toFixed(3)})`;
    ctx.beginPath();
    for (let x = 0; x <= w + 10 * u; x += 10 * u) { const t = x / w, yy = y + Math.sin(t * 2.4 + y / h * 5) * 10 * u + (fbm(n2, t * 1.8 + y / h * 3, 3) - .5) * 60 * u; x ? ctx.lineTo(x, yy) : ctx.moveTo(x, yy); }
    ctx.stroke();
  }
  glow(ctx, w * .5, h * .45, Math.max(w, h) * .75, [[0, 'rgba(0,0,0,0)'], [1, 'rgba(0,0,0,.55)']]);
  ctx.fillStyle = 'rgba(8,5,3,.3)'; ctx.fillRect(0, 0, w, h);
}
function paintGraphite(ctx, w, h) {
  const M = Math.max(w, h);
  ctx.fillStyle = '#0c0d10'; ctx.fillRect(0, 0, w, h);
  glow(ctx, w * .5, h * .38, M * .7, [[0, 'rgba(46,50,60,.55)'], [1, 'rgba(0,0,0,0)']]);
  glow(ctx, w * .15, h * .9, M * .45, [[0, 'rgba(40,56,72,.25)'], [1, 'rgba(0,0,0,0)']]);
}
let grainSrc = null;
function grain(ctx, w, h, a) {
  if (!grainSrc) { grainSrc = document.createElement('canvas'); grainSrc.width = grainSrc.height = 160; const g = grainSrc.getContext('2d'), d = g.createImageData(160, 160), R = rng(42); for (let i = 0; i < d.data.length; i += 4) { const v = R() * 255 | 0; d.data[i] = d.data[i + 1] = d.data[i + 2] = v; d.data[i + 3] = 255; } g.putImageData(d, 0, 0); }
  ctx.save(); ctx.globalAlpha = a; ctx.globalCompositeOperation = 'overlay'; ctx.fillStyle = ctx.createPattern(grainSrc, 'repeat'); ctx.fillRect(0, 0, w, h); ctx.restore();
}
function makeNoise() {
  const c = document.createElement('canvas'); c.width = c.height = 128;
  const g = c.getContext('2d'), d = g.createImageData(128, 128), R = rng(9);
  for (let i = 0; i < d.data.length; i += 4) { const v = R() < .5 ? 0 : 255; d.data[i] = d.data[i + 1] = d.data[i + 2] = v; d.data[i + 3] = 5 + (R() * 9 | 0); }
  g.putImageData(d, 0, 0); root.style.setProperty('--noise', `url(${c.toDataURL()})`);
}
const WALLS = [{ id: 'dusk-ridge', name: '暮岭', paint: paintRidge }, { id: 'ink-mist', name: '松烟', paint: paintInk }, { id: 'deep-tide', name: '深潮', paint: paintTide }, { id: 'walnut', name: '胡桃木', paint: paintWalnut }, { id: 'graphite', name: '石墨', paint: paintGraphite }];
let customImg = null;
function drawWall(ctx, w, h, id) {
  if (id === 'custom' && customImg) { const s = Math.max(w / customImg.naturalWidth, h / customImg.naturalHeight), iw = customImg.naturalWidth * s, ih = customImg.naturalHeight * s; ctx.drawImage(customImg, (w - iw) / 2, (h - ih) / 2, iw, ih); return; }
  (WALLS.find(x => x.id === id) || WALLS[0]).paint(ctx, w, h);
  grain(ctx, w, h, .05);
}
const walls = [$('#wallA'), $('#wallB')]; let wIdx = 0, wKey = '';
function paintWall(force) {
  const dpr = Math.min(2, devicePixelRatio || 1), w = Math.round(innerWidth * dpr), h = Math.round(innerHeight * dpr);
  const id = S.wallpaper === 'custom' && !customImg ? 'dusk-ridge' : S.wallpaper;
  const key = `${id}|${w}x${h}|${id === 'custom' ? customImg.src.length : ''}`;
  if (!force && key === wKey) return;
  wKey = key;
  const cur = walls[wIdx], next = walls[wIdx ^ 1];
  /* WAAPI crossfade — the class-flip + forced-reflow + timer version could
     lose its style-commit to batching and snap instantly (and never really
     faded the outgoing layer anyway); compositor tweens can't be starved */
  const curOp = +getComputedStyle(cur).opacity, nextOp = +getComputedStyle(next).opacity;
  cur.getAnimations?.().forEach(a => a.cancel());
  next.getAnimations?.().forEach(a => a.cancel());
  next.width = w; next.height = h;
  drawWall(next.getContext('2d'), w, h, id);
  cur.classList.remove('top'); next.classList.add('top', 'on'); cur.classList.remove('on');
  if (!motion.reduced()) {
    next.animate([{ opacity: nextOp }, { opacity: 1 }], { duration: motion.dur('scene'), easing: motion.ease('in-out') });
    cur.animate([{ opacity: curOp }, { opacity: 0 }], { duration: motion.dur('scene'), easing: motion.ease('in-out'), fill: 'forwards' });
  }
  wIdx ^= 1;
}
function setCustom(url, select) {
  const im = new Image();
  im.onload = () => { customImg = im; if (select) { S.wallpaper = 'custom'; commit(); toast('已更换壁纸', 'image'); } else if (S.wallpaper === 'custom') paintWall(true); renderWallGrid(); };
  /* the server having no image while ui.json still says "custom" must not
     leave a blank canvas — paintWall's customImg-null guard draws the
     default instead */
  /* drop the stale bitmap too — a sibling DELETE must not keep painting
     the image the server no longer has */
  im.onerror = () => { customImg = null; if (S.wallpaper === 'custom') paintWall(true); };
  im.src = url;
}
const wallUrl = () => `/wallpaper?${sessionId ? 'sess=' + encodeURIComponent(sessionId) + '&' : ''}t=${Date.now()}`;
/* pull the stored image — skips the fetch once an upload is already
   loaded; `wallpaper_changed` passes force so a sibling tab's upload shows */
function loadCustom(force) { if (!customImg || force) setCustom(wallUrl(), false); }
/* 自定义缩略图左上 × — drop the stored file, then fall back to the default
   wall if it was the active pick */
async function wallClear() {
  try { await fetch(wallUrl(), { method: 'DELETE' }); } catch {}
  customImg = null;
  if (S.wallpaper === 'custom') S.wallpaper = 'dusk-ridge';
  commit();
  renderWallGrid();
}
$('#file-wall').addEventListener('change', e => {
  const f = e.target.files[0]; e.target.value = ''; if (!f) return;
  const img = new Image();
  img.onload = () => {
    const c = document.createElement('canvas'), s = Math.min(1, 2560 / img.naturalWidth);
    c.width = Math.round(img.naturalWidth * s); c.height = Math.round(img.naturalHeight * s);
    c.getContext('2d').drawImage(img, 0, 0, c.width, c.height);
    const url = c.toDataURL('image/jpeg', .86);
    URL.revokeObjectURL(img.src);
    /* the file on disk is the source of truth — only select "custom" once
       the PUT lands (a failed upload must not persist a phantom choice) */
    fetch(wallUrl(), { method: 'PUT', headers: { 'content-type': 'text/plain' }, body: url })
      .then(r => { if (!r.ok) throw new Error(r.status); setCustom(url, true); })
      .catch(() => toast('壁纸保存失败', 'alert', 'warn'));
  };
  img.onerror = () => toast('无法读取这张图片', 'alert', 'warn');
  img.src = URL.createObjectURL(f);
});

