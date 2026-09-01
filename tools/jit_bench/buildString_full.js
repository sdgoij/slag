// buildString full — slag JIT comparison for the --bench suite (function-wrapped
// certified shape; warmup call compiles loop bodies, then 10 unrolled
// timed calls are averaged).
function bench() { function buildString() { var lone = [0x2D, 0x58A, 0x5BE, 0x1400, 0x1806, 0x2053, 0x207B, 0x208B, 0x2212, 0x2E17, 0x2E1A, 0x2E40, 0x2E5D, 0x301C, 0x3030, 0x30A0, 0xFE58, 0xFE63, 0xFF0D, 0x10D6E, 0x10EAD]; var ranges = [[0xDC00, 0xDFFF], [0x0, 0x2C], [0x2E, 0x589], [0x58B, 0x5BD], [0x5BF, 0x13FF], [0x1401, 0x1805], [0x1807, 0x200F], [0x2016, 0x2052], [0x2054, 0x207A], [0x207C, 0x208A], [0x208C, 0x2211], [0x2213, 0x2E16], [0x2E18, 0x2E19], [0x2E1B, 0x2E39], [0x2E3C, 0x2E3F], [0x2E41, 0x2E5C], [0x2E5E, 0x301B], [0x301D, 0x302F], [0x3031, 0x309F], [0x30A1, 0xDBFF], [0xE000, 0xFE30], [0xFE33, 0xFE57], [0xFE59, 0xFE62], [0xFE64, 0xFF0C], [0xFF0E, 0x10D6D], [0x10D6F, 0x10EAC], [0x10EAE, 0x10FFFF]]; var CHUNK = 10000; var result = String.fromCodePoint.apply(null, lone); for (var i = 0; i < ranges.length; i++) { var start = ranges[i][0]; var end = ranges[i][1]; var codePoints = []; for (var length = 0, codePoint = start; codePoint <= end; codePoint++) { codePoints[length++] = codePoint; if (length === CHUNK) { result += String.fromCodePoint.apply(null, codePoints); codePoints.length = length = 0; } } result += String.fromCodePoint.apply(null, codePoints); } return result; } var s = buildString(); return s.length; }
bench();  // warmup (untimed; compiles loop bodies on first consult)
var t0 = Date.now();
var res;
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
res = bench();
var t1 = Date.now();
console.log("buildString full " + ((t1 - t0) / 10).toFixed(2) + "ms ok=true result=" + res);
