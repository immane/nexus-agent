import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture() {
  const state = { connected: true, connectGeneration: 1, projects: { enabled: true }, projectChoices: [] };
  const dom = Object.fromEntries(['projectForm', 'projectName', 'projectPath', 'chooseProjectFolder', 'addProject', 'projectStatus', 'project'].map((name) => [name, {
    value: '', dataset: {}, listeners: {},
    addEventListener(name, listener) { this.listeners[name] = listener; },
    setAttribute(name, value) { this[name] = value; },
  }]));
  const requests = [];
  let result = { path: '/projects/Folder with spaces', name: 'Folder with spaces' };
  let failure;
  let wait;
  let release;
  const context = vm.createContext({
    state, dom,
    api: async (path, options) => {
      requests.push({ path, ...options });
      if (wait) await wait;
      if (failure) throw new Error(failure);
      return result;
    },
    ApiError: class extends Error {},
    setFormStatus(element, text, tone) { element.textContent = text; element.dataset.state = tone; },
    updateControls() { vm.runInContext('updateProjectFormControls()', context); },
    populateProjects() { state.projectChoices = state.projects.projects || []; },
    renderProjectsList() {}, renderSidebarTree() {}, updateProjectDisplay() {}, updateSessionDisplay() {}, markUnauthorized() {},
  });
  vm.runInContext(source.slice(source.indexOf('function updateProjectFormControls('), source.indexOf('function renderProviderList(')), context);
  vm.runInContext(source.slice(source.indexOf("dom.chooseProjectFolder.addEventListener('click'"), source.indexOf("dom.newProvider.addEventListener('click'")), context);
  vm.runInContext('updateProjectFormControls()', context);
  return {
    state, dom, requests,
    choose: () => vm.runInContext('chooseProjectFolder()', context),
    submit: () => dom.projectForm.listeners.submit({ preventDefault() {} }),
    respond(value) { result = value; },
    fail(message) { failure = message; },
    pause() { wait = new Promise((resolve) => { release = resolve; }); },
    resume() { release(); },
  };
}

test('project UI uses a folder chooser and keeps the project name optional', () => {
  const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
  assert.match(html, /id="project-path" readonly/);
  assert.match(html, /id="choose-project-folder" type="button"/);
  assert.ok(!html.match(/id="project-name"[^>]*required/));
  const f = fixture();
  assert.equal(f.dom.addProject.disabled, true);
  assert.equal(f.dom.chooseProjectFolder.disabled, false);
});

test('folder selection proposes its name without overwriting custom names or uploading files', async () => {
  const f = fixture();
  f.dom.projectName.value = 'My label';
  await f.choose();
  assert.equal(f.requests[0].path, '/projects/pick-folder');
  assert.equal(f.requests[0].method, 'POST');
  assert.deepEqual(Object.keys(f.requests[0].body), []);
  assert.equal(f.dom.projectPath.value, '/projects/Folder with spaces');
  assert.equal(f.dom.projectName.placeholder, 'Folder with spaces');
  assert.equal(f.dom.projectName.value, 'My label');
  assert.equal(f.dom.addProject.disabled, false);
});

test('blank project names are omitted so the server derives the folder name', async () => {
  const f = fixture();
  await f.choose();
  f.dom.projectName.value = '   ';
  f.respond({ enabled: true, project: { id: 'p', name: 'Folder with spaces' }, projects: [{ id: 'p' }] });
  await f.submit();
  const request = f.requests.at(-1);
  assert.equal(request.path, '/projects');
  assert.equal(request.body.path, '/projects/Folder with spaces');
  assert.equal(Object.hasOwn(request.body, 'name'), false);
  assert.equal(f.dom.projectPath.value, '');
  assert.equal(f.dom.projectName.placeholder, 'Uses the folder name');
  assert.equal(f.dom.addProject.disabled, true);
  assert.match(f.dom.projectStatus.textContent, /Added Folder with spaces/);
});

test('cancelled, failed, malformed and stale folder results preserve the current selection', async () => {
  for (const kind of ['cancel', 'failure', 'malformed', 'stale']) {
    const f = fixture();
    f.dom.projectPath.value = '/previous';
    f.dom.projectName.value = 'Previous label';
    if (kind === 'cancel') f.respond({ cancelled: true });
    if (kind === 'failure') f.fail('No desktop');
    if (kind === 'malformed') f.respond({ path: '/new' });
    if (kind === 'stale') f.pause();
    const pending = f.choose();
    if (kind === 'stale') { f.state.connectGeneration++; f.resume(); }
    await pending;
    assert.equal(f.dom.projectPath.value, '/previous');
    assert.equal(f.dom.projectName.value, 'Previous label');
    assert.equal(f.state.projectPicking, false);
    assert.equal(f.dom.chooseProjectFolder.disabled, false);
  }
});

test('picker and project saves cannot overlap or submit twice; errors retain the draft', async () => {
  const f = fixture();
  f.pause();
  const pending = f.choose();
  assert.equal(f.dom.chooseProjectFolder.disabled, true);
  assert.equal(f.dom.addProject.disabled, true);
  assert.equal(f.dom.projectForm['aria-busy'], 'true');
  await f.choose();
  await f.submit();
  assert.equal(f.requests.length, 1);
  f.resume();
  await pending;
  f.dom.projectName.value = 'Custom label';
  f.fail('Cannot save');
  await f.submit();
  assert.equal(f.requests.at(-1).body.name, 'Custom label');
  assert.equal(f.dom.projectName.value, 'Custom label');
  assert.equal(f.dom.projectPath.value, '/projects/Folder with spaces');
  assert.equal(f.state.projectSaving, false);
  assert.equal(f.dom.projectForm['aria-busy'], 'false');
  assert.equal(f.dom.projectStatus.dataset.state, 'error');
});
