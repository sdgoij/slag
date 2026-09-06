// Depth probe 3: native slag.exe -> E1 (full Slag engine in wasm, interpreted)
// -> a small workload wasm module run by E1's OWN WebAssembly engine.
// The workload bytes are a ~1KB literal (no giant decode inside E1).

const fs = globalThis.fs;
const hex = fs.readFileSync('scratch/e1.hex', 'utf8');
const dec = fs.readFileSync('scratch/bench.dec', 'utf8').trim();
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
console.log('E0: decoded', byteLen, 'bytes in', (Date.now() - tDecode) + 'ms');

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

// Boot E1 and make sure its WebAssembly works on a trivial module first.
const l2body = `
const w = new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([0,97,115,109,1,0,0,0,1,6,1,96,1,127,1,127,3,2,1,0,7,7,1,3,102,105,98,0,0,10,30,1,28,0,32,0,65,2,72,4,127,32,0,5,32,0,65,1,107,16,0,32,0,65,2,107,16,0,106,11,11])), {});
const fib8 = w.exports.fib(8);
console.log('[L2] E1 ran wasm fib(8) =', fib8);

const bmod = new Uint8Array([${dec}]);
const inst = new WebAssembly.Instance(new WebAssembly.Module(bmod), {});
const e = inst.exports;
function bench(name, reps, fn) {
  const t = Date.now();
  let r;
  for (let i = 0; i < reps; i++) r = fn();
  return [Date.now() - t, r];
}
const r1 = bench(1, () => e.fib(24));
const r2 = bench(1, () => e.work(100000, 1000));
const r3 = bench(1, () => e.hash(100000));
const r4 = bench(1, () => e.lcg(200000n));
console.log('[L2] depth2 fib(24)  =', r1[1], r1[0] + 'ms');
console.log('[L2] depth2 work     =', r2[1], r2[0] + 'ms');
console.log('[L2] depth2 hash     =', r3[1], r3[0] + 'ms');
console.log('[L2] depth2 lcg      =', r4[1].toString(), r4[0] + 'ms');
'depth2 ok: fib(24)=' + r1[1] + ' lcg=' + r4[1].toString()
`;
const tE1 = Date.now();
const r = evalIn(instance, l2body);
console.log('L2 script ms:', (Date.now() - tE1) + 'ms', 'status', r.status, '=>', JSON.stringify(r.text));
