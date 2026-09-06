// Depth probe 2: native slag.exe -> E1 (full Slag in wasm, interpreted by the
// native wasm engine) -> E2 (full Slag in wasm, instantiated by E1's own JS).
// E1's JS has no fs/memory access, so it decodes the engine binary from a hex
// constant embedded in the script it evaluates.

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
console.log('E0: decoded', byteLen, 'bytes in', (Date.now() - tDecode) + 'ms');

const mod = new WebAssembly.Module(bytes);
const importNames = WebAssembly.Module.imports(mod)
  .filter((i) => i.module === 'env')
  .map((i) => i.name);

// ---- E1 instance + eval helper ----
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
  if (!p && n) throw new Error('slag_alloc returned 0');
  const view = new Uint8Array(inst.exports.memory.buffer, p, n);
  for (let i = 0; i < n; i++) view[i] = source.charCodeAt(i);
  const status = inst.exports.slag_eval(p, n);
  const isError = inst.exports.slag_result_error();
  const rlen = inst.exports.slag_result_len();
  const rp = inst.exports.slag_result_ptr();
  return { status, isError, text: rlen ? decodeText(rp, rlen) : '' };
}

// Sanity on E1, then build the layer-2 script (hex constant + its own
// E2 instance + evals), evaluated *inside E1*.
console.log('E1 1+2:', JSON.stringify(evalIn(instance, '1 + 2').text));

const prefix = `
const tL2 = Date.now();
console.log('[L2] start');
const HX = '`;
const suffix = `';
const n = HX.length >> 1;
const b = new Uint8Array(n);
let o = 0;
for (let i = 0; i < n; i++) {
  const a = HX.charCodeAt(o++);
  const c = HX.charCodeAt(o++);
  const av = a < 58 ? a - 48 : a < 71 ? a - 55 : a - 87;
  const cv = c < 58 ? c - 48 : c < 71 ? c - 55 : c - 87;
  b[i] = (av << 4) | cv;
}
console.log('[L2] decoded', n, 'in', (Date.now() - tL2) + 'ms');
const m2 = new WebAssembly.Module(b);
console.log('[L2] Module ok');
const names2 = WebAssembly.Module.imports(m2).map((x) => x.name);
let mem2 = null;
const rd2 = (p, l) => new Uint8Array(mem2.buffer, p, l);
const dt2 = (p, l) => { const v = rd2(p, l); let s = ''; for (let i = 0; i < v.length; i++) s += String.fromCharCode(v[i]); return s; };
const env2 = {};
for (const nm of names2) env2[nm] = () => 0;
env2.slag_host_has_dom = () => 0;
env2.slag_host_now_ms = () => Date.now();
env2.slag_host_now_monotonic_ms = () => Date.now();
env2.slag_host_console = (lvl, p, l) => { console.log('[E2 console]', dt2(p, l)); };
const i2 = new WebAssembly.Instance(m2, { env: env2 });
mem2 = i2.exports.memory;
console.log('[L2] E2 instance ok');
function ev2(src) {
  const n2 = src.length;
  const p2 = i2.exports.slag_alloc(n2);
  const v2 = new Uint8Array(i2.exports.memory.buffer, p2, n2);
  for (let i = 0; i < n2; i++) v2[i] = src.charCodeAt(i);
  const st = i2.exports.slag_eval(p2, n2);
  const rl = i2.exports.slag_result_len();
  const rp2 = i2.exports.slag_result_ptr();
  return { status: st, text: rl ? dt2(rp2, rl) : '' };
}
const a = ev2('40 + 2');
console.log('[L2] E2 eval =>', JSON.stringify(a.text), 'status', a.status);
const b2 = ev2('"E2 wasm type: " + typeof WebAssembly');
console.log('[L2] E2 sees =>', JSON.stringify(b2.text));
'[L2] done: E2 40+2 = ' + a.text
`;

const tE1 = Date.now();
const r = evalIn(instance, prefix + hex + suffix);
console.log('L2 script ms:', (Date.now() - tE1) + 'ms', 'status', r.status, '=>', JSON.stringify(r.text));
