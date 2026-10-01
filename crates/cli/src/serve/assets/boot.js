/* boot + __sunmao test handle */
'use strict';

/* ================= boot ================= */
makeNoise();
try { const cu = localStorage.getItem('sunmao.wall.custom'); if (cu) setCustom(cu, false); } catch {}
apply();
show('session');
updateHero();
connect();

// debug/test handle — replay parity harness drives the fold through this
window.__sunmao = { renderReplay, liveEvent, TX: () => TX, EVLOG };

