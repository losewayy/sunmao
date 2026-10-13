import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Window } from 'happy-dom';

const bundle = await readFile(new URL('../../crates/cli/src/serve/assets/ecosystem.js', import.meta.url), 'utf8');
const settle = () => new Promise(resolve => setTimeout(resolve, 80));
function fixture() {
  const window = new Window({ settings: { enableJavaScriptEvaluation: true, suppressInsecureJavaScriptEnvironmentWarning: true } });
  const document = window.document;
  const kinds = ['sliders', 'themes', 'side', 'contrast', 'fontui', 'fontcode', 'motion', 'lang'];
  document.body.innerHTML = '<div id="settings-components-root"></div>' + kinds.map(kind => `<div id="${kind}"></div>`).join('');
  window.eval(bundle);
  const changes = [], overlays = new Set();
  const options = {
    hosts: Object.fromEntries(kinds.map(kind => [kind, document.getElementById(kind)])), gap: 6,
    values: { mode: 'system', side: true, opacity: 56, blur: 24, dim: 16, contrast: 50, fontui: 'Font A', fontcode: 'Code A', motion: 'system', lang: 'auto' },
    labels: { mode: 'Theme', side: 'Translucent sidebar', opacity: 'Opacity', blur: 'Blur', dim: 'Dim', contrast: 'Contrast', fontui: 'UI font', fontcode: 'Code font', motion: 'Motion', lang: 'Language' },
    choices: { mode: [{ value: 'system', label: 'System' }, { value: 'light', label: 'Light' }, { value: 'dark', label: 'Dark' }], fontui: [{ value: 'Font A', label: 'Font A' }, { value: 'Font B', label: 'Font B' }], fontcode: [{ value: 'Code A', label: 'Code A' }], motion: [{ value: 'system', label: 'System' }, { value: 'reduce', label: 'Reduce' }], lang: [{ value: 'auto', label: 'System' }, { value: 'en', label: 'English' }] },
    systemTheme: 'Current dark',
    onChange(key, value) { changes.push([key, value]); options.values = { ...options.values, [key]: value }; window.SunmaoUI.updateAppearance(options); },
    onOverlay(id, open) { if (open) overlays.add(id); else overlays.delete(id); },
  };
  window.SunmaoUI.updateAppearance(options);
  return { window, document, options, changes, overlays };
}

test('switch and theme changes have one callback and external resets do not write back', async () => {
  const f = fixture();
  try {
    await settle();
    const control = f.document.querySelector('[role="switch"]');
    assert.equal(control.getAttribute('aria-checked'), 'true');
    control.click();
    await settle();
    assert.deepEqual(f.changes, [['side', false]]);
    const dark = [...f.document.querySelectorAll('[role="radio"]')].find(el => el.textContent === 'Dark');
    dark.click();
    await settle();
    assert.deepEqual(f.changes.at(-1), ['mode', 'dark']);
    const count = f.changes.length;
    f.options.values = { ...f.options.values, side: true, mode: 'light' };
    f.window.SunmaoUI.updateAppearance(f.options);
    await settle();
    assert.equal(control.getAttribute('aria-checked'), 'true');
    assert.equal(f.changes.length, count);
  } finally { await f.window.happyDOM.close(); }
});

test('sliders retain bounds, units and keyboard updates', async () => {
  const f = fixture();
  try {
    await settle();
    const sliders = [...f.document.querySelectorAll('input[type="range"]')];
    assert.equal(sliders.length, 4);
    const blur = sliders.find(input => input.getAttribute('aria-label') === 'Blur');
    assert.equal(blur.value, '24');
    assert.equal(blur.max, '40');
    assert.equal(blur.getAttribute('aria-valuetext'), '24px');
    blur.dispatchEvent(new f.window.KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
    await settle();
    assert.deepEqual(f.changes.at(-1), ['blur', 25]);
  } finally { await f.window.happyDOM.close(); }
});

test('select uses keyboard choices and hiding settings releases its popup', async () => {
  const f = fixture();
  try {
    await settle();
    const trigger = f.document.querySelector('#fontui button');
    trigger.click();
    await settle();
    assert.ok(f.overlays.has('fontui'));
    const item = [...f.document.querySelectorAll('[role="option"]')].find(el => el.textContent.includes('Font B'));
    assert.ok(item);
    item.click();
    await settle();
    assert.deepEqual(f.changes.at(-1), ['fontui', 'Font B']);
    assert.equal(f.overlays.size, 0);
    trigger.click();
    await settle();
    f.window.SunmaoUI.hideAppearance();
    await settle();
    assert.equal(f.overlays.size, 0);
    assert.equal(f.document.querySelector('[role="option"]'), null);
    f.window.SunmaoUI.updateAppearance(f.options);
    await settle();
    assert.equal(f.document.querySelector('[role="option"]'), null, 're-entering settings starts with closed pickers');
  } finally { await f.window.happyDOM.close(); }
});
