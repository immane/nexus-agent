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
  const children = [];
  const state = { session: 'session', run: 'first', active: true, connected: true };
  const dom = {
    message: { value: 'second task' },
    sideQuestion: { open: false, dataset: {}, showModal() { this.open = true; } },
    sideQuestionAnswer: { textContent: '' },
    sideQuestionUsage: { textContent: '' },
    sideQuestionText: { textContent: '' },
    sideQuestionLoading: { hidden: true },
    sideQuestionCopy: { disabled: true },
  };
  const inbox = {
    items: [{ id: 'draft', input: 'second task' }],
    recent: [{ id: 'first-id', input: 'first task', run: 'first' }],
  };
  const context = vm.createContext({
    state, dom,
    document: { hidden: false },
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
    renderChildCompletion: item => children.push(item),
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
  return { state, dom, inbox, submitted, rendered, messages, children, run: code => vm.runInContext(code, context) };
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

test('an adopted child follow-up is displayed without a fabricated user turn', async () => {
  const f = fixture();
  f.state.active = false;
  f.inbox.recent.push({
    id: 'child-item', kind: 'child-completion', parent_run: 'first',
    run: 'child-run', job_id: 'job-1', status: 'completed',
  });
  f.inbox.items = [];
  await f.run('refreshPromptInbox(state.session, { adopt: true })');
  assert.equal(f.state.run, 'child-run');
  assert.equal(f.state.active, true);
  assert.deepEqual(f.messages, []);
  assert.equal(f.children.length, 1);
  assert.equal(f.children[0].job_id, 'job-1');
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
  assert.equal(f.dom.sideQuestionText.textContent, 'why?');
  assert.equal(f.dom.sideQuestion.dataset.state, 'done');
  assert.equal(f.dom.sideQuestionLoading.hidden, true);
  assert.equal(f.dom.sideQuestionCopy.disabled, false);
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

test('child completion renders as a helper card through the safe Markdown path', () => {
  const functions = source.slice(source.indexOf('function renderChildCompletion('), source.indexOf('function renderHistory('))
    + source.slice(source.indexOf('const DELEGATE_ROLES'), source.indexOf('function finishDelegateTool('));
  const rendered = [];
  const markdown = [];
  const state = { delegateJobs: new Map([['job-1', { role: 'explore', task: 'find the parser', state: 'running' }]]) };
  const context = vm.createContext({
    state,
    node(tag, css, text) {
      return { tag, css, text, dataset: {}, children: [], append(...children) { this.children.push(...children); } };
    },
    appendNode: element => rendered.push(element),
    renderMarkdown: (element, text) => markdown.push(text),
    renderHelperStatus() {},
    formatCompactTokens: value => `${value}`,
  });
  vm.runInContext(functions, context);
  vm.runInContext(`renderChildCompletion({job_id: 'job-1', status: 'completed', text: '<script>data</script>', input_tokens: 12, output_tokens: 3})`, context);
  assert.equal(rendered.length, 1);
  assert.equal(rendered[0].tag, 'details');
  assert.equal(rendered[0].dataset.status, 'completed');
  const summary = rendered[0].children[0];
  assert.equal(summary.children[1].text, 'Explore · find the parser');
  assert.equal(summary.children[2].text, 'Completed');
  assert.deepEqual(markdown, ['<script>data</script>']);
  assert.equal(state.delegateJobs.get('job-1').state, 'completed');
  assert.match(rendered[0].children[1].children[1].text, /Helper usage 12 in · 3 out/);
  vm.runInContext(`renderChildCompletion({job_id: 'job-1', status: 'completed', text: 'again'})`, context);
  assert.equal(rendered.length, 1, 'a job result renders once');
});

test('/btw failures show an error state and keep the draft for another try', async () => {
  const f = fixture();
  f.dom.message.value = '/btw why?';
  f.run("api = async () => { throw new Error('provider unavailable'); }");
  await f.run('sendMessage()');
  assert.equal(f.dom.sideQuestion.open, true);
  assert.equal(f.dom.sideQuestion.dataset.state, 'error');
  assert.equal(f.dom.sideQuestionAnswer.textContent, 'Could not answer: provider unavailable');
  assert.equal(f.dom.sideQuestionLoading.hidden, true);
  assert.equal(f.dom.sideQuestionCopy.disabled, true);
  assert.equal(f.dom.message.value, '/btw why?');
});
