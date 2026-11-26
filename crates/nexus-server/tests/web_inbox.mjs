import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture() {
  const functions = source.slice(source.indexOf('async function refreshPromptInbox('), source.indexOf('function appendUserMessage('));
  const submitted = [];
  const rendered = [];
  const messages = [];
  const state = { session: 'session', run: 'first', active: true, connected: true };
  const dom = {
    message: { value: 'second task' },
    sideQuestion: { open: false, showModal() { this.open = true; } },
    sideQuestionAnswer: { textContent: '' },
    sideQuestionUsage: { textContent: '' },
  };
  const inbox = {
    items: [{ id: 'draft', input: 'second task' }],
    recent: [{ id: 'first-id', input: 'first task', run: 'first' }],
  };
  const context = vm.createContext({
    state, dom,
    crypto: { randomUUID: () => 'draft' },
    api: async (path, options) => {
      if (options?.method === 'POST') {
        submitted.push(options.body);
        if (path.endsWith('/side-question')) return { answer: 'An aside.', usage: {} };
        if (options.body.delivery === 'steer') return { reply: 'steering', id: 'draft' };
        return { reply: 'queued', id: 'draft' };
      }
      return inbox;
    },
    renderPromptInbox: input => rendered.push(input),
    appendUserMessage: text => messages.push(text),
    ensureSession: async () => {},
    attachStream: async () => {},
    stopStream() {}, resetRunState() {}, setSessionTitleFromInput() {},
    updateControls() {}, setRunStatus() {}, updateSessionDisplay() {},
    addNotice() { assert.fail('unexpected notice'); },
    setTimeout,
    renderMarkdown: (element, answer) => { element.textContent = answer; },
  });
  vm.runInContext(functions, context);
  return { state, dom, inbox, submitted, rendered, messages, run: code => vm.runInContext(code, context) };
}

test('sending while active queues without displaying the waiting prompt as a user turn', async () => {
  const f = fixture();
  await f.run('sendMessage()');
  assert.equal(f.submitted.length, 1);
  assert.equal(f.submitted[0].delivery, 'queue');
  assert.equal(f.submitted[0].submission_id, 'draft');
  assert.equal(f.dom.message.value, '');
  assert.equal(f.state.run, 'first');
  assert.equal(f.rendered[0].items[0].input, 'second task');
  assert.deepEqual(f.messages, []);
});

test('the next accepted inbox run is adopted once without submitting it again', async () => {
  const f = fixture();
  f.state.active = false;
  f.inbox.recent.push({ id: 'draft', input: 'second task', run: 'second' });
  f.inbox.items = [];
  await f.run('refreshPromptInbox(state.session, { adopt: true })');
  await f.run('refreshPromptInbox(state.session, { adopt: true })');
  assert.equal(f.state.run, 'second');
  assert.equal(f.state.active, true);
  assert.deepEqual(f.messages, ['second task']);
  assert.equal(f.submitted.length, 0);
});

test('/btw opens a separate answer without queueing or adding a user turn', async () => {
  const f = fixture();
  f.dom.message.value = '/btw why?';
  await f.run('sendMessage()');
  assert.equal(f.submitted.length, 1);
  assert.equal(f.submitted[0].question, 'why?');
  assert.equal(f.submitted[0].delivery, undefined);
  assert.equal(f.dom.sideQuestion.open, true);
  assert.equal(f.dom.sideQuestionAnswer.textContent, 'An aside.');
  assert.equal(f.dom.sideQuestionUsage.textContent, 'Usage not reported');
  assert.equal(f.state.run, 'first');
  assert.deepEqual(f.messages, []);
});

test('the explicit Steer action targets the current run without adding a premature user turn', async () => {
  const f = fixture();
  await f.run('sendMessage({ steer: true })');
  assert.equal(f.submitted[0].delivery, 'steer');
  assert.equal(f.submitted[0].run, 'first');
  assert.equal(f.dom.message.value, '');
  assert.deepEqual(f.messages, []);
});
