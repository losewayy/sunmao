import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Window } from 'happy-dom';

const bundle = await readFile(new URL('../../crates/cli/src/serve/assets/ecosystem.js', import.meta.url), 'utf8');

function fixture() {
  const window = new Window({ url: 'http://localhost/', settings: { enableJavaScriptEvaluation: true, suppressInsecureJavaScriptEnvironmentWarning: true } });
  window.document.body.innerHTML = '<button id="trigger">Open</button><div id="palette" hidden></div><div id="imv" hidden></div><div id="ecosystem-root"></div>';
  window.eval(bundle);
  const document = window.document;
  const trigger = document.getElementById('trigger');
  trigger.focus();
  return { window, document, ui: window.SunmaoUI, trigger };
}

const settle = () => new Promise(resolve => setTimeout(resolve, 80));

function paletteOptions(document, items, onClosed = () => {}) {
  return { host: document.getElementById('palette'), items, label: '命令面板', closeLabel: '关闭', placeholder: '搜索', empty: '没有匹配的命令', hints: ['选择', '执行', '关闭'], onClosed };
}

test('command menu provides combobox semantics, filtering and keyboard execution', async () => {
  const { window, document, ui } = fixture();
  try {
    const picked = [];
    ui.openPalette(paletteOptions(document, [
      { g: '操作', t: '新对话', i: 'pen', run: () => picked.push('new') },
      { g: '操作', t: '打开设置', i: 'settings', run: () => picked.push('settings') },
    ]));
    await settle();
    const input = document.querySelector('#palette [role="combobox"]');
    assert.ok(input, 'React component mounts into the existing shell');
    assert.equal(input.getAttribute('role'), 'combobox');
    assert.equal(document.querySelector('[role="dialog"]').getAttribute('aria-modal'), 'true');
    assert.equal(document.activeElement, input, 'dialog focuses its search input');
    assert.equal(document.querySelectorAll('[role="option"]').length, 2);
    input.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true, cancelable: true }));
    await settle();
    input.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
    await settle();
    assert.deepEqual(picked, ['settings']);
    assert.equal(document.getElementById('palette').hidden, true);
    ui.openPalette(paletteOptions(document, [{ g: '操作', t: '新对话', i: 'pen', run() {} }]));
    await settle();
    const fresh = document.querySelector('#palette [role="combobox"]');
    Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set.call(fresh, '不存在');
    fresh.dispatchEvent(new window.InputEvent('input', { bubbles: true, inputType: 'insertText', data: '不存在' }));
    await settle();
    assert.equal(document.querySelectorAll('[role="option"]').length, 0);
    assert.ok(document.querySelector('[cmdk-empty]'));
  } finally { await window.happyDOM.close(); }
});

test('dialog restores focus and reopening does not keep stale commands', async () => {
  const { window, document, ui, trigger } = fixture();
  try {
    let closed = 0;
    const options = paletteOptions(document, [{ g: '操作', t: '旧命令', i: 'pen', run() {} }], () => closed++);
    ui.openPalette(options);
    await settle();
    document.querySelector('#palette [role="combobox"]').dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    await settle();
    assert.equal(document.getElementById('palette').hidden, true);
    assert.equal(document.activeElement, trigger);
    assert.equal(closed, 1);
    ui.openPalette(paletteOptions(document, [{ g: '操作', t: '新命令', i: 'pen', run() {} }], () => closed++));
    ui.closePalette();
    ui.openPalette(paletteOptions(document, [{ g: '操作', t: '最终命令', i: 'pen', run() {} }], () => closed++));
    await settle();
    assert.equal(document.getElementById('palette').hidden, false);
    assert.equal(document.querySelector('[role="option"]').textContent, '最终命令');
    assert.equal(closed, 1, 'an obsolete close cannot settle the reopened dialog');
  } finally { await window.happyDOM.close(); }
});

test('image dialog exposes its name, caption and keyboard close', async () => {
  const { window, document, ui, trigger } = fixture();
  try {
    let closed = 0;
    ui.openImage({ host: document.getElementById('imv'), src: 'data:image/png;base64,', caption: '<caption>', label: '图片预览', closeLabel: '关闭', onClosed: () => closed++ });
    await settle();
    const dialog = document.querySelector('[role="dialog"]');
    assert.ok(dialog);
    assert.equal(document.getElementById(dialog.getAttribute('aria-labelledby')).textContent, '图片预览');
    assert.equal(document.getElementById('imv-cap').textContent, '<caption>');
    assert.equal(document.getElementById('imv-img').alt, '<caption>');
    document.activeElement.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }));
    await settle();
    assert.equal(document.getElementById('imv').hidden, true);
    assert.equal(document.getElementById('imv-img'), null, 'closing releases the image DOM');
    assert.equal(document.activeElement, trigger);
    assert.equal(closed, 1);
  } finally { await window.happyDOM.close(); }
});
