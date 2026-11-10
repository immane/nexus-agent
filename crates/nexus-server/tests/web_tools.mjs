// Run with: node --test crates/nexus-server/tests/web_tools.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

// Only the DOM operations used by tool cards; no network or provider is needed.
class Element {
  constructor(tag, text = '') {
    this.tagName = tag;
    this.data = text;
    this.children = [];
    this.dataset = {};
    this.listeners = {};
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  setAttribute() {}
  addEventListener(name, callback) { this.listeners[name] = callback; }
  set textContent(text) { this.data = text; this.children = []; }
  get textContent() { return this.data + this.children.map((child) => child.textContent).join(''); }
}

function fixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const cards = [];
  const context = vm.createContext({
    document: {
      createElement: (tag) => new Element(tag),
      createTextNode: (text) => new Element('#text', text),
    },
    cards,
    flushText() {},
    markStreamingDone() {},
    setRunStatus() {},
    appendNode: (card) => cards.push(card),
    queueText: (node, text) => { node.data += text; },
  });
  vm.runInContext(`
    const MAX_TOOL_CHARS = 64000;
    const HIDDEN_TOOL_NAMES = new Set(['host_read', 'host_list']);
    const state = { tools: new Map() };
    ${source.slice(source.indexOf('function node('), source.indexOf('function setRunStatus('))}
    ${source.slice(source.indexOf('function toolPresentation('), source.indexOf('/* Approvals */'))}
  `, context);
  return { cards, run: (code) => vm.runInContext(code, context) };
}

test('exec starts compact, shows safely quoted argv and keeps complete output', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'exec', tool: 'host_exec', args_preview: JSON.stringify({ argv: ['echo', "it's a test"] }) });`);
  assert.match(cards[0].textContent, /echo 'it'\\''s a test'/);
  assert.equal(cards[0].dataset.kind, 'exec');
  assert.equal(cards[0].open, false);
  assert.equal(cards[0].children[0].children[0].textContent, 'RUN');
  run(`updateToolOutput({ call: 'exec', preview: 'first chunk' });`);
  assert.match(cards[0].textContent, /first chunk/);
  run(`finishTool({ call: 'exec', outcome: { status: 'Succeeded', content: 'final output' } });`);
  assert.equal(cards[0].open, false);
  assert.equal(cards[0].dataset.state, 'ok');
  assert.match(cards[0].textContent, /final output/);
  assert.doesNotMatch(cards[0].textContent, /first chunk/);
});

test('write and patch start compact and render literal content and a proposed diff', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'write', tool: 'host_write', args_preview: JSON.stringify({ path: 'src/main.rs', content: '<script>unsafe()</script>\\nsecond line' }) });`);
  assert.equal(cards[0].dataset.kind, 'write');
  assert.equal(cards[0].open, false);
  assert.equal(cards[0].children[0].children[0].textContent, 'WRITE');
  assert.match(cards[0].textContent, /src\/main.rs/);
  const content = cards[0].children[1].children[0].children[1];
  assert.equal(content.children.length, 2);
  assert.equal(content.children[0].textContent, '<script>unsafe()</script>');
  run(`startTool({ call: 'patch', tool: 'host_patch', args_preview: JSON.stringify({ path: 'test.txt', old_text: 'before', new_text: 'after' }) });`);
  const sections = cards[1].children[1].children;
  const diff = sections[0].children[1];
  assert.equal(cards[1].open, false);
  assert.equal(cards[1].children[0].children[0].textContent, 'EDIT');
  assert.equal(diff.className, 'tool-diff');
  assert.equal(diff.children[0].className, 'tool-diff-line removed');
  assert.equal(diff.children[0].textContent, '- before');
  assert.equal(diff.children[1].className, 'tool-diff-line added');
  assert.equal(diff.children[1].textContent, '+ after');
});

test('diff preserves context, handles insertion/deletion and bounds large previews', () => {
  const { run } = fixture();
  assert.equal(run(`toolPatchDiff('same\\nbefore\\nend', 'same\\nafter\\nend').rows.map(row => row.text).join('\\n')`),
    '  same\n- before\n+ after\n  end');
  assert.equal(run(`toolPatchDiff('', 'new').rows[0].text`), '+ new');
  assert.equal(run(`toolPatchDiff('old', '').rows[0].text`), '- old');
  assert.equal(run(`toolPatchDiff('same', 'same').unchanged`), true);
  assert.equal(run(`toolPatchDiff('x'.repeat(64001), 'new').truncated`), true);
  assert.equal(run(`toolPatchDiff('old', '<script>unsafe()</script>').rows[1].text`), '+ <script>unsafe()</script>');
  assert.equal(run(`toolPresentation('host_write', undefined).kind`), 'write');
  assert.equal(run(`toolPresentation('host_patch', undefined).kind`), 'patch');
});

test('withheld write previews explain the limit and show the result instead', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'large', tool: 'host_write' });`);
  assert.equal(cards[0].children[0].children[0].textContent, 'WRITE');
  const target = cards[0].children[0].children[1];
  assert.equal(target.textContent, 'Preview unavailable');
  assert.equal(target.dataset.unavailable, 'true');
  assert.match(cards[0].textContent, /did not provide an argument preview/);
  run(`finishTool({ call: 'large', outcome: { status: 'Succeeded', content: 'wrote 505 bytes' } });`);
  assert.equal(target.textContent, 'wrote 505 bytes');
  assert.equal(target.dataset.unavailable, undefined);
  assert.equal(target.title, 'wrote 505 bytes');
});

test('withheld exec previews never show stdout as the command', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'exec', tool: 'host_exec' });`);
  run(`finishTool({ call: 'exec', outcome: { status: 'Succeeded', content: 'some stdout' } });`);
  assert.equal(cards[0].children[0].children[1].textContent, 'Command preview unavailable');
  assert.doesNotMatch(cards[0].children[0].textContent, /some stdout/);
});

test('unavailable exec arguments retain raw previews without guessing a command', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'broken', tool: 'host_exec', args_preview: '{"argv":[' });`);
  assert.equal(cards[0].dataset.kind, 'exec');
  assert.match(cards[0].textContent, /Command preview unavailable/);
  assert.match(cards[0].textContent, /Raw arguments · host_exec/);
  assert.match(cards[0].textContent, /\{"argv":\[/);
  run(`startTool({ call: 'custom', tool: 'custom_tool', args_preview: '{"value":42}' });`);
  assert.equal(cards[1].dataset.kind, 'generic');
});

test('runtime tool-name-prefixed previews produce exec and file summaries', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'exec', tool: 'host_exec', args_preview: 'host_exec {"argv":["cat","Cargo.toml"]}' });`);
  assert.match(cards[0].children[0].textContent, /RUNcat Cargo.toml/);
  run(`startTool({ call: 'write', tool: 'host_write', args_preview: 'host_write {"path":"new.txt","content":"hello"}' });`);
  assert.equal(cards[1].dataset.kind, 'write');
  assert.match(cards[1].textContent, /new.txt/);
  run(`startTool({ call: 'mismatch', tool: 'host_exec', args_preview: 'other_tool {"argv":["false"]}' });`);
  assert.match(cards[2].textContent, /Command preview unavailable/);
});

test('manual expansion survives success; failures and missing results stay visible', () => {
  const { cards, run } = fixture();
  run(`startTool({ call: 'manual', tool: 'host_exec', args_preview: '{}' });`);
  cards[0].children[0].listeners.click();
  cards[0].open = true;
  run(`finishTool({ call: 'manual', outcome: { status: 'Succeeded' } });`);
  assert.equal(cards[0].open, true);
  run(`startTool({ call: 'failed', tool: 'host_exec', args_preview: '{}' });`);
  cards[1].open = false;
  cards[1].children[0].listeners.click();
  run(`finishTool({ call: 'failed', outcome: { status: 'TimedOut' } });`);
  assert.equal(cards[1].open, true);
  assert.equal(cards[1].dataset.state, 'busy');
  run(`startTool({ call: 'lost', tool: 'host_exec', args_preview: '{}' }); markUnfinishedTools();`);
  assert.equal(cards[2].dataset.state, 'error');
  assert.equal(cards[2].open, true);
});
