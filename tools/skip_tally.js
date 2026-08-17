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
  "regExpUtils.js",
  "atomicsHelper.js",
  "temporalHelpers.js",
]);

const AREAS = [
  ["language", "test262/test/language"],
  ["built-ins", "test262/test/built-ins"],
  ["annexB", "test262/test/annexB"],
];

function classify(file, source) {
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
  if (features.has("Temporal")) {
    // Mirrors run_fixture: only the implemented clusters run; the rest of
    // the Temporal namespace stays skipped, along with the Duration
    // fixtures that rely on Plain*/ZonedDateTime arithmetic (or the full
    // namespace) beyond what the shell implements.
    const rest = file.split("/Temporal/")[1] ?? "";
    const implemented =
      rest.startsWith("Duration/") ||
      rest.startsWith("Instant/") ||
      rest.startsWith("Now/") ||
      rest.startsWith("toStringTag/") ||
      !rest.includes("/");
    if (!implemented) return "Temporal type not yet implemented";
    const skipped = [
      "getOwnPropertyNames.js",
      "Duration/compare/calendar-temporal-object.js",
      "Duration/prototype/round/calendar-temporal-object.js",
      "Duration/prototype/total/calendar-temporal-object.js",
      "Duration/prototype/round/exact-multiple-of-larger-unit-plaindate.js",
      "Duration/prototype/round/exact-multiple-of-larger-unit-zoned.js",
      "Duration/prototype/round/next-day-out-of-range.js",
      "Duration/prototype/round/relativeto-rounding-date.js",
      "Duration/prototype/round/roundingincrement-days-large.js",
      "Duration/prototype/round/roundingincrement-non-integer.js",
      "Duration/prototype/total/relativeto-duration-out-of-range-added-to-relative-date.js",
      "Duration/prototype/total/relativeto-plaindate-add24hourdaystonormalizedtimeduration-out-of-range.js",
      "Duration/prototype/total/relativeto-plaindate-large-time-component-out-of-range.js",
      "Duration/prototype/total/relativeto-total-of-each-unit.js",
      "Duration/prototype/total/throws-if-date-time-invalid-with-plaindate-relative.js",
      "Duration/prototype/total/throws-if-date-time-invalid-with-zoneddatetime-relative.js",
    ];
    if (skipped.includes(rest)) return "requires Plain*/ZonedDateTime beyond the shell";
  }
  if (features.has("await-dictionary")) return "await-dictionary";
  if (features.has("ShadowRealm")) return "ShadowRealm";
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
    const reason = classify(file, fs.readFileSync(file, "utf8"));
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
