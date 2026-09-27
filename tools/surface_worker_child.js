// The worker round trip, in its own process: a bridge limitation surfaces as a
// Rust panic (`V8Inspector::connect` refuses by design), and a panic aborts the
// parent rather than throwing into it, so the surface checklist runs this here
// and reads the answer off stdout.
const worker = new Worker(new URL("./surface_worker.js", import.meta.url).href, {
  type: "module",
});
const reply = await new Promise((resolve, reject) => {
  worker.onmessage = (event) => resolve(event.data);
  worker.onerror = (event) => reject(new Error("worker error: " + (event.message || "?")));
  worker.postMessage({ hello: "worker" });
  setTimeout(() => reject(new Error("worker did not answer within 5s")), 5000);
});
console.log(JSON.stringify(reply));
worker.terminate();
