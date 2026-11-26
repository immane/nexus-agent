import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture() {
  const state = {
    connected: true, session: 'old', selection: { provider: 'local', model: 'fast' },
    connecting: false, historyLoading: false, submitting: false, inFlightUnknown: false,
    sessionProvider: 'local', sessionModel: 'slow', sessionProjectId: 'project-a',
    savedPersistence: true, choices: [], projects: { enabled: true },
    projectChoices: [], savedSessions: [], approvals: new Map(),
  };
  const dom = new Proxy({}, { get(target, key) {
    return target[key] ||= {
      dataset: {}, value: 'draft', childElementCount: 1,
      setAttribute(name, value) { this[name] = value; }, querySelectorAll() { return []; },
    };
  } });
  const requests = [];
  let release;
  let failure;
  const context = vm.createContext({
    state, dom,
    api: async (path, options) => {
      requests.push({ path, ...options });
      if (release) await release.promise;
      if (failure) throw failure;
      return { session: 'saved' };
    },
    savedSessionMeta: () => ({}), rememberSession() {}, rememberCurrentSession() {},
    stopStream() {}, resetRunState() {}, ensureProjectExpanded() {}, renderSidebarTree() {},
    updateSessionDisplay() {}, populateModels() {}, loadSessionHistory: async () => {},
    refreshSavedSessions: async () => {}, setRunStatus() {}, addNotice() {},
    updateComposerActivity() {}, updateModelDisplay() {}, updateProjectDisplay() {},
    updateConfigBusy() {},
    updateProjectFormControls() {},
    updateSidebarProjectControls() {},
    window: { confirm() { throw new Error('Unexpected confirmation'); } },
    ApiError: class ApiError extends Error { constructor(status, message) { super(message); this.status = status; } },
  });
  for (const [start, end] of [
    ['async function openSavedSession(', 'function resetRunState('],
    ['async function ensureSession(', '/* Sending */'],
  ]) vm.runInContext(source.slice(source.indexOf(start), source.indexOf(end)), context);
  return {
    state, dom, requests, run: (code) => vm.runInContext(code, context),
    pause() { release = {}; release.promise = new Promise((resolve) => { release.resolve = resolve; }); },
    resume() { release.resolve(); },
    fail() { failure = new Error('offline'); },
  };
}

test('session selection skips confirmation and blocks conflicting actions while loading', async () => {
  const f = fixture();
  f.pause();
  const pending = f.run("openSavedSession('saved')");
  assert.equal(f.state.historyLoading, true);
  assert.equal(f.dom.sessionLoading.hidden, false);
  assert.equal(f.dom.main['aria-busy'], 'true');
  assert.equal(f.dom.send.disabled, true);
  assert.equal(f.dom.newSession.disabled, true);
  await f.run("openSavedSession('another')");
  assert.equal(f.requests.length, 1);
  f.resume();
  await pending;
  assert.equal(f.state.session, 'saved');
  assert.equal(f.state.historyLoading, false);
  assert.equal(f.dom.sessionLoading.hidden, true);
});

test('restore failures clear loading and preserve the current session', async () => {
  const f = fixture();
  f.fail();
  await f.run("openSavedSession('saved')");
  assert.equal(f.state.session, 'old');
  assert.equal(f.dom.sessionLoading.hidden, true);
  assert.equal(f.dom.main['aria-busy'], 'false');
});

test('model selection remains available during a run and changes the next task only', async () => {
  const f = fixture();
  f.state.active = true;
  f.run('updateControls()');
  assert.equal(f.dom.modelTrigger.disabled, false);
  assert.equal(f.dom.model.disabled, false);
  assert.equal(f.state.sessionModel, 'slow');
  f.state.active = false;
  await f.run('ensureSession()');
  assert.equal(f.requests[0].path, '/sessions/old/restore');
  assert.deepEqual(JSON.parse(JSON.stringify(f.requests[0].body)), {
    provider: 'local', model: 'fast', project: 'project-a',
  });
  assert.equal(f.state.session, 'old');
  assert.equal(f.state.sessionModel, 'fast');
  await f.run('ensureSession()');
  assert.equal(f.requests.length, 1);
});

test('failed model switches preserve the binding and missing persistence is explicit', async () => {
  const f = fixture();
  f.fail();
  await assert.rejects(f.run('ensureSession()'), /offline/);
  assert.equal(f.state.sessionModel, 'slow');
  f.state.savedPersistence = false;
  await assert.rejects(f.run('ensureSession()'), /requires --session-store/);
  assert.equal(f.requests.length, 1);
});

test('turning teamwork off is sent as a binding and rebinds an idle saved session', async () => {
  const f = fixture();
  f.state.sessionModel = 'fast';
  f.state.sessionDelegation = true;
  f.state.teamwork = false;
  await f.run('ensureSession()');
  assert.equal(f.requests[0].path, '/sessions/old/restore');
  assert.equal(f.requests[0].body.delegation, false);
  assert.equal(f.state.sessionDelegation, false);
  await f.run('ensureSession()');
  assert.equal(f.requests.length, 1, 'an unchanged binding is not restored again');

  f.state.session = null;
  f.state.projectSelection = { id: 'project-a' };
  await f.run('ensureSession()');
  assert.equal(f.requests[1].path, '/sessions');
  assert.equal(f.requests[1].body.delegation, false);
});
