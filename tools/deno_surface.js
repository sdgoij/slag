// A broad Deno surface checklist: every case runs independently under a
// deadline and reports, so one run surfaces the whole list of gaps instead of
// stopping at the first. Complements deno_smoke.js (the language core) with the
// parts of Deno a host actually leans on: workers, serve/HTTP, node: builtins,
// streams, more of the crypto and fs surface.
//
// Runs under a deno built with Slag as its engine (deno/ links the v8 crate
// bridge in this repo):
//
//     (cd deno && cargo build -p deno)   # needs deno/'s own .cargo config
//     ./deno/target/debug/deno.exe run -A tools/deno_surface.js

const failures = [];
let passed = 0;
let skipped = 0;

const DEADLINE_MS = 10000;

// A prefix bound for bisecting: `deno run -A target/deno_surface.js 12` runs the
// first twelve cases and exits, which is how a failure that needs earlier cases
// (a collection, a promoted object) is reduced to the shortest prefix that
// still reproduces it.
const STOP_AFTER = Number(Deno.args[0] ?? 0);

function withDeadline(promise, name, ms) {
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(
      () => reject(new Error("timed out after " + ms + "ms")),
      ms,
    );
  });
  return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
}

async function check(name, fn, deadline = DEADLINE_MS) {
  const started = Date.now();
  try {
    await withDeadline(Promise.resolve().then(fn), name, deadline);
    passed++;
    console.log("ok   " + name + "  (" + (Date.now() - started) + "ms)");
  } catch (error) {
    const detail = error && error.stack ? String(error.stack).split("\n").slice(0, 2).join(" | ") : String(error);
    failures.push([name, detail]);
    console.log("FAIL " + name + ": " + (error && error.message || error));
  }
  if (STOP_AFTER && passed + failures.length >= STOP_AFTER) {
    console.log("stopping after " + STOP_AFTER + " cases");
    Deno.exit(0);
  }
}

function eq(actual, expected, what) {
  const a = typeof actual === "string" ? actual : JSON.stringify(actual);
  const b = typeof expected === "string" ? expected : JSON.stringify(expected);
  if (a !== b) throw new Error((what || "value") + " = " + a + " want " + b);
}

function truthy(value, what) {
  if (!value) throw new Error((what || "value") + " is falsy");
}

// Host paths use backslashes on Windows, so path comparisons normalize.
function slash(text) {
  return text.replace(/\\/g, "/");
}

// ---- structural / weak references ---------------------------------------
await check("WeakRef + FinalizationRegistry", async () => {
  let collected = null;
  const registry = new FinalizationRegistry((held) => {
    collected = held;
  });
  let target = { big: new Array(1000).fill(0) };
  const ref = new WeakRef(target);
  registry.register(target, "gone");
  truthy(ref.deref() === target, "deref before drop");
  target = null;
  for (let i = 0; i < 50; i++) await new Promise((r) => setTimeout(r, 10));
  // The collector may not have run yet: either answer is legal, but the
  // registry must not have thrown and the ref must not read a stale box.
  if (ref.deref() !== undefined) truthy(ref.deref().big.length === 1000, "live target");
});

// ---- richer crypto (the class-template path, harder) ---------------------
await check("crypto subtle: AES-GCM + PBKDF2 + ECDSA", async () => {
  const raw = new Uint8Array(32).fill(7);
  const base = await crypto.subtle.importKey("raw", raw, "AES-GCM", false, ["encrypt", "decrypt"]);
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const cipher = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, base, new TextEncoder().encode("secret"));
  const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, base, cipher);
  eq(new TextDecoder().decode(plain), "secret", "AES-GCM round trip");

  const material = await crypto.subtle.importKey("raw", new TextEncoder().encode("pw"), "PBKDF2", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits({ name: "PBKDF2", salt: new Uint8Array(8), iterations: 1000, hash: "SHA-256" }, material, 256);
  eq(bits.byteLength, 32, "PBKDF2 bits");

  const key = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, ["sign", "verify"]);
  const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key.privateKey, new Uint8Array([1, 2, 3]));
  const ok = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key.publicKey, sig, new Uint8Array([1, 2, 3]));
  eq(ok, true, "ECDSA verify");
});

await check("crypto classes are real classes", () => {
  const raw = crypto.getRandomValues(new Uint8Array(4));
  truthy(objectToString(raw) === "[object Uint8Array]", "typed array tag");
  const key = crypto.subtle;
  eq(typeof key.digest, "function", "subtle.digest");
  eq(typeof key.generateKey, "function", "subtle.generateKey");
  truthy(Object.getPrototypeOf(crypto) !== Object.prototype, "crypto has a real prototype");
  truthy(crypto instanceof Object, "instanceof Object");
});

// A function template's `.prototype` names its constructor back, as V8's does, so
// an instance's `constructor` is the class rather than `Object`.
await check("crypto classes are real classes (prototype.constructor)", () => {
  eq(Object.getPrototypeOf(crypto).constructor.name, "Crypto", "constructor name");
  eq(crypto.constructor, Object.getPrototypeOf(crypto).constructor, "the instance reaches it too");
});

function objectToString(value) {
  return Object.prototype.toString.call(value);
}

// ---- node: builtins ------------------------------------------------------
await check("node:path / node:os / node:buffer", async () => {
  const path = await import("node:path");
  eq(slash(path.join("a", "b", "..", "c")), "a/c", "path.join");
  eq(path.basename("/x/y/z.txt"), "z.txt", "path.basename");
  eq(path.extname("a.tar.gz"), ".gz", "path.extname");
  eq(path.isAbsolute("C:/x"), true, "path.isAbsolute");

  const os = await import("node:os");
  eq(typeof os.platform(), "string", "os.platform");
  eq(typeof os.tmpdir(), "string", "os.tmpdir");

  const { Buffer } = await import("node:buffer");
  const b = Buffer.from("hello");
  eq(b.toString("utf8"), "hello", "Buffer round trip");
  eq(b.toString("base64"), "aGVsbG8=", "Buffer base64");
  eq(Buffer.from("aGVsbG8=", "base64").toString(), "hello", "Buffer from base64");
  eq(Buffer.byteLength("abc"), 3, "Buffer.byteLength");
});

await check("node:fs sync + promises", async () => {
  const fs = await import("node:fs");
  const fsp = await import("node:fs/promises");
  const dir = await fsp.mkdtemp("surface-");
  fs.writeFileSync(dir + "/a.txt", "node-fs");
  eq(fs.readFileSync(dir + "/a.txt", "utf8"), "node-fs", "readFileSync");
  eq(fs.existsSync(dir + "/a.txt"), true, "existsSync");
  const listed = fs.readdirSync(dir);
  eq(listed.length, 1, "readdirSync");
  await fsp.unlink(dir + "/a.txt");
  eq(fs.existsSync(dir + "/a.txt"), false, "unlink");
  await fsp.rmdir(dir);
});

await check("node:events + node:util", async () => {
  const { EventEmitter, once } = await import("node:events");
  const emitter = new EventEmitter();
  const seen = [];
  emitter.on("x", (v) => seen.push(v));
  emitter.emit("x", 1);
  emitter.emit("x", 2);
  eq(seen, [1, 2], "EventEmitter.on");
  const waiting = once(emitter, "y");
  emitter.emit("y", 42);
  eq((await waiting)[0], 42, "events.once");

  const util = await import("node:util");
  eq(util.inspect({ a: 1 }), "{ a: 1 }", "util.inspect");
  eq(await util.promisify((v, cb) => cb(null, v * 2))(21), 42, "util.promisify");
  eq(util.format("%s-%d", "a", 3), "a-3", "util.format");
});

await check("node:crypto + node:assert + node:url", async () => {
  const part = Deno.args[1] ?? "all";
  if (part === "crypto" || part === "all") {
    const { createHash, randomBytes } = await import("node:crypto");
    eq(createHash("sha256").update("abc").digest("hex"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "sha256");
    eq(randomBytes(8).length, 8, "randomBytes");
  }

  if (part === "assert" || part === "all") {
    const assert = (await import("node:assert")).default;
    assert.strictEqual(1, 1);
    assert.deepStrictEqual({ a: [1] }, { a: [1] });
    let threw = false;
    try {
      assert.strictEqual(1, 2);
    } catch {
      threw = true;
    }
    truthy(threw, "assert throws");
  }

  if (part === "url" || part === "all") {
    const url = await import("node:url");
    eq(url.fileURLToPath("file:///C:/x/y.txt").replace(/\\/g, "/"), "C:/x/y.txt", "fileURLToPath");
  }
});

await check("node:stream Readable", async () => {
  const { Readable } = await import("node:stream");
  const chunks = [];
  for await (const chunk of Readable.from([Buffer.from("a"), Buffer.from("b")])) {
    chunks.push(chunk.toString());
  }
  eq(chunks.join(""), "ab", "Readable.from");
});

await check("node:process / globalThis.process", async () => {
  const proc = globalThis.process ?? (await import("node:process")).default;
  eq(typeof proc.platform, "string", "process.platform");
  truthy(Array.isArray(proc.argv), "process.argv");
  eq(typeof proc.cwd(), "string", "process.cwd");
  eq(typeof proc.env, "object", "process.env");
  eq(proc.version.startsWith("v"), true, "process.version");
});

// ---- web platform --------------------------------------------------------
await check("fetch + Deno.serve (loopback)", async () => {
  const server = Deno.serve({ port: 0, hostname: "127.0.0.1", onListen: () => {} }, (request) => {
    const url = new URL(request.url);
    if (url.pathname === "/json") {
      return Response.json({ path: url.pathname, method: request.method });
    }
    return new Response("hello " + url.searchParams.get("name"), {
      headers: { "content-type": "text/plain" },
    });
  });
  const port = server.addr.port;
  const text = await (await fetch("http://127.0.0.1:" + port + "/?name=slag")).text();
  eq(text, "hello slag", "text body");
  const json = await (await fetch("http://127.0.0.1:" + port + "/json")).json();
  eq(json.path, "/json", "Response.json");
  await server.shutdown();
});

await check("Request/Response/Headers/AbortController", async () => {
  const request = new Request("http://x/y?z=1", { method: "PUT", body: "body" });
  eq(request.method, "PUT", "method");
  eq(new URL(request.url).searchParams.get("z"), "1", "search");
  eq(await request.text(), "body", "request body");

  const response = new Response("abc", { headers: { etag: "1" } });
  eq(response.headers.get("etag"), "1", "headers");
  eq(await response.text(), "abc", "response body");

  const controller = new AbortController();
  controller.abort(new Error("stop"));
  eq(controller.signal.aborted, true, "aborted");
  eq(controller.signal.reason.message, "stop", "abort reason");
  eq(AbortSignal.timeout(1000) instanceof AbortSignal, true, "AbortSignal.timeout");
});

await check("streams: ReadableStream transform + pipe", async () => {
  // Byte chunks, because `TextDecoderStream` decodes bytes: a stream of strings
  // is what a wrong test hands it.
  const bytes = new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode("a"));
      controller.enqueue(new TextEncoder().encode("b"));
      controller.close();
    },
  });
  const upper = new TransformStream({
    transform(chunk, controller) {
      controller.enqueue(new TextEncoder().encode(new TextDecoder().decode(chunk).toUpperCase()));
    },
  });
  let out = "";
  for await (const chunk of bytes.pipeThrough(upper).pipeThrough(new TextDecoderStream())) {
    out += chunk;
  }
  eq(out, "AB", "pipeThrough");

  const reader = new ReadableStream({
    start(controller) {
      controller.enqueue(1);
      controller.close();
    },
  }).getReader();
  eq((await reader.read()).value, 1, "default reader");
});

await check("WebSocket round trip", async () => {
  const server = Deno.serve({ port: 0, hostname: "127.0.0.1", onListen: () => {} }, (request) => {
    const { socket, response } = Deno.upgradeWebSocket(request);
    socket.onmessage = (event) => socket.send("echo:" + event.data);
    return response;
  });
  const socket = new WebSocket("ws://127.0.0.1:" + server.addr.port + "/");
  const reply = await new Promise((resolve, reject) => {
    socket.onopen = () => socket.send("ping");
    socket.onmessage = (event) => resolve(event.data);
    socket.onerror = (event) => reject(new Error("ws error: " + (event.message || "?")));
  });
  eq(reply, "echo:ping", "websocket echo");
  socket.close();
  await server.shutdown();
});

await check("WebAssembly instantiate", async () => {
  // (module (func (export "add") (param i32 i32) (result i32) (i32.add (local.get 0) (local.get 1)))
  //         (memory (export "mem") 1))
  const bytes = new Uint8Array([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f,
    0x03, 0x02, 0x01, 0x00,
    0x05, 0x03, 0x01, 0x00, 0x01,
    0x07, 0x0d, 0x02, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, 0x03, 0x6d, 0x65, 0x6d, 0x02, 0x00,
    0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b,
  ]);
  const { instance } = await WebAssembly.instantiate(bytes);
  eq(instance.exports.add(2, 3), 5, "wasm add");
  truthy(instance.exports.mem instanceof WebAssembly.Memory, "wasm memory export");
  truthy(WebAssembly.validate(bytes), "validate");
});

await check("URLPattern + atob/btoa + reportError", () => {
  const pattern = new URLPattern({ pathname: "/users/:id" });
  const matched = pattern.exec("https://x/users/42");
  eq(matched.pathname.groups.id, "42", "URLPattern groups");
  eq(atob(btoa("hi")), "hi", "atob/btoa");
  eq(typeof reportError, "function", "reportError");
});

await check("BroadcastChannel + MessageChannel + postMessage", async () => {
  // Two channels: the spec delivers to every same-named channel *except* the
  // sender, so a lone channel that posts to itself receives nothing.
  const sender = new BroadcastChannel("surface");
  const receiver = new BroadcastChannel("surface");
  const received = new Promise((resolve) => {
    receiver.onmessage = (event) => resolve(event.data);
  });
  sender.postMessage({ a: 1 });
  eq((await received).a, 1, "BroadcastChannel");

  const { port1, port2 } = new MessageChannel();
  const fromPort = new Promise((resolve) => {
    port2.onmessage = (event) => resolve(event.data);
  });
  port1.postMessage("through");
  eq(await fromPort, "through", "MessageChannel");
  sender.close();
  receiver.close();
  port1.close();
  port2.close();
});

// ---- Deno APIs -----------------------------------------------------------
await check("Deno.consoleSize + isatty + memoryUsage + systemMemoryInfo", () => {
  eq(typeof Deno.isatty(1), "boolean", "isatty");
  const memory = Deno.memoryUsage();
  truthy(memory.heapTotal > 0, "heapTotal");
  truthy(Deno.systemMemoryInfo().total > 0, "systemMemoryInfo");
  eq(typeof Deno.hostname(), "string", "hostname");
  eq(typeof Deno.osRelease(), "string", "osRelease");
  eq(typeof Deno.umask(), "number", "umask");
});

await check("Deno.readDir async + stat + realPath + readLink", async () => {
  const dir = await Deno.makeTempDir();
  await Deno.mkdir(dir + "/sub");
  await Deno.writeTextFile(dir + "/file.txt", "x");
  const names = [];
  for await (const entry of Deno.readDir(dir)) names.push(entry.name);
  eq(names.toSorted(), ["file.txt", "sub"], "readDir async");
  const info = await Deno.stat(dir + "/file.txt");
  eq(info.isFile, true, "isFile");
  truthy(info.mtime instanceof Date, "mtime is a Date");
  truthy((await Deno.realPath(dir + "/file.txt")).endsWith("file.txt"), "realPath");
  await Deno.remove(dir, { recursive: true });
});

await check("Deno symlink + readLink + lstat", async () => {
  const dir = await Deno.makeTempDir();
  await Deno.writeTextFile(dir + "/target.txt", "t");
  await Deno.symlink(dir + "/target.txt", dir + "/link.txt");
  eq(await Deno.readTextFile(dir + "/link.txt"), "t", "through symlink");
  eq(
    slash(await Deno.readLink(dir + "/link.txt")),
    slash(dir + "/target.txt"),
    "readLink",
  );
  eq((await Deno.lstat(dir + "/link.txt")).isSymlink, true, "lstat isSymlink");
  await Deno.remove(dir, { recursive: true });
});

await check("Deno.command with piped stdio", async () => {
  const child = new Deno.Command(Deno.execPath(), {
    args: ["eval", "console.log('from-child')"],
    stdout: "piped",
    stderr: "piped",
  }).spawn();
  const output = await child.output();
  eq(output.success, true, "success");
  eq(new TextDecoder().decode(output.stdout).trim(), "from-child", "stdout");
  const status = await child.status;
  eq(status.code, 0, "status code");
});

await check("Deno.connect TCP loopback (raw echo)", async () => {
  const listener = Deno.listen({ port: 0, hostname: "127.0.0.1" });
  const port = listener.addr.port;
  const server = (async () => {
    const connection = await listener.accept();
    const buffer = new Uint8Array(64);
    const n = await connection.read(buffer);
    await connection.write(buffer.subarray(0, n));
    connection.close();
  })();
  const client = await Deno.connect({ port, hostname: "127.0.0.1" });
  await client.write(new TextEncoder().encode("raw"));
  const reply = new Uint8Array(64);
  const n = await client.read(reply);
  eq(new TextDecoder().decode(reply.subarray(0, n)), "raw", "echo");
  client.close();
  await server;
  listener.close();
});

await check("Deno.errors instanceof + error kinds", async () => {
  let notFound = null;
  try {
    await Deno.stat("no-such-path-xyz");
  } catch (error) {
    notFound = error;
  }
  truthy(notFound instanceof Deno.errors.NotFound, "NotFound");
  truthy(notFound instanceof Error, "is an Error");
  truthy(new Deno.errors.PermissionDenied("x") instanceof Deno.errors.PermissionDenied, "construct");
  eq(typeof Deno.errors.BadResource, "function", "BadResource exists");
});

await check("Deno.permissions request/revoke/state", async () => {
  const status = await Deno.permissions.query({ name: "env" });
  eq(status.state, "granted", "env granted (run -A)");
  // A revoke drops the grant, so the new state is `prompt` ("ask on next use")
  // rather than `denied` — deno's own answer for a permission it no longer holds.
  // What `request` answers afterwards is a prompt-capability question (there is
  // no TTY here), so this case stops at the revoke.
  const revoked = await Deno.permissions.revoke({ name: "env" });
  eq(revoked.state, "prompt", "revoke state");
});

await check("Deno timers: unrefTimer + refTimer + interval", async () => {
  const id = setInterval(() => {}, 60_000);
  Deno.unrefTimer(id);
  Deno.refTimer(id);
  clearInterval(id);
  const order = [];
  await new Promise((resolve) => setTimeout(() => {
    order.push("timeout");
    resolve();
  }, 1));
  await new Promise((resolve) => queueMicrotask(() => {
    order.push("microtask");
    resolve();
  }));
  eq(order, ["timeout", "microtask"], "timers fired");
});

await check("Deno.inspect deep + custom inspect", () => {
  const deep = { a: [1, { b: new Map([[1, 2]]) }], c: new Date(0) };
  const text = Deno.inspect(deep, { depth: 5 });
  truthy(text.includes("Map"), "inspect shows Map");
  const custom = {
    [Symbol.for("Deno.customInspect")]() {
      return "CUSTOM";
    },
  };
  eq(Deno.inspect(custom), "CUSTOM", "custom inspect");
});

await check("import.meta + dynamic import of a data URL", async () => {
  eq(typeof import.meta.url, "string", "import.meta.url");
  truthy(import.meta.url.startsWith("file:"), "file url");
  const module = await import("data:text/javascript,export const answer = 42;");
  eq(module.answer, 42, "data url module");
});

await check("deno json: import a JSON module", async () => {
  const module = await import("data:application/json,{\"x\":1}", { with: { type: "json" } });
  eq(module.default.x, 1, "json module");
});

// ---- workers -------------------------------------------------------------
await check("Worker: module worker posts back", async () => {
  // In a child process: what a worker needs beyond this point can surface as a
  // bridge refusal, and the bridge expresses those as a Rust panic, which aborts
  // the process rather than throwing into this one. The child prints the worker's
  // answer as JSON, or fails in a way read off its own output.
  const { fileURLToPath } = await import("node:url");
  const child = fileURLToPath(new URL("./surface_worker_child.js", import.meta.url));
  const output = await new Deno.Command(Deno.execPath(), {
    args: ["run", "-A", child],
    stdout: "piped",
    stderr: "piped",
  }).output();
  const out = new TextDecoder().decode(output.stdout).trim();
  const err = new TextDecoder().decode(output.stderr).trim();
  truthy(output.success, "child exit " + output.code + ": " + (err || out).split("\n").slice(0, 3).join(" | "));
  const reply = JSON.parse(out);
  eq(reply.echo.hello, "worker", "echo");
  eq(reply.digest, 3, "worker Map/Set state");
  eq(reply.uuidLength, 36, "worker crypto");
  eq(reply.hasOps, true, "worker core ops");
  // A whole second deno boots here (its own snapshot, its own worker), so this
  // case needs a deadline measured in process starts rather than statements.
}, 60000);

// ---- report --------------------------------------------------------------
console.log("");
console.log(passed + " passed, " + failures.length + " failed" + (skipped ? ", " + skipped + " skipped" : ""));
for (const [name, detail] of failures) {
  console.log("");
  console.log("=== " + name + " ===");
  console.log(detail);
}
