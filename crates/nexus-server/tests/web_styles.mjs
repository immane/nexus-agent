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

test('conversation results do not blur the backdrop', () => {
  for (const { selectors, body } of rules) {
    if (/\.(?:tool-card|helper-card|approval-card|notice|run-card)\b/.test(selectors)) {
      assert.doesNotMatch(body, /backdrop-filter\s*:/);
    }
  }
});

test('session loading shows its indicator without a dimmed or blurred overlay', () => {
  const loading = rule('.session-loading');
  assert.match(loading, /display:\s*flex\s*;/);
  assert.doesNotMatch(loading, /(?:background(?:-color)?|backdrop-filter)\s*:/);
});

test('focusing a visually hidden setting cannot scroll the settings dialog out of its frame', () => {
  // `overflow: hidden` boxes remain scrollable by focus; `clip` does not.
  const dialog = rules.filter((entry) => entry.selectors === 'dialog').map((entry) => entry.body).join('\n');
  assert.match(dialog, /overflow:\s*clip\s*;/);
  assert.doesNotMatch(dialog, /overflow:\s*(auto|scroll)\s*;/);
  // Hidden radios are positioned inside their own option, so focus scrolls
  // at most the settings panel that contains them.
  const all = (selector) => rules.filter((entry) => entry.selectors === selector).map((entry) => entry.body).join('\n');
  for (const selector of ['.segmented label', '.settings-content', 'dialog .theme-card']) {
    assert.match(all(selector), /position:\s*relative\s*;/, selector);
  }
});
