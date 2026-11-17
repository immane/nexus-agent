import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const css = readFileSync(new URL('../web/app.css', import.meta.url), 'utf8');
const values = [...html.matchAll(/type="radio" name="theme" value="([^"]+)"/g)].map((match) => match[1]);

function fixture(saved, blocked = false) {
  const inputs = values.map((value) => ({ name: 'theme', value, checked: false }));
  const listeners = {};
  const storage = new Map(saved === undefined ? [] : [['nexus.theme', saved]]);
  const dom = {
    root: { dataset: {} },
    theme: {
      querySelectorAll: () => inputs,
      addEventListener: (name, callback) => { listeners[name] = callback; },
    },
  };
  const context = vm.createContext({
    dom,
    localStorage: {
      getItem: (key) => { if (blocked) throw new Error('Unavailable'); return storage.get(key); },
      setItem: (key, value) => { if (blocked) throw new Error('Unavailable'); storage.set(key, value); },
    },
  });
  vm.runInContext(source.slice(source.indexOf('const themes = '), source.indexOf('/* Installation */')), context);
  return {
    dom, inputs, storage,
    selected: () => inputs.filter((input) => input.checked).map((input) => input.value),
    change(value) {
      const target = inputs.find((input) => input.value === value);
      for (const input of inputs) input.checked = input === target;
      listeners.change({ target });
    },
    dispatch: (target) => listeners.change({ target }),
  };
}

test('theme cards replace the dropdown and share the supported palette identities', () => {
  assert.equal(values.length, 10);
  assert.equal(new Set(values).size, values.length);
  assert.ok(!html.includes('<select id="theme"'));
  assert.match(html, /<fieldset class="theme-picker" id="theme"/);
  for (const value of values) {
    assert.ok(html.includes(`class="theme-preview" data-theme="${value}"`));
    if (value !== 'graphite') assert.ok(css.includes(`.theme-preview[data-theme="${value}"]`));
    const f = fixture(value);
    assert.deepEqual(f.selected(), [value]);
    assert.equal(f.dom.root.dataset.theme, value === 'system' ? undefined : value);
  }
});

test('card changes apply immediately, persist, and restore after reload', () => {
  const f = fixture('paper');
  for (const value of values) {
    f.change(value);
    assert.deepEqual(f.selected(), [value]);
    assert.equal(f.dom.root.dataset.theme, value === 'system' ? undefined : value);
    assert.equal(f.storage.get('nexus.theme'), value);
    assert.deepEqual(fixture(f.storage.get('nexus.theme')).selected(), [value]);
  }
});

test('missing, invalid and blocked storage use system; blocked writes still allow switching', () => {
  for (const saved of [undefined, '', 'unknown', '<script>']) {
    const f = fixture(saved);
    assert.deepEqual(f.selected(), ['system']);
    assert.equal(f.dom.root.dataset.theme, undefined);
  }
  const f = fixture('forest', true);
  assert.deepEqual(f.selected(), ['system']);
  f.change('amethyst');
  assert.equal(f.dom.root.dataset.theme, 'amethyst');
  assert.deepEqual(f.selected(), ['amethyst']);
  f.change('system');
  assert.equal(f.dom.root.dataset.theme, undefined);
});

test('unrelated, unchecked and unsupported change events cannot select a theme', () => {
  const f = fixture('ocean');
  for (const target of [
    { name: 'other', value: 'paper', checked: true },
    { name: 'theme', value: 'paper', checked: false },
    { name: 'theme', value: 'unknown', checked: true },
  ]) f.dispatch(target);
  assert.deepEqual(f.selected(), ['ocean']);
  assert.equal(f.dom.root.dataset.theme, 'ocean');
  assert.equal(f.storage.get('nexus.theme'), 'ocean');
});
