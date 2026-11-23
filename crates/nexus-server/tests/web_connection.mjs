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

test('desktop login clears the fragment before exchanging the one-use code', async () => {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const code = 'c'.repeat(64);
  const calls = [];
  const replacements = [];
  const context = vm.createContext({
    URLSearchParams,
    TOKEN_PATTERN: /^[a-f\d]{64}$/i,
    location: { hash: `#nexus-login=${code}`, pathname: '/', search: '' },
    history: { replaceState: (...args) => replacements.push(args) },
    api: async (...args) => {
      calls.push(args);
      return { token: 'd'.repeat(64) };
    },
  });
  const start = source.indexOf('async function redeemDesktopLoginCode()');
  const end = source.indexOf('void (async () => {', start);
  vm.runInContext(source.slice(start, end), context);

  const exchange = vm.runInContext('redeemDesktopLoginCode()', context);
  assert.deepEqual(replacements, [[null, '', '/']]);
  assert.equal(await exchange, 'd'.repeat(64));
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [[
    '/desktop-login',
    { method: 'POST', credential: '', body: { code } },
  ]]);
});

test('malformed desktop login fragments are removed without an API request', async () => {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  let requested = false;
  const context = vm.createContext({
    URLSearchParams,
    TOKEN_PATTERN: /^[a-f\d]{64}$/i,
    location: { hash: '#nexus-login=invalid', pathname: '/', search: '' },
    history: { replaceState() {} },
    api: async () => { requested = true; },
  });
  const start = source.indexOf('async function redeemDesktopLoginCode()');
  const end = source.indexOf('void (async () => {', start);
  vm.runInContext(source.slice(start, end), context);
  assert.equal(await vm.runInContext('redeemDesktopLoginCode()', context), '');
  assert.equal(requested, false);
});
