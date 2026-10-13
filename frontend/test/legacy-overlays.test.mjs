import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { Window } from 'happy-dom';

const source = await readFile(new URL('../../crates/cli/src/serve/assets/palette.js', import.meta.url), 'utf8');
function fixture() {
  const window = new Window({ settings: { enableJavaScriptEvaluation: true, suppressInsecureJavaScriptEnvironmentWarning: true } });
  const document = window.document;
  document.body.innerHTML = '<div id="app"></div><div id="palette" hidden></div><div id="imv" hidden></div><div id="tip"></div><div id="rail"></div><div id="toasts"></div><button id="anchor" data-tip="Help"></button>';
  const overlays = [];
  let options, cleanups = 0;
  Object.assign(window, {
    $: selector => document.querySelector(selector), app: document.getElementById('app'),
    t: value => value, esc: value => String(value), ic: () => '',
    motion: { dur: () => 0, delay: () => 0, px: (_, fallback) => fallback, hold: () => 0 },
    brOverlay: value => overlays.push(value),
    SunmaoUI: {
      openPalette: value => { options = value; value.host.hidden = false; }, closePalette() {},
      autoUpdate: (_, __, update) => { update(); return () => cleanups++; },
      computePosition: async (_, __, options) => {
        options.middleware.forEach(middleware => middleware.apply?.({ availableHeight: 1000 }));
        return { x: 10, y: 20, placement: 'bottom-start' };
      },
      offset: () => ({}), flip: () => ({}), shift: () => ({}), size: options => options,
    },
  });
  window.eval(source + '\n;palSource = () => []; window.Test = { openPalette, pop, closePop, hideTip };');
  return { window, document, overlays, test: window.Test, options: () => options, cleanups: () => cleanups };
}

test('closing one modal preserves native overlay parking for another modal', async () => {
  const f = fixture();
  try {
    f.test.openPalette();
    f.document.getElementById('palette').hidden = true;
    f.document.getElementById('imv').hidden = false;
    f.options().onClosed();
    assert.equal(f.overlays.at(-1), true);
  } finally { await f.window.happyDOM.close(); }
});

test('popover closes its autoUpdate subscription and ignores late placement', async () => {
  const f = fixture();
  try {
    const anchor = f.document.getElementById('anchor');
    const pop = f.test.pop(anchor, 'menu');
    f.test.closePop();
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(f.cleanups(), 1);
    assert.equal(pop.style.left, '', 'a pending position result cannot update a closed popover');
    assert.equal(f.overlays.at(-1), null);
  } finally { await f.window.happyDOM.close(); }
});

test('keyboard tooltip describes the control and cleans up on blur', async () => {
  const f = fixture();
  try {
    const anchor = f.document.getElementById('anchor');
    anchor.setAttribute('aria-describedby', 'existing');
    anchor.focus();
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(anchor.getAttribute('aria-describedby'), 'existing tip');
    assert.ok(f.document.getElementById('tip').classList.contains('show'));
    anchor.blur();
    assert.equal(anchor.getAttribute('aria-describedby'), 'existing');
    assert.equal(f.cleanups(), 1);
  } finally { await f.window.happyDOM.close(); }
});

test('available viewport room does not expand a menu past its CSS height limit', async () => {
  const f = fixture();
  try {
    const pop = f.test.pop(f.document.getElementById('anchor'), '<div class="mp-list" style="max-height:100px">menu</div>');
    await new Promise(resolve => setTimeout(resolve, 20));
    assert.equal(pop.querySelector('.mp-list').style.maxHeight, '100px');
    f.test.closePop();
  } finally { await f.window.happyDOM.close(); }
});

test('legacy popover cleanup leaves component-owned select portals intact', async () => {
  const f = fixture();
  try {
    const select = f.document.createElement('div');
    select.className = 'pop ui-select-popup';
    f.document.body.appendChild(select);
    f.test.pop(f.document.getElementById('anchor'), 'menu');
    assert.equal(select.isConnected, true, 'shared styling does not grant legacy code ownership of a React portal');
    f.test.closePop();
  } finally { await f.window.happyDOM.close(); }
});
