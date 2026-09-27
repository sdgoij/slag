// A worker for the surface checklist: proves a second isolate, restored from the
// same snapshot, runs user code and can post back.
self.onmessage = (event) => {
  const map = new Map([["worker", 1]]);
  const set = new Set(["a", "b"]);
  const digest = map.get("worker") + set.size;
  const uuid = crypto.randomUUID();
  self.postMessage({
    echo: event.data,
    digest,
    uuidLength: uuid.length,
    hasOps: typeof Deno[Deno.internal]?.core?.ops === "object",
  });
};

// A second message route, so the main thread can check the worker's own
// `close()` path too.
self.addEventListener("message", (event) => {
  if (event.data === "close-me") self.close();
});
