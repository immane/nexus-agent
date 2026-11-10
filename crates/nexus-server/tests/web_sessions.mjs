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
    this.disabled = false;
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  setAttribute(name, value) { this[name] = value; }
  querySelectorAll(selector) {
    const tag = selector.replace(/^\./, '');
    const found = [];
    const visit = (element) => {
      for (const child of element.children) {
        if (child.tagName === tag || child.className === tag) found.push(child);
        visit(child);
      }
    };
    visit(this);
    return found;
  }
  set textContent(text) { this.data = text; this.children = []; }
  get textContent() { return this.data + this.children.map((child) => child.textContent).join(''); }
}

function fixture() {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const state = {
    connected: true,
    persistence: true,
    active: false,
    submitting: false,
    connecting: false,
    historyLoading: false,
    session: null,
    savedSessions: [],
    savedPersistence: true,
  };
  const dom = {
    savedSessions: new Element('div'),
    savedCount: new Element('span'),
    savedNote: new Element('p'),
  };
  const context = vm.createContext({
    state,
    dom,
    document: { createElement: (tag) => new Element(tag) },
  });
  vm.runInContext(`
    const node = ${source.slice(source.indexOf('function node('), source.indexOf('function clip('))}
    ${source.slice(source.indexOf('function renderSavedSessions('), source.indexOf('async function deleteSavedSession('))}
  `, context);
  return { state, dom, run: () => vm.runInContext('renderSavedSessions()', context) };
}

test('saved sessions render disabled, empty, and populated states', () => {
  const { state, dom, run } = fixture();
  run();
  assert.match(dom.savedSessions.textContent, /No saved sessions/);
  assert.equal(dom.savedCount.textContent, '0');

  state.savedPersistence = false;
  run();
  assert.match(dom.savedSessions.textContent, /--session-store/);

  state.savedPersistence = true;
  state.savedSessions = [
    { session: 'sess-b', title: 'Second task', messages: 4, active: false },
    { session: 'sess-a', title: 'First task', messages: 9, active: true },
  ];
  state.session = 'sess-a';
  run();
  assert.equal(dom.savedCount.textContent, '2');
  assert.equal(dom.savedSessions.children.length, 2);
  assert.equal(dom.savedSessions.children[0].dataset.state, 'idle');
  assert.equal(dom.savedSessions.children[1].dataset.state, 'current');
  assert.match(dom.savedSessions.children[1].textContent, /First task/);
});

test('saved actions disable during live work', () => {
  const { state, dom, run } = fixture();
  state.savedSessions = [{ session: 'sess-a', title: 'Task', messages: 2, active: false }];
  run();
  const [item] = dom.savedSessions.children;
  assert.equal(item.children[0].disabled, false);
  assert.equal(item.children[1].disabled, false);
  state.active = true;
  // updateControls applies the busy state; render alone keeps the initial state.
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const start = source.indexOf('  const savedBusy = state.active');
  const end = source.indexOf('  for (const record of state.approvals.values())');
  const context = vm.createContext({ state, dom });
  vm.runInContext(source.slice(start, end), context);
  assert.equal(item.children[0].disabled, true);
  assert.equal(item.children[1].disabled, true);
});
