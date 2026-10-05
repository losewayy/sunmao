/* UI language.
 *
 * Chinese is this page's source language, so `t()`'s key IS the Chinese
 * string and `assets/i18n.en.js` + `assets/i18n.en.panels.js` map it to
 * English — no key vocabulary to keep in sync, and an untranslated line
 * shows up as Chinese text rather than as a key name. The table ships as
 * two classic scripts (chrome and settings panels) because one file would
 * run past the repo's god-file budget; both are loaded together and merged
 * into the single `I18N_EN` the lookup below reads. The two file names are
 * a size device, not a taxonomy: `panels` holds whatever a panel source json
 * declares in `_scratch/i18n/merge.mjs` (settings, channels, schedules,
 * default-model) and `chrome` everything else, so a string both halves use
 * lands in one of them. Nothing here depends on which half a key is in, and
 * everything here runs at render time, so switching the language reloads
 * once instead of hunting live nodes.
 *
 * Translations are plain text: no `"`, `<`, `>` and no HTML entity. A value
 * is injected both as text and into attributes, so a quote has to be “ ”,
 * and the site that interpolates it into an attribute wraps it in esc()
 * (enforced at generation time by merge.mjs).
 *
 * The brand follows the language: 榫卯 in Chinese, sunmao everywhere else.
 * `S.lang` is the setting ('auto' | 'zh' | 'en'); 'auto' reads the system
 * locale, so a Chinese Windows shows 榫卯 with nothing configured.
 */
let uiLang = 'zh';

/* One lookup table out of the two halves. `typeof` guards, not a plain
   reference: the annotate prelude injects this file into a browsed page's
   realm, which may carry only one half of the dictionary. */
const I18N_EN = Object.assign({},
  typeof I18N_EN_CHROME !== 'undefined' ? I18N_EN_CHROME : {},
  typeof I18N_EN_PANELS !== 'undefined' ? I18N_EN_PANELS : {});

function detectLang(pref) {
  if (pref === 'zh' || pref === 'en') return pref;
  return (navigator.language || 'zh').toLowerCase().startsWith('zh') ? 'zh' : 'en';
}

function fillVars(s, vars) {
  if (!vars) return s;
  return s.replace(/\{(\w+)\}/g, (m, k) => (vars[k] == null ? m : String(vars[k])));
}

function t(zh, vars) {
  if (uiLang === 'zh') return fillVars(zh, vars);
  const hit = typeof I18N_EN !== 'undefined' ? I18N_EN[zh] : undefined;
  return fillVars(hit == null ? zh : hit, vars);
}

function brand() { return uiLang === 'zh' ? '榫卯' : 'sunmao'; }

/* Static markup declares its own translation: `data-i18n` for an element
   whose whole content is the text, `data-i18n-tail` for the text that
   follows an icon (replaces the last text node, so the svg survives),
   `data-i18n-tip` / `data-i18n-ph` / `data-i18n-al` for the tooltip,
   placeholder and aria-label attributes. */
function applyI18n(root) {
  const r = root || document;
  r.querySelectorAll('[data-i18n]').forEach(el => { el.textContent = t(el.dataset.i18n); });
  r.querySelectorAll('[data-i18n-tail]').forEach(el => {
    const last = [...el.childNodes].reverse().find(n => n.nodeType === 3 && n.nodeValue.trim());
    if (last) last.nodeValue = t(el.dataset.i18nTail);
  });
  r.querySelectorAll('[data-i18n-tip]').forEach(el => { el.dataset.tip = t(el.dataset.i18nTip); });
  r.querySelectorAll('[data-i18n-ph]').forEach(el => el.setAttribute('placeholder', t(el.dataset.i18nPh)));
  r.querySelectorAll('[data-i18n-al]').forEach(el => el.setAttribute('aria-label', t(el.dataset.i18nAl)));
}

/* settings › 外观: 跟随系统 / 中文 / English — persisted in ui.json and
   applied by a reload. `uiLang` is resolved from the store before the first
   render (boot.js), so there is no flash of the other language. */
function langPick() {
  const cur = (typeof S === 'object' && S.lang) || 'auto';
  menuPop(langAnchor(), [
    { label: t('界面语言') },
    { v: 'auto', t: t('跟随系统'), d: detectLang('auto') === 'zh' ? '中文' : 'English', on: cur === 'auto' },
    { v: 'zh', t: '中文', on: cur === 'zh' },
    { v: 'en', t: 'English', on: cur === 'en' },
  ], v => { S.lang = v; save(); location.reload(); }, { place: 'top', align: 'end' });
}
function langAnchor() { return document.getElementById('pv-lang'); }
function langLabel() {
  const cur = (typeof S === 'object' && S.lang) || 'auto';
  return cur === 'zh' ? '中文' : cur === 'en' ? 'English' : t('跟随系统');
}
