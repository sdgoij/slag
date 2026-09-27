// Broad Deno API smoke test: every case runs independently and reports, so one
// run surfaces the whole list of gaps instead of stopping at the first.
//
// Runs under a deno built with Slag as its engine (deno/ links the v8 crate
// bridge in this repo):
//
//     (cd deno && cargo build -p deno)   # needs deno/'s own .cargo config
//     ./deno/target/debug/deno.exe run -A tools/deno_smoke.js
//
// The cases are ECMAScript and web-platform surface, so upstream deno (V8) is
// the reference: a case that behaves differently there is this engine's gap.
// tools/deno_surface.js is the companion checklist for what a host leans on
// (workers, serve/HTTP, node: builtins, streams).

const failures = [];
let passed = 0;

async function check(name, fn) {
  try {
    await fn();
    passed++;
    console.log("ok   " + name);
  } catch (error) {
    failures.push([name, String(error && error.stack || error)]);
    console.log("FAIL " + name + ": " + (error && error.message || error));
  }
}

function eq(actual, expected, what) {
  const a = typeof actual === "string" ? actual : JSON.stringify(actual);
  const b = typeof expected === "string" ? expected : JSON.stringify(expected);
  if (a !== b) throw new Error((what || "value") + " = " + a + " want " + b);
}

// ---- language core -------------------------------------------------------
await check("numbers/bitwise/unary", () => {
  eq(~5, -6, "~5");
  eq(- -3, 3, "-(-3)");
  eq(+"42", 42, "+'42'");
  eq(typeof undefinedName, "undefined", "typeof free");
  eq(1 / -0, -Infinity, "1/-0");
  eq(0.1 + 0.2, 0.30000000000000004, "float");
  eq(2 ** 53 + 1, 9007199254740992, "precision");
});

await check("bigint", () => {
  eq(typeof 10n, "bigint", "typeof bigint");
  eq((2n ** 64n).toString(), "18446744073709551616", "2n**64n");
  eq(Number(5n) + 1, 6, "Number(5n)+1");
  eq(BigInt.asIntN(8, 200n).toString(), "-56", "asIntN");
});

await check("strings", () => {
  eq("abc".at(-1), "c", "at");
  eq("a-b-c".replaceAll("-", "+"), "a+b+c", "replaceAll");
  eq([..."a\u{1F600}b"].length, 3, "spread length");
  eq("ß".toUpperCase(), "SS", "case");
  eq("  x ".trim(), "x", "trim");
  eq("abc".padStart(5, "0"), "00abc", "padStart");
  eq(String.fromCodePoint(0x1F600).length, 2, "fromCodePoint");
  eq("a\u0301".normalize("NFC").length, 1, "normalize");
  eq(/(?<y>\d+)/.exec("x12").groups.y, "12", "named group");
});

await check("arrays", () => {
  eq([1, [2, [3]]].flat(2), [1, 2, 3], "flat");
  eq([3, 1, 2].toSorted(), [1, 2, 3], "toSorted");
  eq([1, 2, 3].toReversed(), [3, 2, 1], "toReversed");
  eq([1, 2, 3].findLast((x) => x < 3), 2, "findLast");
  eq(Array.from({ length: 3 }, (_, i) => i), [0, 1, 2], "Array.from");
  eq([1, 2, 3].with(0, 9), [9, 2, 3], "with");
  const [a, ...rest] = [1, 2, 3];
  eq([a, rest], [1, [2, 3]], "destructure");
});

await check("collections", () => {
  const m = new Map([["a", 1]]);
  m.set("b", 2);
  eq([...m.keys()], ["a", "b"], "Map keys");
  eq(new Set([1, 1, 2]).size, 2, "Set");
  const wm = new WeakMap();
  const key = {};
  wm.set(key, 1);
  eq(wm.get(key), 1, "WeakMap");
  eq(Object.groupBy([1, 2, 3], (x) => (x % 2 ? "odd" : "even")).odd, [1, 3], "Object.groupBy");
  eq(Map.groupBy([1, 2], (x) => x % 2).get(0), [2], "Map.groupBy");
});

await check("objects/reflect/proxy", () => {
  const o = { a: 1, b: 2 };
  eq(Object.entries(o), [["a", 1], ["b", 2]], "entries");
  eq(Object.fromEntries([["x", 1]]), { x: 1 }, "fromEntries");
  eq(Object.hasOwn(o, "a"), true, "hasOwn");
  eq(Reflect.ownKeys({ a: 1 }).length, 1, "ownKeys");
  const p = new Proxy({}, { get: (_, k) => (k === "z" ? 7 : undefined) });
  eq(p.z, 7, "proxy get");
  eq("#x" in {}, false, "private in");
});

await check("classes", () => {
  class A {
    #x = 1;
    static s = 2;
    static #sp = 3;
    static { this.s += 1; }
    get x() { return this.#x; }
    static get sp() { return A.#sp; }
  }
  class B extends A {
    constructor() { super(); this.y = this.x + 1; }
    toString() { return "B" + this.y; }
  }
  eq(new A().x, 1, "private field");
  eq(A.s, 3, "static block");
  eq(A.sp, 3, "static private");
  eq(String(new B()), "B2", "derived");
});

await check("async", async () => {
  const p = Promise.withResolvers();
  p.resolve(5);
  eq(await p.promise, 5, "withResolvers");
  eq(await Promise.all([1, Promise.resolve(2)]), [1, 2], "all");
  eq((await Promise.allSettled([Promise.reject(1)])).length, 1, "allSettled");
  const order = [];
  queueMicrotask(() => order.push("m"));
  order.push("s");
  await Promise.resolve();
  eq(order, ["s", "m"], "microtasks");
});

await check("generators", () => {
  function* g() { yield 1; yield* [2, 3]; }
  eq([...g()], [1, 2, 3], "generator");
  const it = g();
  eq([it.next().value, it.next().value], [1, 2], "manual");
});

await check("async generators + for-await", async () => {
  async function* ag() { yield 1; await null; yield 2; }
  const out = [];
  for await (const v of ag()) out.push(v);
  eq(out, [1, 2], "for-await");
});

await check("iterators/spread", () => {
  const o = { *[Symbol.iterator]() { yield 1; yield 2; } };
  eq([...o], [1, 2], "custom iterable");
  eq([...new Set([1, 1, 2])], [1, 2], "set spread");
  eq([...new Uint8Array([1, 2])], [1, 2], "typed array spread");
});

await check("regexp", () => {
  eq("a1b2".match(/\d/g), ["1", "2"], "match");
  eq([...("a1b2".matchAll(/(\d)/g))].length, 2, "matchAll");
  eq(/(?<=a)b/.test("ab"), true, "lookbehind");
  eq(/\p{Letter}/u.test("é"), true, "unicode prop");
  eq("x".replace(/x/, (m) => m.toUpperCase()), "X", "replace fn");
});

await check("text encoding", () => {
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const bytes = enc.encode("héllo → 😀");
  eq(Array.from(bytes).length, 15, "utf8 length");
  eq(dec.decode(bytes), "héllo → 😀", "roundtrip");
  eq(dec.decode(new Uint8Array([0xff]), { fatal: true }).length >= 0, true, "fatal");
});

await check("url", () => {
  const u = new URL("https://a.example/x/y?q=1#f");
  eq(u.hostname, "a.example", "hostname");
  eq(u.pathname, "/x/y", "pathname");
  eq(u.searchParams.get("q"), "1", "searchParams");
  eq(new URLSearchParams("a=1&b=2").getAll("a"), ["1"], "usp");
  eq(encodeURIComponent("a b"), "a%20b", "encodeURIComponent");
  eq(new URLPattern({ pathname: "/x/:id" }).test("https://h/x/1"), true, "URLPattern");
});

await check("json", () => {
  eq(JSON.stringify({ a: [1, null, true] }), '{"a":[1,null,true]}', "stringify");
  eq(JSON.parse('{"a":1}').a, 1, "parse");
  eq(JSON.stringify({ a: undefined, b: () => 1 }), "{}", "skip fn");
  eq(JSON.parse(JSON.stringify({ d: new Date(0) })).d, "1970-01-01T00:00:00.000Z", "date");
});

await check("intl", () => {
  eq(new Intl.NumberFormat("en-US").format(1234.5), "1,234.5", "numfmt");
  eq(typeof new Intl.DateTimeFormat("en-US", { timeZone: "UTC" }).format(0), "string", "dtfmt");
  eq("a".localeCompare("b") < 0, true, "collator");
});

await check("structured clone", () => {
  const src = { a: 1, b: new Map([["k", [1, 2]]]), c: new Uint8Array([1, 2]) };
  const copy = structuredClone(src);
  eq(copy.b.get("k"), [1, 2], "map");
  eq(copy.c instanceof Uint8Array, true, "u8");
  eq(structuredClone(new Date(0)).getTime(), 0, "date");
});

await check("typed arrays / buffers", () => {
  const b = new ArrayBuffer(8);
  const v = new DataView(b);
  v.setFloat64(0, 1.5);
  eq(v.getFloat64(0), 1.5, "dataview");
  eq(new Uint8Array([1, 2]).byteLength, 2, "byteLength");
  eq(ArrayBuffer.isView(new Int16Array(1)), true, "isView");
  eq(new Uint8Array(b, 4).length, 4, "slice view");
  eq(Atomics.load(new Int32Array(new SharedArrayBuffer(4)), 0), 0, "atomics");
});

await check("errors", () => {
  try { null.x; } catch (e) { eq(e instanceof TypeError, true, "TypeError"); }
  try { undefinedName; } catch (e) { eq(e instanceof ReferenceError, true, "ReferenceError"); }
  class My extends Error {}
  eq(new My("m").message, "m", "subclass");
  eq(new AggregateError([], "a").errors.length, 0, "AggregateError");
  eq(Error("e", { cause: 1 }).cause, 1, "cause");
});

await check("eval/Function", () => {
  eq(eval("1+1"), 2, "eval");
  const f = new Function("a", "b", "return a + b;");
  eq(f(1, 2), 3, "Function");
  const g = (0, eval)("typeof this");
  eq(g, "object", "indirect eval");
});

await check("events", () => {
  const target = new EventTarget();
  let seen = 0;
  target.addEventListener("x", () => seen++);
  target.dispatchEvent(new Event("x"));
  eq(seen, 1, "dispatch");
  const ce = new CustomEvent("c", { detail: 5 });
  eq(ce.detail, 5, "detail");
  const ac = new AbortController();
  ac.abort();
  eq(ac.signal.aborted, true, "abort");
});

await check("streams", async () => {
  const rs = new ReadableStream({
    start(c) { c.enqueue("a"); c.enqueue("b"); c.close(); },
  });
  const out = [];
  for await (const chunk of rs) out.push(chunk);
  eq(out, ["a", "b"], "readable");
  const t = new TransformStream({
    transform(chunk, c) { c.enqueue(chunk + "!"); },
  });
  const w = t.writable.getWriter();
  w.write("x").then(() => w.close());
  eq(await t.readable.getReader().read().then((r) => r.value), "x!", "transform");
});

await check("timers", async () => {
  const t0 = Date.now();
  await new Promise((r) => setTimeout(r, 20));
  eq(Date.now() - t0 >= 10, true, "setTimeout");
  const id = setInterval(() => {}, 1000);
  clearInterval(id);
  eq(typeof Deno.unrefTimer, "function", "unrefTimer");
});

await check("crypto", async () => {
  const raw = crypto.getRandomValues(new Uint8Array(4));
  eq(raw.length, 4, "getRandomValues");
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode("abc"));
  eq(digest.byteLength, 32, "digest");
  const k = await crypto.subtle.importKey("raw", new Uint8Array(32).fill(1), {
    name: "HMAC",
    hash: "SHA-256",
  }, false, ["sign"]);
  const sig = await crypto.subtle.sign("HMAC", k, new Uint8Array([1]));
  eq(sig.byteLength, 32, "hmac");
});

// ---- Deno APIs -----------------------------------------------------------
await check("Deno basics", () => {
  eq(typeof Deno.version.deno, "string", "version");
  eq(typeof Deno.build.os, "string", "build.os");
  eq(Array.isArray(Deno.args), true, "args");
  eq(typeof Deno.cwd(), "string", "cwd");
  eq(typeof Deno.pid, "number", "pid");
  eq(typeof Deno.inspect({ a: 1 }), "string", "inspect");
  // The public `Deno` has no `core` after bootstrap: the surface lives at
  // `Deno[Deno.internal].core` (deno scrubs the public one — see
  // `libs/core/runtime/bindings.rs` on `Deno.core`).
  eq(typeof Deno[Deno.internal].core.ops, "object", "internal core.ops");
  eq(Deno.core === undefined, true, "public Deno.core is scrubbed");
});

await check("Deno fs sync", () => {
  const dir = Deno.makeTempDirSync();
  Deno.writeTextFileSync(dir + "/a.txt", "hello");
  eq(Deno.readTextFileSync(dir + "/a.txt"), "hello", "read");
  eq(Deno.statSync(dir + "/a.txt").size, 5, "stat");
  // `readDirSync` answers an iterable of entries, not an array.
  eq([...Deno.readDirSync(dir)].length, 1, "readDir");
  Deno.renameSync(dir + "/a.txt", dir + "/b.txt");
  Deno.copyFileSync(dir + "/b.txt", dir + "/c.txt");
  eq([...Deno.readDirSync(dir)].length, 2, "copy");
  Deno.removeSync(dir, { recursive: true });
  let gone = false;
  try {
    Deno.statSync(dir);
  } catch {
    gone = true;
  }
  eq(gone, true, "removed");
});

await check("Deno fs async", async () => {
  const dir = await Deno.makeTempDir();
  await Deno.writeTextFile(dir + "/a.txt", "hi");
  eq(await Deno.readTextFile(dir + "/a.txt"), "hi", "read");
  const file = await Deno.open(dir + "/a.txt", { read: true });
  const buf = new Uint8Array(2);
  await file.read(buf);
  file.close();
  eq(Array.from(buf), [104, 105], "read bytes");
  await Deno.remove(dir, { recursive: true });
});

await check("Deno errors", () => {
  let caught;
  try { Deno.readTextFileSync("no/such/file/at/all"); } catch (e) { caught = e; }
  eq(caught instanceof Deno.errors.NotFound, true, "NotFound");
});

await check("Deno env", () => {
  Deno.env.set("SLAG_SMOKE", "1");
  eq(Deno.env.get("SLAG_SMOKE"), "1", "get");
  Deno.env.delete("SLAG_SMOKE");
  eq(Deno.env.get("SLAG_SMOKE"), undefined, "deleted");
  eq(typeof Deno.env.toObject(), "object", "toObject");
  eq(typeof Deno.env.get("PATH"), "string", "PATH");
});

await check("Deno permissions", () => {
  eq(Deno.permissions.querySync({ name: "read", path: "." }).state, "granted", "read granted");
});

await check("Deno command", async () => {
  const out = await new Deno.Command(Deno.execPath(), {
    args: ["eval", "console.log('child ok')"],
  }).output();
  eq(new TextDecoder().decode(out.stdout).trim(), "child ok", "child stdout");
  eq(out.code, 0, "exit code");
});

await check("import.meta and dynamic import", async () => {
  eq(typeof import.meta.url, "string", "import.meta.url");
  eq(typeof import.meta.dirname, "string", "dirname");
  const mod = await import("./deno_smoke_dep.js");
  eq(mod.answer, 42, "dynamic import");
});

// ---- report --------------------------------------------------------------
console.log("");
console.log(passed + " passed, " + failures.length + " failed");
if (failures.length > 0) {
  console.log("");
  for (const [name, stack] of failures) {
    console.log("=== " + name + " ===");
    console.log(stack.split("\n").slice(0, 4).join("\n"));
  }
  Deno.exit(1);
}
