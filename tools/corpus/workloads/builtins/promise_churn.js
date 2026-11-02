// Promise record, resolving-function and reaction churn.
//
// Every iteration creates a promise, runs its executor (which allocates the two
// resolving functions and stores them in the promise-state table), settles it,
// and registers a reaction. The microtasks do not run inside `bench` (the
// runner never empties the stack between timed calls), so the completion value
// is `s` at return: zero on every engine. What is measured is the synchronous
// state-table work, which the corpus otherwise never exercises.
function bench() {
  var s = 0;
  for (var i = 0; i < 2000; i++) {
    var p = new Promise(function (resolve) { resolve(i); });
    p.then(function (v) { s += v; });
  }
  return s;
}
bench();
