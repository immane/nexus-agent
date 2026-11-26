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

function composerFixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const context = vm.createContext({});
  vm.runInContext(source.slice(source.indexOf('function formatCompactTokens('), source.indexOf('function icon(')), context);
  vm.runInContext(source.slice(source.indexOf('function usageBreakdown('), source.indexOf('function usageChip(')), context);
  return (code) => vm.runInContext(code, context);
}

test('compact token counts match chip labels', () => {
  const run = composerFixture();
  assert.equal(run('formatCompactTokens(656)'), '656');
  assert.equal(run('formatCompactTokens(57285)'), '57.3K');
  assert.equal(run('formatCompactTokens(1000000)'), '1M');
  assert.equal(run('formatCompactTokens(null)'), '—');
});

test('usage breakdown follows each protocol cache convention and keeps unknowns unknown', () => {
  const run = composerFixture();
  const chat = run(`usageBreakdown({ input: 56629, output: 656, cacheRead: 54528, cacheWrite: null }, 'chat_completions')`);
  assert.equal(chat.total, 57285);
  assert.equal(chat.uncached, 2101);
  assert.equal(Math.round(chat.hitRate * 1000) / 10, 96.3);
  const anthropic = run(`usageBreakdown({ input: 100, output: 10, cacheRead: 900, cacheWrite: 0 }, 'anthropic_messages')`);
  assert.equal(anthropic.total, 1010);
  assert.equal(anthropic.uncached, 100);
  assert.equal(anthropic.hitRate, 0.9);
  assert.equal(run(`usageBreakdown({ input: null, output: null }, 'chat_completions')`), null);
  assert.equal(run(`usageBreakdown({ input: 10, output: 2, cacheRead: null }, 'chat_completions')`).hitRate, null);
});
