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

  // ---- DOM host (the dogfood demo): real browser nodes behind opaque ids ----
  // Scripts running *inside* Slag reach the page through document/element
  // host objects; this is the browser half of that bridge (wasm_binding/dom.rs).
  const DOM_TAG = { NULL: 0, BOOL: 1, NUMBER: 2, TEXT: 3, NODE: 4 };
  const dom = { nextId: 1, nodes: new Map(), listeners: new Set() };
  const domNode = (id) => dom.nodes.get(id);
  const domRegister = (node) => {
    const id = dom.nextId++;
    dom.nodes.set(id, node);
    return id;
  };
  const domEncode = (value) => {
    if (value === null || value === undefined) return { tag: DOM_TAG.NULL, payload: new Uint8Array(0) };
    if (typeof value === 'boolean') return { tag: DOM_TAG.BOOL, payload: Uint8Array.of(value ? 1 : 0) };
    if (typeof value === 'number') {
      const buffer = new ArrayBuffer(8);
      new DataView(buffer).setFloat64(0, value, true);
      return { tag: DOM_TAG.NUMBER, payload: new Uint8Array(buffer) };
    }
    return { tag: DOM_TAG.TEXT, payload: new TextEncoder().encode(String(value)) };
  };
  const domReadValue = (ptr, len) => {
    const view = new Uint8Array(exports.memory.buffer, ptr, len);
    const payload = view.subarray(1);
    switch (view[0]) {
      case DOM_TAG.NULL: return null;
      case DOM_TAG.BOOL: return payload[0] !== 0;
      case DOM_TAG.NUMBER:
        return new DataView(payload.buffer, payload.byteOffset, payload.byteLength).getFloat64(0, true);
      case DOM_TAG.TEXT: return decoder.decode(payload);
      case DOM_TAG.NODE:
        return new DataView(payload.buffer, payload.byteOffset, payload.byteLength).getUint32(0, true);
      default: return null;
    }
  };
  const domWrite = (ptr, cap, tag, payload) => {
    const total = payload.length + 1;
    if (total > cap) return 0;
    const view = new Uint8Array(exports.memory.buffer, ptr, total);
    view[0] = tag;
    view.set(payload, 1);
    return total;
  };
  const domGetProp = (node, name) => {
    switch (name) {
      case 'textContent': return node.textContent;
      case 'value': return node.value;
      case 'className': return node.className;
      case 'innerHTML': return node.innerHTML;
      case 'title': return node.title;
      case 'id': return node.id;
      case 'placeholder': return node.placeholder;
      case 'hidden': return node.hidden;
      case 'disabled': return node.disabled;
      case 'scrollTop': return node.scrollTop;
      case 'scrollHeight': return node.scrollHeight;
      default: return null;
    }
  };
  const domSetProp = (node, name, value) => {
    switch (name) {
      case 'textContent': node.textContent = value === null ? '' : String(value); break;
      case 'value': node.value = String(value ?? ''); break;
      case 'className': node.className = String(value ?? ''); break;
      case 'innerHTML': node.innerHTML = String(value ?? ''); break;
      case 'title': node.title = String(value ?? ''); break;
      case 'id': node.id = String(value ?? ''); break;
      case 'placeholder': node.placeholder = String(value ?? ''); break;
      case 'hidden': node.hidden = Boolean(value); break;
      case 'disabled': node.disabled = Boolean(value); break;
      case 'scrollTop': node.scrollTop = Number(value) || 0; break;
      case 'scrollHeight': node.scrollHeight = Number(value) || 0; break;
      default: break;
    }
  };
  // Native event -> engine event props. Entry layout (see dom.rs
  // `decode_event_props`): name-length byte, name, then a u32 (LE) value
  // length followed by the typed value bytes.
  const domEventProps = (event) => {
    const parts = [];
    const add = (name, value) => {
      const encoded = new TextEncoder().encode(name);
      if (encoded.length > 255) return;
      const { tag, payload } = domEncode(value);
      const valueLen = payload.length + 1;
      parts.push(encoded.length);
      for (const byte of encoded) parts.push(byte);
      const lenBytes = new Uint8Array(4);
      new DataView(lenBytes.buffer).setUint32(0, valueLen, true);
      for (const byte of lenBytes) parts.push(byte);
      parts.push(tag);
      for (const byte of payload) parts.push(byte);
    };
    const has = (name) => typeof event !== 'undefined' && event !== null && name in event;
    add('key', has('key') ? String(event.key) : '');
    add('code', has('code') ? String(event.code) : '');
    add('ctrlKey', Boolean(event && event.ctrlKey));
    add('metaKey', Boolean(event && event.metaKey));
    add('shiftKey', Boolean(event && event.shiftKey));
    add('altKey', Boolean(event && event.altKey));
    add('repeat', Boolean(event && event.repeat));
    add('button', event && typeof event.button === 'number' ? event.button : 0);
    return new Uint8Array(parts);
  };

  // Copy text to the host clipboard (async API best-effort, execCommand
  // fallback for plain http hosts).
  const copyNative = (text) => {
    if (navigator.clipboard && window.isSecureContext) {
      try {
        navigator.clipboard.writeText(text).catch(() => {});
        return true;
      } catch {}
    }
    try {
      const helper = document.createElement('textarea');
      helper.value = text;
      helper.setAttribute('readonly', '');
      helper.style.position = 'fixed';
      helper.style.opacity = '0';
      document.body.appendChild(helper);
      helper.select();
      const ok = document.execCommand('copy');
      helper.remove();
      return ok;
    } catch {
      return false;
    }
  };

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
      // 5 unhandled rejection. A sink exposing `__raw(level, text)` receives
      // the level so it can distinguish rejections from console.error.
      slag_host_console: (level, ptr, len) => {
        const text = decoder.decode(new Uint8Array(exports.memory.buffer, ptr, len));
        if (typeof sink.__raw === 'function') {
          sink.__raw(level, text);
          return;
        }
        const line = level === 5 ? `[unhandled rejection] ${text}` : text;
        consoleTarget(sink, level)(line);
      },
      // Whether the module should install the DOM bridge. The dogfood UI
      // engine enables it; the sandbox engine that runs user scripts runs
      // with it off so scripts cannot touch the page.
      slag_host_has_dom: () => (host.dom === false ? 0 : 1),
      // DOM bridge host: element property reads/writes and listener wiring
      // for scripts running inside the engine (see dom.rs).
      slag_host_dom_get: (id, namePtr, nameLen, respPtr, respCap) => {
        const node = domNode(id);
        if (!node) return 0;
        const name = decoder.decode(new Uint8Array(exports.memory.buffer, namePtr, nameLen));
        const { tag, payload } = domEncode(domGetProp(node, name));
        return domWrite(respPtr, respCap, tag, payload);
      },
      slag_host_dom_set: (id, namePtr, nameLen, valuePtr, valueLen) => {
        const node = domNode(id);
        if (!node) return;
        const name = decoder.decode(new Uint8Array(exports.memory.buffer, namePtr, nameLen));
        domSetProp(node, name, domReadValue(valuePtr, valueLen));
      },
      slag_host_dom_by_id: (namePtr, nameLen) => {
        const name = decoder.decode(new Uint8Array(exports.memory.buffer, namePtr, nameLen));
        const element = document.getElementById(name);
        return element ? domRegister(element) : 0;
      },
      slag_host_dom_create: (tagPtr, tagLen) => {
        const tag = decoder.decode(new Uint8Array(exports.memory.buffer, tagPtr, tagLen));
        const element = document.createElement(tag);
        return element ? domRegister(element) : 0;
      },
      slag_host_dom_append: (parent, child) => {
        const parentNode = domNode(parent);
        const childNode = domNode(child);
        if (parentNode && childNode) parentNode.appendChild(childNode);
      },
      slag_host_dom_query: (selPtr, selLen, respPtr, respCap) => {
        const selector = decoder.decode(new Uint8Array(exports.memory.buffer, selPtr, selLen));
        const ids = [];
        for (const node of document.querySelectorAll(selector)) ids.push(domRegister(node));
        const total = ids.length * 4;
        if (total > respCap) return 0;
        const view = new DataView(exports.memory.buffer, respPtr, total);
        ids.forEach((id, index) => view.setUint32(index * 4, id, true));
        return ids.length;
      },
      slag_host_dom_class: (id, op, force, tokenPtr, tokenLen) => {
        const node = domNode(id);
        if (!node || !node.classList) return 0;
        const token = decoder.decode(new Uint8Array(exports.memory.buffer, tokenPtr, tokenLen));
        switch (op) {
          case 0: node.classList.add(token); break;
          case 1: node.classList.remove(token); break;
          case 2:
            return node.classList.toggle(token, force === -1 ? undefined : force === 1) ? 1 : 0;
          case 3: return node.classList.contains(token) ? 1 : 0;
          default: return 0;
        }
        return node.classList.contains(token) ? 1 : 0;
      },
      slag_host_dom_focus: (id) => {
        const node = domNode(id);
        if (node && typeof node.focus === 'function') node.focus();
      },
      slag_host_dom_root: () => {
        const element = document.documentElement;
        return element ? domRegister(element) : 0;
      },
      slag_host_dom_dataset_get: (id, namePtr, nameLen, respPtr, respCap) => {
        const node = domNode(id);
        if (!node || !node.dataset) return 0;
        const name = decoder.decode(new Uint8Array(exports.memory.buffer, namePtr, nameLen));
        const { tag, payload } = domEncode(node.dataset[name] ?? null);
        return domWrite(respPtr, respCap, tag, payload);
      },
      slag_host_dom_dataset_set: (id, namePtr, nameLen, valuePtr, valueLen) => {
        const node = domNode(id);
        if (!node) return;
        const name = decoder.decode(new Uint8Array(exports.memory.buffer, namePtr, nameLen));
        const value = decoder.decode(new Uint8Array(exports.memory.buffer, valuePtr, valueLen));
        if (!node.dataset) node.dataset = {};
        node.dataset[name] = value;
      },
      slag_host_copy: (textPtr, textLen) => {
        const text = decoder.decode(new Uint8Array(exports.memory.buffer, textPtr, textLen));
        return copyNative(text) ? 1 : 0;
      },
      // Sandbox-run requests from the dogfood UI: engine code cannot reach
      // the separate user engine, so it asks the host to evaluate there.
      // `host.userRun`/`host.userReset` (supplied by the page) own the
      // deferral; the import only forwards the request.
      slag_host_user_run: (textPtr, textLen) => {
        const source = decoder.decode(new Uint8Array(exports.memory.buffer, textPtr, textLen));
        if (typeof host.userRun === 'function') host.userRun(source);
      },
      slag_host_user_reset: () => {
        if (typeof host.userReset === 'function') host.userReset();
      },
      slag_host_dom_listen: (id, typePtr, typeLen) => {
        const node = domNode(id);
        if (!node) return;
        const type = decoder.decode(new Uint8Array(exports.memory.buffer, typePtr, typeLen));
        const key = `${id}:${type}`;
        if (dom.listeners.has(key)) return;
        dom.listeners.add(key);
        node.addEventListener(type, (nativeEvent) => {
          if (typeof exports.slag_dom_event !== 'function') return;
          const typeBytes = new TextEncoder().encode(type);
          const propsBytes = domEventProps(nativeEvent);
          const typeLen = typeBytes.length;
          const propsLen = propsBytes.length;
          const typePtr = exports.slag_alloc(typeLen);
          const propsPtr = exports.slag_alloc(propsLen);
          if ((typeLen > 0 && typePtr === 0) || (propsLen > 0 && propsPtr === 0)) {
            if (typeLen > 0) exports.slag_dealloc(typePtr, typeLen);
            if (propsLen > 0) exports.slag_dealloc(propsPtr, propsLen);
            return;
          }
          if (typeLen > 0) new Uint8Array(exports.memory.buffer, typePtr, typeLen).set(typeBytes);
          if (propsLen > 0) new Uint8Array(exports.memory.buffer, propsPtr, propsLen).set(propsBytes);
          let result = 0;
          try {
            result = exports.slag_dom_event(id, typePtr, typeLen, propsPtr, propsLen);
          } finally {
            if (typeLen > 0) exports.slag_dealloc(typePtr, typeLen);
            if (propsLen > 0) exports.slag_dealloc(propsPtr, propsLen);
          }
          if (result < 0) {
            const errorText = readResult();
            if (errorText) consoleTarget(sink, 3)(`[dom event failed] ${errorText}`);
          } else if (result > 0 && typeof nativeEvent.preventDefault === 'function') {
            nativeEvent.preventDefault();
          }
          // Handlers ran inside the engine. Jobs they queued (microtask
          // continuations, timers already due) only progress when drained,
          // and engine timers they scheduled need the host drain armed —
          // api.eval/api.drain do both after a run, so do the same here.
          if (typeof exports.slag_drain === 'function' && exports.slag_drain() !== 0) {
            const text = readResult();
            if (text) consoleTarget(sink, 3)(`[dom event job failed] ${text}`);
          }
          armTimer();
        });
      },
      slag_host_storage_get: (keyPtr, keyLen, respPtr, respCap) => {
        const key = decoder.decode(new Uint8Array(exports.memory.buffer, keyPtr, keyLen));
        const { tag, payload } = domEncode(localStorage.getItem(key));
        return domWrite(respPtr, respCap, tag, payload);
      },
      slag_host_storage_set: (keyPtr, keyLen, valuePtr, valueLen) => {
        const key = decoder.decode(new Uint8Array(exports.memory.buffer, keyPtr, keyLen));
        const value = domReadValue(valuePtr, valueLen);
        localStorage.setItem(key, value === null ? '' : String(value));
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

  // One host timer at a time, armed for the *earliest* pending engine
  // timer deadline; drain when it fires and re-arm. Engine code can queue
  // timers from DOM-event dispatch too, so re-arm when a newer timer has an
  // earlier deadline than the one already armed (otherwise it would fire
  // late, when the existing host timer finally drains).
  let timer = null;
  let timerDeadline = Infinity;
  const clearTimer = () => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    timerDeadline = Infinity;
  };
  const monotonicNow = () =>
    typeof performance !== 'undefined' && typeof performance.now === 'function'
      ? performance.now()
      : Date.now();
  const armTimer = () => {
    if (!autoDrain) return;
    const delay = exports.slag_next_timeout_ms();
    if (!(delay >= 0)) return; // -1 (or NaN): nothing pending
    const deadline = monotonicNow() + delay;
    if (timer !== null && deadline >= timerDeadline) return;
    clearTimer();
    timer = setTimeout(() => {
      timer = null;
      timerDeadline = Infinity;
      if (exports.slag_drain() !== 0) {
        const text = readResult();
        if (text) consoleTarget(sink, 3)(`[timer job failed] ${text}`);
      }
      armTimer();
    }, Math.max(0, Math.ceil(delay)));
    timerDeadline = deadline;
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
