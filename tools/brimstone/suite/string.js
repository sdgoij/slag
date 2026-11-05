var t0 = Date.now();
var s = '';
for (var i = 0; i < 200000; i++) s += 'ab';
var n = s.length;
var found = s.indexOf('ababab');
console.log('string ' + (Date.now() - t0) + ' ' + n + ' ' + found);
