// JS-API memory-bridge cost probe (.notes/wasm-analysis.md §7 item 8).
//
// `WebAssembly.Memory.prototype.buffer` used to be a *copy* of the engine's
// linear memory, reconciled by copying the whole buffer into the cell before a
// run and the whole cell back out after — so once `.buffer` had been touched,
// every later JS -> wasm call paid ~3 full copies of the linear memory. The two
// loops below isolate that from wasm call overhead in one process: until
// `.buffer` is materialised there is nothing to reconcile.
//
// Run:  target/release/slag.exe tools/wasm_memory_bridge.js
//
// Measured on a 256-page (16 MiB) memory, 200 calls, medians of three:
//   before aliasing  15.0 us/call without the buffer, 5225.0 us/call with it
//   after aliasing   15.0 us/call without the buffer,    0.0 us/call with it
// (the "with" loop now finishes inside Date.now's 1 ms resolution)
const HEX = globalThis.fs.readFileSync('tools/wasm_memory_bridge.hex', 'utf8');
const bytes = new Uint8Array(HEX.length >> 1);
const hexValue = (code) => (code < 58 ? code - 48 : code < 71 ? code - 55 : code - 87);
for (let i = 0; i < bytes.length; i++) {
  bytes[i] = (hexValue(HEX.charCodeAt(2 * i)) << 4) | hexValue(HEX.charCodeAt(2 * i + 1));
}

const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes));
const mem = instance.exports.mem;
const touch = instance.exports.touch;
const N = 200;

function loop() {
  let acc = 0;
  for (let i = 0; i < N; i++) acc += touch(i & 0xFF);
  return acc;
}

let t0 = Date.now();
const cold = loop();
const coldMs = Date.now() - t0;

// Materialising the buffer registers it as the cell's live buffer, so every
// later call used to pay the two copies.
const buf = mem.buffer;
const view = new Uint8Array(buf);

t0 = Date.now();
const warm = loop();
const warmMs = Date.now() - t0;

const perCall = (ms) => ((ms * 1000) / N).toFixed(1);
console.log('pages', buf.byteLength / 65536, 'calls', N, 'acc', cold + warm);
console.log('no buffer  ', coldMs, 'ms  (', perCall(coldMs), 'us/call )');
console.log('with buffer', warmMs, 'ms  (', perCall(warmMs), 'us/call )');
console.log('bridge cost', warmMs - coldMs, 'ms over', N, 'calls =',
  perCall(warmMs - coldMs), 'us/call; peek', view[0]);
