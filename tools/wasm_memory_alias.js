// Semantics probe for the aliased JS-API memory path (.notes/wasm-analysis.md
// §7 item 8): `Memory.prototype.buffer` is a view over the engine's own byte
// block, so neither side needs a copy pass.
//
// Run:  target/release/slag.exe tools/wasm_memory_alias.js
//
// Every assertion below either failed or was vacuous under the old copy bridge,
// except the mid-run one, which that design bought with two extra full-memory
// copies per host call. The expected behaviour of the two grow cases was
// cross-checked against V8 (node v24.12.0): a detached buffer reports a zero
// `byteLength`, and constructing a view over it throws a TypeError rather than
// yielding a zero-length view.
const fs = globalThis.fs;
const bytes = (path) => {
  const hex = fs.readFileSync(path, 'utf8');
  const out = new Uint8Array(hex.length >> 1);
  const value = (code) => (code < 58 ? code - 48 : code < 71 ? code - 55 : code - 87);
  for (let i = 0; i < out.length; i++) {
    out[i] = (value(hex.charCodeAt(2 * i)) << 4) | value(hex.charCodeAt(2 * i + 1));
  }
  return out;
};

function check(name, actual, expected) {
  const ok = actual === expected;
  console.log((ok ? 'ok   ' : 'FAIL ') + name + ': ' + actual + ' (expected ' + expected + ')');
  if (!ok) throw new Error(name);
}

const instance = new WebAssembly.Instance(
  new WebAssembly.Module(bytes('tools/wasm_memory_alias.hex')));
const mem = instance.exports.mem;

// JS -> wasm: a write through a view reaches the engine with no flush.
const view = new Uint8Array(mem.buffer);
view[0] = 42;
check('JS write is visible to wasm', instance.exports.read0(), 42);

// wasm -> JS: a wasm write lands in the view with no refresh.
instance.exports.write0(7);
check('wasm write is visible to JS', view[0], 7);

// A grow from JS detaches the old buffer (spec 4.4.3) and the fresh buffer
// aliases the same (grown) memory.
const oldBuffer = mem.buffer;
mem.grow(1);
const grown = mem.buffer;
check('the grown buffer is a new object', grown === oldBuffer, false);
check('the stale buffer reports a zero length', oldBuffer.byteLength, 0);
let detachedThrew = false;
try {
  new Uint8Array(oldBuffer);
} catch (error) {
  detachedThrew = error instanceof TypeError;
}
check('constructing over the stale buffer throws', detachedThrew, true);
const grownView = new Uint8Array(grown);
check('the grown buffer is two pages', grownView.length, 2 * 65536);
check('the grown buffer preserved the byte', grownView[0], 7);
grownView[0] = 9;
check('writes through the grown buffer reach wasm', instance.exports.read0(), 9);

// A grow from inside wasm is reflected on the next `buffer` access.
instance.exports.grow1();
const afterWasmGrow = new Uint8Array(mem.buffer);
check('a wasm-side grow is reflected by buffer', afterWasmGrow.length, 3 * 65536);
check('a wasm-side grow preserved the byte', afterWasmGrow[0], 9);
check('the twice-grown memory hands out a fresh view', mem.buffer === grown, false);

// A host call mid-run reads the live memory through its own views: wasm stores
// 7 and the imported function peeks it before returning.
let hostInstance = null;
const hostImports = {
  env: {
    peek() {
      return new Uint8Array(hostInstance.exports.mem.buffer)[0];
    },
  },
};
hostInstance = new WebAssembly.Instance(
  new WebAssembly.Module(bytes('tools/wasm_memory_alias_host.hex')), hostImports);
check('a host call mid-run sees the wasm write', hostInstance.exports.probe(), 7);

console.log('memory alias probe: all assertions hold');
