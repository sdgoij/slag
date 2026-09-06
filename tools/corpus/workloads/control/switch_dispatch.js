// A switch dispatch over a cycling selector.
function bench() {
  var s = 0;
  for (var i = 0; i < 3000000; i++) {
    switch (i & 7) {
      case 0: s += 1; break;
      case 1: s += 2; break;
      case 2: s += 3; break;
      case 3: s += 4; break;
      case 4: s += 5; break;
      case 5: s += 6; break;
      case 6: s += 7; break;
      default: s += 8; break;
    }
  }
  return s;
}
bench();
