// Diagnostic: inside E1, probe the bench module export-by-export and test
// whether a second live instance breaks the first.
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

const FIB_ONLY = [0,97,115,109,1,0,0,0,1,6,1,96,1,127,1,127,3,2,1,0,7,7,1,3,102,105,98,0,0,10,30,1,28,0,32,0,65,2,72,4,127,32,0,5,32,0,65,1,107,16,0,32,0,65,2,107,16,0,106,11,11];
const body = `
const mk = (arr) => new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array(arr)), {});
const instB = mk([${dec}]);
const eb = instB.exports;
console.log('[diag] export types:', typeof eb.fib, typeof eb.work, typeof eb.hash, typeof eb.lcg);
try { console.log('[diag] fib(10) =', eb.fib(10)); } catch (e) { console.log('[diag] fib(10) THREW', String(e)); }
try { console.log('[diag] work(1000,10) =', eb.work(1000, 10)); } catch (e) { console.log('[diag] work THREW', String(e)); }
try { console.log('[diag] hash(1000) =', eb.hash(1000)); } catch (e) { console.log('[diag] hash THREW', String(e)); }
try { const v = eb.lcg(1000n); console.log('[diag] lcg(1000n) =', v.toString()); } catch (e) { console.log('[diag] lcg THREW', String(e)); }
const instW = mk([${FIB_ONLY.join(',')}]);
try { console.log('[diag] 2nd instance fib(10) =', instW.exports.fib(10)); } catch (e) { console.log('[diag] 2nd fib THREW', String(e)); }
try { console.log('[diag] 1st instance fib(10) again =', eb.fib(10)); } catch (e) { console.log('[diag] 1st fib again THREW', String(e)); }
try { console.log('[diag] 2nd instance fib(10) again =', instW.exports.fib(10)); } catch (e) { console.log('[diag] 2nd fib again THREW', String(e)); }
'diag done'
`;
const t = Date.now();
const r = evalIn(instance, body);
console.log('diag ms:', (Date.now() - t) + 'ms', 'status', r.status, '=>', JSON.stringify(r.text));
