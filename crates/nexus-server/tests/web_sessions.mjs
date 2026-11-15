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
  const storage = new Map();
  const state = {
    connected: true,
    active: false,
    submitting: false,
    connecting: false,
    historyLoading: false,
    runTerminal: false,
    inFlightUnknown: false,
    cancelling: false,
    pendingApprovalSeen: false,
    lastOutcome: null,
    session: null,
    sessionTitle: '',
    sessionProjectId: null,
    selection: { label: 'Demo script' },
    projects: { enabled: true },
    projectChoices: [
      { id: 'proj-a', label: 'Alpha', path: '/alpha' },
      { id: 'proj-b', label: 'Beta', path: '/beta' },
    ],
    projectSelection: null,
    collapsedProjects: new Set(),
    savedSessions: [],
    savedPersistence: true,
    approvals: new Map(),
  };
  const dom = {
    sidebarTree: new Element('div'),
    treeCount: new Element('span'),
    addProjectSidebar: new Element('button'),
  };
  const context = vm.createContext({
    state,
    dom,
    localStorage: {
      getItem: (key) => storage.has(key) ? storage.get(key) : null,
      setItem: (key, value) => storage.set(key, String(value)),
      removeItem: (key) => storage.delete(key),
    },
    document: { createElement: (tag) => new Element(tag) },
  });
  vm.runInContext(`
    const node = ${source.slice(source.indexOf('function node('), source.indexOf('function clip('))}
    ${source.slice(source.indexOf('const SESSION_INDEX_KEY'), source.indexOf('function rememberCurrentSession('))}
    ${source.slice(source.indexOf('function describeModelLabel('), source.indexOf('function updateModelDisplay('))}
    ${source.slice(source.indexOf('function selectedOutcome('), source.indexOf('function outcomeTone('))}
    ${source.slice(source.indexOf('function currentSessionStateLabel('), source.indexOf('function syncCurrentSessionTree('))}
    ${source.slice(source.indexOf('const COLLAPSED_PROJECTS_KEY'), source.indexOf('function setFormStatus('))}
  `, context);
  return {
    state,
    dom,
    storage,
    run: () => vm.runInContext('renderSidebarTree()', context),
    setIndex: (index) => storage.set('nexus.sessions', JSON.stringify(index)),
  };
}

test('project tree renders projects with their sessions nested underneath', () => {
  const { state, dom, run, setIndex } = fixture();
  state.connected = false;
  run();
  assert.match(dom.sidebarTree.textContent, /Connect to load projects/);
  assert.equal(dom.treeCount.textContent, '0');

  state.connected = true;
  state.savedSessions = [
    { session: 'sess-b', title: 'Second task', messages: 4, active: false },
    { session: 'sess-a', title: 'First task', messages: 9, active: false },
  ];
  state.session = 'sess-a';
  state.sessionProjectId = 'proj-a';
  setIndex({ 'sess-a': { project: 'proj-a' }, 'sess-b': { project: 'proj-b' } });
  run();

  assert.equal(dom.treeCount.textContent, '2');
  const projects = dom.sidebarTree.querySelectorAll('.tree-project');
  assert.equal(projects.length, 2);
  const [alpha, beta] = projects;
  assert.match(alpha.textContent, /Alpha/);
  assert.match(beta.textContent, /Beta/);
  const [first] = alpha.querySelectorAll('.tree-session');
  assert.match(first.textContent, /First task/);
  assert.equal(first.dataset.state, 'current');
  const [second] = beta.querySelectorAll('.tree-session');
  assert.match(second.textContent, /Second task/);
  assert.equal(second.dataset.state, 'idle');
});

test('projects collapse without losing session entries', () => {
  const { state, dom, run, setIndex } = fixture();
  state.savedSessions = [{ session: 'sess-a', title: 'First task', messages: 9, active: false }];
  setIndex({ 'sess-a': { project: 'proj-a' } });
  state.collapsedProjects.add('project:proj-a');
  run();

  const [toggle] = dom.sidebarTree.querySelectorAll('.tree-project-toggle');
  const [panel] = dom.sidebarTree.querySelectorAll('.tree-sessions');
  assert.equal(toggle['aria-expanded'], 'false');
  assert.equal(panel.hidden, true);
  assert.match(panel.textContent, /First task/);
});

test('sessions without a project stay in an unassigned group', () => {
  const { state, dom, run } = fixture();
  state.savedSessions = [{ session: 'sess-c', title: 'Loose task', messages: 2, active: false }];
  run();

  const projects = dom.sidebarTree.querySelectorAll('.tree-project');
  assert.equal(projects.length, 3);
  assert.match(projects[2].textContent, /No project/);
  assert.match(projects[2].textContent, /Loose task/);
});

test('disabled history keeps the current session visible and explains the setting', () => {
  const { state, dom, run } = fixture();
  state.savedPersistence = false;
  state.session = 'sess-live';
  state.sessionTitle = 'Live task';
  state.sessionProjectId = 'proj-a';
  run();

  assert.match(dom.sidebarTree.textContent, /Live task/);
  assert.match(dom.sidebarTree.textContent, /--session-store/);
});

test('saved actions disable during live work', () => {
  const { state, dom, run, setIndex } = fixture();
  state.savedSessions = [{ session: 'sess-a', title: 'Task', messages: 2, active: false }];
  setIndex({ 'sess-a': { project: 'proj-a' } });
  run();
  const [item] = dom.sidebarTree.querySelectorAll('.tree-session');
  assert.equal(item.children[0].disabled, false);
  assert.equal(item.children[1].disabled, false);
  state.active = true;
  // updateControls applies the busy state; render alone keeps the initial state.
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const start = source.lastIndexOf('  const savedBusy = state.active');
  const end = source.indexOf('  for (const record of state.approvals.values())');
  const context = vm.createContext({ state, dom });
  vm.runInContext(source.slice(start, end), context);
  assert.equal(item.children[0].disabled, true);
  assert.equal(item.children[1].disabled, true);
});
