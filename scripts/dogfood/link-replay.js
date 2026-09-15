// Dates the moments of a `script -I -O -T` recording by replaying its output
// through xterm.js headless -- the same engine Termius embeds -- so what a
// moment is read off is the cell the user saw rather than a byte on the wire.
//
// The timing log is what turns a screen into a time: its `O` records carry
// the child's output in the chunks the pty delivered it in, each with the
// delay since the record before it, so feeding one record at a time and
// summing the delays dates every frame. `I` records date what was typed.
//
// Dev-only. Nothing in `task ci` runs this; `scripts/dogfood/link-record.sh`
// is its one caller, and `npm install` in this directory is what puts
// `@xterm/headless` where it can find it.
//
// Usage:
//   node link-replay.js report --out OUT --timing TM --cols N --rows N \
//        --needle TEXT [--cmd-re RE] [--frames DIR]
//   node link-replay.js throttle BYTES_PER_SECOND
const fs = require('fs');
const path = require('path');

function arg(name, fallback) {
  const i = process.argv.indexOf(name);
  if (i === -1 || i + 1 >= process.argv.length) {
    if (fallback === undefined) {
      console.error(`link-replay: ${name} is required`);
      process.exit(2);
    }
    return fallback;
  }
  return process.argv[i + 1];
}

// A slow reader of the pty, standing in for the link the user is on: the
// recorder puts it where `script` writes, so `script` stops reading the pty
// when it fills and the editor under test feels the back-pressure a remote
// terminal applies.
function throttle(rate) {
  const window = 20;
  const slice = Math.max(1, Math.floor((rate * window) / 1000));
  let budget = slice;
  process.stdin.on('data', (chunk) => {
    budget -= chunk.length;
    if (budget <= 0) process.stdin.pause();
  });
  const timer = setInterval(() => {
    budget = slice;
    process.stdin.resume();
  }, window);
  process.stdin.on('end', () => {
    clearInterval(timer);
    process.exit(0);
  });
}

// Every record of a multi-stream timing log, in order, each with the time
// since the session started. The `H` header records carry no delay and no
// bytes and are dropped once read.
function records(file) {
  const out = [];
  let t = 0;
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    const m = line.match(/^([IOSH]) ([0-9.]+) (.*)$/);
    if (!m) continue;
    t += Number(m[2]);
    if (m[1] === 'H') continue;
    out.push({ stream: m[1], ms: t * 1000, bytes: Number(m[3]) });
  }
  return out;
}

// The child's own bytes, with the header line `script` writes above them and
// the footer it writes below them left out: the `O` records count neither.
function payload(file, total) {
  const raw = fs.readFileSync(file);
  const head = raw.indexOf(0x0a) + 1;
  return raw.subarray(head, head + total);
}

function rows(term) {
  const buffer = term.buffer.active;
  const out = [];
  for (let y = 0; y < term.rows; y++) {
    out.push(buffer.getLine(y).translateToString(true));
  }
  return out;
}

// The colours the file's own line is drawn in, over the cells carrying a
// glyph, from the column the needle starts at rightwards. That row and not
// the rectangle below it: the rows under the needle are still being filled
// in on the frames right after it appears, so a reading over them grows on
// the next chunk whatever the colours do, and reports the paint finishing
// as the syntax arriving. A line that arrived uncoloured carries one
// colour, and syntax adds to the set.
function colours(term, row, col) {
  const line = term.buffer.active.getLine(row);
  const seen = [];
  if (!line) return seen;
  for (let x = col; x < term.cols; x++) {
    const cell = line.getCell(x);
    if (!cell || cell.getChars().trim() === '') continue;
    const key = `${cell.isFgDefault() ? 'd' : cell.getFgColor()}`;
    if (seen.indexOf(key) === -1) seen.push(key);
  }
  return seen;
}

function report() {
  const { Terminal } = require('@xterm/headless');
  const timing = arg('--timing');
  const cols = Number(arg('--cols'));
  const rows_ = Number(arg('--rows'));
  const needle = arg('--needle');
  const cmdRe = new RegExp(arg('--cmd-re', '^:'));
  const frames = arg('--frames', '');
  const recs = records(timing);
  const total = recs
    .filter((r) => r.stream === 'O')
    .reduce((sum, r) => sum + r.bytes, 0);
  const data = payload(arg('--out'), total);
  const term = new Terminal({
    cols,
    rows: rows_,
    scrollback: 0,
    allowProposedApi: true,
  });

  const moments = {};
  const typed = [];
  let offset = 0;
  let textRow = -1;
  let textCol = -1;
  let baseColours = [];
  // the alternate screen leaving is the session handing the terminal back,
  // which is the last moment the table reports
  const altExit = data.indexOf(Buffer.from('\x1b[?1049l'));

  const note = (name, ms) => {
    if (moments[name] !== undefined) return;
    moments[name] = ms;
    if (!frames) return;
    fs.writeFileSync(path.join(frames, `${name}.txt`), rows(term).join('\n'));
  };

  (async () => {
    for (const rec of recs) {
      if (rec.stream === 'I') {
        typed.push(rec.ms);
        continue;
      }
      const chunk = data.subarray(offset, offset + rec.bytes);
      offset += rec.bytes;
      await new Promise((done) => term.write(chunk, done));
      if (altExit !== -1 && offset > altExit) note('handback', rec.ms);
      const screen = rows(term);
      if (textRow === -1) {
        for (let y = 0; y < screen.length; y++) {
          const at = screen[y].indexOf(needle);
          if (at === -1) continue;
          textRow = y;
          textCol = at;
          note('text', rec.ms);
          baseColours = colours(term, y, at);
          break;
        }
      } else if (moments.highlight === undefined) {
        const now = colours(term, textRow, textCol);
        if (now.some((c) => baseColours.indexOf(c) === -1)) {
          note('highlight', rec.ms);
        }
      }
      if (typed.length > 0 && moments.cmdline === undefined) {
        if (screen.some((line) => cmdRe.test(line.trim()))) {
          note('cmdline', rec.ms);
        }
      }
    }
    for (const name of ['text', 'highlight', 'cmdline', 'handback']) {
      console.log(`${name}_ms=${moments[name] === undefined ? '' : moments[name].toFixed(1)}`);
    }
    typed.forEach((ms, i) => console.log(`typed${i + 1}_ms=${ms.toFixed(1)}`));
    console.log(`text_row=${textRow}`);
    console.log(`base_colours=${baseColours.length}`);
    process.exit(0);
  })();
}

const mode = process.argv[2];
if (mode === 'throttle') {
  throttle(Number(process.argv[3]));
} else if (mode === 'report') {
  report();
} else {
  console.error('link-replay: usage: report | throttle');
  process.exit(2);
}
