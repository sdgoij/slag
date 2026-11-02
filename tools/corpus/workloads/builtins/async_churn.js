// Async-function creation and await-resume scheduling.
//
// Each call returns a promise and parks a resume in the await-resume table.
// The resumes do not run inside `bench` (the runner never empties the stack
// between timed calls), so the completion value is zero on every engine; what
// is measured is the synchronous allocation and state-table work.
async function step(i) {
  var v = await i;
  return v;
}

function bench() {
  var s = 0;
  for (var i = 0; i < 2000; i++) {
    step(i);
  }
  return s;
}
bench();
