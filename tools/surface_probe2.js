// Topic probes for the deno surface checklist (tools/deno_surface.js): each
// `which` selector isolates one subsystem, so a failure can be reduced without
// running the whole checklist.
//
// Usage: ./deno/target/debug/deno.exe run -A tools/surface_probe2.js randomBytes
//
// Selectors: randomBytes, revoke, broadcast, messagechannel, worker, streams.
const which = process.argv[2];

if (which === "randomBytes") {
  const { randomBytes } = await import("node:crypto");
  const b = randomBytes(8);
  console.log("type:", typeof b, "ctor:", b?.constructor?.name);
  console.log("length:", b.length, "byteLength:", b.byteLength);
  console.log("isUint8Array:", b instanceof Uint8Array);
  console.log("Buffer.isBuffer:", (await import("node:buffer")).Buffer.isBuffer(b));
  console.log("String(b):", String(b));
  console.log("Object.keys:", Object.keys(b));
  console.log("getPrototypeOf name:", Object.getPrototypeOf(b)?.constructor?.name);
}

if (which === "revoke") {
  const before = await Deno.permissions.query({ name: "env" });
  console.log("before:", before.state, typeof before.state);
  const revoked = await Deno.permissions.revoke({ name: "env" });
  console.log("revoked:", revoked, typeof revoked);
  console.log("revoked.state:", revoked?.state, typeof revoked?.state);
  console.log("revoked.partial:", revoked?.partial);
  console.log("keys:", revoked && Object.getOwnPropertyNames(Object.getPrototypeOf(revoked)));
  const sync = Deno.permissions.revokeSync({ name: "env" });
  console.log("revokeSync state:", sync?.state, typeof sync?.state);
}

if (which === "broadcast") {
  const channel = new BroadcastChannel("probe");
  console.log("constructed:", typeof channel);
  let got = null;
  channel.onmessage = (event) => {
    got = event.data;
  };
  console.log("posting");
  channel.postMessage({ a: 1 });
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 25));
  console.log("received:", JSON.stringify(got));
  channel.close();
}

if (which === "messagechannel") {
  const { port1, port2 } = new MessageChannel();
  let got = null;
  port2.onmessage = (event) => {
    got = event.data;
  };
  port1.postMessage("through");
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 25));
  console.log("received:", JSON.stringify(got));
  port1.close();
  port2.close();
}

if (which === "worker") {
  // The worker bootstrap fails in setLocationHref; run it and print the stack.
  const worker = new Worker(new URL("./surface_worker.js", import.meta.url).href, {
    type: "module",
  });
  const done = await new Promise((resolve) => {
    worker.onmessage = (event) => resolve(["message", event.data]);
    worker.onerror = (event) => resolve(["error", (event.error && event.error.stack) || event.message]);
    setTimeout(() => resolve(["timeout", null]), 3000);
  });
  console.log("worker outcome:", done[0]);
  console.log(String(done[1]).split("\n").slice(0, 6).join("\n"));
  worker.terminate();
}

if (which === "streams") {
  // The node:stream Readable path, which fails with a leaf-body error.
  const { Readable } = await import("node:stream");
  const chunks = [];
  for await (const chunk of Readable.from([Buffer.from("a"), Buffer.from("b")])) {
    chunks.push(chunk.toString());
  }
  console.log("Readable.from:", chunks.join(""));
  const stream = Readable.from(["x"]);
  const single = await new Promise((resolve, reject) => {
    stream.on("data", (chunk) => resolve(String(chunk)));
    stream.on("error", reject);
  });
  console.log("single:", single);
}
