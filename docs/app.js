// The demo app (docs/index.html logic) running *inside* Slag. The browser
// only instantiated the engine and evaluated this file — every UI action is
// driven by code executing in the wasm engine through the DOM bridge.
//
// User scripts do NOT run in this realm: the page hosts a second Slag engine
// (its own agent/realm, no DOM) and evaluates them there on request
// (__host.userRun), so Clear can drop that engine — globals and pending
// timers included — without tearing down the UI. Its console lines and
// completions are delivered back into __userLine below, which renders them
// through the same pane machinery as this app's own output.

const status = document.getElementById('status');
const src = document.getElementById('src');
const runBtn = document.getElementById('run');
const runModeEl = document.getElementById('run-mode');
const clearBtn = document.getElementById('clear');
const themeBtn = document.getElementById('theme');
const modeBtns = Array.from(document.querySelectorAll('.seg[data-mode]'));
const scriptView = document.getElementById('script-view');
const replView = document.getElementById('repl-view');
const scriptOut = document.getElementById('out');
const replOut = document.getElementById('repl-out');
const replForm = document.getElementById('repl-form');
const replInput = document.getElementById('repl-input');
const replPrompt = document.getElementById('repl-prompt');
const replClearBtn = document.getElementById('repl-clear');
const copyBtn = document.getElementById('copy');
const replCopyBtn = document.getElementById('repl-copy');
const exampleEl = document.getElementById('example');
const srcHl = document.getElementById('src-hl');

const DEFAULT_SAMPLE = src.value;
// The bridge cannot iterate a node's children, so keep per-pane text
// buffers for copy-to-clipboard.
let scriptLines = [];
let replLines = [];
let activeOut = scriptOut;
let mode = 'script';
let replBuffer = '';
let replHistory = [];
let replHistPos = 0;
let replDraft = '';
let replGreeted = false;

// ---- theme (auto → light → dark), mirrored onto <html data-theme> ----
const THEMES = ['auto', 'light', 'dark'];
const THEME_GLYPH = { auto: '◐', light: '☀', dark: '☾' };
const THEME_LABEL = { auto: 'Theme: follow system', light: 'Theme: light', dark: 'Theme: dark' };
const applyTheme = (theme) => {
  document.documentElement.dataset.theme = theme;
  localStorage.setItem('slag-theme', theme);
  themeBtn.textContent = THEME_GLYPH[theme];
  themeBtn.title = THEME_LABEL[theme];
};
const savedTheme = localStorage.getItem('slag-theme');
applyTheme(['auto', 'light', 'dark'].includes(savedTheme) ? savedTheme : 'auto');
themeBtn.addEventListener('click', () => {
  const next = THEMES[(THEMES.indexOf(document.documentElement.dataset.theme) + 1) % THEMES.length];
  applyTheme(next);
});

const setStatus = (state, text) => {
  status.className = `status ${state}`;
  status.innerHTML = `<i class="dot"></i>${text}`;
};

// ---- console line rendering (engine-side DOM) ----
const safeString = (value) => {
  if (value === null) return 'null';
  if (value === undefined) return 'undefined';
  if (typeof value === 'string') return value;
  try {
    return String(value);
  } catch {
    return '[object Object]';
  }
};
const lineInto = (host, kind, tag, text) => {
  const el = document.createElement('div');
  el.className = `line ${kind}`;
  if (tag) {
    const tagEl = document.createElement('span');
    tagEl.className = 'tag';
    tagEl.textContent = tag;
    el.appendChild(tagEl);
  }
  // The bridge has no text nodes; a content span carries the message.
  const content = document.createElement('span');
  content.textContent = text;
  el.appendChild(content);
  host.appendChild(el);
  host.scrollTop = host.scrollHeight;
  const buffer = host === scriptOut ? scriptLines : host === replOut ? replLines : null;
  if (buffer) buffer.push((tag ? tag + ' ' : '') + text);
};
const clearInto = (host) => {
  host.textContent = '';
  if (host === scriptOut) scriptLines = [];
  else if (host === replOut) replLines = [];
};
const emit = (kind, tag, text) => lineInto(activeOut, kind, activeOut === replOut ? null : tag, text);

// User code's console.* routes through these (they replace the engine's
// plain host console), so script output lands in the right pane as lines.
const consoleLine = (kind, tag, args) => {
  emit(kind, tag, Array.from(args).map(safeString).join(' '));
};
console.log = (...args) => consoleLine('js', '[js]', args);
console.info = (...args) => consoleLine('info', '[info]', args);
console.warn = (...args) => consoleLine('warn', '[warn]', args);
console.error = (...args) => consoleLine('error', '[error]', args);
console.debug = (...args) => consoleLine('debug', '[debug]', args);

// ---- repl helpers ----
const renderReplPrompt = () => {
  replPrompt.textContent = replBuffer ? '...' : '>';
};
const greetRepl = () => {
  if (replGreeted) return;
  replGreeted = true;
  lineInto(replOut, 'system', null, 'Welcome to Slag — JavaScript in your browser');
  lineInto(replOut, 'system', null, 'Type ".help" for more information.');
};

// Only evaluate once strings/comments/templates are closed and bracket
// depth is back to zero, so a block can span several entered lines.
const inputComplete = (source) => {
  let depth = 0;
  let inString = null;
  let lineComment = false;
  let blockComment = false;
  const chars = Array.from(source);
  for (let i = 0; i < chars.length; i += 1) {
    const c = chars[i];
    if (lineComment) {
      if (c === '\n') lineComment = false;
      continue;
    }
    if (blockComment) {
      if (c === '*' && chars[i + 1] === '/') {
        blockComment = false;
        i += 1;
      }
      continue;
    }
    if (inString) {
      if (c === '\\') {
        i += 1;
      } else if (c === inString) {
        inString = null;
      }
      continue;
    }
    if (c === '/' && chars[i + 1] === '/') {
      lineComment = true;
      i += 1;
    } else if (c === '/' && chars[i + 1] === '*') {
      blockComment = true;
      i += 1;
    } else if (c === "'" || c === '"' || c === '`') {
      inString = c;
    } else if (c === '(' || c === '[' || c === '{') {
      depth += 1;
    } else if (c === ')' || c === ']' || c === '}') {
      depth -= 1;
    }
  }
  return depth <= 0 && inString === null && !lineComment && !blockComment;
};

const runDotCommand = (command) => {
  switch (command) {
    case '.help':
      lineInto(replOut, 'system', null, '.break   cancel the current multi-line input');
      lineInto(replOut, 'system', null, '.clear   clear the console and reset the engine context');
      lineInto(replOut, 'system', null, '.exit    leave the REPL and return to Script mode');
      lineInto(replOut, 'system', null, '.help    print this help message');
      break;
    case '.clear':
      clearInto(replOut);
      __host.userReset();
      break;
    case '.break':
      replBuffer = '';
      renderReplPrompt();
      break;
    case '.exit':
      setMode('script');
      break;
    default:
      lineInto(replOut, 'error', null, 'Invalid REPL keyword');
  }
  replOut.scrollTop = replOut.scrollHeight;
};

// ---- modes ----
const setMode = (next) => {
  mode = next;
  modeBtns.forEach((btn) => {
    btn.className = `seg${btn.dataset.mode === next ? ' active' : ''}`;
  });
  scriptView.hidden = next !== 'script';
  replView.hidden = next !== 'repl';
  activeOut = next === 'repl' ? replOut : scriptOut;
  if (next === 'repl') {
    greetRepl();
    replInput.disabled = false;
    replInput.focus();
  }
};

// ---- examples ----
const EXAMPLES = {
  default: DEFAULT_SAMPLE,
  objects: `// Objects, classes, JSON, destructuring, and accessors.

class Point {
  constructor(x, y) {
    this.x = x;
    this.y = y;
  }
  norm() {
    return Math.hypot(this.x, this.y);
  }
}
const p = new Point(3, 4);
console.log('norm of (3, 4):', p.norm());

const record = { name: 'slag', tags: ['spec', 'faithful'] };
console.log('JSON:', JSON.stringify(record));
`,
  async: `// Promises, microtasks, and real timers (the glue drains the engine).

console.log('1: sync start');
Promise.resolve('2: microtask').then(console.log);
setTimeout(() => console.log('5: macrotask after ~100ms'), 100);

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function demo() {
  console.log('3: async body');
  await delay(150);
  console.log('6: after await ~150ms');
}
demo();
console.log('4: sync end');
`,
  regexp: `// Regular expressions with full ECMAScript semantics.

const re = /^(?<area>\\d{3})-(?<exch>\\d{3})-(?<num>\\d{4})$/;
const match = re.exec('800-555-0199');
console.log('named groups:', JSON.stringify(match.groups));

console.log('unicode property:', /^\\p{Script=Greek}+$/u.test('αβγ'));
console.log('backreference:', /^(ab)\\1$/.test('abab'));
`,
};
exampleEl.addEventListener('change', () => {
  const source = EXAMPLES[exampleEl.value];
  if (source === undefined) return;
  src.value = source;
  paint();
  clearInto(scriptOut);
  __host.userReset();
  lineInto(scriptOut, 'system', null, `loaded “${exampleEl.value}” · engine context reset`);
});

// ---- running code ----
const quoteCompletion = (value) => {
  const text = safeString(value);
  return "'" + text.replace(/\\/g, '\\\\').replace(/'/g, "\\'") + "'";
};

// ---- sandbox output sink ----
// User scripts evaluate in a separate realm; the module queues their
// console/rejection lines and eval completions and calls this sink (levels
// 0-4 console, 5 rejection, 6 completion, 7 completion error). Rendering
// through emit/lineInto keeps copy buffers and pane styling identical to
// this app's own output.
const USER_LINE_KINDS = [
  ['js', '[js]'],
  ['info', '[info]'],
  ['warn', '[warn]'],
  ['error', '[error]'],
  ['debug', '[debug]'],
];
const userLineSink = (level, typeCode, text) => {
  if (level <= 4) {
    const [kind, tag] = USER_LINE_KINDS[level];
    emit(kind, tag, text);
    return;
  }
  if (level === 5) {
    emit('error', '[rejection]', text);
    return;
  }
  if (level === 6) {
    const shown = typeCode === 5 ? quoteCompletion(text) : text;
    if (activeOut === replOut) {
      lineInto(replOut, typeCode === 1 ? 'out-void' : 'out', null, shown);
    } else if (shown) {
      lineInto(scriptOut, 'result', '→', shown);
    }
    return;
  }
  if (level === 7) {
    if (activeOut === replOut) lineInto(replOut, 'error', null, '✗ ' + text);
    else lineInto(scriptOut, 'error', '✗', text);
  }
};
globalThis.__userLine = userLineSink;

// ---- editor syntax highlighting (runs on Slag, painted into #src-hl) ----
const KEYWORDS = new Set([
  'async', 'await', 'break', 'case', 'catch', 'class', 'const', 'continue',
  'debugger', 'default', 'delete', 'do', 'else', 'export', 'extends', 'false',
  'finally', 'for', 'function', 'get', 'if', 'import', 'in', 'instanceof',
  'let', 'new', 'null', 'of', 'return', 'set', 'static', 'super', 'switch',
  'this', 'throw', 'true', 'try', 'typeof', 'var', 'void', 'while', 'with',
  'yield',
]);
const isWordStart = (c) => /[A-Za-z_$]/.test(c) || c.charCodeAt(0) > 127;
const isWordChar = (c) => /[A-Za-z0-9_$]/.test(c) || c.charCodeAt(0) > 127;
const skipQuote = (code, start) => {
  const quote = code[start];
  let i = start + 1;
  while (i < code.length) {
    const ch = code[i];
    if (ch === '\\') { i += 2; continue; }
    if (ch === quote) return i + 1;
    if (ch === '\n') return i;
    i += 1;
  }
  return code.length;
};
const skipTemplate = (code, start) => {
  let i = start + 1;
  while (i < code.length) {
    const ch = code[i];
    if (ch === '\\') { i += 2; continue; }
    if (ch === '`') return i + 1;
    if (ch === '$' && code[i + 1] === '{') {
      i = findHoleEnd(code, i + 2) + 1;
      continue;
    }
    i += 1;
  }
  return code.length;
};
const findHoleEnd = (code, from) => {
  let depth = 1;
  let i = from;
  while (i < code.length) {
    const c = code[i];
    if (c === '\\') { i += 2; continue; }
    if (c === "'" || c === '"') { i = skipQuote(code, i); continue; }
    if (c === '`') { i = skipTemplate(code, i); continue; }
    if (c === '/' && code[i + 1] === '/') {
      const nl = code.indexOf('\n', i);
      i = nl === -1 ? code.length : nl;
      continue;
    }
    if (c === '/' && code[i + 1] === '*') {
      const end = code.indexOf('*/', i + 2);
      i = end === -1 ? code.length : end + 2;
      continue;
    }
    if (c === '{') depth += 1;
    else if (c === '}') {
      depth -= 1;
      if (depth === 0) return i;
    }
    i += 1;
  }
  return code.length;
};
const scanNumber = (code, start) => {
  const c = code[start];
  const c1 = code[start + 1] || '';
  let i = start;
  if (c === '0' && /[xX]/.test(c1)) {
    i = start + 2;
    while (i < code.length && /[0-9a-fA-F_]/.test(code[i])) i += 1;
  } else if (c === '0' && /[bB]/.test(c1)) {
    i = start + 2;
    while (i < code.length && /[01_]/.test(code[i])) i += 1;
  } else if (c === '0' && /[oO]/.test(c1)) {
    i = start + 2;
    while (i < code.length && /[0-7_]/.test(code[i])) i += 1;
  } else {
    while (i < code.length && /[0-9_]/.test(code[i])) i += 1;
    if (code[i] === '.') {
      i += 1;
      while (i < code.length && /[0-9_]/.test(code[i])) i += 1;
    }
    if (i < code.length && /[eE]/.test(code[i])) {
      const k = i;
      let j = i + 1;
      if (j < code.length && /[+-]/.test(code[j])) j += 1;
      if (j < code.length && /[0-9]/.test(code[j])) {
        i = j;
        while (i < code.length && /[0-9_]/.test(code[i])) i += 1;
      } else {
        i = k;
      }
    }
  }
  if (code[i] === 'n') i += 1;
  return i;
};
const tokenize = (code) => {
  const spans = [];
  const push = (cls, text) => {
    if (!text) return;
    const last = spans[spans.length - 1];
    if (last && last.cls === cls) last.text += text;
    else spans.push({ cls, text });
  };
  let i = 0;
  while (i < code.length) {
    const c = code[i];
    if (/\s/.test(c)) {
      let j = i + 1;
      while (j < code.length && /\s/.test(code[j])) j += 1;
      push(null, code.slice(i, j));
      i = j;
      continue;
    }
    if (c === '/' && code[i + 1] === '/') {
      let j = i + 2;
      while (j < code.length && code[j] !== '\n') j += 1;
      push('tok-comment', code.slice(i, j));
      i = j;
      continue;
    }
    if (c === '/' && code[i + 1] === '*') {
      const end = code.indexOf('*/', i + 2);
      const j = end === -1 ? code.length : end + 2;
      push('tok-comment', code.slice(i, j));
      i = j;
      continue;
    }
    if (c === '"' || c === "'") {
      const j = skipQuote(code, i);
      push('tok-string', code.slice(i, j));
      i = j;
      continue;
    }
    if (c === '`') {
      let j = i + 1;
      let seg = i;
      while (j < code.length) {
        const ch = code[j];
        if (ch === '\\') { j += 2; continue; }
        if (ch === '`') { j += 1; break; }
        if (ch === '$' && code[j + 1] === '{') {
          const end = findHoleEnd(code, j + 2);
          push('tok-string', code.slice(seg, j));
          push('tok-string', '${');
          for (const inner of tokenize(code.slice(j + 2, end))) push(inner.cls, inner.text);
          if (end < code.length) push('tok-string', '}');
          j = end + 1;
          seg = j;
          continue;
        }
        j += 1;
      }
      push('tok-string', code.slice(seg, j));
      i = j;
      continue;
    }
    if (/[0-9]/.test(c) || (c === '.' && /[0-9]/.test(code[i + 1] || ''))) {
      const j = scanNumber(code, i);
      push('tok-number', code.slice(i, j));
      i = j;
      continue;
    }
    if (isWordStart(c)) {
      let j = i + 1;
      while (j < code.length && isWordChar(code[j])) j += 1;
      const word = code.slice(i, j);
      if (KEYWORDS.has(word)) {
        push('tok-keyword', word);
      } else {
        let k = j;
        while (k < code.length && /\s/.test(code[k])) k += 1;
        push(code[k] === '(' ? 'tok-func' : null, word);
      }
      i = j;
      continue;
    }
    push(null, c);
    i += 1;
  }
  return spans;
};
const HTML_ESCAPES = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
const escHtml = (text) => text.replace(/[&<>"']/g, (ch) => HTML_ESCAPES[ch]);
const highlight = (source) => {
  let html = '';
  for (const { cls, text } of tokenize(source)) {
    const safe = escHtml(text);
    html += cls ? `<span class="${cls}">${safe}</span>` : safe;
  }
  return html;
};
const paint = () => {
  if (!srcHl) return;
  srcHl.innerHTML = highlight(src.value);
  srcHl.scrollTop = src.scrollTop;
};

const runScript = () => {
  clearInto(scriptOut);
  const runMode = runModeEl.value;
  const dumpKind = { tokens: 0, ast: 1, bytecode: 2 }[runMode];
  if (runMode !== 'execute' && dumpKind === undefined) {
    lineInto(scriptOut, 'system', null, `run mode “${runMode}” is not supported here — use Execute, Tokens, AST, or Bytecode`);
    return;
  }
  if (runMode === 'execute') {
    // Execute runs in the separate sandbox realm (persistent until Clear);
    // output and the completion come back asynchronously through
    // __userLine, rendered into the pane that requested the run.
    __host.userRun(src.value);
    return;
  }
  try {
    const text = __host.dump(dumpKind, src.value);
    if (text) lineInto(scriptOut, 'out', null, text);
  } catch (error) {
    lineInto(scriptOut, 'error', '✗', error && error.message ? safeString(error.message) : safeString(error));
  }
};

runBtn.addEventListener('click', runScript);
src.addEventListener('keydown', (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
    event.preventDefault();
    runScript();
  }
});
// Keep the token overlay in step with the editor text and its scrolling.
src.addEventListener('input', paint);
src.addEventListener('scroll', () => {
  const top = src.scrollTop;
  if (typeof top === 'number' && srcHl) srcHl.scrollTop = top;
});
clearBtn.addEventListener('click', () => {
  clearInto(scriptOut);
  __host.userReset();
  lineInto(scriptOut, 'system', null, 'console cleared · sandbox context reset');
});
const flashCopy = (button, lines) => {
  const text = lines.join('\n');
  const ok = __host.copy(text);
  const previous = button.textContent;
  button.textContent = ok ? 'Copied' : 'Copy failed';
  button.disabled = true;
  setTimeout(() => {
    button.textContent = previous;
    button.disabled = false;
  }, 1200);
};
copyBtn.addEventListener('click', () => flashCopy(copyBtn, scriptLines));

// ---- REPL ----
replForm.addEventListener('submit', (event) => {
  event.preventDefault();
  if (replInput.disabled) return;
  const raw = replInput.value;
  if (!replBuffer && raw.trim() === '') {
    replInput.focus();
    return;
  }
  lineInto(replOut, 'input', replBuffer ? '...' : '>', raw);
  replInput.value = '';
  const trimmed = raw.trim();
  const dotAtTop = !replBuffer && trimmed.startsWith('.');
  const midBreak = replBuffer && (trimmed === '.break' || trimmed === '.exit');
  if (dotAtTop || midBreak) {
    runDotCommand(trimmed);
    replInput.focus();
    return;
  }
  replBuffer = replBuffer ? `${replBuffer}\n${raw}` : raw;
  if (!inputComplete(replBuffer)) {
    renderReplPrompt();
    replInput.focus();
    return;
  }
  const source = replBuffer;
  replBuffer = '';
  renderReplPrompt();
  replHistory.push(source);
  replHistPos = replHistory.length;
  // The line evaluates in the shared sandbox realm (persistent across REPL
  // entries until .clear / Clear); its result arrives via __userLine.
  __host.userRun(source);
  replOut.scrollTop = replOut.scrollHeight;
  replInput.focus();
});

// Up/down walk the history (only at the top level of a submission).
replInput.addEventListener('keydown', (event) => {
  if (replBuffer) return;
  if (event.key === 'ArrowUp') {
    event.preventDefault();
    if (replHistory.length === 0) return;
    if (replHistPos === replHistory.length) replDraft = replInput.value;
    if (replHistPos > 0) {
      replHistPos -= 1;
      replInput.value = replHistory[replHistPos];
    }
  } else if (event.key === 'ArrowDown') {
    event.preventDefault();
    if (replHistPos < replHistory.length) {
      replHistPos += 1;
      replInput.value = replHistPos === replHistory.length ? replDraft : replHistory[replHistPos];
    } else {
      replInput.value = replDraft;
    }
  }
});
replClearBtn.addEventListener('click', () => {
  clearInto(replOut);
  replBuffer = '';
  renderReplPrompt();
  __host.userReset();
  replInput.focus();
});
replCopyBtn.addEventListener('click', () => {
  flashCopy(replCopyBtn, replLines);
});

modeBtns.forEach((btn) => btn.addEventListener('click', () => setMode(btn.dataset.mode)));

// ---- ready: run the embedded sample once so the page is alive ----
setMode('script');
setStatus('ready', 'ready');
paint(); // paint the initial sample's tokens onto the overlay
runScript();
