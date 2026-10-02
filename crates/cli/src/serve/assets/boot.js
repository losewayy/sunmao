/* boot + __sunmao test handle */
'use strict';

/* ================= boot ================= */
makeNoise();
/* one-shot migration: a pre-server-storage `sunmao.wall.custom` dataURL gets
   pushed to PUT /wallpaper once; the key is dropped when the upload lands */
try { const cu = localStorage.getItem('sunmao.wall.custom'); if (cu) fetch(wallUrl(), { method: 'PUT', headers: { 'content-type': 'text/plain' }, body: cu }).then(r => { if (r.ok) localStorage.removeItem('sunmao.wall.custom'); }).catch(() => {}); } catch {}
if (S.wallpaper === 'custom') loadCustom();
apply();
loadUi(); /* server ui.json overrides the localStorage first-frame cache */
show('session');
updateHero();
connect();

// debug/test handle — replay parity harness drives the fold through this
window.__sunmao = { renderReplay, liveEvent, TX: () => TX, EVLOG };

