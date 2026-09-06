// Depth probe 1: native slag.exe -> its wasm engine runs a full Slag
// (wasm_binding.wasm) as module E1, then we eval scripts inside E1.

const fs = globalThis.fs;
const hex = fs.readFileSync('scratch/e1.hex', 'utf8');
const byteLen = hex.length >> 1;
const tDecode = Date.now();
const bytes = new Uint8Array(byteLen);
{
  const h = hex;
  let o = 0;
  for (let i = 0; i < byteLen; i++) {
    const a = h.charCodeAt(o++);
    const b = h.charCodeAt(o++);
    const av = a < 58 ? a - 48 : a < 71 ? a - 55 : a - 87;
    const bv = b < 58 ? b - 48 : b < 71 ? b - 55 : b - 87;
    bytes[i] = (av << 4) | bv;
  }
}
console.log('decoded', byteLen, 'bytes in', (Date.now() - tDecode) + 'ms');

const tModule = Date.now();
const mod = new WebAssembly.Module(bytes);
console.log('Module(bytes) ok in', (Date.now() - tModule) + 'ms');

// Inner engine env: discover imports, stub everything, forward console.
const importNames = WebAssembly.Module.imports(mod).filter((i) => i.module === 'env').map((i) => i.name);
let mem = null;
const read = (ptr, len) => new Uint8Array(mem.buffer, ptr, len);
function decodeText(ptr, len) {
  const v = read(ptr, len);
  let s = '';
  for (let i = 0; i < v.length; i++) s += String.fromCharCode(v[i]);
  return s;
}
const env = {};
for (const name of importNames) env[name] = () => 0;
env.slag_host_has_dom = () => 0;
env.slag_host_now_ms = () => Date.now();
env.slag_host_now_monotonic_ms = () => Date.now();
env.slag_host_console = (level, ptr, len) => {
  console.log('[E1 console]', decodeText(ptr, len));
};

const tInst = Date.now();
let instance;
try {
  instance = new WebAssembly.Instance(mod, { env });
} catch (e) {
  console.log('INSTANTIATE FAILED:', String(e));
  throw e;
}
mem = instance.exports.memory;
console.log('Instance ok in', (Date.now() - tInst) + 'ms');
console.log('E1 exports:', Object.keys(instance.exports).join(', '));

function evalIn(source) {
  const n = source.length;
  const p = instance.exports.slag_alloc(n);
  if (!p && n) throw new Error('slag_alloc returned 0');
  const view = new Uint8Array(instance.exports.memory.buffer, p, n);
  for (let i = 0; i < n; i++) view[i] = source.charCodeAt(i);
  const status = instance.exports.slag_eval(p, n);
  const isError = instance.exports.slag_result_error();
  const rlen = instance.exports.slag_result_len();
  const rp = instance.exports.slag_result_ptr();
  const text = rlen ? decodeText(rp, rlen) : '';
  return { status, isError, text };
}

const tEval = Date.now();
let r = evalIn('1 + 2');
console.log('E1 eval ms:', (Date.now() - tEval) + 'ms', 'status', r.status, 'err', r.isError, '=>', JSON.stringify(r.text));

r = evalIn('"inner wasm: " + typeof WebAssembly + " / " + (typeof WebAssembly.Module)');
console.log('E1 sees:', JSON.stringify(r.text));
