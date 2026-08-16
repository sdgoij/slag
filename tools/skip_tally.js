// Tally the test262 fixtures the harness would skip, mirroring the skip
// taxonomy in crates/test262/src/lib.rs `run_fixture`. Runs on Slag itself
// via the CLI's `fs` host module: `slag skip_tally.js`.
"use strict";

const ALLOWED_INCLUDES = new Set([
  "assert.js",
  "compareArray.js",
  "detachArrayBuffer.js",
  "isConstructor.js",
  "propertyHelper.js",
  "testAtomics.js",
  "testTypedArray.js",
  "nativeErrors.js",
  "decimalToHexString.js",
  "nans.js",
  "compareIterator.js",
  "assertRelativeDateMs.js",
  "iteratorZipUtils.js",
  "dateConstants.js",
  "deepEqual.js",
  "promiseHelper.js",
  "proxyTrapsHelper.js",
  "fnGlobalObject.js",
  "nativeFunctionMatcher.js",
  "wellKnownIntrinsicObjects.js",
  "asyncHelpers.js",
  "byteConversionValues.js",
  "resizableArrayBufferUtils.js",
]);

const AREAS = [
  ["language", "test262/test/language"],
  ["built-ins", "test262/test/built-ins"],
  ["annexB", "test262/test/annexB"],
];

function classify(source) {
  let frontmatter = "";
  const start = source.indexOf("/*---");
  if (start >= 0) {
    const end = source.indexOf("---*/", start);
    frontmatter = source.slice(start, end >= 0 ? end : source.length);
  }
  const flags = new Set();
  const features = new Set();
  const includes = [];
  for (const raw of frontmatter.split(/[\r\n]+/).filter(Boolean)) {
    const line = raw.trim();
    const list = (prefix) => {
      const open = line.indexOf("[", line.indexOf(prefix));
      const close = line.indexOf("]", open);
      if (open < 0 || close <= open) return [];
      return line
        .slice(open + 1, close)
        .split(",")
        .map((item) => item.trim())
        .filter(Boolean);
    };
    if (line.startsWith("flags:")) {
      for (const item of list("flags:")) flags.add(item);
    } else if (line.startsWith("features:")) {
      for (const item of list("features:")) features.add(item);
    } else if (line.startsWith("includes:")) {
      for (const item of list("includes:")) includes.push(item);
    }
  }
  if (features.has("Temporal")) return "Temporal";
  if (features.has("await-dictionary")) return "await-dictionary";
  if (features.has("ShadowRealm")) return "ShadowRealm";
  if (features.has("source-phase-imports")) return "source-phase-imports";
  if (features.has("import-text")) return "import-text";
  if (features.has("import-defer")) return "import-defer";
  if (flags.has("CanBlockIsTrue")) return "CanBlockIsTrue";
  const unsupported = includes.filter((item) => !ALLOWED_INCLUDES.has(item)).sort();
  if (unsupported.length > 0) return "includes:" + unsupported.join(",");
  return null;
}

function walk(root, out) {
  for (const name of fs.readdirSync(root)) {
    const full = root + "/" + name;
    if (name.endsWith("_FIXTURE.js")) continue;
    const stat = fs.statSync(full);
    if (stat.isDirectory()) {
      walk(full, out);
    } else if (name.endsWith(".js")) {
      out.push(full);
    }
  }
  return out;
}

function render(tally) {
  return [...tally.entries()].sort((a, b) => b[1] - a[1])
    .map(([reason, count]) => `   ${String(count).padStart(6)}  ${reason}`)
    .join("\n");
}

const grand = new Map();
for (const [area, root] of AREAS) {
  const files = walk(root, []);
  const tally = new Map();
  for (const file of files) {
    const reason = classify(fs.readFileSync(file));
    tally.set(reason, (tally.get(reason) ?? 0) + 1);
  }
  console.log(`== ${area}: ${files.length} fixtures`);
  console.log(render(tally));
  const runnable = tally.get(null) ?? 0;
  console.log(
    `   ${String(files.length - runnable).padStart(6)}  SKIPPED total, ${runnable} runnable`
  );
  for (const [reason, count] of tally) {
    grand.set(reason, (grand.get(reason) ?? 0) + count);
  }
}
console.log("== ALL AREAS");
console.log(render(grand));
"tally complete";
