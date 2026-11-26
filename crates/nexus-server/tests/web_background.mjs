// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture(globals = {}, includeHistory = false) {
  const context = vm.createContext(globals);
  vm.runInContext(source.slice(
    source.indexOf("const BACKGROUND_SETTINGS_KEY"),
    source.indexOf('function openBackgroundStore('),
  ), context);
  if (includeHistory) {
    for (const [start, end] of [
      ['function rememberBackground(', 'function updateBackgroundControls('],
      ['async function useRecentBackground(', 'async function chooseBackground('],
    ]) {
      vm.runInContext(source.slice(source.indexOf(start), source.indexOf(end)), context);
    }
  }
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

test('image palettes ignore transparent pixels and isolated accents', () => {
  const run = fixture();
  assert.equal(run('extractImageColors([255, 0, 0, 0])'), null);
  assert.equal(run(`extractImageColors([
    20, 40, 68, 255, 22, 42, 70, 255, 24, 44, 72, 255,
    255, 0, 0, 255, 255, 255, 255, 0,
  ]).base`), '#162a46');
  assert.equal(run('extractImageColors([120, 120, 120, 255]).base'), '#787878');
});

function pixels(...regions) {
  const data = [];
  for (const [[r, g, b], count] of regions) for (let i = 0; i < count; i += 1) data.push(r, g, b, 255);
  return JSON.stringify(data);
}

test('the accent is the most vivid color that covers a meaningful area', () => {
  const run = fixture();
  // Mostly navy with a large orange region and a tiny green speck.
  const colors = run(`extractImageColors(${pixels([[20, 30, 70], 700], [[240, 140, 40], 280], [[0, 255, 0], 10])})`);
  assert.equal(colors.base, '#141e46');
  assert.match(colors.accent, /^#f/, `accent ${colors.accent} should be the orange region`);
  assert.ok(colors.lightness < 0.2);
});

test('image palettes pick a scheme by brightness and keep every text color at AA contrast', () => {
  const run = fixture();
  const cases = [
    ['dark navy', pixels([[12, 18, 48], 800], [[80, 110, 230], 200]), 'auto', 'dark'],
    ['bright pastel', pixels([[247, 232, 240], 700], [[210, 225, 245], 300]), 'auto', 'light'],
    ['neutral gray', pixels([[128, 128, 128], 1000]), 'auto', 'dark'],
    ['pastel forced dark', pixels([[247, 232, 240], 1000]), 'dark', 'dark'],
    ['navy forced light', pixels([[12, 18, 48], 1000]), 'light', 'light'],
    ['saturated red', pixels([[230, 20, 20], 1000]), 'auto', 'dark'],
    ['saturated yellow', pixels([[250, 230, 0], 1000]), 'auto', 'light'],
  ];
  for (const [name, data, mode, scheme] of cases) {
    const result = run(`buildImagePalette(extractImageColors(${data}), '${mode}')`);
    assert.equal(result.scheme, scheme, name);
    const p = result.palette;
    const ratio = (a, b) => run(`contrastRatio('${a}', '${b}')`);
    for (const surface of ['bg', 'panel', 'raised', 'user-bg']) {
      assert.ok(ratio(p.fg, p[surface]) >= 7, `${name}: fg on ${surface}`);
    }
    for (const [text, surface] of [['muted', 'bg'], ['muted', 'panel'], ['accent', 'bg'], ['accent', 'panel'],
      ['accent', 'accent-soft'], ['accent-ink', 'accent'], ['danger', 'bg'], ['warning', 'bg']]) {
      assert.ok(ratio(p[text], p[surface]) >= 4.5, `${name}: ${text} on ${surface} is ${ratio(p[text], p[surface]).toFixed(2)}`);
    }
  }
});

test('color conversion round-trips hex values', () => {
  const run = fixture();
  for (const hex of ['#000000', '#ffffff', '#162a46', '#f29b58', '#7f7f7f']) {
    assert.equal(run(`hslToHex(...rgbToHsl(hexToRgb('${hex}')))`), hex);
  }
});

test('turning off image colors clears the theme and invalidates extraction in flight', async () => {
  const properties = new Map();
  const storage = new Map();
  const dataset = { theme: 'paper' };
  const checkbox = { checked: true };
  let finishDecode;
  let closed = 0;
  const run = fixture({
    dom: {
      root: { dataset, style: {
        setProperty: (name, value) => properties.set(name, value),
        removeProperty: (name) => properties.delete(name),
      } },
      backgroundPalette: checkbox,
    },
    localStorage: { setItem: (key, value) => storage.set(key, value), removeItem: (key) => storage.delete(key) },
    setFormStatus() {},
    createImageBitmap: () => new Promise((resolve) => { finishDecode = () => resolve({
      width: 960, height: 480, close: () => { closed += 1; },
    }); }),
    document: { createElement: () => ({ getContext: () => ({
      drawImage: (_, x, y, width, height) => assert.deepEqual([x, y, width, height], [0, 0, 48, 24]),
      getImageData: () => ({ data: [10, 20, 30, 255] }),
    }) }) },
  });
  const first = run('backgroundBlob = {}; refreshBackgroundPalette()');
  finishDecode();
  await first;
  assert.equal(dataset.backgroundPalette, 'image');
  assert.equal(properties.get('--background-image-color'), '#0a141e');

  const pending = run('refreshBackgroundPalette()');
  run('setBackgroundPalette(false)');
  finishDecode();
  await pending;
  assert.equal(dataset.backgroundPalette, undefined);
  assert.equal(dataset.theme, 'paper');
  assert.equal(properties.has('--background-image-color'), false);
  assert.equal(checkbox.checked, false);
  assert.equal(storage.size, 0);
  assert.equal(closed, 2);
});

test('recent backgrounds are capped, reuse moves an entry to the front, and deleting it keeps the active image', async () => {
  const stored = new Map();
  let active;
  const run = fixture({
    dom: {},
    backgroundTransaction: async (mode, action) => {
      assert.equal(mode, 'readwrite');
      action({ put: (value, key) => stored.set(key, value) });
    },
    showBackground: (blob) => { active = blob; },
    renderBackgroundHistory() {},
    setFormStatus() {},
  }, true);
  for (const id of 'abcdef') await run(`useRecentBackground({ id: '${id}', blob: '${id}' })`);
  assert.deepEqual(Array.from(stored.get('recent'), ({ id }) => id), ['f', 'e', 'd', 'c', 'b']);

  await run("useRecentBackground(recentBackgrounds.find(({ id }) => id === 'c'))");
  assert.deepEqual(Array.from(stored.get('recent'), ({ id }) => id), ['c', 'f', 'e', 'd', 'b']);
  await run("deleteRecentBackground('c')");
  assert.deepEqual(Array.from(stored.get('recent'), ({ id }) => id), ['f', 'e', 'd', 'b']);
  assert.equal(stored.get('current'), 'c');
  assert.equal(active, 'c');
});

test('the applied history entry is stored as the current background', async () => {
  const stored = new Map();
  const run = fixture({
    dom: {},
    backgroundTransaction: async (mode, action) => {
      action({ put: (value, key) => stored.set(key, value) });
    },
    showBackground() {},
    renderBackgroundHistory() {},
    setFormStatus() {},
  }, true);
  await run(`useRecentBackground({ id: 'a', blob: 'a' })`);
  await run(`useRecentBackground({ id: 'b', blob: 'b' })`);
  await run("useRecentBackground(recentBackgrounds.find(({ id }) => id === 'a'))");
  assert.equal(stored.get('current-id'), 'a');
  assert.equal(run('currentBackgroundId'), 'a');
});
