// Depth-2 timing: the workload module run by E1's own wasm engine (E1 itself
// being interpreted by native slag.exe's wasm engine). Same sizes as t1.js.
const fs = globalThis.fs;
const hex = fs.readFileSync('scratch/e1.hex', 'utf8');
const dec = fs.readFileSync('scratch/bench.dec', 'utf8').trim();
const byteLen = hex.length >> 1;
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
const mod = new WebAssembly.Module(bytes);
const importNames = WebAssembly.Module.imports(mod)
  .filter((i) => i.module === 'env')
  .map((i) => i.name);
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
const instance = new WebAssembly.Instance(mod, { env });
mem = instance.exports.memory;
function evalIn(inst, source) {
  const n = source.length;
  const p = inst.exports.slag_alloc(n);
  const view = new Uint8Array(inst.exports.memory.buffer, p, n);
  for (let i = 0; i < n; i++) view[i] = source.charCodeAt(i);
  const status = inst.exports.slag_eval(p, n);
  const rlen = inst.exports.slag_result_len();
  const rp = inst.exports.slag_result_ptr();
  return { status, text: rlen ? decodeText(rp, rlen) : '' };
}

const body = `
const e = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([${dec}])), {}).exports;
function time(fn) {
  const t = Date.now();
  const r = fn();
  return [Date.now() - t, r];
}
const r1 = time(() => e.fib(24));
const r2 = time(() => e.work(100000, 1000));
const r3 = time(() => e.hash(100000));
const r4 = time(() => e.lcg(200000n));
console.log('[L2] depth2 fib(24)  =', r1[1], r1[0] + 'ms');
console.log('[L2] depth2 work     =', r2[1], r2[0] + 'ms');
console.log('[L2] depth2 hash     =', r3[1], r3[0] + 'ms');
console.log('[L2] depth2 lcg      =', r4[1].toString(), r4[0] + 'ms');
'depth2 done'
`;
const t = Date.now();
const r = evalIn(instance, body);
console.log('depth2 total ms:', (Date.now() - t) + 'ms', 'status', r.status, '=>', JSON.stringify(r.text));
