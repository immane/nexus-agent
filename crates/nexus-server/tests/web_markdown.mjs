import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

class Element {
  constructor(tag, text = '') {
    this.tagName = tag;
    this.text = text;
    this.children = [];
    this.attributes = {};
    this.listeners = {};
    this.dataset = {};
    this.isConnected = true;
  }
  append(...children) {
    this.children.push(...children.map(child => {
      const element = child instanceof Element ? child : new Element('#text', child);
      element.parent = this;
      return element;
    }));
  }
  replaceWith(element) {
    this.parent.children[this.parent.children.indexOf(this)] = element;
    element.parent = this.parent;
  }
  replaceChildren() { this.children = []; }
  setAttribute(name, value) { this.attributes[name] = value; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  querySelectorAll() { return find(this, 'span').filter(element => element.dataset.projectImage); }
  get textContent() { return this.text + this.children.map(child => child.textContent).join(''); }
  set textContent(text) { this.text = text; this.children = []; }
}

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');
function fixture(overrides = {}) {
  return vm.runInNewContext(`
  ${source.slice(source.indexOf('function node('), source.indexOf('function setRunStatus('))}
  ${source.slice(source.indexOf('/* Markdown */'), source.indexOf('function assistantWarning('))}
  ({ render: (text, root, session) => renderMarkdown(root, text, session), wait: () => projectImageQueue });
`, {
    URL,
    AbortSignal,
    location: new URL('http://127.0.0.1:8471/'),
    document: {
      createElement: tag => new Element(tag),
      createTextNode: text => new Element('#text', text),
      createElementNS: (namespace, tag) => Object.assign(new Element(tag), { namespace }),
      ...overrides.document,
    },
    flushText() {},
    ...Object.fromEntries(Object.entries(overrides).filter(([key]) => key !== 'document')),
  });
}
const { render } = fixture();

function find(element, tag) {
  return (element.tagName === tag ? [element] : []).concat(element.children.flatMap(child => find(child, tag)));
}

test('Markdown loads same-origin images directly, but external HTTPS only after a click', () => {
  const root = new Element('div');
  render('![Nexus](/assets/icon-192.png "Icon") ![Plot](https://example.com/plot(1).png)', root);
  const [local] = find(root, 'img');
  assert.equal(find(root, 'img').length, 1);
  assert.equal(local.src, 'http://127.0.0.1:8471/assets/icon-192.png');
  assert.equal(local.alt, 'Nexus');
  assert.equal(local.title, 'Icon');
  const [button] = find(root, 'button');
  assert.equal(button.textContent, 'Load image: Plot');
  assert.equal(button.title, 'example.com');
  button.listeners.click();
  const [, external] = find(root, 'img');
  assert.equal(external.src, 'https://example.com/plot(1).png');
  assert.equal(external.alt, 'Plot');
  assert.equal(external.referrerPolicy, 'no-referrer');
});

test('unsafe image targets, raw HTML, escaped markup and code do not create images', () => {
  const root = new Element('div');
  render(String.raw`![file](file:///tmp/plot.png) ![data](data:image/png;base64,ABC) ![script](javascript:alert(1)) ![plain](http://example.com/a.png) ![credentials](https://user@example.com/a.png) <img src="https://example.com/a.png"> \![escaped](/assets/icon-192.png)`, root);
  assert.equal(find(root, 'img').length, 0);
  assert.equal(find(root, 'button').length, 0);
  assert.match(root.textContent, /file data script plain credentials <img/);
  render('`![code](/assets/icon-192.png)`', root);
  assert.equal(find(root, 'code')[0].textContent, '![code](/assets/icon-192.png)');
  assert.equal(find(root, 'img').length, 0);
});

test('inline and display formulas render as native MathML after their delimiters arrive', () => {
  const root = new Element('div');
  render(String.raw`First $x_1^2`, root);
  assert.equal(root.textContent, 'First $x_1^2');
  render(String.raw`First $x_1^2$, then \(\alpha \le 2\).
$$
\frac{1}{\sqrt{x}}
$$
\[\sum_{i=1}^{10} i\]`, root);
  const math = find(root, 'math');
  assert.equal(math.length, 4);
  assert.equal(math[0].namespace, 'http://www.w3.org/1998/Math/MathML');
  assert.equal(find(math[0], 'msubsup')[0].textContent, 'x12');
  assert.equal(math[1].textContent, 'α≤2');
  assert.equal(math[2].attributes.display, 'block');
  assert.equal(find(math[2], 'mfrac').length, 1);
  assert.equal(find(math[2], 'msqrt').length, 1);
  assert.equal(math[3].attributes.display, 'block');
});

test('unsupported math, money and code stay literal', () => {
  const root = new Element('div');
  const inline = String.raw`$5 and $10, \$20, $\unknown{x}$ and \(unfinished`;
  render(`${inline}\n\n\`\`\`tex\n$y$\n\`\`\`\n\n\`$z$\`\n\n\\[\\unknown{y}\\]`, root);
  assert.equal(find(root, 'math').length, 0);
  assert.equal(find(root, 'p')[0].textContent, inline.replace('\\$', '$'));
  assert.equal(find(root, 'p').at(-1).textContent, String.raw`\[\unknown{y}\]`);
  assert.deepEqual(find(root, 'code').map(code => code.textContent), ['$y$', '$z$']);
});

test('an unfinished formula does not swallow an inline code span', () => {
  const root = new Element('div');
  render('Price $5, then `$x$`', root);
  assert.equal(find(root, 'math').length, 0);
  assert.equal(find(root, 'code')[0].textContent, '$x$');
});

test('visible project SVGs use authenticated fetch and never expose the decoded blob as a page link', async () => {
  const root = new Element('div');
  const state = { session: 'sess-a', credential: 'test-token' };
  const observed = [];
  const calls = [];
  const revoked = [];
  let onVisible;
  let bitmap;
  const { render, wait } = fixture({
    state,
    IntersectionObserver: class {
      constructor(callback) { onVisible = callback; }
      observe(target) { observed.push(target); }
    },
    URL: class extends URL {
      static createObjectURL(blob) {
        assert.equal(blob.type, 'image/svg+xml');
        return 'blob:private';
      }
      static revokeObjectURL(url) { revoked.push(url); }
    },
    fetch: async (url, options) => {
      calls.push([url, options]);
      return new Response('<svg/>', { headers: { 'content-type': 'application/octet-stream' } });
    },
    document: {
      createElement(tag) {
        const element = new Element(tag);
        if (tag === 'img') Object.assign(element, {
          naturalWidth: 3200, naturalHeight: 1600, async decode() { assert.equal(this.src, 'blob:private'); },
        });
        if (tag === 'canvas') element.getContext = () => ({
          drawImage(...args) { bitmap = args; },
        });
        return element;
      },
    },
  });
  render('![Chart](assets/my%20chart.svg)', root);
  assert.equal(observed.length, 0, 'no fetching during streaming');
  render('![Chart](assets/my%20chart.svg)', root, state.session);
  assert.equal(observed.length, 1);
  assert.equal(calls.length, 0, 'preview waits until visible');
  onVisible([{ target: observed[0], isIntersecting: true }], { unobserve() {} });
  await wait();
  assert.equal(calls[0][0], '/sessions/sess-a/project-image');
  assert.equal(calls[0][1].headers.Authorization, 'Bearer test-token');
  assert.deepEqual(JSON.parse(calls[0][1].body), { path: 'assets/my chart.svg' });
  const [canvas] = find(root, 'canvas');
  assert.equal(canvas.attributes['aria-label'], 'Chart');
  assert.deepEqual(bitmap.slice(1), [0, 0, 1600, 800]);
  assert.deepEqual(revoked, ['blob:private']);
  assert.equal(find(root, 'img').length, 0);
  assert.equal(find(root, 'a').length, 0);

  render('![Old](assets/old.svg)', root, state.session);
  state.session = 'sess-b';
  onVisible([{ target: observed[1], isIntersecting: true }], { unobserve() {} });
  await wait();
  assert.equal(calls.length, 1, 'stale session preview must not request a file');
});
