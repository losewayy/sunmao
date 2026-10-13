import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Window } from 'happy-dom';

const bundle = await readFile(new URL('../../crates/cli/src/serve/assets/ecosystem.js', import.meta.url), 'utf8');
const settle = () => new Promise(resolve => setTimeout(resolve, 80));
function fixture(allowFreeform) {
  const window = new Window({ settings: { enableJavaScriptEvaluation: true, suppressInsecureJavaScriptEnvironmentWarning: true } });
  const document = window.document;
  document.body.innerHTML = '<button id="anchor">Models</button><div id="popup"></div><button id="next">Next</button>';
  window.eval(bundle);
  const selected = [];
  const options = {
    anchor: document.getElementById('anchor'), selected: '', label: 'Model', placeholder: 'Search', empty: 'No saved model', allowFreeform,
    rows: [{ value: '@route', label: '@route', group: 'Routes', description: 'vendor/a → vendor/b' }, { value: 'vendor/a', label: 'vendor/a', group: 'vendor' }, { value: 'vendor/b', label: 'vendor/b', group: 'vendor' }],
    onSelect: value => selected.push(value), onClose() {},
  };
  options.anchor.focus();
  const dispose = window.SunmaoUI.mountModelPicker(document.getElementById('popup'), options);
  return { window, document, selected, dispose, options };
}
function type(f, value) {
  const input = f.document.querySelector('[role="combobox"]');
  Object.getOwnPropertyDescriptor(f.window.HTMLInputElement.prototype, 'value').set.call(input, value);
  input.dispatchEvent(new f.window.InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
}
function key(f, key) {
  f.document.querySelector('[role="combobox"]').dispatchEvent(new f.window.KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
}

test('saved model picker filters groups and selects with the keyboard', async () => {
  const f = fixture(false);
  try {
    await settle();
    assert.equal(f.document.activeElement.getAttribute('role'), 'combobox');
    assert.equal(f.document.querySelectorAll('[role="group"]').length, 2);
    type(f, 'vendor/');
    await settle();
    const rows = [...f.document.querySelectorAll('[role="option"]')];
    assert.ok(rows.some(row => row.textContent.includes('@route')), 'route descriptions are searchable');
    key(f, 'ArrowDown');
    await settle();
    key(f, 'Enter');
    await settle();
    assert.deepEqual(f.selected, ['vendor/a']);
  } finally { f.dispose(); await f.window.happyDOM.close(); }
});

test('saved-only roles cannot submit an unknown typed selector', async () => {
  const f = fixture(false);
  try {
    type(f, 'unknown/model');
    await settle();
    assert.equal(f.document.querySelectorAll('[role="option"]').length, 0);
    key(f, 'Enter');
    await settle();
    assert.deepEqual(f.selected, []);
  } finally { f.dispose(); await f.window.happyDOM.close(); }
});

test('session picker accepts a literal selector even when saved models also match', async () => {
  const f = fixture(true);
  try {
    type(f, 'vendor/');
    await settle();
    assert.equal(f.document.querySelector('[role="option"]').textContent, 'vendor/');
    key(f, 'Enter');
    await settle();
    assert.deepEqual(f.selected, ['vendor/']);
    f.document.getElementById('next').focus();
    f.dispose();
    assert.equal(f.document.activeElement.id, 'next', 'cleanup must not steal focus from a later action');
  } finally { await f.window.happyDOM.close(); }
});
