import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

test('long histories load the latest page and prioritize older important records', async () => {
  const paths = [];
  const context = vm.createContext({
    api: async (path) => {
      paths.push(path);
      if (paths.length === 1) return { last_sequence: 700, next_before: 601, items: [{ sequence: 700 }] };
      if (paths.length === 2) return { next_before: 1, items: [{ sequence: 4 }, { sequence: 2 }] };
      return { next_before: 3, items: [{ sequence: 3 }, { sequence: 4, content: 'full result' }] };
    },
  });
  vm.runInContext(source.slice(source.indexOf('async function loadHistoryPreview('),
    source.indexOf('function renderHistoricalAssistant(')), context);
  const preview = await vm.runInContext(`loadHistoryPreview('/history')`, context);
  assert.deepEqual(paths, ['/history?before=latest&limit=100', '/history?before=601&limit=100&view=important']);
  assert.deepEqual(Array.from(preview.items, item => item.sequence), [2, 4, 700]);
  assert.equal(preview.cursor, 601, 'hidden tool details remain available from the recent boundary');
  const result = await vm.runInContext(`(async () => {
    const page = await loadOlderHistory('/history', 5, 1, false);
    return mergeHistoryItems([{ sequence: 4 }, { sequence: 9 }], page.items);
  })()`, context);
  assert.equal(paths.at(-1), '/history?before=5&limit=100');
  assert.deepEqual(Array.from(result, item => item.sequence), [3, 4, 9]);
  assert.equal(result[1].content, 'full result');
});

function fixture(respond) {
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
  const notices = [];
  let release;
  let failure;
  const context = vm.createContext({
    state, dom,
    api: async (path, options) => {
      requests.push({ path, ...options });
      if (release) await release.promise;
      if (failure) throw failure;
      if (respond) return respond(path, requests.length, ApiError);
      return { session: 'saved' };
    },
    savedSessionMeta: () => ({}), rememberSession() {}, rememberCurrentSession() {},
    stopStream() {}, resetRunState() {}, ensureProjectExpanded() {}, renderSidebarTree() {},
    updateSessionDisplay() {}, populateModels() {}, loadSessionHistory: async () => {},
    refreshSavedSessions: async () => {}, setRunStatus() {}, addNotice: (_, message) => notices.push(message),
    crypto: { randomUUID: () => 'submission-1' },
    startDisplayingRun: (run) => { state.run = run; },
    updateComposerActivity() {}, updateModelDisplay() {}, updateProjectDisplay() {},
    updateConfigBusy() {},
    updateProjectFormControls() {},
    updateSidebarProjectControls() {},
    window: { confirm() { throw new Error('Unexpected confirmation'); } },
  });
  const ApiError = vm.runInContext(
    `${source.slice(source.indexOf('class ApiError'), source.indexOf('async function api('))}\nApiError`,
    context,
  );
  for (const [start, end] of [
    ['async function openSavedSession(', 'function resetRunState('],
    ['async function ensureSession(', '/* Sending */'],
    ['async function sendMessage(', 'function appendUserMessage('],
  ]) vm.runInContext(source.slice(source.indexOf(start), source.indexOf(end)), context);
  return {
    state, dom, requests, notices, run: (code) => vm.runInContext(code, context),
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

test('sending to an expired idle session restores the same settings and retries the same submission', async () => {
  const f = fixture((_, attempt, ApiError) => {
    if (attempt === 1) throw new ApiError(404, 'unknown session');
    if (attempt === 2) return { session: 'old', delegation: false };
    return { reply: 'accepted', run: 'next-run' };
  });
  Object.assign(f.state, { sessionModel: 'fast', teamwork: false, sessionDelegation: false });

  await f.run('sendMessage()');

  assert.deepEqual(f.requests.map(({ path }) => path), [
    '/sessions/old/runs', '/sessions/old/restore', '/sessions/old/runs',
  ]);
  assert.deepEqual(JSON.parse(JSON.stringify(f.requests[1].body)), {
    provider: 'local', model: 'fast', project: 'project-a', delegation: false,
  });
  assert.equal(f.requests[2].body, f.requests[0].body);
  assert.equal(f.requests[2].body.submission_id, 'submission-1');
  assert.equal(f.state.session, 'old');
  assert.equal(f.state.run, 'next-run');
  assert.equal(f.dom.message.value, '');
  assert.deepEqual(f.notices, []);
});

for (const { name, fail, requests, message, unknown = false, saved = true } of [
  { name: 'missing saved history', fail: (attempt, E) => new E(404,
    attempt === 1 ? 'unknown session' : 'saved session was not found'),
  requests: 2, message: /saved session was not found/ },
  { name: 'failed restore connection', fail: (attempt, E) => attempt === 1
    ? new E(404, 'unknown session') : new Error('offline'),
  requests: 2, message: /Could not restore the session/ },
  { name: 'disabled persistence', fail: (_, E) => new E(404, 'unknown session'),
    requests: 1, message: /Copy your draft before starting a new session/, saved: false },
  { name: 'a second expiration', fail: (attempt, E) => attempt === 2
    ? null : new E(404, 'unknown session'),
  requests: 3, message: /unknown session/ },
  { name: 'an unrelated 404', fail: (_, E) => new E(404, 'unknown route'),
    requests: 1, message: /unknown route/ },
  { name: 'an uncertain submission', fail: () => new Error('offline'),
    requests: 1, message: /Do not resend blindly/, unknown: true },
  { name: 'an uncertain retry', fail: (attempt, E) => {
    if (attempt === 1) return new E(404, 'unknown session');
    return attempt === 3 ? new Error('offline') : null;
  }, requests: 3, message: /Do not resend blindly/, unknown: true },
]) {
  test(`${name} leaves the draft intact without further submissions`, async () => {
    const f = fixture((_, attempt, ApiError) => {
      const error = fail(attempt, ApiError);
      if (error) throw error;
      return { session: 'old' };
    });
    Object.assign(f.state, { sessionModel: 'fast', savedPersistence: saved });
    await f.run('sendMessage()');

    assert.equal(f.requests.length, requests);
    assert.equal(f.dom.message.value, 'draft');
    assert.equal(f.state.session, 'old');
    assert.equal(f.state.inFlightUnknown, unknown);
    assert.equal(f.state.submitting, false);
    assert.match(f.notices[0], message);
  });
}

test('sidebar width stays within its limits and leaves half the window for the conversation', () => {
  const context = vm.createContext({});
  vm.runInContext(source.slice(source.indexOf('const SIDEBAR_WIDTH_KEY'), source.indexOf('function viewportCssWidth(')), context);
  const clamp = (width, viewport) => vm.runInContext(`clampSidebarWidth(${width}, ${viewport})`, context);
  assert.equal(clamp(379.4, 1440), 379);
  assert.equal(clamp(100, 1440), 200);
  assert.equal(clamp(900, 1440), 480);
  assert.equal(clamp(450, 800), 400);
  assert.equal(clamp(300, 300), 200);
  assert.equal(clamp(NaN, 1440), null);
});
