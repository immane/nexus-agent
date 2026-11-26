import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture() {
  const functions = source.slice(
    source.indexOf('const CODE_KEYWORDS'),
    source.indexOf('function appendHighlightedCode('),
  );
  const context = vm.createContext({});
  vm.runInContext(functions, context);
  return { tokens: (code, language) => vm.runInContext('codeTokens', context)(code, language) };
}

function kinds(tokens) {
  return tokens.filter(([kind]) => kind).map(([kind, text]) => `${kind}:${text}`);
}

test('python keywords, functions, comments, strings, and numbers are colored without losing text', () => {
  const f = fixture();
  const code = 'import math\n# note\ndef norm(u):\n    return "x" + 1\n';
  const tokens = f.tokens(code, 'python');
  assert.equal(tokens.map(([, text]) => text).join(''), code);
  const colored = kinds(tokens);
  assert.ok(colored.includes('kw:import'));
  assert.ok(colored.includes('cmt:# note'));
  assert.ok(colored.includes('kw:def'));
  assert.ok(colored.includes('fn:norm'));
  assert.ok(colored.includes('kw:return'));
  assert.ok(colored.includes('str:"x"'));
  assert.ok(colored.includes('num:1'));
});

test('javascript, rust, sql, and markup use their comment and tag rules', () => {
  const f = fixture();
  const js = f.tokens('const y = compute(1); // note\nlet x = `raw`;', 'js');
  assert.equal(js.map(([, text]) => text).join(''), 'const y = compute(1); // note\nlet x = `raw`;');
  assert.ok(kinds(js).includes('kw:const'));
  assert.ok(kinds(js).includes('fn:compute'));
  assert.ok(kinds(js).includes('cmt:// note'));
  assert.ok(kinds(js).includes('str:`raw`'));

  const rust = f.tokens('fn main() { // hi\n    let x = 0x10;\n}', 'rust');
  assert.ok(kinds(rust).includes('kw:fn'));
  assert.ok(kinds(rust).includes('fn:main'));
  assert.ok(kinds(rust).includes('cmt:// hi'));
  assert.ok(kinds(rust).includes('num:0x10'));

  const sql = f.tokens('SELECT * FROM t -- note', 'sql');
  assert.ok(kinds(sql).includes('kw:SELECT'));
  assert.ok(kinds(sql).includes('kw:FROM'));
  assert.ok(kinds(sql).includes('cmt:-- note'));

  const html = f.tokens('<div class="x"><!-- note --></div>', 'html');
  assert.ok(kinds(html).includes('tag:<div'));
  assert.ok(kinds(html).includes('str:"x"'));
  assert.ok(kinds(html).includes('cmt:<!-- note -->'));
  assert.ok(kinds(html).includes('tag:</div'));
});

test('css colors hex values and comments; text and unknown empty labels stay plain', () => {
  const f = fixture();
  const css = f.tokens('a { color: #fff; /* note */ }', 'css');
  assert.ok(kinds(css).includes('num:#fff'));
  assert.ok(kinds(css).includes('cmt:/* note */'));
  assert.equal(f.tokens('def x', 'text'), null);
  assert.equal(f.tokens('def x', ''), null);
  assert.equal(f.tokens('def x', 'markdown'), null);
});
