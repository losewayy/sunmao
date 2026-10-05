/* boot + __sunmao test handle */
'use strict';

/* ================= boot ================= */
/* Every step runs behind its own guard. A throw in one of them (a dirty ui
   value, a shape the render path did not expect) used to take the rest of
   the boot with it — i18n, the shell, the title, and the websocket — which
   left the page stuck on its loading state with nothing to recover it. */
const step = (label, fn) => { try { return fn(); } catch (e) { console.error('boot: ' + label + ' failed', e); } };

step('noise', () => makeNoise());
/* one-shot migration: a pre-server-storage `sunmao.wall.custom` dataURL gets
   pushed to PUT /wallpaper once; the key is dropped when the upload lands */
try { const cu = localStorage.getItem('sunmao.wall.custom'); if (cu) fetch(wallUrl(), { method: 'PUT', headers: { 'content-type': 'text/plain' }, body: cu }).then(r => { if (r.ok) localStorage.removeItem('sunmao.wall.custom'); }).catch(() => {}); } catch {}
step('wallpaper', () => { if (S.wallpaper === 'custom') loadCustom(); });
step('apply', () => apply());
/* the language is already resolved from the ui store (state.js); translate
   the static shell before the first paint, and let the brand follow it */
step('lang', () => { document.documentElement.lang = uiLang === 'zh' ? 'zh-CN' : 'en'; });
step('title', () => { document.title = brand(); });
step('i18n', () => applyI18n());
step('loadUi', () => loadUi()); /* server ui.json overrides the localStorage first-frame cache */
step('show', () => show('session'));
step('hero', () => updateHero());
step('connect', () => connect());

// debug/test handle — replay parity harness drives the fold through this
step('handle', () => { window.__sunmao = { renderReplay, liveEvent, TX: () => TX, EVLOG }; });

