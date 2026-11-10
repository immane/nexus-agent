// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

function fixture(blocked = false) {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const values = new Map();
  const localStorage = Object.fromEntries(['getItem', 'setItem', 'removeItem'].map((method) => [method, (key, value) => {
    if (blocked) throw new Error('Storage unavailable');
    if (method === 'getItem') return values.get(key) ?? null;
    if (method === 'setItem') values.set(key, value);
    else if (method === 'removeItem') values.delete(key);
  }]));
  const context = vm.createContext({ localStorage });
  vm.runInContext(`const TOKEN_PATTERN = /^[a-f\\d]{64}$/i;
    ${source.slice(source.indexOf('function readSavedCredential('), source.indexOf('/* Connection */'))}`, context);
  return { values, run: (code) => vm.runInContext(code, context) };
}

test('saved server token survives a reload and can be forgotten', () => {
  const { run, values } = fixture();
  assert.equal(run('readSavedCredential()'), '');
  assert.equal(run(`saveCredential('a'.repeat(64))`), true);
  assert.equal(run('readSavedCredential()'), 'a'.repeat(64));
  assert.equal(values.get('nexus.server-token'), 'a'.repeat(64));
  assert.equal(run(`saveCredential('')`), true);
  assert.equal(run('readSavedCredential()'), '');
  assert.equal(values.size, 0);
});

test('corrupt saved tokens are removed without attempting to use them', () => {
  const { run, values } = fixture();
  for (const value of ['not-a-token', 'a'.repeat(63), 'a'.repeat(65)]) {
    values.set('nexus.server-token', value);
    assert.equal(run('readSavedCredential()'), '');
    assert.equal(values.size, 0);
  }
});

test('blocked storage leaves manual in-memory connection available', () => {
  const { run } = fixture(true);
  assert.equal(run('readSavedCredential()'), '');
  assert.equal(run(`saveCredential('a'.repeat(64))`), false);
  assert.equal(run(`saveCredential('')`), false);
});
