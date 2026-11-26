import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../web/app.js', import.meta.url), 'utf8');

function fixture() {
  const functions = source.slice(
    source.indexOf('function imagePreviewSource('),
    source.indexOf('const MATH_SOURCE_LIMIT'),
  );
  const focused = [];
  const bodyClasses = new Set();
  const lightboxImage = {
    src: '',
    alt: '',
    removeAttribute(name) { if (name === 'src') this.src = ''; },
  };
  const lightbox = {
    hidden: true,
    focus: () => focused.push('overlay'),
  };
  const context = vm.createContext({
    dom: {
      imageLightbox: lightbox,
      imageLightboxImage: lightboxImage,
    },
    document: {
      body: {
        classList: {
          add: value => bodyClasses.add(value),
          remove: value => bodyClasses.delete(value),
          contains: value => bodyClasses.has(value),
        },
      },
      createTextNode: text => ({ text }),
    },
  });
  vm.runInContext(functions, context);
  return {
    bodyClasses, focused, lightbox, lightboxImage,
    run: code => vm.runInContext(code, context),
  };
}

test('clicking a rendered image opens a full-screen preview with its source and description', () => {
  const f = fixture();
  const opened = f.run(`enlargeImage({
    tagName: 'IMG',
    currentSrc: 'http://127.0.0.1/project-image.png',
    src: 'http://127.0.0.1/fallback.png',
    alt: 'Plot',
    focus: () => {},
    getAttribute: () => null,
  })`);
  assert.equal(opened, true);
  assert.equal(f.lightbox.hidden, false);
  assert.equal(f.lightboxImage.src, 'http://127.0.0.1/project-image.png');
  assert.equal(f.lightboxImage.alt, 'Plot');
  assert.equal(f.bodyClasses.has('image-lightbox-open'), true);
  assert.deepEqual(f.focused, ['overlay']);
});

test('project-image canvases are rasterized before preview', () => {
  const f = fixture();
  const opened = f.run(`enlargeImage({
    tagName: 'CANVAS',
    toDataURL: () => 'data:image/png;base64,AAAA',
    focus: () => {},
    getAttribute: name => name === 'aria-label' ? 'Enlarge project image' : null,
  })`);
  assert.equal(opened, true);
  assert.equal(f.lightboxImage.src, 'data:image/png;base64,AAAA');
  assert.equal(f.lightboxImage.alt, 'Enlarge project image');
});

test('closing clears the preview, restores scrolling, and returns focus', () => {
  const f = fixture();
  f.run(`enlargeImage({
    tagName: 'IMG',
    currentSrc: 'http://127.0.0.1/image.png',
    src: 'http://127.0.0.1/image.png',
    alt: 'Chart',
    focus: () => { globalThis.sourceFocused = true; },
    getAttribute: () => null,
  })`);
  f.run('closeImageLightbox()');
  assert.equal(f.lightbox.hidden, true);
  assert.equal(f.lightboxImage.src, '');
  assert.equal(f.bodyClasses.has('image-lightbox-open'), false);
  assert.equal(f.run('globalThis.sourceFocused'), true);
  assert.equal(f.run('closeImageLightbox()'), undefined);
});

test('invalid sources are ignored and the event helper only matches rendered images', () => {
  const f = fixture();
  assert.equal(f.run('enlargeImage(null)'), false);
  assert.equal(f.run("enlargeImage({ tagName: 'DIV' })"), false);
  assert.equal(f.lightbox.hidden, true);
  const match = f.run(`imageFromEvent({
    target: { closest: selector => selector === 'img.md-image, canvas.md-image' ? { tagName: 'IMG' } : null },
  })`);
  assert.equal(match.tagName, 'IMG');
  assert.equal(f.run('imageFromEvent({ target: {} })'), null);
  assert.equal(f.run('imageFromEvent({ target: null })'), null);
});
