// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

test('settings categories select one panel, retain form drafts and reset scrolling', () => {
  const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const ids = [...html.matchAll(/data-settings-section="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(ids.length, 7);
  assert.equal(new Set(ids).size, ids.length);
  const dom = {
    settingsButtons: ids.map((id) => ({
      dataset: { settingsSection: id },
      attributes: {},
      setAttribute(key, value) { this.attributes[key] = value; },
      removeAttribute(key) { delete this.attributes[key]; },
    })),
    settingsPanels: ids.map((id) => {
      assert.ok(html.includes(`id="${id}"`));
      assert.ok(html.includes(`aria-controls="${id}"`));
      return { id, hidden: id !== 'appearance-panel', draft: 'unsaved form value' };
    }),
    settingsContent: { scrollTop: 200 },
  };
  const context = vm.createContext({ dom });
  vm.runInContext(source.slice(source.indexOf('function selectSettingsSection('), source.indexOf('/* Wiring */')), context);
  for (const id of ids) {
    dom.settingsContent.scrollTop = 200;
    vm.runInContext(`selectSettingsSection(${JSON.stringify(id)})`, context);
    assert.deepEqual(dom.settingsPanels.filter((panel) => !panel.hidden).map((panel) => panel.id), [id]);
    assert.deepEqual(dom.settingsButtons.filter((button) => button.attributes['aria-current'] === 'page')
      .map((button) => button.dataset.settingsSection), [id]);
    assert.equal(dom.settingsContent.scrollTop, 0);
    assert.ok(dom.settingsPanels.every((panel) => panel.draft === 'unsaved form value'));
  }
  vm.runInContext(`selectSettingsSection('unknown')`, context);
  assert.equal(dom.settingsPanels.find((panel) => !panel.hidden).id, ids.at(-1));
});

function thinkingFixture(config = {}) {
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
  const dom = Object.fromEntries([...source.slice(0, source.indexOf('const state =')).matchAll(/(\w+): document.querySelector\(/g)]
    .map(([, key]) => [key, {
      value: '', dataset: {}, options: [], disabled: false, textContent: '', listeners: {},
      addEventListener(type, listener) { this.listeners[type] = listener; },
      querySelectorAll() { return []; },
    }]));
  const sections = [...html.matchAll(/data-settings-section="([^"]+)"/g)].map(([, id]) => id);
  dom.settingsPanels = sections.map((id) => ({ id, hidden: id !== 'appearance-panel' }));
  dom.settingsButtons = sections.map((id) => ({
    dataset: { settingsSection: id }, setAttribute() {}, removeAttribute() {},
  }));
  dom.settingsContent = { scrollTop: 0 };
  const state = {
    connected: true, credential: 'test-token', thinkingSaving: false, approvals: new Map(),
    config: {
      providers: [{ id: 'local', display_name: 'Local', adapter: 'direct', protocol: 'chat_completions',
        endpoint: 'http://localhost:11434/v1', credential: { env: 'LOCAL_KEY' }, default_model: 'qwen' }],
      models: [{ id: 'fast', provider: 'local', name: 'qwen' }],
      ...config,
    },
  };
  const requests = [];
  let failure = null;
  let release;
  let wait = null;
  const context = vm.createContext({
    dom, state,
    confirm: () => true,
    fetch: async (path, options) => {
      const body = options.body === undefined ? undefined : JSON.parse(options.body);
      requests.push({ path, ...options, body });
      if (wait) await wait;
      if (failure) return { ok: false, status: failure.status, json: async () => ({ error: failure.message }) };
      const summary = JSON.parse(JSON.stringify(state.config));
      if (options.method === 'DELETE') {
        const id = decodeURIComponent(path.split('/').at(-1));
        summary.providers = summary.providers.filter((provider) => provider.id !== id);
      } else if (path === '/config/thinking') {
        if (body.task_reasoning_effort === 'inherit') delete summary.task_reasoning_effort;
        else summary.task_reasoning_effort = body.task_reasoning_effort;
      } else {
        const entries = path === '/config/providers' ? summary.providers : summary.models;
        const entry = { ...body };
        delete entry.api_token;
        if (entry.task_reasoning_effort === 'inherit') delete entry.task_reasoning_effort;
        const index = entries.findIndex((item) => item.id === entry.id);
        if (index < 0) entries.push(entry);
        else entries[index] = entry;
      }
      return { ok: true, json: async () => summary };
    },
    setFormStatus(element, message, status) { element.textContent = message; element.dataset.state = status; },
    optionalCount(element) { return element.value ? Number(element.value) : null; },
    markUnauthorized() { state.connected = false; },
    renderProjectsList() {}, renderSidebarProjects() {}, renderProviderList() {}, renderModelList() {},
    updateProjectDisplay() {}, updateModelDisplay() {}, updateSidebarProjectControls() {},
    updateComposerActivity() {}, updateConnectionDetail() {}, populateModels() {}, updateSessionDisplay() {},
  });
  const run = (code) => vm.runInContext(code, context);
  run(source.slice(source.indexOf('class ApiError '), source.indexOf('function markUnauthorized(')));
  run(source.slice(source.indexOf('function updateProviderCredentialFields('), source.indexOf('function populateModels(')));
  run(source.slice(source.indexOf('function updateControls('), source.indexOf('function resetRunState(')));
  run(source.slice(source.indexOf('function selectSettingsSection('), source.indexOf('/* Wiring */')));
  run(source.slice(source.indexOf("dom.newProvider.addEventListener('click'"), source.indexOf("dom.message.addEventListener('input'")));
  run('resetProviderForm(); resetModelForm(); renderConfigEditors({ hydrateThinking: true }); updateControls();');
  return {
    dom, state, requests, run,
    submit: (form) => dom[form].listeners.submit({ preventDefault() {} }),
    fail(status, message) { failure = { status, message }; },
    pause() { wait = new Promise((resolve) => { release = resolve; }); },
    resume() { release(); },
  };
}

test('thinking controls hydrate omitted defaults without writes or copying inherited settings', () => {
  for (const effort of [undefined, 'low', 'none']) {
    const fixture = thinkingFixture(effort === undefined ? {} : { task_reasoning_effort: effort });
    const { dom, state, requests, run } = fixture;
    const before = JSON.stringify(state.config);
    assert.equal(dom.taskThinking.value, effort ?? 'inherit');
    run("editProvider('local'); editModel('fast');");
    assert.equal(dom.providerTaskThinking.value, 'inherit');
    assert.equal(dom.modelTaskThinking.value, 'inherit');
    assert.equal(JSON.stringify(state.config), before);
    assert.equal(requests.length, 0);
  }
});

test('provider and model editors preserve explicit thinking and reset new forms to inherit', () => {
  const { dom, state, requests, run } = thinkingFixture({ task_reasoning_effort: 'none' });
  for (const effort of ['low', 'none', undefined]) {
    state.config.providers[0].task_reasoning_effort = effort;
    state.config.models[0].task_reasoning_effort = effort;
    run("editProvider('local'); editModel('fast');");
    assert.equal(dom.providerTaskThinking.value, effort ?? 'inherit');
    assert.equal(dom.modelTaskThinking.value, effort ?? 'inherit');
    dom.newProvider.listeners.click();
    dom.newModel.listeners.click();
    assert.equal(dom.providerTaskThinking.value, 'inherit');
    assert.equal(dom.modelTaskThinking.value, 'inherit');
  }
  assert.equal(requests.length, 0);
});

test('global thinking saves low, off and explicit inherit using authenticated API and canonical config', async () => {
  const { dom, state, requests, submit } = thinkingFixture();
  for (const effort of ['low', 'none', 'inherit']) {
    dom.taskThinking.value = effort;
    await submit('thinkingForm');
    const request = requests.at(-1);
    assert.equal(request.path, '/config/thinking');
    assert.equal(request.method, 'POST');
    assert.deepEqual(request.body, { task_reasoning_effort: effort });
    assert.equal(request.headers.Authorization, 'Bearer test-token');
    assert.equal(request.headers['Content-Type'], 'application/json');
    assert.equal(state.config.task_reasoning_effort, effort === 'inherit' ? undefined : effort);
    assert.equal(Object.hasOwn(state.config, 'task_reasoning_effort'), effort !== 'inherit');
    assert.equal(dom.taskThinking.value, effort);
    assert.equal(dom.thinkingStatus.dataset.state, 'ok');
  }
});

test('provider and model thinking overrides save and clear without changing inherited defaults', async () => {
  const { dom, state, requests, submit, run } = thinkingFixture({ task_reasoning_effort: 'low' });
  for (const scope of ['provider', 'model']) {
    const entries = scope === 'provider' ? 'providers' : 'models';
    for (const effort of ['low', 'none', 'inherit']) {
      run(scope === 'provider' ? "editProvider('local')" : "editModel('fast')");
      dom[`${scope}TaskThinking`].value = effort;
      await submit(`${scope}Form`);
      assert.equal(requests.at(-1).path, `/config/${entries}`);
      assert.equal(requests.at(-1).body.task_reasoning_effort, effort);
      assert.equal(state.config[entries][0].task_reasoning_effort, effort === 'inherit' ? undefined : effort);
      assert.equal(Object.hasOwn(state.config[entries][0], 'task_reasoning_effort'), effort !== 'inherit');
      assert.equal(state.config.task_reasoning_effort, 'low');
      assert.equal(dom[`${scope}Status`].dataset.state, 'ok');
    }
  }
});

test('unrelated provider and model saves preserve the global thinking draft across panel changes', async () => {
  for (const scope of ['provider', 'model']) {
    const { dom, state, requests, submit, run } = thinkingFixture({ task_reasoning_effort: 'low' });
    run("selectSettingsSection('thinking-form')");
    dom.taskThinking.value = 'none';
    run(`selectSettingsSection('${scope}s-block')`);
    run(scope === 'provider' ? "editProvider('local')" : "editModel('fast')");
    await submit(`${scope}Form`);
    run("selectSettingsSection('thinking-form')");
    assert.equal(dom.settingsPanels.find((panel) => !panel.hidden).id, 'thinking-form');
    assert.equal(dom.taskThinking.value, 'none');
    assert.equal(state.config.task_reasoning_effort, 'low');
    assert.equal(requests.length, 1);
    await submit('thinkingForm');
    assert.equal(requests.at(-1).path, '/config/thinking');
    assert.equal(requests.at(-1).body.task_reasoning_effort, 'none');
    assert.equal(state.config.task_reasoning_effort, 'none');
    assert.equal(dom.taskThinking.value, 'none');
  }
});

test('explicit connection hydration replaces a thinking draft with the loaded setting', () => {
  const { dom, state, requests, run } = thinkingFixture({ task_reasoning_effort: 'low' });
  dom.taskThinking.value = 'none';
  run('renderConfigEditors({ hydrateThinking: true })');
  assert.equal(dom.taskThinking.value, 'low');
  dom.taskThinking.value = 'none';
  delete state.config.task_reasoning_effort;
  run('renderConfigEditors({ hydrateThinking: true })');
  assert.equal(dom.taskThinking.value, 'inherit');
  assert.equal(requests.length, 0);
});

test('thinking save disables controls, prevents duplicate saves and does not write until confirmed', async () => {
  const fixture = thinkingFixture();
  const { dom, state, requests, submit } = fixture;
  fixture.pause();
  dom.taskThinking.value = 'none';
  const before = state.config;
  const saving = submit('thinkingForm');
  assert.equal(dom.saveThinking.disabled, true);
  assert.equal(dom.taskThinking.disabled, true);
  assert.equal(dom.thinkingStatus.dataset.state, 'busy');
  assert.equal(state.config, before);
  await submit('thinkingForm');
  assert.equal(requests.length, 1);
  fixture.resume();
  await saving;
  assert.equal(dom.saveThinking.disabled, false);
  assert.equal(dom.taskThinking.disabled, false);
  assert.equal(state.config.task_reasoning_effort, 'none');
});

test('provider API tokens save only with a stored reference and clear after success', async () => {
  const { dom, state, requests, submit, run } = thinkingFixture();
  run("editProvider('local')");
  dom.providerCredentialKind.value = 'stored';
  dom.providerCredentialKind.listeners.change();
  dom.providerCredentialName.value = 'local';
  dom.providerApiToken.value = 'secret-token';
  await submit('providerForm');
  const request = requests.at(-1);
  assert.equal(request.path, '/config/providers');
  assert.equal(request.body.credential.stored, 'local');
  assert.equal(request.body.api_token, 'secret-token');
  assert.equal(dom.providerApiToken.value, '');
  assert.equal(dom.deleteProvider.hidden, false);
  assert.equal(Object.hasOwn(state.config.providers[0], 'api_token'), false);
  assert.equal(dom.providerStatus.dataset.state, 'ok');

  run("editProvider('local')");
  dom.providerCredentialKind.value = 'env';
  dom.providerCredentialKind.listeners.change();
  dom.providerCredentialName.value = 'LOCAL_KEY';
  await submit('providerForm');
  assert.equal(Object.hasOwn(requests.at(-1).body, 'api_token'), false);
  assert.equal(dom.providerTokenField.hidden, true);
});

test('provider deletion uses the authenticated DELETE route and updates the list', async () => {
  const { dom, state, requests, run } = thinkingFixture();
  run("editProvider('local')");
  assert.equal(dom.deleteProvider.hidden, false);
  await dom.deleteProvider.listeners.click();
  const request = requests.at(-1);
  assert.equal(request.path, '/config/providers/local');
  assert.equal(request.method, 'DELETE');
  assert.equal(request.headers.Authorization, 'Bearer test-token');
  assert.deepEqual(state.config.providers, []);
  assert.equal(dom.providerStatus.dataset.state, 'ok');
  assert.equal(dom.deleteProvider.hidden, true);
});

test('provider enabled status is loaded and saved without affecting other fields', async () => {
  const { dom, state, requests, submit, run } = thinkingFixture();
  run("editProvider('local')");
  assert.equal(dom.providerEnabled.value, 'true');
  dom.providerEnabled.value = 'false';
  await submit('providerForm');
  assert.equal(requests.at(-1).body.enabled, false);
  assert.equal(state.config.providers[0].enabled, false);
  run("editProvider('local')");
  assert.equal(dom.providerEnabled.value, 'false');
  dom.providerEnabled.value = 'true';
  await submit('providerForm');
  assert.equal(requests.at(-1).body.enabled, true);
  assert.equal(requests.at(-1).body.credential.env, 'LOCAL_KEY');
});

test('failed thinking saves leave config unchanged and preserve drafts; auth errors disconnect', async () => {
  for (const scope of ['thinking', 'provider', 'model']) {
    for (const status of [400, 401]) {
      const fixture = thinkingFixture({ task_reasoning_effort: 'low' });
      const { dom, state, run, submit } = fixture;
      run("editProvider('local'); editModel('fast');");
      const control = scope === 'thinking' ? dom.taskThinking : dom[`${scope}TaskThinking`];
      control.value = 'none';
      const before = state.config;
      const serialized = JSON.stringify(before);
      fixture.fail(status, 'Thinking override rejected');
      await submit(`${scope}Form`);
      assert.equal(state.config, before);
      assert.equal(JSON.stringify(state.config), serialized);
      assert.equal(control.value, 'none');
      assert.equal(dom[`${scope}Status`].textContent, 'Thinking override rejected');
      assert.equal(dom[`${scope}Status`].dataset.state, 'error');
      assert.equal(state.connected, status !== 401);
      assert.equal(state.thinkingSaving, false);
    }
  }
});

test('disconnected thinking settings cannot save', async () => {
  const { dom, state, requests, run, submit } = thinkingFixture();
  state.connected = false;
  run('updateControls()');
  assert.equal(dom.saveThinking.disabled, true);
  assert.equal(dom.taskThinking.disabled, true);
  await submit('thinkingForm');
  assert.equal(requests.length, 0);
});
