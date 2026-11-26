// Run with: node --test crates/nexus-server/tests/web_*.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const css = readFileSync(new URL('../web/app.css', import.meta.url), 'utf8');

function variables(body) {
  return Object.fromEntries([...body.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6})\s*;/g)].map(([, name, value]) => [name, value]));
}

// The base rule defines the dark defaults; each palette overrides a subset.
const base = variables(css.match(/:root,\n\.theme-preview \{([^}]*)\}/)[1]);
const lightBase = variables(css.match(/:root\[data-theme="paper"\],\n\.theme-preview\[data-theme="paper"\] \{([^}]*)\}/)[1]);
const palettes = [...css.matchAll(/:root\[data-theme="([\w-]+)"\],\n\.theme-preview\[data-theme="\1"\] \{([^}]*)\}/g)]
  .map(([, name, body]) => {
    const light = /color-scheme:\s*light/.test(body);
    return [name, { ...(light ? lightBase : base), ...variables(body) }];
  });
palettes.unshift(['graphite', base]);

function luminance(hex) {
  const channels = [1, 3, 5].map((index) => parseInt(hex.slice(index, index + 2), 16) / 255)
    .map((value) => value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
  return 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
}

function contrast(a, b) {
  const [high, low] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (high + 0.05) / (low + 0.05);
}

test('every palette keeps text, muted text, links, and button labels at WCAG AA contrast', () => {
  assert.ok(palettes.length >= 20, `found ${palettes.length} palettes`);
  const pairs = [
    ['fg', 'bg'], ['fg', 'panel'], ['fg', 'user-bg'],
    ['muted', 'bg'], ['muted', 'panel'],
    ['accent', 'bg'], ['accent', 'panel'], ['accent-ink', 'accent'],
    ['danger', 'bg'], ['warning', 'bg'],
  ];
  const failures = [];
  for (const [name, colors] of palettes) {
    for (const [text, surface] of pairs) {
      const ratio = contrast(colors[text], colors[surface]);
      if (ratio < 4.5) failures.push(`${name}: ${text} on ${surface} is ${ratio.toFixed(2)}`);
    }
  }
  assert.deepEqual(failures, []);
});
