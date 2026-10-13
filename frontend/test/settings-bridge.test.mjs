import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Window } from 'happy-dom';

const source = await readFile(new URL('../../crates/cli/src/serve/assets/settings.js', import.meta.url), 'utf8');
function fixture() {
  const window = new Window({ settings: { enableJavaScriptEvaluation: true, suppressInsecureJavaScriptEnvironmentWarning: true } });
  const document = window.document;
  document.body.innerHTML = '<button id="anchor"></button>';
  let mounted, commits = 0;
  const messages = [];
  Object.assign(window, {
    t: (value, vars) => vars ? value.replace('{n}', vars.n) : value,
    S: { fonts: { ui: 'Font A', code: 'Code A' } },
    MODELS: { providers: { vendor: { catalog: [{ id: 'a' }, { id: 'b' }] } }, routes: { route: ['vendor/a'] }, selectors: ['vendor/a'], default_model: 'vendor/a' },
    modelLabel: 'vendor/a', turnMode: 'standard', popAnchor: null, popEl: null,
    provNames: () => ['vendor'], wsSend: value => messages.push(value),
    closePop: () => { window.popEl = null; },
    pop(anchor, _, options) { const host = document.createElement('div'); window.popEl = host; options.onMount(host); },
    SunmaoUI: { mountModelPicker: (_, options) => { mounted = options; return () => {}; } },
    recordCommit: () => commits++,
  });
  window.eval(source + '\n;commit = recordCommit; window.Test = { modelPop, defaultModelPop, changeAppearance };');
  return { window, document, options: () => mounted, messages, commits: () => commits };
}

test('an unset Fusion role stays unset instead of inheriting the session picker selection', async () => {
  const f = fixture();
  try {
    f.window.Test.modelPop(f.document.getElementById('anchor'), { selected: '', allowFreeform: false, emptyLabel: 'Inherited', onSelect() {} });
    assert.equal(f.options().selected, '');
    assert.equal(f.options().rows[0].value, '');
    assert.equal(f.options().allowFreeform, false);
  } finally { await f.window.happyDOM.close(); }
});

test('default model uses the backend selector allowlist and session picks keep the wire shape', async () => {
  const f = fixture();
  try {
    f.window.Test.defaultModelPop(f.document.getElementById('anchor'));
    assert.deepEqual(Array.from(f.options().rows, row => row.value), ['vendor/a']);
    assert.equal(f.options().allowFreeform, false);
    f.window.Test.modelPop(f.document.getElementById('anchor'));
    f.options().onSelect('vendor/custom');
    assert.equal(f.messages.length, 1);
    assert.equal(f.messages[0].type, 'model');
    assert.equal(f.messages[0].sel, 'vendor/custom');
  } finally { await f.window.happyDOM.close(); }
});

test('appearance bridge preserves percentages and nested font state', async () => {
  const f = fixture();
  try {
    f.window.Test.changeAppearance('opacity', 72);
    f.window.Test.changeAppearance('dim', 24);
    f.window.Test.changeAppearance('fontui', 'Custom Font');
    f.window.Test.changeAppearance('side', false);
    assert.equal(f.window.S.panelOpacity, 0.72);
    assert.equal(f.window.S.dim, 0.24);
    assert.equal(f.window.S.fonts.ui, 'Custom Font');
    assert.equal(f.window.S.fonts.code, 'Code A');
    assert.equal(f.window.S.translucentSidebar, false);
    assert.equal(f.commits(), 4);
  } finally { await f.window.happyDOM.close(); }
});
