// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

function fixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const context = vm.createContext({ state: { lastUsage: null } });
  vm.runInContext(source.slice(
    source.indexOf('function formatTokenCount('),
    source.indexOf('function updateUsage('),
  ), context);
  return { run: (code) => vm.runInContext(code, context) };
}

test('usage counts render in thousands with at most two decimals', () => {
  const { run } = fixture();
  for (const [input, expected] of [
    [0, '0k'],
    [7, '0.01k'],
    [191, '0.19k'],
    [1000, '1k'],
    [1200, '1.2k'],
    [1234, '1.23k'],
    [12345, '12.35k'],
    [123456, '123.46k'],
  ]) {
    assert.equal(run(`formatTokenCount(${input})`), expected);
  }
});

test('usage line formats input, output, and cache counters consistently', () => {
  const { run } = fixture();
  const line = run(`state.lastUsage = {
    input: 12345,
    output: 678,
    cacheRead: 1200,
    cacheWrite: 0,
  }; usageParts().join(' / ')`);
  assert.equal(line, '12.35k in / 0.68k out / 1.2k cache read / 0k cache write');
});

test('missing usage fields are omitted', () => {
  const { run } = fixture();
  const line = run(`state.lastUsage = { input: null, output: 2000 }; usageParts().join(' / ')`);
  assert.equal(line, '2k out');
});
