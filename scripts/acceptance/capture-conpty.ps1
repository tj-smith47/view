<#
.SYNOPSIS
  Records what a pty does to a scripted bare-nvim session: the raw byte
  stream, what a resize emits, what an exit code survives, what clock the
  host offers, and what a per-child kill leaves behind.

.DESCRIPTION
  Windows is the platform this exists for -- ConPTY is a repainting
  terminal emulator sitting between the child and the master, unlike a unix
  pty's byte pipe -- but the capture program it carries is platform-neutral
  and the unix arm of the comparison runs the identical source. The Rust
  source lives inline between the CAPTURE-SOURCE sentinels below so one file
  can be copied to a Windows host on its own; the unix arm extracts it:

    awk '/^# CAPTURE-CARGO-BEGIN/{f=1;next} /^# CAPTURE-CARGO-END/{f=0} f' \
      scripts/acceptance/capture-conpty.ps1 | sed '1d;$d' > Cargo.toml
    awk '/^# CAPTURE-SOURCE-BEGIN/{f=1;next} /^# CAPTURE-SOURCE-END/{f=0} f' \
      scripts/acceptance/capture-conpty.ps1 | sed '1d;$d' > src/main.rs

  Both arms then report a fingerprint of that source, carriage returns
  excluded, so a doc quoting the two streams can show they came from one
  program rather than two that resemble each other.

  The capture program builds against view-oracle's own PtySession rather
  than opening a pty of its own: the hermetic environment a spawned editor
  needs (all four standard-path roots, on Windows too) lives there, and a
  capture that let the operator's own configuration reach the child would
  be measuring that configuration.

.EXAMPLE
  scp scripts/acceptance/capture-conpty.ps1 winserver:capture-conpty.ps1
  ssh winserver 'powershell -NoProfile -File capture-conpty.ps1 -Repo $HOME\view-src -Out $HOME\conpty-capture'

.PARAMETER Repo
  A checkout of this tree on the capture host (path dependencies and the
  .engine-pin file are read from it).

.PARAMETER Nvim
  The pinned engine binary. Defaults to the nvim-win64 unpack CI uses.

.PARAMETER Cols, Rows
  The pty size. Run a second capture at another size to confirm the
  recorded figures move with the input.

.PARAMETER NoCpr
  Leaves the child's cursor-position request unanswered, which is what the
  tree's own responder does today. The control run for the finding: under
  ConPTY nothing at all reaches the master until that request is answered.
#>
param(
  [string]$Repo = "$HOME\view-src",
  [string]$Nvim = "",
  [string]$Out = "$HOME\conpty-capture",
  [int]$Cols = 80,
  [int]$Rows = 24,
  [string]$Label = "conpty",
  [switch]$NoCpr
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $Repo)) { throw "no tree at $Repo" }
$pin = (Get-Content (Join-Path $Repo ".engine-pin")).Trim()

if ($Nvim -eq "") {
  foreach ($candidate in @((Join-Path $Repo "nvim-win64\bin\nvim.exe"),
                           (Join-Path $HOME "nvim-win64\bin\nvim.exe"))) {
    if (Test-Path $candidate) { $Nvim = $candidate; break }
  }
}
if ($Nvim -eq "" -or -not (Test-Path $Nvim)) { throw "no nvim.exe found; pass -Nvim" }

$reported = (& $Nvim --version | Select-Object -First 1)
Write-Output "engine pin: $pin"
Write-Output "engine binary: $Nvim"
Write-Output "engine reports: $reported"
if ($reported -notmatch [regex]::Escape($pin)) {
  Write-Output "ENGINE PIN DRIFT: $reported does not carry $pin"
}

$crate = Join-Path $Out "crate"
New-Item -ItemType Directory -Force -Path (Join-Path $crate "src") | Out-Null

# CAPTURE-CARGO-BEGIN
$CaptureCargo = @'
[package]
name = "capture-conpty"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
portable-pty = "0.9.0"
view-oracle = { path = "__REPO__/crates/view-oracle" }
view-test-support = { path = "__REPO__/crates/view-test-support" }

[workspace]
'@
# CAPTURE-CARGO-END

# CAPTURE-SOURCE-BEGIN
$CaptureMain = @'
//! Records what one pty implementation does to a scripted bare-nvim
//! session, in a form two hosts can be compared byte for byte.
//!
//! Driven by scripts/acceptance/capture-conpty.ps1, which carries this
//! source; see that file's header for the unix arm's extraction.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use view_oracle::pty::{PtySession, QueryPolicy};
use view_test_support::host_deadline;

/// The corpus entry this capture replays: corpus/insert-basic.toml, the
/// shortest entry in the oracle's corpus, hand-translated from its key
/// notation because the corpus drivers send notation over RPC and a pty
/// takes bytes.
const INPUT_NOTATION: &str = "ihello world<Esc>0x";
const INPUT_BYTES: &[u8] = b"ihello world\x1b0x";

/// What the child paints once it has started, and what the replayed script
/// leaves on screen. Both are waited for rather than slept past.
const STARTUP_NEEDLE: &str = "Nvim";
const SETTLED_NEEDLE: &str = "ello world";

/// How long a drained settle window lasts before the stream is called
/// quiet, how often the timed drain samples the recorded length (the
/// resolution every latency figure below carries), and how often a wait on
/// screen content looks.
const SETTLE: Duration = Duration::from_millis(300);
const SAMPLE: Duration = Duration::from_micros(200);
const POLL: Duration = Duration::from_millis(2);

struct Config {
    nvim: PathBuf,
    out: PathBuf,
    cols: u16,
    rows: u16,
    label: String,
    /// Whether this capture answers the cursor-position request the child
    /// writes. Off is the control: a terminal that never answers is what
    /// the tree's own responder is today, and the difference between the
    /// two runs is the finding.
    answer_cpr: bool,
}

impl Config {
    fn from_args() -> Result<Self, Box<dyn Error>> {
        let mut nvim = None;
        let mut out = None;
        let mut cols = 80u16;
        let mut rows = 24u16;
        let mut label = "pty".to_string();
        let mut answer_cpr = true;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--nvim" => nvim = Some(PathBuf::from(value()?)),
                "--out" => out = Some(PathBuf::from(value()?)),
                "--cols" => cols = value()?.parse()?,
                "--rows" => rows = value()?.parse()?,
                "--label" => label = value()?,
                "--no-cpr" => answer_cpr = false,
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        Ok(Self {
            nvim: nvim.ok_or("--nvim is required")?,
            out: out.ok_or("--out is required")?,
            cols,
            rows,
            label,
            answer_cpr,
        })
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let cfg = Config::from_args()?;
    let mut out = String::new();

    writeln!(out, "== host")?;
    writeln!(out, "  label:      {}", cfg.label)?;
    writeln!(out, "  os:         {}", std::env::consts::OS)?;
    writeln!(out, "  arch:       {}", std::env::consts::ARCH)?;
    writeln!(out, "  cpus:       {}", view_test_support::cpus())?;
    writeln!(
        out,
        "  load:       {}",
        view_test_support::host_load()
            .map_or_else(|| "unreported".to_string(), |l| format!("{l:.2}"))
    )?;
    writeln!(out, "  size:       {}x{}", cfg.cols, cfg.rows)?;
    writeln!(out, "  engine:     {}", cfg.nvim.display())?;
    writeln!(out, "  version:    {}", nvim_version(&cfg.nvim)?)?;
    writeln!(out, "  source:     {}", source_fingerprint())?;
    writeln!(out, "  child TERM: xterm-256color, COLORTERM truecolor")?;
    writeln!(out, "  queries:    answered as a full-tier terminal")?;
    writeln!(
        out,
        "  cursor pos: {}",
        if cfg.answer_cpr {
            "answered by this capture"
        } else {
            "left unanswered, as the tree's own responder leaves it"
        }
    )?;
    writeln!(out, "  script:     {INPUT_NOTATION} (corpus/insert-basic.toml)")?;

    stream_and_resize(&cfg, &mut out)?;
    probe_origin(&cfg, &mut out)?;
    exit_code(&cfg, &mut out)?;
    clock(&mut out)?;
    teardown(&cfg, &mut out)?;

    std::fs::create_dir_all(&cfg.out)?;
    let path = cfg
        .out
        .join(format!("{}-{}x{}.txt", cfg.label, cfg.cols, cfg.rows));
    std::fs::write(&path, out.as_bytes())?;
    print!("{out}");
    println!("wrote {}", path.display());
    Ok(())
}

/// The startup stream, the stream the replayed script produces, what a
/// resize emits, and what an explicit redraw of the same screen emits --
/// the last two together being the only way to say whether a resize
/// repaint is distinguishable from an engine-driven one.
fn stream_and_resize(cfg: &Config, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut session = spawn(cfg)?;
    session.record_raw_output();
    let mut cpr = Cpr::new(cfg);

    let started = wait_screen(&mut session, &mut cpr, STARTUP_NEEDLE, host_deadline(Duration::from_secs(20)));
    settle(&mut session, &mut cpr);
    let startup = session.raw_output().to_vec();

    session.send(INPUT_BYTES)?;
    let settled = wait_screen(&mut session, &mut cpr, SETTLED_NEEDLE, host_deadline(Duration::from_secs(20)));
    settle(&mut session, &mut cpr);
    let after_input = session.raw_output()[startup.len()..].to_vec();

    writeln!(out, "\n== startup stream")?;
    writeln!(out, "  painted:  {started}")?;
    writeln!(out, "  cpr answered: {}", cpr.answered)?;
    dump(out, &startup);

    writeln!(out, "\n== scripted stream")?;
    writeln!(out, "  sent:     {}", escaped(INPUT_BYTES))?;
    writeln!(out, "  settled:  {settled}")?;
    dump(out, &after_input);

    writeln!(out, "\n== parsed grid after the script")?;
    grid(&mut session, out);

    let mark = session.raw_output().len();
    let dispatched = Instant::now();
    session.resize(cfg.cols + 20, cfg.rows + 6)?;
    let resize_timing = timed_drain(&mut session, &mut cpr, mark, dispatched);
    let resized = session.raw_output()[mark..].to_vec();

    writeln!(out, "\n== resize {}x{} to {}x{}", cfg.cols, cfg.rows, cfg.cols + 20, cfg.rows + 6)?;
    report_timing(out, &resize_timing);
    dump(out, &resized);
    writeln!(out, "\n== parsed grid after the resize")?;
    grid(&mut session, out);

    let mark = session.raw_output().len();
    let dispatched = Instant::now();
    session.send(b"\x0c")?;
    let redraw_timing = timed_drain(&mut session, &mut cpr, mark, dispatched);
    let redrawn = session.raw_output()[mark..].to_vec();

    writeln!(out, "\n== engine-driven redraw at the same size (ctrl-l)")?;
    report_timing(out, &redraw_timing);
    dump(out, &redrawn);

    session.send(b":qa!\r")?;
    let status = session.wait_for_exit(host_deadline(Duration::from_secs(10)));
    writeln!(out, "\n  quit status: {status:?}")?;
    Ok(())
}

/// Whether the cursor-position request belongs to the editor or to the
/// terminal between it and the master: the same pty, running a program
/// that writes one line and exits. A request that appears here was written
/// by something other than the editor, which decides whether a harness has
/// to answer it for every child or only for this one.
fn probe_origin(cfg: &Config, out: &mut String) -> Result<(), Box<dyn Error>> {
    let (program, first, second) = if cfg!(windows) {
        ("cmd.exe", "/c", "echo hi")
    } else {
        ("/bin/sh", "-c", "echo hi")
    };
    let mut cmd = CommandBuilder::new(program);
    cmd.arg(first);
    cmd.arg(second);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut session =
        PtySession::spawn_configured_with(cmd, cfg.cols, cfg.rows, QueryPolicy::AnswerFullTier)?;
    session.record_raw_output();
    let mut cpr = Cpr::new(cfg);
    settle(&mut session, &mut cpr);
    let status = session.wait_for_exit(host_deadline(Duration::from_secs(20)));
    let raw = session.raw_output().to_vec();

    writeln!(out, "\n== cursor-position request with no editor")?;
    writeln!(out, "  command:  {program} {first} {second}")?;
    writeln!(out, "  status:   {status:?}")?;
    writeln!(
        out,
        "  carries the request: {}",
        raw.windows(CPR_REQUEST.len())
            .any(|window| window == CPR_REQUEST)
    )?;
    dump(out, &raw);
    Ok(())
}

/// `:cq 3` through the pty, observed at the parent: the propagation
/// contract a Windows harness leg would silently lose.
fn exit_code(cfg: &Config, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut session = spawn(cfg)?;
    session.record_raw_output();
    let mut cpr = Cpr::new(cfg);
    let started = wait_screen(&mut session, &mut cpr, STARTUP_NEEDLE, host_deadline(Duration::from_secs(20)));
    session.send(b":cq 3\r")?;
    let status = session.wait_for_exit(host_deadline(Duration::from_secs(20)));

    writeln!(out, "\n== exit code")?;
    writeln!(out, "  painted:     {started}")?;
    writeln!(out, "  cpr answered: {}", cpr.answered)?;
    writeln!(out, "  sent:        :cq 3<CR>")?;
    match status {
        Some(status) => {
            writeln!(out, "  exit_code(): {}", status.exit_code())?;
            writeln!(out, "  success():   {}", status.success())?;
        }
        None => writeln!(out, "  exit_code(): the child never exited")?,
    }
    Ok(())
}

/// What `Instant` resolves to here, measured as the smallest non-zero
/// difference between consecutive readings. That is an upper bound on the
/// true tick (the loop's own cost sits inside it), which is the direction
/// that matters: a bound below a millisecond settles whether the
/// millisecond-scale bench boundaries are measurable.
fn clock(out: &mut String) -> Result<(), Box<dyn Error>> {
    const ROUNDS: usize = 200_000;
    let mut smallest: Option<Duration> = None;
    let mut equal = 0usize;
    let mut total = Duration::ZERO;
    for _ in 0..ROUNDS {
        let first = Instant::now();
        let second = Instant::now();
        let delta = second.saturating_duration_since(first);
        total += delta;
        if delta.is_zero() {
            equal += 1;
        } else if smallest.is_none_or(|held| delta < held) {
            smallest = Some(delta);
        }
    }
    writeln!(out, "\n== clock")?;
    writeln!(out, "  rounds:            {ROUNDS}")?;
    writeln!(out, "  identical pairs:   {equal}")?;
    writeln!(
        out,
        "  smallest non-zero: {}",
        smallest.map_or_else(|| "none observed".to_string(), |d| format!("{d:?}"))
    )?;
    writeln!(out, "  mean pair cost:    {:?}", total / u32::try_from(ROUNDS)?)?;
    Ok(())
}

/// What a per-child kill leaves behind. The oracle's unix path signals the
/// child's whole process group; Windows has no such lever in that path, so
/// the question is whether a grandchild the editor spawned outlives the
/// editor.
fn teardown(cfg: &Config, out: &mut String) -> Result<(), Box<dyn Error>> {
    let mut session = spawn(cfg)?;
    session.record_raw_output();
    let mut cpr = Cpr::new(cfg);
    let _ = wait_screen(&mut session, &mut cpr, STARTUP_NEEDLE, host_deadline(Duration::from_secs(20)));
    let pid = session.pid().ok_or("the pty child reported no pid")?;
    session.send(b":terminal\r")?;

    let cap = host_deadline(Duration::from_secs(20));
    let dispatched = Instant::now();
    let mut spawned = Vec::new();
    while dispatched.elapsed() < cap {
        spawned = descendants(pid);
        if !spawned.is_empty() {
            break;
        }
        answer_cpr(&mut session, &mut cpr);
        std::thread::sleep(POLL);
    }

    writeln!(out, "\n== teardown")?;
    writeln!(out, "  editor pid:  {pid}")?;
    writeln!(out, "  cpr answered: {}", cpr.answered)?;
    writeln!(out, "  it spawned:  {}", describe(&spawned))?;

    session.kill();
    let status = session.wait_for_exit(host_deadline(Duration::from_secs(10)));
    let cap = host_deadline(Duration::from_secs(5));
    let dispatched = Instant::now();
    let mut alive: Vec<(u32, String)> = Vec::new();
    while dispatched.elapsed() < cap {
        alive = spawned
            .iter()
            .filter(|(kid, _)| still_running(*kid))
            .cloned()
            .collect();
        if alive.is_empty() {
            break;
        }
        std::thread::sleep(SAMPLE * 250);
    }

    writeln!(out, "  after kill:  editor status {status:?}")?;
    writeln!(out, "  survivors:   {}", describe(&alive))?;
    writeln!(
        out,
        "  kill lever:  {}",
        if cfg!(unix) {
            "process group (nix::sys::signal::killpg)"
        } else {
            "per-child only (portable_pty::Child::kill)"
        }
    )?;

    // only pids this capture's own editor spawned are reaped here, by pid:
    // the process table is shared with other sessions on this host
    for (kid, _) in &alive {
        reap(*kid);
    }
    Ok(())
}

fn spawn(cfg: &Config) -> Result<PtySession, Box<dyn Error>> {
    let mut cmd = CommandBuilder::new(cfg.nvim.as_os_str());
    cmd.arg("--clean");
    // pinned rather than inherited: the tier the child derives decides how
    // much escape traffic each of its frames carries, which is the thing
    // being compared across the two arms
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    Ok(PtySession::spawn_configured_with(
        cmd,
        cfg.cols,
        cfg.rows,
        QueryPolicy::AnswerFullTier,
    )?)
}

/// The cursor-position request a child writes, and the reply a terminal
/// sitting at the home cell answers with. `QueryPolicy` carries neither:
/// its responder knows the device-attributes fence and the three optional
/// capability queries, and nothing in this tree answers this one.
const CPR_REQUEST: &[u8] = b"\x1b[6n";
const CPR_REPLY: &[u8] = b"\x1b[1;1R";

/// How many cursor-position requests a session has written and how many of
/// them this capture has answered, so the artifact can say whether the
/// child was waiting on a terminal that never replied.
struct Cpr {
    scanned: usize,
    answered: usize,
    answering: bool,
}

impl Cpr {
    fn new(cfg: &Config) -> Self {
        Self {
            scanned: 0,
            answered: 0,
            answering: cfg.answer_cpr,
        }
    }
}

/// Answers every cursor-position request the child has written since the
/// last look. Scanned incrementally with an overlap, because every wait
/// below calls this at its own sampling rate and rescanning the whole
/// recording each time would cost more than the capture measures.
fn answer_cpr(session: &mut PtySession, cpr: &mut Cpr) {
    if !cpr.answering {
        return;
    }
    let raw = session.raw_output();
    if raw.len() < CPR_REQUEST.len() {
        return;
    }
    let from = cpr.scanned.saturating_sub(CPR_REQUEST.len() - 1);
    let hits = raw[from..]
        .windows(CPR_REQUEST.len())
        .filter(|window| *window == CPR_REQUEST)
        .count();
    cpr.scanned = raw.len();
    for _ in 0..hits {
        let _ = session.send(CPR_REPLY);
        cpr.answered += 1;
    }
}

/// Waits for `needle` through the session's recording drain rather than
/// through `PtySession::wait_for`. That method's own wait pulls chunks
/// straight into the parser and past the raw recorder, so every byte a
/// blocking wait consumed would be missing from the stream this capture
/// publishes -- which is the whole artifact.
fn wait_screen(session: &mut PtySession, cpr: &mut Cpr, needle: &str, cap: Duration) -> bool {
    let dispatched = Instant::now();
    loop {
        answer_cpr(session, cpr);
        if session.with_screen(|screen| screen.contents().contains(needle)) {
            return true;
        }
        if dispatched.elapsed() >= cap {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Drains until the recorded stream has stopped growing for a settle
/// window, so a segment boundary falls in a silence rather than mid-frame.
fn settle(session: &mut PtySession, cpr: &mut Cpr) {
    let cap = host_deadline(Duration::from_secs(5));
    let dispatched = Instant::now();
    let mut seen = session.raw_output().len();
    let mut quiet_since = Instant::now();
    while dispatched.elapsed() < cap && quiet_since.elapsed() < host_deadline(SETTLE) {
        answer_cpr(session, cpr);
        let len = session.raw_output().len();
        if len > seen {
            seen = len;
            quiet_since = Instant::now();
        }
        std::thread::sleep(SAMPLE);
    }
}

/// Samples the recorded length from before the act that provoked the
/// output, so a stall between the dispatch and the first sample lands
/// inside the reported figure instead of hiding under it.
struct Timing {
    first: Option<Duration>,
    last: Option<Duration>,
    bytes: usize,
}

fn timed_drain(session: &mut PtySession, cpr: &mut Cpr, mark: usize, dispatched: Instant) -> Timing {
    let cap = host_deadline(Duration::from_secs(5));
    let mut timing = Timing {
        first: None,
        last: None,
        bytes: 0,
    };
    let mut seen = mark;
    let mut quiet_since = Instant::now();
    while dispatched.elapsed() < cap {
        answer_cpr(session, cpr);
        let len = session.raw_output().len();
        if len > seen {
            let at = dispatched.elapsed();
            timing.first.get_or_insert(at);
            timing.last = Some(at);
            seen = len;
            quiet_since = Instant::now();
        } else if timing.first.is_some() && quiet_since.elapsed() >= SETTLE {
            break;
        }
        std::thread::sleep(SAMPLE);
    }
    timing.bytes = seen - mark;
    timing
}

fn report_timing(out: &mut String, timing: &Timing) {
    let _ = writeln!(
        out,
        "  first byte:  {}",
        timing
            .first
            .map_or_else(|| "nothing arrived".to_string(), |d| format!("{d:?}"))
    );
    let _ = writeln!(
        out,
        "  last byte:   {}",
        timing
            .last
            .map_or_else(|| "nothing arrived".to_string(), |d| format!("{d:?}"))
    );
    let _ = writeln!(out, "  sampled at:  {SAMPLE:?} resolution");
    let _ = writeln!(out, "  bytes:       {}", timing.bytes);
}

/// Both resolutions of the same screen: what the parser calls the text
/// (`contents`, which is what every screen assertion in this tree reads)
/// and what its cells hold row by row. They part company when a painter
/// fills every column of a row, so publishing only one of them would hide
/// the divergence rather than record it.
fn grid(session: &mut PtySession, out: &mut String) {
    let (cursor, size) = session.with_screen(|screen| (screen.cursor_position(), screen.size()));
    let contents = session.screen();
    let _ = writeln!(out, "  cursor:         row {} col {}", cursor.0, cursor.1);
    let _ = writeln!(out, "  parser size:    {} rows {} cols", size.0, size.1);
    let _ = writeln!(out, "  contents lines: {}", contents.lines().count());
    let _ = writeln!(out, "  contents:");
    for line in contents.lines() {
        let _ = writeln!(out, "    |{}", line.trim_end());
    }
    let _ = writeln!(out, "  cells:");
    for row in 0..size.0 {
        let rendered: String = session.with_screen(|screen| {
            (0..size.1)
                .map(|col| {
                    // an untouched cell reports nothing, and rendering that
                    // as nothing would shift every column after it
                    screen.cell(row, col).map_or(" ".to_string(), |cell| {
                        let held = cell.contents();
                        if held.is_empty() {
                            " ".to_string()
                        } else {
                            held.to_string()
                        }
                    })
                })
                .collect()
        });
        let _ = writeln!(out, "    |{}", rendered.trim_end());
    }
}

fn dump(out: &mut String, bytes: &[u8]) {
    let _ = writeln!(out, "  bytes:    {}", bytes.len());
    let _ = writeln!(out, "  escaped:");
    for line in wrap(&escaped(bytes)) {
        let _ = writeln!(out, "    {line}");
    }
    let _ = writeln!(out, "  hex:");
    for line in wrap(&hex(bytes)) {
        let _ = writeln!(out, "    {line}");
    }
}

fn escaped(bytes: &[u8]) -> String {
    let mut rendered = String::new();
    for byte in bytes {
        match byte {
            b'\\' => rendered.push_str("\\\\"),
            0x20..=0x7e => rendered.push(char::from(*byte)),
            other => {
                let _ = write!(rendered, "\\x{other:02x}");
            }
        }
    }
    rendered
}

fn hex(bytes: &[u8]) -> String {
    let mut rendered = String::new();
    for byte in bytes {
        let _ = write!(rendered, "{byte:02x} ");
    }
    rendered.trim_end().to_string()
}

/// Wraps on a character count rather than a byte count: an escaped dump is
/// ASCII by construction, and a hex dump is too.
fn wrap(rendered: &str) -> Vec<String> {
    rendered
        .chars()
        .collect::<Vec<_>>()
        .chunks(68)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn nvim_version(nvim: &PathBuf) -> Result<String, Box<dyn Error>> {
    let output = Command::new(nvim).arg("--version").output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("unreported")
        .trim()
        .to_string())
}

/// A fingerprint of this program's own source, so two arms can show they
/// ran one capture rather than two that resemble each other. Carriage
/// returns are excluded: the Windows arm writes the source through
/// PowerShell and the unix arm extracts it with awk, and the line ending is
/// the one difference neither side chose.
fn source_fingerprint() -> String {
    const SOURCE: &str = include_str!("main.rs");
    let normalized = || SOURCE.bytes().filter(|byte| *byte != b'\r');
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in normalized() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a-64 {hash:016x} over {} bytes", normalized().count())
}

fn process_rows() -> Vec<(u32, u32, String)> {
    #[cfg(windows)]
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance Win32_Process | ForEach-Object { \"$($_.ProcessId) $($_.ParentProcessId) $($_.Name)\" }",
        ])
        .output();
    #[cfg(not(windows))]
    let output = Command::new("ps").args(["-eo", "pid=,ppid=,comm="]).output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent = fields.next()?.parse().ok()?;
            Some((pid, parent, fields.next().unwrap_or("?").to_string()))
        })
        .collect()
}

fn descendants(root: u32) -> Vec<(u32, String)> {
    let rows = process_rows();
    let mut children: BTreeMap<u32, Vec<(u32, String)>> = BTreeMap::new();
    for (pid, parent, name) in rows {
        children.entry(parent).or_default().push((pid, name));
    }
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for (kid, name) in children.get(&pid).into_iter().flatten() {
            found.push((*kid, name.clone()));
            frontier.push(*kid);
        }
    }
    found
}

fn still_running(pid: u32) -> bool {
    process_rows().iter().any(|(known, _, _)| *known == pid)
}

fn reap(pid: u32) {
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
    #[cfg(not(windows))]
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();
}

fn describe(found: &[(u32, String)]) -> String {
    if found.is_empty() {
        return "none".to_string();
    }
    found
        .iter()
        .map(|(pid, name)| format!("{pid} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}
'@
# CAPTURE-SOURCE-END

$repoForToml = $Repo -replace '\\', '/'
# the trailing newline is written explicitly, and Set-Content is told to add
# none of its own: the unix arm's awk extraction ends the file with one, and
# the source fingerprint the capture reports is only comparable if both
# sides hold the same bytes
Set-Content -Path (Join-Path $crate "Cargo.toml") `
  -Value ($CaptureCargo.Replace('__REPO__', $repoForToml) + "`n") -NoNewline
Set-Content -Path (Join-Path $crate "src\main.rs") -Value ($CaptureMain + "`n") -NoNewline

$captureArgs = @("--nvim", $Nvim, "--out", $Out, "--cols", $Cols, "--rows", $Rows, "--label", $Label)
if ($NoCpr) { $captureArgs += "--no-cpr" }
& cargo run --quiet --manifest-path (Join-Path $crate "Cargo.toml") -- @captureArgs
Write-Output "EXIT=$LASTEXITCODE"
