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
//   node link-replay.js answer --cols N --rows N --wire PATH \
//        --reply PATH --ready PATH
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

// The live consumer: the pty's output arrives on `--wire`, and everything
// the emulator wants to say back is appended to `--reply`, which is the
// fifo `script` is relaying into the pty. So the editor under test talks to
// something that answers, the way it does on the user's own terminal.
//
// `--ready` is a fifo written once the emulator is built and before either
// of the other two is opened, and the recorder reads it before starting the
// editor: node's own startup is some 30 ms, and spent inside the recording
// it lands on the editor's clock as a slow terminal -- the queries in a
// recording taken without it were answered 33 ms after they were asked. The
// two fifos are then opened in the order `script` opens its own ends of
// them, input first, because each open waits for the other side.
function answer() {
  const { Terminal } = require('@xterm/headless');
  const cols = Number(arg('--cols'));
  const rows_ = Number(arg('--rows'));
  const wire = arg('--wire');
  const replyPath = arg('--reply');
  const term = new Terminal({
    cols,
    rows: rows_,
    scrollback: 0,
    allowProposedApi: true,
  });
  fs.writeFileSync(arg('--ready'), 'ready\n');
  const back = fs.openSync(replyPath, 'a');
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
  const stream = fs.createReadStream(wire);
  stream.on('data', (chunk) => term.write(chunk));
  stream.on('end', () => {
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

// The colours the needle's own line is drawn in, over the cells carrying a
// glyph, from the column the needle starts at up to where that line ends.
// That row and not the rectangle below it: the rows under the needle are
// still being filled in on the frames right after it appears, so a reading
// over them grows on the next chunk whatever the colours do, and reports the
// paint finishing as the syntax arriving. A line that arrived uncoloured
// carries one colour, and syntax adds to the set.
//
// The line ends at the first run of three blank cells, and the rest of the
// row is left out: a notification float on the right margin, or a tree pane
// opening on the left, puts its own colours on the same row, and reading to
// the screen's edge reported the pane's colours as the file's -- the reading
// this replaces dated the syntax at the frame a tree pane opened on.
function colours(term, row, col) {
  const line = term.buffer.active.getLine(row);
  const seen = [];
  if (!line) return seen;
  let blanks = 0;
  for (let x = col; x < term.cols; x++) {
    const cell = line.getCell(x);
    if (!cell) break;
    if (cell.getChars().trim() === '') {
      blanks += 1;
      if (blanks >= 3) break;
      continue;
    }
    blanks = 0;
    const key = `${cell.isFgDefault() ? 'd' : cell.getFgColor()}`;
    if (seen.indexOf(key) === -1) seen.push(key);
  }
  return seen;
}

// Where the needle is on this frame, or `null` while it is not on screen.
//
// Re-found on every frame rather than held from the frame it first appeared
// on: a pane opening moves the file's text sideways and down, and a fixed
// cell then belongs to whatever took that position.
function locate(screen, needle) {
  for (let y = 0; y < screen.length; y++) {
    const at = screen[y].indexOf(needle);
    if (at !== -1) return { row: y, col: at };
  }
  return null;
}

// The title of the framed box each side draws the typed line inside:
// view's own palette, and noice's cmdline popup, which is what a config
// loading noice gets instead of nvim's own last-row command line.
const PALETTE_TITLE = '\u2500 Command \u2500';
const POPUP_TITLE = '\u2500 Cmdline \u2500';

// Whether the command line is on screen, wherever this config paints it.
//
// Three shapes, all read off cells rather than off a pattern over the
// screen: the pattern this replaces matched `use std::path::PathBuf;` in
// the file itself, so the moment was dated at the first record after the
// key whatever was drawn.
//
// view draws its palette's framed box. Bare nvim draws either its own
// command line in the last row -- a `:` in the first cell, which is what
// the plugin-free fixture shows -- or, under noice, a framed box titled
// Cmdline in the middle of the screen. The last-row reading found nothing
// at all on the user's own config for that reason, and a reading over `:`
// cells finds nothing either: noice's prompt row carries the cursor and no
// glyph, so the only `:` on screen is the file's own.
function cmdlineShown(term, screen, palette) {
  if (screen.some((line) => line.indexOf(palette ? PALETTE_TITLE : POPUP_TITLE) !== -1)) {
    return true;
  }
  if (palette) return false;
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
      const at = locate(screen, needle);
      if (textRow === -1) {
        if (at) {
          textRow = at.row;
          textCol = at.col;
          note('text', rec.ms);
          baseColours = colours(term, at.row, at.col);
        }
      } else if (moments.highlight === undefined && at) {
        const now = colours(term, at.row, at.col);
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
