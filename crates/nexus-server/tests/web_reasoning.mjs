// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

class Element {
  constructor(tag, text = '') {
    this.tagName = tag;
    this.data = text;
    this.children = [];
    this.dataset = {};
    this.listeners = {};
    this.scrollTop = 0;
    this.scrollHeight = 1000;
    this.clientHeight = 100;
  }
  append(...children) { this.children.push(...children); }
  appendData(text) { this.data += text; }
  setAttribute() {}
  addEventListener(name, callback) { this.listeners[name] = callback; }
  get textContent() { return this.data + this.children.map(child => child.textContent).join(''); }
  set textContent(text) { this.data = text; this.children = []; }
}

function fixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const cards = [];
  const frames = new Map();
  let nextFrame = 0;
  const context = vm.createContext({
    document: {
      createElement: tag => new Element(tag),
      createTextNode: text => new Element('#text', text),
    },
    requestAnimationFrame: callback => { frames.set(++nextFrame, callback); return nextFrame; },
    cancelAnimationFrame: id => frames.delete(id),
    appendNode: card => cards.push(card),
    scrollToBottom() {},
    setRunStatus() {},
    addNotice() {},
    renderAssistant() {},
  });
  vm.runInContext(`
    const MAX_ASSISTANT_CHARS = 200000;
    const state = { reasoningItems: new Map(), assistantItems: new Map(), pinned: true, lastSeq: null };
    const pendingText = new Map();
    let textFrame = 0;
    ${source.slice(source.indexOf('function node('), source.indexOf('function setRunStatus('))}
    ${source.slice(source.indexOf('function flushText('), source.indexOf('function appendNode('))}
    ${source.slice(source.indexOf('function handleEvent('), source.indexOf('/* Markdown'))}
    ${source.slice(source.indexOf('function markStreamingDone('), source.indexOf('function appendAssistantText('))}
    ${source.slice(source.indexOf('function scrollReasoning('), source.indexOf('function appendContextNotice('))}
  `, context);
  return {
    cards,
    run: code => vm.runInContext(code, context),
    flush() {
      const callbacks = [...frames.values()];
      frames.clear();
      for (const callback of callbacks) callback();
    },
  };
}

test('live thinking starts collapsed, batches literal text and follows the last line', () => {
  const { cards, run, flush } = fixture();
  run(`appendReasoning({ turn: 't', item: 'r', text: 'first line\\n' });`);
  run(`appendReasoning({ turn: 't', item: 'r', text: '<script>literal</script>' });`);
  flush();
  const card = cards[0];
  const preview = card.children[0].children[1];
  const body = card.children[1];
  assert.equal(cards.length, 1);
  assert.equal(card.open, false);
  assert.equal(preview.textContent, 'first line\n<script>literal</script>');
  assert.equal(body.textContent, preview.textContent);
  assert.equal(body.children.length, 1);
  assert.equal(preview.scrollTop, preview.scrollHeight);
  assert.equal(card.dataset.streaming, 'true');
});

test('expanded thinking follows updates but lets the reader scroll up', () => {
  const { cards, run, flush } = fixture();
  run(`appendReasoning({ turn: 't', item: 'r', text: 'first' });`);
  flush();
  const card = cards[0];
  const body = card.children[1];
  card.open = true;
  card.listeners.toggle();
  flush();
  assert.equal(body.scrollTop, body.scrollHeight);
  body.scrollTop = 100;
  body.listeners.scroll();
  run(`appendReasoning({ turn: 't', item: 'r', text: '\\nsecond' });`);
  flush();
  assert.equal(body.scrollTop, 100);
  assert.equal(body.textContent, 'first\nsecond');
  card.open = false;
  card.listeners.toggle();
  flush();
  card.open = true;
  card.listeners.toggle();
  flush();
  assert.equal(body.scrollTop, body.scrollHeight);
  run('markStreamingDone()');
  assert.equal(card.open, true);
  assert.equal(card.dataset.streaming, 'false');
  assert.equal(card.children[0].children[0].textContent, 'Thoughts');
});

test('thinking is bounded with an explicit warning and independent turn/item keys', () => {
  const { cards, run, flush } = fixture();
  run(`appendReasoning({ turn: 't', item: 'r', text: 'x'.repeat(200001) });`);
  run(`appendReasoning({ turn: 't', item: 'r', text: 'ignored' });`);
  flush();
  assert.equal(cards[0].children[1].textContent.length, 200000);
  assert.equal(cards[0].children[2].hidden, false);
  run(`appendReasoning({ turn: 'next', item: 'r', text: 'next thought' });`);
  flush();
  assert.equal(cards.length, 2);
  assert.equal(cards[0].dataset.streaming, 'false');
  assert.equal(cards[1].open, false);
  assert.equal(cards[1].children[1].textContent, 'next thought');
});

test('reasoning events deduplicate SSE replay and finish with the model response', () => {
  const { cards, run, flush } = fixture();
  const event = JSON.stringify({ seq: 1, kind: 'assistant-reasoning', detail: { turn: 't', item: 'r', text: 'thought' } });
  run(`handleEvent(${event}); handleEvent(${event});`);
  flush();
  assert.equal(cards[0].children[1].textContent, 'thought');
  run(`handleEvent({seq: 2, kind: 'model-request-finished', detail: {}})`);
  assert.equal(cards[0].dataset.streaming, 'false');
});

test('thinking stylesheet retains the single-line preview and bounded expanded panel', () => {
  const css = readFileSync(new URL('../web/app.css', import.meta.url), 'utf8');
  const rules = new Map([...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .map(([, selector, declarations]) => [selector.trim(), declarations]));
  const preview = rules.get('.reasoning-preview') ?? '';
  assert.match(preview, /height:\s*1\.6em\s*;/);
  assert.match(preview, /line-height:\s*1\.6\s*;/);
  assert.match(preview, /overflow:\s*hidden\s*;/);
  assert.match(preview, /white-space:\s*pre-wrap\s*;/);
  assert.match(preview, /overflow-wrap:\s*anywhere\s*;/);
  assert.match(rules.get('.reasoning-card[open] .reasoning-preview') ?? '', /display:\s*none\s*;/);
  const body = rules.get('.reasoning-body') ?? '';
  assert.match(body, /max-height:\s*320px\s*;/);
  assert.match(body, /overflow-y:\s*auto\s*;/);
  assert.match(body, /white-space:\s*pre-wrap\s*;/);
  assert.match(body, /overflow-wrap:\s*anywhere\s*;/);
});
