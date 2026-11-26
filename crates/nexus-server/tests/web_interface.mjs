// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
const css = readFileSync(new URL('../web/app.css', import.meta.url), 'utf8');

function parse(raw) {
  const context = vm.createContext({});
  vm.runInContext(source.slice(source.indexOf('function clampStep('), source.indexOf('function parseBackgroundSettings(')), context);
  vm.runInContext(source.slice(source.indexOf('const INTERFACE_KEY'), source.indexOf('function interfaceZoom(')), context);
  return { ...vm.runInContext(`parseInterfaceSettings(${JSON.stringify(raw)})`, context) };
}

const defaults = { uiScale: 100, contentSize: 15, contentWidth: 'default', lineSpacing: 'default', motion: 'system' };

test('interface settings accept only the offered choices', () => {
  assert.deepEqual(parse(null), defaults);
  assert.deepEqual(parse('not json'), defaults);
  assert.deepEqual(parse(JSON.stringify({ uiScale: 125, contentSize: 17, contentWidth: 'wide', lineSpacing: 'relaxed', motion: 'reduce' })),
    { uiScale: 125, contentSize: 17, contentWidth: 'wide', lineSpacing: 'relaxed', motion: 'reduce' });
  assert.deepEqual(parse(JSON.stringify({ uiScale: 300, contentSize: 99, contentWidth: 'toString', lineSpacing: '__proto__', motion: 'fast' })),
    { ...defaults, contentSize: 20 });
  assert.equal(parse(JSON.stringify({ contentSize: 2 })).contentSize, 13);
  assert.equal(parse(JSON.stringify({ uiScale: '110' })).uiScale, 110);
});

test('every interface choice in the page is understood by the parser', () => {
  const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
  const keys = { 'ui-scale': 'uiScale', 'content-width': 'contentWidth', 'line-spacing': 'lineSpacing', motion: 'motion' };
  const choices = [...html.matchAll(/type="radio" name="(ui-scale|content-width|line-spacing|motion)" value="([^"]+)"/g)];
  assert.ok(choices.length >= 13);
  for (const [, name, value] of choices) {
    const typed = name === 'ui-scale' ? Number(value) : value;
    assert.equal(parse(JSON.stringify({ [keys[name]]: typed }))[keys[name]], typed, `${name}=${value}`);
  }
});

test('zoom-sensitive viewport sizes are divided by the interface scale', () => {
  assert.match(css, /zoom:\s*var\(--ui-scale, 1\)/);
  const unscaled = [...css.matchAll(/[^;{}]*\b100dvh\b[^;]*;/g)].map(([rule]) => rule.trim())
    .filter((rule) => !rule.includes('/ var(--ui-scale, 1)'));
  assert.deepEqual(unscaled, []);
});
