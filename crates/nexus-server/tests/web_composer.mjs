// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

test('composer activity follows run lifecycle, including subsequent submissions', () => {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const state = { connected: true, active: false, submitting: false, runTerminal: false, inFlightUnknown: false };
  const dom = { composer: { dataset: {} }, composerActivity: { hidden: true } };
  const context = vm.createContext({ state, dom });
  vm.runInContext(source.slice(source.indexOf('function updateComposerActivity('), source.indexOf('function updateControls(')), context);
  const check = (changes, running) => {
    Object.assign(state, changes);
    vm.runInContext('updateComposerActivity()', context);
    assert.equal(dom.composer.dataset.activity, running ? 'running' : 'idle');
    assert.equal(dom.composerActivity.hidden, !running);
  };
  check({}, false);
  check({ submitting: true }, true);
  check({ submitting: false, active: true }, true);
  check({ cancelling: true }, true);
  check({ runTerminal: true, active: false }, false);
  check({ submitting: true }, true);
  check({ connected: false }, false);
  check({ connected: true, inFlightUnknown: true }, false);
});
