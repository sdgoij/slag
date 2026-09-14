// The wasm DOM-bridge harness, run by the Slag CLI itself:
//
//   cargo build -p slag --example wasm_binding --target wasm32-unknown-unknown --release
//   target/release/slag tools/wasm_dom_test.js      # from the repo root
//
// It loads `docs/slag.wasm` through the page's own glue (`docs/slag.js`),
// installs a fake DOM, and drives the engine-side DOM bridge (dom.rs): element
// accessors, classList/dataset, getElementById/appendChild, and a fired native
// event dispatching back into a JS listener. No Node and no browser — the
// host here is Slag, using its own WebAssembly engine and `fs`.
//
// The glue is an ES module and Slag runs this file as a Script, so the `export`
// keywords are stripped before it is evaluated. Slag provides no
// TextEncoder/TextDecoder, so minimal UTF-8 shims are installed first.

const fs = globalThis.fs;

// ---- host codecs Slag does not provide ----
globalThis.TextEncoder = class {
  encode(text) {
    const out = [];
    for (const ch of String(text)) {
      const cp = ch.codePointAt(0);
      if (cp < 0x80) {
        out.push(cp);
      } else if (cp < 0x800) {
        out.push(0xc0 | (cp >> 6), 0x80 | (cp & 63));
      } else if (cp < 0x10000) {
        out.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 63), 0x80 | (cp & 63));
      } else {
        out.push(
          0xf0 | (cp >> 18),
          0x80 | ((cp >> 12) & 63),
          0x80 | ((cp >> 6) & 63),
          0x80 | (cp & 63),
        );
      }
    }
    return Uint8Array.from(out);
  }
};
globalThis.TextDecoder = class {
  decode(bytes) {
    const view = bytes;
    let text = '';
    let i = 0;
    while (i < view.length) {
      const b0 = view[i++];
      let cp;
      if (b0 < 0x80) {
        cp = b0;
      } else if (b0 < 0xe0) {
        cp = ((b0 & 0x1f) << 6) | (view[i++] & 63);
      } else if (b0 < 0xf0) {
        cp = ((b0 & 0x0f) << 12) | ((view[i++] & 63) << 6) | (view[i++] & 63);
      } else {
        cp =
          ((b0 & 0x07) << 18) |
          ((view[i++] & 63) << 12) |
          ((view[i++] & 63) << 6) |
          (view[i++] & 63);
      }
      text += String.fromCodePoint(cp);
    }
    return text;
  }
};

// ---- fake DOM (what the glue's DOM host calls into) ----
class FakeClassList {
  constructor() {
    this.tokens = new Set();
  }
  add(token) {
    this.tokens.add(token);
  }
  remove(token) {
    this.tokens.delete(token);
  }
  toggle(token, force) {
    const want = force === undefined ? !this.tokens.has(token) : Boolean(force);
    if (want) this.tokens.add(token);
    else this.tokens.delete(token);
    return want;
  }
  contains(token) {
    return this.tokens.has(token);
  }
}

class FakeNode {
  constructor(tag) {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.classList = new FakeClassList();
    this.dataset = {};
    this.style = {};
    this.listeners = new Map();
    this.textContent = '';
    this.className = '';
  }
  appendChild(child) {
    this.children.push(child);
    return child;
  }
  addEventListener(type, handler) {
    const list = this.listeners.get(type) || [];
    list.push(handler);
    this.listeners.set(type, list);
  }
  fire(type, event) {
    const list = this.listeners.get(type) || [];
    for (const handler of list) handler(event || {});
  }
  setAttribute() {}
  remove() {}
  select() {}
  focus() {
    this.focused = true;
  }
}

const walk = (node, visit) => {
  visit(node);
  for (const child of node.children) walk(child, visit);
};

const rootNode = new FakeNode('html');
globalThis.document = {
  documentElement: rootNode,
  body: new FakeNode('body'),
  createElement: (tag) => new FakeNode(tag),
  getElementById(name) {
    let found = null;
    walk(rootNode, (node) => {
      if (node.id === name) found = node;
    });
    return found;
  },
  querySelectorAll: () => [],
  execCommand: () => false,
};
const storage = new Map();
globalThis.localStorage = {
  getItem: (key) => (storage.has(key) ? storage.get(key) : null),
  setItem: (key, value) => storage.set(key, String(value)),
};

// ---- the page's glue, evaluated as a script ----
const glueText = fs
  .readFileSync('docs/slag.js', 'utf8')
  .replace(/(^|\n)export\s+/g, '$1');
eval(glueText + '\n;globalThis.__glue = { instantiate: instantiate, fromUrl: fromUrl };');
const glue = globalThis.__glue;

const bytes = fs.readFileSync('docs/slag.wasm');
console.log('wasm bytes:', bytes.length, '/', bytes instanceof Uint8Array);

const failures = [];
const check = (label, actual, expected) => {
  const ok = actual === expected;
  if (!ok) {
    failures.push(
      label + ': got ' + JSON.stringify(actual) + ', want ' + JSON.stringify(expected),
    );
  }
  console.log((ok ? 'ok   ' : 'FAIL ') + label);
};

const lines = [];
const sink = {};
for (const name of ['log', 'info', 'warn', 'error', 'debug']) {
  sink[name] = (line) => lines.push(name + ': ' + line);
}

(async function () {
  try {
    const api = await glue.instantiate(bytes, { console: sink, autoDrain: false });

    // ---- basic engine smoke (no DOM) ----
    check('eval 1 + 2', api.eval('1 + 2'), '3');
    api.eval('globalThis.n = 40;');
    check('state persists', api.eval('globalThis.n + 2'), '42');
    let threw = '';
    try {
      api.eval('null.x');
    } catch (error) {
      threw = error.message;
    }
    check('throw surfaces', threw.startsWith('TypeError:'), true);
    api.eval('globalThis.r = 0; setTimeout(() => { globalThis.r = 7; }, 0);');
    api.drain();
    check('timer runs', api.eval('globalThis.r'), '7');

    // ---- DOM bridge (the refactored dom.rs) ----
    lines.length = 0;
    api.eval(`
      const el = document.createElement('div');
      el.id = 'box';
      el.textContent = 'hello';
      document.documentElement.appendChild(el);

      const box = document.getElementById('box');
      console.log('text', box.textContent);
      box.textContent = 'changed';
      console.log('after', box.textContent);
      box.className = 'a b';
      console.log('className', box.className);

      box.classList.add('b');
      box.classList.add('c');
      box.classList.remove('a');
      console.log('class', box.classList.contains('b'), box.classList.contains('a'), box.classList.contains('c'));

      box.dataset.role = 'button';
      console.log('dataset', box.dataset.role, box.dataset.missing == null);
      console.log('stable', box.dataset === box.dataset, box.classList === box.classList);

      globalThis.hits = 0;
      box.addEventListener('click', (event) => {
        globalThis.hits = globalThis.hits + 1;
        globalThis.key = event.key;
        box.textContent = 'clicked';
      });
      console.log('wired');

      if (!(box instanceof Object)) { throw new Error('element is not an object'); }
    `);

    check('console: text', lines[0], 'log: text hello');
    check('console: accessor write+read', lines[1], 'log: after changed');
    check('console: className', lines[2], 'log: className a b');
    check('console: classList ops', lines[3], 'log: class true false true');
    check('console: dataset', lines[4], 'log: dataset button true');
    check('console: stable dataset/classList identity', lines[5], 'log: stable true true');
    check('console: listener wired', lines[6], 'log: wired');

    const box = document.getElementById('box');
    check('accessor write reached the host node', box.textContent, 'changed');
    check('classList reached the host node', box.classList.contains('c'), true);
    check('dataset reached the host node', box.dataset.role, 'button');

    // A native event: the glue's listener calls back into the engine.
    box.fire('click', { key: 'K' });
    check('listener ran', api.eval('globalThis.hits'), '1');
    check('event props delivered', api.eval('globalThis.key'), 'K');
    check('listener wrote back through the accessor', box.textContent, 'clicked');
  } catch (error) {
    failures.push('harness threw: ' + error);
  }

  if (failures.length > 0) {
    console.error(failures.length + ' failure(s):');
    for (const failure of failures) console.error('  ' + failure);
    // Fail the process: a timer callback that throws is a job error, which the
    // CLI reports with a non-zero exit (there is no process.exit in Slag).
    setTimeout(function () {
      throw new Error('wasm DOM bridge: ' + failures.length + ' check(s) failed');
    }, 0);
  } else {
    console.log('wasm DOM bridge: all checks passed');
  }
})();
