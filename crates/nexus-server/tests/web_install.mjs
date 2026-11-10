// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

function installFixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const listeners = {};
  const buttonListeners = {};
  const registrations = [];
  const button = {
    hidden: true,
    disabled: false,
    addEventListener(name, callback) { buttonListeners[name] = callback; },
  };
  const note = { textContent: '' };
  const window = {
    navigator: { standalone: false },
    addEventListener(name, callback) { listeners[name] = callback; },
    matchMedia() {
      return { matches: false, addEventListener() {} };
    },
  };
  const context = vm.createContext({
    window,
    navigator: {
      serviceWorker: {
        register(path, options) {
          registrations.push({ path, options });
          return Promise.resolve();
        },
      },
    },
    dom: { installApp: button, installNote: note },
  });
  const start = source.indexOf('let deferredInstallPrompt');
  const end = source.indexOf('/* Small helpers */');
  assert.ok(start >= 0 && end > start);
  vm.runInContext(source.slice(start, end), context);
  return { button, listeners, buttonListeners, note, registrations };
}

test('install prompt shows the action and hides it after acceptance', async () => {
  const { button, listeners, buttonListeners, note, registrations } = installFixture();
  assert.equal(button.hidden, true);
  assert.match(note.textContent, /browser's Install/);
  assert.equal(registrations.length, 1);
  assert.equal(registrations[0].path, '/sw.js');
  assert.equal(registrations[0].options.scope, '/');

  let prevented = false;
  let prompted = false;
  listeners.beforeinstallprompt({
    preventDefault() { prevented = true; },
    prompt() { prompted = true; },
    userChoice: Promise.resolve({ outcome: 'accepted' }),
  });
  assert.equal(prevented, true);
  assert.equal(button.hidden, false);
  assert.match(note.textContent, /standalone window/);

  await buttonListeners.click();
  assert.equal(prompted, true);
  assert.equal(button.hidden, true);
  assert.match(note.textContent, /Installing Nexus/);
});

test('dismissal and appinstalled leave a truthful note', async () => {
  const { button, listeners, buttonListeners, note } = installFixture();
  listeners.beforeinstallprompt({
    preventDefault() {},
    prompt() {},
    userChoice: Promise.resolve({ outcome: 'dismissed' }),
  });
  await buttonListeners.click();
  assert.equal(button.hidden, true);
  assert.match(note.textContent, /dismissed/);

  listeners.appinstalled();
  assert.equal(button.hidden, true);
  assert.match(note.textContent, /installed/);
});

test('manifest and install assets match the declared contract', () => {
  const manifest = JSON.parse(readFileSync(new URL('../web/manifest.webmanifest', import.meta.url), 'utf8'));
  assert.equal(manifest.name, 'Nexus Agent');
  assert.equal(manifest.start_url, '/');
  assert.equal(manifest.scope, '/');
  assert.equal(manifest.display, 'standalone');
  assert.deepEqual(manifest.icons.map((icon) => icon.sizes), ['192x192', '512x512']);

  for (const size of [192, 512]) {
    const bytes = readFileSync(new URL(`../web/icon-${size}.png`, import.meta.url));
    assert.deepEqual([...bytes.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    assert.equal(bytes.readUInt32BE(16), size);
    assert.equal(bytes.readUInt32BE(20), size);
  }

  const worker = readFileSync(new URL('../web/sw.js', import.meta.url), 'utf8');
  assert.match(worker, /addEventListener\('fetch'/);
  assert.doesNotMatch(worker, /caches\.open/);
});
