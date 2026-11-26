// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

function fixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const context = vm.createContext({});
  vm.runInContext(source.slice(
    source.indexOf("const BACKGROUND_SETTINGS_KEY"),
    source.indexOf('function openBackgroundStore('),
  ), context);
  return (code) => vm.runInContext(code, context);
}

test('background settings are clamped to the slider ranges and steps', () => {
  const run = fixture();
  assert.deepEqual({ ...run(`parseBackgroundSettings('{"dim":37,"blur":5}')`) }, { dim: 35, blur: 6 });
  assert.deepEqual({ ...run(`parseBackgroundSettings('{"dim":500,"blur":-3}')`) }, { dim: 90, blur: 0 });
  assert.deepEqual({ ...run(`parseBackgroundSettings('not json')`) }, { dim: 55, blur: 0 });
  assert.deepEqual({ ...run(`parseBackgroundSettings(null)`) }, { dim: 55, blur: 0 });
  assert.deepEqual({ ...run(`parseBackgroundSettings('{"dim":"x"}')`) }, { dim: 55, blur: 0 });
});

test('images shrink to the longest-edge limit without upscaling', () => {
  const run = fixture();
  assert.deepEqual({ ...run('fitWithin(6000, 4000, 2560)') }, { width: 2560, height: 1707 });
  assert.deepEqual({ ...run('fitWithin(1080, 1920, 2560)') }, { width: 1080, height: 1920 });
  assert.deepEqual({ ...run('fitWithin(3000, 10, 2560)') }, { width: 2560, height: 9 });
});

test('only raster image types are accepted for re-encoding', () => {
  const run = fixture();
  assert.equal(run(`BACKGROUND_TYPES.has('image/svg+xml')`), false);
  assert.equal(run(`BACKGROUND_TYPES.has('image/webp')`), true);
});
