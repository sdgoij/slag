// A rendered frame of a text UI: a long-lived cell buffer, closures created at
// one site per frame, a string-keyed style cache, a dispatch table, rebuilt row
// strings, and a cell field whose type is not uniform.
//
// The dimensions this row adds over the `objects`/`calls` rows are the ones a
// real application has and a steady-state micro-row does not:
//   - the runner calls `bench()` many times in one process, so a sample times a
//     WARM frame over state older than the sample (the style cache and the
//     buffer are populated once);
//   - every frame creates closures at stable sites and calls them from the
//     frame body, which is compiled code, so the callee is entered nested
//     (`jit_depth > 0`) rather than at rest;
//   - the row helpers are non-leaf (they call `String.fromCharCode` and the
//     style resolver), which is what makes them guard-eligible in the
//     optimizing tier;
//   - `row.link` holds a Number on unlinked rows and a String on linked ones,
//     and `paint` reads it arithmetically, so the typer's read guard cannot be
//     proven. That is the heterogeneous-payload shape a UI has, and a guard
//     that fails has to be paid for once per call when the body is nested.
//
// The checksum is a function of the rendered buffer alone, so it does not
// depend on how many frames a sample happened to batch.

var ROWS = 48;
var COLS = 120;

var rows = [];
var styles = new Map();
var dispatch = [];

function glyph(r, c) {
  return (c * 7 + r * 31) & 63;
}

function make_style(kind, fg, bg) {
  return { kind: kind, fg: fg, bg: bg };
}

function resolve(row) {
  var key = row.kind + ":" + row.fg + ":" + row.bg;
  var style = styles.get(key);
  if (style === void 0) {
    style = make_style(row.kind, row.fg, row.bg);
    styles.set(key, style);
  }
  return style;
}

function paint(row, style) {
  var link = row.link | 0;
  var text = "";
  for (var c = 0; c < COLS; c++) {
    var g = (row.cells[c] + style.fg * 3 + style.bg + link) & 25;
    text += String.fromCharCode(65 + g);
  }
  row.line = text;
  return text.length;
}

function checksum() {
  var sum = 0;
  for (var r = 0; r < ROWS; r++) {
    var line = rows[r].line;
    for (var c = 0; c < COLS; c += 8) {
      sum = (sum + line.charCodeAt(c)) & 0x3fffffff;
    }
  }
  return sum;
}

dispatch.push(function tick(state) { return state.ticks + 1; });
dispatch.push(function status(state) { return state.rows + 1; });
dispatch.push(function clock(state) { return state.ticks + state.rows; });
dispatch.push(function mode(state) { return state.rows; });
dispatch.push(function scale(state) { return state.ticks; });
dispatch.push(function focus(state) { return state.rows + state.ticks; });
dispatch.push(function scroll(state) { return state.ticks + 1; });
dispatch.push(function cursor(state) { return state.rows; });

(function init() {
  for (var r = 0; r < ROWS; r++) {
    var cells = new Array(COLS);
    for (var c = 0; c < COLS; c++) cells[c] = glyph(r, c);
    rows.push({
      cells: cells,
      line: "",
      fg: r % 7,
      bg: r % 5,
      kind: (r & 1) === 0 ? "text" : 1,
      link: r % 3 === 0 ? "L" + r : 0
    });
  }
})();

function bench() {
  var state = { ticks: 0, rows: ROWS };
  for (let r = 0; r < ROWS; r++) {
    const paint_row = () => paint(rows[r], resolve(rows[r]));
    paint_row();
  }
  for (var i = 0; i < dispatch.length; i++) {
    state.ticks = dispatch[i](state);
  }
  // `state.ticks` feeds the dispatch chain only; the checksum is the buffer's,
  // so the result is frame-index independent.
  return checksum();
}
bench();
