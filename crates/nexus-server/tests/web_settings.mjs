// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

test('settings categories select one panel, retain form drafts and reset scrolling', () => {
  const html = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
  const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
  const ids = [...html.matchAll(/data-settings-section="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(ids.length, 6);
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
