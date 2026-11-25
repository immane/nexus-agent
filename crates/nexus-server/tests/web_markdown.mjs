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
  get childElementCount() { return this.children.filter(child => child.tagName !== '#text').length; }
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

test('Taylor formulas render explanatory text, spacing and derivative primes with fixed-size parentheses', () => {
  const root = new Element('div');
  render(String.raw`$$f(x)=\sum_{n=0}^{N}\frac{f^{(n)}(a)}{n!}(x-a)^n+R_N(x)$$
$$R_N(x)=\frac{f^{(N+1)}(\xi)}{(N+1)!}(x-a)^{N+1},\qquad \xi \text{ 介于 } a \text{ 与 } x \text{ 之间}$$
$$f(x)=f(a)+f'(a)(x-a)+\frac{f''(a)}{2!}(x-a)^2+\cdots$$
$$f(x)=\sum_{n=0}^{\infty}\frac{f^{(n)}(a)}{n!}(x-a)^n,\qquad |x-a|<\rho$$`, root);
  const math = find(root, 'math');
  assert.equal(math.length, 4);
  const parentheses = find(math[0], 'mo').filter(node => ['(', ')'].includes(node.textContent));
  assert.ok(parentheses.length > 0);
  assert.ok(parentheses.every(node => node.attributes.stretchy === 'false'));
  assert.deepEqual(find(math[1], 'mtext').map(node => node.textContent), [' 介于 ', ' 与 ', ' 之间']);
  assert.equal(find(math[1], 'mspace')[0].attributes.width, '2em');
  assert.ok(find(math[2], 'msup').some(node => node.textContent === 'f′'));
  assert.ok(find(math[2], 'msup').some(node => node.textContent === 'f′′'));
  assert.equal(find(math[3], 'mspace')[0].attributes.width, '2em');
});

test('advanced constructs preserve the MathML layout and the meaning of symbols', () => {
  const root = new Element('div');
  render(String.raw`\begin{align*}
  \mathbf{A} &= \begin{pmatrix}1&2\\3&4\end{pmatrix}\\[3pt]
  f(x) &= \begin{cases}\sqrt[3]{x}&x\geq 0\\-1&\text{otherwise}\end{cases}
  \end{align*}
\[\left.\frac{dy}{dx}\right|_{x=0}=\mathbb{R}\]
$$\sum_{i=0}^{n} x_i + \int_0^1 t\,dt + \binom{n}{k}$$
Inline $\sum_{i=0}^{n} x_i$.

| Quantity | Formula |
| --- | --- |
| Norm | $\left\lVert x\right\rVert$ |
| Absolute value | $|x|$ |`, root);
  const math = find(root, 'math');
  assert.equal(math.length, 6, root.children.map(node => `${node.tagName}: ${node.textContent}`).join('\n'));
  assert.equal(find(math[0], 'mtable').length, 3);
  const alignedRows = find(math[0], 'mtable')[0].children;
  assert.equal(alignedRows.length, 2);
  assert.deepEqual(alignedRows[0].children.map(cell => cell.attributes.class), ['md-math-r', 'md-math-l']);
  assert.equal(find(math[0], 'mroot')[0].textContent, 'x3');
  assert.equal(find(math[0], 'mi').find(node => node.textContent === '𝐀')?.attributes.mathvariant, 'normal');
  assert.equal(find(math[0], 'mo').filter(node => node.textContent === '{').length, 1);
  assert.equal(find(math[1], 'mo').find(node => node.textContent === '|')?.attributes.stretchy, 'true');
  assert.ok(math[1].textContent.endsWith('ℝ'));
  assert.equal(find(math[2], 'munderover')[0].textContent, '∑i=0n');
  assert.equal(find(math[2], 'msubsup')[0].textContent, '∫01');
  assert.equal(find(math[2], 'mfrac')[0].attributes.linethickness, '0');
  assert.equal(find(math[3], 'msubsup')[0].textContent, '∑i=0n');
  assert.equal(find(root, 'td').length, 4);
  assert.equal(math[4].textContent, '‖x‖');
  assert.equal(math[5].textContent, '|x|');
});

test('malformed constructs are literal, and TeX text cannot create arbitrary DOM', () => {
  const root = new Element('div');
  const malformed = [
    String.raw`$$\begin{matrix}x\end{array}$$`,
    String.raw`$$\left(\frac{1}{2}$$`,
    String.raw`$$\sqrt[3{x}$$`,
    String.raw`$$\begin{array}{l}1&2\end{array}$$`,
    String.raw`$$\href{javascript:alert(1)}{x}$$`,
    '$$' + '{'.repeat(30) + 'x' + '}'.repeat(30) + '$$',
  ];
  render(malformed.join('\n') + '\n' + String.raw`$$\text{<img src=x onerror=foo>}$$`, root);
  assert.deepEqual(find(root, 'p').map(node => node.textContent), malformed);
  assert.equal(find(root, 'mtext')[0].textContent, '<img src=x onerror=foo>');
  assert.equal(find(root, 'img').length, 0);
});

test('the common-formula compatibility corpus renders without source fallback', () => {
  const groups = {
    algebra: String.raw`x=\frac{-b\pm\sqrt{b^2-4ac}}{2a}
(a+b)^2=a^2+2ab+b^2
\sqrt[3]{-8}=-2
\left(\frac ab\right)^{-1}=\frac ba
\left\lvert x\right\rvert\le 1
\left\lceil x\right\rceil-\left\lfloor x\right\rfloor
\binom nk=\frac{n!}{k!(n-k)!}
{n\choose k}={n!\over k!(n-k)!}
a\not\equiv b
\boxed{x=2}
\left\langle v,w\right\rangle=0
\left[0,\frac 12\right)
(x_1+x_2+\cdots+x_n)^2
f\colon\mathbb R\to\mathbb R
x=\dfrac{a+b}{c}+\tfrac 12
u\asymp v,\quad a\propto b`,
    calculus: String.raw`\lim_{x\to 0}\frac{\sin x}{x}=1
\lim_{n\to\infty}\left(1+\frac{1}{n}\right)^n=e
\int_0^1 x^2\,dx=\tfrac{1}{3}
\int\limits_0^\infty e^{-x}\,dx=1
\iint_D (x+y)\,dx\,dy
\iiint_\Omega \rho\,dV
\oint_C \vec F\cdot d\vec r
\frac{\partial^2 f}{\partial x\,\partial y}
\left.\frac{df}{dx}\right|_{x=1}
f(x)=\sum_{n=0}^{\infty} a_n x^n
\sum\nolimits_{i=1}^{n} i
f'''(t)=f^{(3)}(t)
\frac{d}{dx}\ln x=\frac1x
\nabla\cdot\mathbf{F}=0
\nabla\times\mathbf{F}
\underbrace{x+x+\cdots+x}_{n\text{ terms}}
\overbrace{1+1+1}^{3}
\limsup_{n\to\infty}a_n\ge\liminf_{n\to\infty}a_n
\left.\frac{d}{dt}e^{At}\right|_{t=0}=A
y(t)=C_1e^{\lambda_1t}+C_2e^{\lambda_2 t}`,
    linearAlgebra: String.raw`\mathbf{A}\mathbf{x}=\mathbf{b}
\boldsymbol{\alpha}+\bm{\beta}
\begin{pmatrix} a & b \\ c & d \end{pmatrix}
\begin{bmatrix} 1 & 0 \\ 0 & 1 \end{bmatrix}
\begin{vmatrix} a & b \\ c & d \end{vmatrix}=ad-bc
\begin{Vmatrix} a \\ b \end{Vmatrix}
\begin{Bmatrix} x & y \\ z & w \end{Bmatrix}
\begin{matrix} x & y \\ z & w \end{matrix}
\begin{smallmatrix}1&2\\3&4\end{smallmatrix}
\left(\begin{array}{rr}1&-1\\0&2\end{array}\right)
\mathbf{A}^{-1}\mathbf{A}=\mathbf{I}
\mathbf{Q}^{\mathsf T}\mathbf{Q}=\mathbf{I}
\operatorname{rank}(A)+\dim\ker(A)=n
\det(A-\lambda I)=0
\lVert x\rVert_2=\sqrt{x^{\mathsf T}x}
\hat{\mathbf{u}}\cdot\hat{\mathbf{v}}=\cos\theta
\{e_1,\ldots,e_n\}
\begin{pmatrix}1&\cdots&0\\\vdots&\ddots&\vdots\\0&\cdots&1\end{pmatrix}`,
    probability: String.raw`\Pr(A\mid B)=\frac{\Pr(B\mid A)\Pr(A)}{\Pr(B)}
\Pr\left(X\le x\right)
\mathbb E[X]=\sum_x x\,p(x)
\mathbb{E}\left[X\mid Y\right]
\operatorname{Var}(X)=\mathbb{E}[X^2]-(\mathbb{E}[X])^2
\operatorname{Cov}(X,Y)
\mathcal N(\mu,\sigma^2)
X\sim\mathrm{Bernoulli}(p)
\binom nk p^k(1-p)^{n-k}
\frac1{\sigma\sqrt{2\pi}}\exp\left(-\frac{(x-\mu)^2}{2\sigma^2}\right)
\sqrt n(\bar X_n-\mu)\xrightarrow{d}\mathcal N(0,\sigma^2)
X_n\xrightarrow[n\to\infty]{p} X
\widehat{\theta}=\operatorname*{argmax}_{\theta}L(\theta)
\rho_{XY}=\frac{\sigma_{XY}}{\sigma_X\sigma_Y}
\mathbb P\bigl(A\cup B\bigr)
\left\{x\in\mathbb R\middle|x>0\right\}`,
    setsAndSymbols: String.raw`\forall x\in A\;\exists y\in B
A\subseteq B\implies B^c\subseteq A^c
A\cap B=\varnothing
A\setminus B
x\notin\mathbb Z
p\land q\iff q\land p
\neg(p\lor q)\equiv\neg p\land\neg q
x\mapsto x^2
\mathfrak g\oplus\mathfrak h
V\otimes W
x_n\longrightarrow x
\aleph_0\prec\aleph_1
\alpha\neq\varepsilon,\quad\phi\ne\varphi
\mathcal F \colon \mathcal A\longrightarrow \mathcal B
\sum_{\substack{i\in I\\j\in J}} a_{ij}
\bigcup_{n\ge 0} A_n\supseteq A_0
\text{当 }x\,\text{趋近于 }0
\hbar=\frac h{2\pi}`,
    displayLayout: String.raw`\begin{cases}x^2&x\ge 0\\-x&x<0\end{cases}
\begin{dcases}\frac1x&x\ne0\\0&x=0\end{dcases}
\begin{aligned}a&=b+c\\&=d\end{aligned}
\begin{align}x&=y+z\\y&=2z\notag\end{align}
\begin{alignedat}{2}a&=b&\quad c&=d\end{alignedat}
\begin{gathered} a+b=c\\ c+d=e\end{gathered}
\begin{equation*}x^{2}+y^{2}=r^{2}\end{equation*}
\begin{split}(x+y)^2&=x^2+2xy\\ &\quad+y^2\end{split}
\left.\begin{array}{c}x=1\\y=2\end{array}\right\}
\left\langle\frac{1}{x},\sqrt{x}\right\rangle
\dfrac{1}{1+\dfrac{1}{1+x}}
\widehat{AB}\quad\widetilde{\mathbf{x}}\quad\overline{z}
\overset{\mathrm{def}}{=}\underset{x\to0}{\longrightarrow}
\xleftarrow[\text{below}]{\text{above}}
\phantom{000} x
\left\lfloor\frac{x}{2}\right\rfloor
\displaystyle\sum_{i=1}^n i\tag{1}
\Biggl\langle x,y\Biggr\rangle
\boxed{\mathrm{H}_2\mathrm{O}}
\overrightarrow{AB}\perp\overrightarrow{BC}`,
  };
  let count = 0;
  for (const [group, examples] of Object.entries(groups)) {
    for (const formula of examples.split('\n')) {
      const root = new Element('div');
      render(`$$${formula}$$`, root);
      assert.equal(find(root, 'math').length, 1, `${group}: ${formula}`);
      count += 1;
    }
  }
  assert.ok(count >= 100);
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
