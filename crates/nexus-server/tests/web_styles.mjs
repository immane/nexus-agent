// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const css = readFileSync(new URL('../web/app.css', import.meta.url), 'utf8');
const rules = [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^{}]*)\}/g)]
  .map(([, selectors, body]) => ({ selectors: selectors.trim(), body }));
const rule = (selector) => rules.filter((entry) => entry.selectors === selector).at(-1)?.body ?? '';

test('only dropdown controls render without a border', () => {
  assert.match(rule('select'), /border-color:\s*transparent\s*;/);
  assert.doesNotMatch(css, /border-color:\s*transparent\s*!important/);
});

test('dropdown focus uses a fill cue instead of a thick frame', () => {
  assert.match(rule('select:focus'), /outline:\s*none\s*;/);
  assert.match(rule('select:focus'), /box-shadow:\s*none\s*;/);
  assert.match(rule('select:focus'), /background-color:\s*var\(--accent-soft\)\s*;/);
});

test('framed controls keep their normal border', () => {
  assert.match(rule('button,\nselect,\ninput'), /border:\s*1px solid var\(--line\)\s*;/);
  assert.match(rule('.composer'), /border:\s*1px solid var\(--line\)\s*;/);
  assert.match(rule('.composer:focus-within'), /border-color:\s*var\(--accent\)\s*;/);
});
