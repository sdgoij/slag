// Slag's browser/Node glue for the wasm_binding example (see ../wasm_binding.rs):
// instantiate the module and expose a small eval API. No bundler or
// wasm-bindgen required. Named `.js` (with the folder's package.json
// `"type": "module"`) so simple static servers serve it as JavaScript.
//
//   import { instantiate } from './slag.js';
//   const bytes = await (await fetch(url)).arrayBuffer();
//   const slag = await instantiate(bytes);
//   console.log(slag.eval('1 + 2'));          // "3"
//   slag.eval('globalThis.answer = 42;');      // state persists
//
// Engine timers (setTimeout/setInterval) are driven automatically: after
// each eval/drain the glue reads `slag_next_timeout_ms()` and arms a host
// timer that drains the engine when it comes due. Pass `{ autoDrain: false }`
// to disable this and call `slag.drain()` yourself.
//
// The host may pass { console } to route script console.* output elsewhere.

const decoder = new TextDecoder();

function consoleTarget(sink, level) {
  const name = { 0: 'log', 1: 'info', 2: 'warn', 3: 'error', 4: 'debug', 5: 'error' }[level] ?? 'log';
  return sink[name] ?? sink.log ?? console[name] ?? console.log;
}

export async function instantiate(bytes, host = {}) {
  const sink = host.console ?? console;
  const autoDrain = host.autoDrain !== false;
  let exports = null;
  const imports = {
    env: {
      // Wall clock: milliseconds since the Unix epoch.
      slag_host_now_ms: () => Date.now(),
      // Monotonic clock: setTimeout deadlines (performance.now when present).
      slag_host_now_monotonic_ms: () =>
        typeof performance !== 'undefined' && typeof performance.now === 'function'
          ? performance.now()
          : Date.now(),
      // A console line: level 0 log, 1 info, 2 warn, 3 error, 4 debug,
      // 5 unhandled rejection.
      slag_host_console: (level, ptr, len) => {
        const text = decoder.decode(new Uint8Array(exports.memory.buffer, ptr, len));
        const line = level === 5 ? `[unhandled rejection] ${text}` : text;
        consoleTarget(sink, level)(line);
      },
    },
  };
  const { instance } = await WebAssembly.instantiate(bytes, imports);
  exports = instance.exports;

  const readResult = () => {
    const ptr = exports.slag_result_ptr();
    const len = exports.slag_result_len();
    return len === 0
      ? ''
      : decoder.decode(new Uint8Array(exports.memory.buffer, ptr, len));
  };

  // One host timer at a time: arm it for the earliest pending engine timer,
  // then drain and re-arm (intervals re-queue inside the drain).
  let timer = null;
  const clearTimer = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };
  const armTimer = () => {
    if (!autoDrain || timer !== null) return;
    const delay = exports.slag_next_timeout_ms();
    if (!(delay >= 0)) return; // -1 (or NaN): nothing pending
    timer = setTimeout(() => {
      timer = null;
      if (exports.slag_drain() !== 0) {
        const text = readResult();
        if (text) consoleTarget(sink, 3)(`[timer job failed] ${text}`);
      }
      armTimer();
    }, Math.max(0, Math.ceil(delay)));
  };

  const api = {
    /// The raw wasm exports (advanced use).
    exports,
    /// Evaluate `source` and return the completion value's text. Throws an
    /// Error carrying the rendered script error on failure.
    eval(source) {
      const bytes = new TextEncoder().encode(source);
      const len = bytes.length;
      const ptr = exports.slag_alloc(len);
      if (len > 0 && ptr === 0) throw new Error('slag_alloc failed');
      if (len > 0) new Uint8Array(exports.memory.buffer, ptr, len).set(bytes);
      let status;
      try {
        status = exports.slag_eval(ptr, len);
      } finally {
        exports.slag_dealloc(ptr, len);
      }
      const text = readResult();
      if (status !== 0) {
        const error = new Error(text || 'evaluation failed');
        error.slag = true;
        throw error;
      }
      armTimer();
      return text;
    },
    /// The last eval's completion `typeof` code: 0 unknown, 1 undefined,
    /// 2 boolean, 3 number, 4 bigint, 5 string, 6 symbol, 7 function,
    /// 8 object. Read right after a successful `eval`.
    resultType() {
      return exports.slag_result_type();
    },
    /// Render a debug dump of `source` without evaluating it — kind 0
    /// tokens, 1 AST, 2 bytecode (the CLI's dump flags). Throws on a
    /// parse/compile error.
    dump(kind, source) {
      const bytes = new TextEncoder().encode(source);
      const len = bytes.length;
      const ptr = exports.slag_alloc(len);
      if (len > 0 && ptr === 0) throw new Error('slag_alloc failed');
      if (len > 0) new Uint8Array(exports.memory.buffer, ptr, len).set(bytes);
      let status;
      try {
        status = exports.slag_dump(kind, ptr, len);
      } finally {
        exports.slag_dealloc(ptr, len);
      }
      const text = readResult();
      if (status !== 0) {
        const error = new Error(text || 'dump failed');
        error.slag = true;
        throw error;
      }
      return text;
    },
    /// Run pending microtasks and due timers immediately; returns false when
    /// a job errored.
    drain() {
      const ok = exports.slag_drain() === 0;
      armTimer();
      return ok;
    },
    /// Drop the realm; the next eval starts fresh.
    reset() {
      clearTimer();
      return exports.slag_reset() === 0;
    },
    /// Cancel any armed host timer and stop auto-draining.
    dispose() {
      clearTimer();
    },
  };
  return api;
}

/// Convenience for browsers: fetch the module from a URL first.
export async function fromUrl(url, host = {}) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`fetch ${url}: ${response.status}`);
  return instantiate(await response.arrayBuffer(), host);
}
