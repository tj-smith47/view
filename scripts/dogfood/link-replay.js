// Stands in for the terminal a lag report came from, in the two ways a
// recording needs one: live, answering the queries an editor asks its
// terminal at startup, and afterwards, replaying the recording through
// xterm.js headless -- the same engine Termius embeds -- so what a moment
// is read off is the cell the user saw rather than a byte on the wire.
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
//   node link-replay.js answer --cols N --rows N --reply PATH
//   node link-replay.js report --out OUT --timing TM --in IN --cols N \
//        --rows N --needle TEXT [--palette] [--frames DIR]
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

// The background colour an OSC 11 query is answered with. Any colour would
// do -- what the editor is waiting on is a reply, not a shade -- and this
// is the one xterm.js itself defaults its background to.
const BACKGROUND = 'rgb:0000/0000/0000';
const FOREGROUND = 'rgb:ffff/ffff/ffff';

// Registers the colour queries xterm.js does not answer on its own.
//
// nvim blocks for up to 100 ms on the OSC 11 background reply at startup
// (`runtime/lua/vim/_core/defaults.lua`), and a pty whose master is a file
// or a pipe answers nothing, so an arm recorded without this pays a wait no
// terminal-attached session pays and the whole comparison tilts. The
// cursor, device and DECRQSS replies xterm.js already writes itself.
function registerColourQueries(term, reply) {
  const answer = (code, colour) => (data) => {
    if (data !== '?') return true;
    reply(`\x1b]${code};${colour}\x07`);
    return true;
  };
  term.parser.registerOscHandler(10, answer(10, FOREGROUND));
  term.parser.registerOscHandler(11, answer(11, BACKGROUND));
  term.parser.registerOscHandler(12, answer(12, FOREGROUND));
}

// The live consumer: the pty's output arrives on stdin, and everything the
// emulator wants to say back is appended to `--reply`, which is the fifo
// `script` is relaying into the pty. So the editor under test talks to
// something that answers, the way it does on the user's own terminal.
function answer() {
  const { Terminal } = require('@xterm/headless');
  const cols = Number(arg('--cols'));
  const rows_ = Number(arg('--rows'));
  const replyPath = arg('--reply');
  const back = fs.openSync(replyPath, 'a');
  const term = new Terminal({
    cols,
    rows: rows_,
    scrollback: 0,
    allowProposedApi: true,
  });
  const reply = (text) => {
    try {
      fs.writeSync(back, text);
    } catch (err) {
      // the run is over and the fifo's reader is gone; a reply nobody can
      // receive is not a failure of the recording
    }
  };
  registerColourQueries(term, reply);
  term.onData(reply);
  process.stdin.on('data', (chunk) => term.write(chunk));
  process.stdin.on('end', () => {
    fs.closeSync(back);
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

// view's own palette chrome, which is the title of the box it draws the
// typed line inside.
const PALETTE_TITLE = '\u2500 Command \u2500';

// Whether the command line is on screen. view draws a framed box carrying
// its own title; bare nvim echoes the `:` into the first cell of the last
// row. Both read off cells rather than off a pattern over the screen: the
// pattern this replaces matched `use std::path::PathBuf;` in the file
// itself, so the moment was dated at the first record after the key
// whatever was drawn.
function cmdlineShown(term, screen, palette) {
  if (palette) {
    return screen.some((line) => line.indexOf(PALETTE_TITLE) !== -1);
  }
  const line = term.buffer.active.getLine(term.rows - 1);
  if (!line) return false;
  const cell = line.getCell(0);
  return !!cell && cell.getChars() === ':';
}

// Whether an input record is the emulator answering a query rather than a
// step this recording typed. Every reply the live consumer writes goes down
// the same pty as the steps and is logged beside them, so a count of input
// records would date the `:` at whichever reply happened to be fourth.
// Every reply opens with escape and carries more than that one byte; the
// only escape a step sends is the bare one that closes the palette.
function isReply(chunk) {
  return chunk.length > 1 && chunk[0] === 0x1b;
}

function report() {
  const { Terminal } = require('@xterm/headless');
  const timing = arg('--timing');
  const cols = Number(arg('--cols'));
  const rows_ = Number(arg('--rows'));
  const needle = arg('--needle');
  // which chrome the typed `:` is answered by: view's framed palette, or
  // bare nvim's own last-row command line
  const palette = process.argv.indexOf('--palette') !== -1;
  const frames = arg('--frames', '');
  const recs = records(timing);
  const total = recs
    .filter((r) => r.stream === 'O')
    .reduce((sum, r) => sum + r.bytes, 0);
  const data = payload(arg('--out'), total);
  const typedTotal = recs
    .filter((r) => r.stream === 'I')
    .reduce((sum, r) => sum + r.bytes, 0);
  const typedData = payload(arg('--in'), typedTotal);
  let typedOffset = 0;
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
        const sent = typedData.subarray(typedOffset, typedOffset + rec.bytes);
        typedOffset += rec.bytes;
        if (!isReply(sent)) typed.push(rec.ms);
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
        if (cmdlineShown(term, screen, palette)) {
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
if (mode === 'answer') {
  answer();
} else if (mode === 'report') {
  report();
} else {
  console.error('link-replay: usage: answer | report');
  process.exit(2);
}
