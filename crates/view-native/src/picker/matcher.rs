//! The picker's nucleo-backed matcher worker (spec section 18): a single
//! long-lived thread that owns every `Nucleo` handle for the process,
//! receiving [`MatchRequest`]s off a channel and streaming ranked results
//! back as `Msg::PickerResults`. `view-core::native::picker::PickerState`
//! never holds a matcher handle itself (see that module's doc), so this is
//! the only place in the workspace that touches `nucleo`.
//!
//! # Session reuse
//!
//! One [`Session`] (a `Nucleo<PickerItem>` plus the `Source` it was built
//! for) stays alive across requests: a keystroke against an already-open
//! picker only reparses the query pattern, it never re-walks the
//! filesystem or re-lists buffers. A session is rebuilt from scratch only
//! when a request's `source` no longer matches the cached one -- a
//! different picker verb, or a different `Files` root.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;

use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};

use view_core::msg::Msg;
use view_core::native::picker::{PickerItem, Source};

use crate::picker::sources;

/// How many ranked rows the worker streams per `Msg::PickerResults`: enough
/// for the picker overlay's own row budget, small enough that copying the
/// ranked slice off nucleo's snapshot on every tick stays cheap even while
/// a large `Files` scan is still running.
const STREAM_ROWS: u32 = 200;

/// How long one `Nucleo::tick` may block waiting for matcher/scan progress
/// before the worker checks its request channel again for a newer,
/// superseding query. Nucleo's own docs recommend 10ms; short enough that a
/// keystroke arriving mid-scan preempts within one tick instead of waiting
/// out a whole pass over a large corpus.
const TICK_BUDGET_MS: u64 = 10;

/// One picker query handed to the matcher worker off the runtime loop
/// thread; mirrors `Effect::PickerQuery`'s fields verbatim (see that
/// variant's doc for the contract each one carries).
pub struct MatchRequest {
    pub generation: u64,
    pub needle: String,
    pub source: Source,
    pub resolved: Option<Vec<PickerItem>>,
}

/// Everything the runtime loop can hand the matcher worker over its one
/// channel. A query and a close share a channel rather than each getting
/// its own, so the worker never has to pick between two receivers to learn
/// which arrived first -- `Close` racing ahead of a stale `Query` (a picker
/// closed the instant after its last keystroke queued) must be seen in
/// send order, which one channel guarantees and two separate ones would
/// not.
pub enum WorkerRequest {
    /// Mirrors `Effect::PickerQuery`; see [`MatchRequest`]'s own doc.
    Query(MatchRequest),
    /// Mirrors `Effect::PickerClose`: drops the worker's live `Session`,
    /// cancelling any `Files` scan still in flight against it (see
    /// `Session`'s `Drop`) -- see that effect's doc for why this cannot
    /// wait for the next differently-sourced query instead.
    Close,
}

/// Spawns the matcher worker: one thread for the process's lifetime, torn
/// down only when `rx` disconnects (the runtime's `Executor` drops its
/// sender at process exit, the same shutdown shape `view::clipboard::spawn`
/// uses). Long-lived rather than per-session, so a `Files` scan already
/// walked survives a picker close/reopen instead of re-walking the tree on
/// every open.
pub fn spawn(rx: Receiver<WorkerRequest>, tx: SyncSender<Msg>) -> JoinHandle<()> {
    std::thread::spawn(move || run(&rx, &tx))
}

/// One cached matcher instance plus the source it was built for.
struct Session {
    source: Source,
    nucleo: Nucleo<PickerItem>,
    /// Set once a `Files` scan thread for `source` has started, so a second
    /// query against the same root does not spawn a second walker.
    scan_started: AtomicBool,
    /// Flipped by this session's own `Drop` to stop a `Files` scan thread
    /// still walking when the session is replaced or torn down, so an
    /// abandoned scan does not keep consuming disk and CPU pushing into an
    /// injector nothing reads anymore. Shared with
    /// `sources::spawn_file_scan` via `Arc`, so the walker thread observes
    /// the same flag this session's drop sets.
    cancel: Arc<AtomicBool>,
}

impl Session {
    fn new(source: Source) -> Self {
        Self {
            source,
            // A no-op notify callback: this worker drives its own tick loop
            // synchronously (see `stream_until_preempted`) instead of
            // waiting on nucleo's background-thread wakeup, so there is
            // nothing for the callback to signal.
            nucleo: Nucleo::new(Config::DEFAULT, Arc::new(|| {}), None, 1),
            scan_started: AtomicBool::new(false),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Session {
    /// Signals rather than joins: a slow-to-notice walker thread (blocked
    /// in a disk syscall) must never stall the matcher worker's own
    /// responsiveness while it waits for the signal to be noticed, so this
    /// only sets the flag and returns, leaving the walker thread to exit on
    /// its own time.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// Drops the worker's live session, cancelling any `Files` scan still in
/// flight against it (`Session`'s own `Drop` flips the cancel flag -- see
/// its doc). The one place a close the picker overlay itself initiated,
/// rather than a later query for a different source, tears the session
/// down -- shared by [`run`]'s own `WorkerRequest::Close` arm and this
/// module's close test, so the test exercises the exact code production
/// runs rather than a look-alike.
fn close_session(session: &mut Option<Session>) {
    *session = None;
}

fn run(rx: &Receiver<WorkerRequest>, tx: &SyncSender<Msg>) {
    let mut session: Option<Session> = None;
    let mut next = rx.recv();
    while let Ok(request) = next {
        next = match request {
            WorkerRequest::Close => {
                close_session(&mut session);
                rx.recv()
            }
            WorkerRequest::Query(request) => handle_query(&mut session, request, rx, tx),
        };
    }
}

/// Runs one `MatchRequest` to completion or preemption, returning the next
/// request to process: whatever preempted this one mid-stream (a newer
/// query, or a close -- either stops `stream_until_preempted`'s tick loop
/// the same way), or whatever `rx.recv()` yields once this one finishes
/// cleanly instead.
fn handle_query(
    session: &mut Option<Session>,
    request: MatchRequest,
    rx: &Receiver<WorkerRequest>,
    tx: &SyncSender<Msg>,
) -> Result<WorkerRequest, RecvError> {
    ensure_session(session, &request.source);
    let Some(active) = session.as_mut() else {
        // `ensure_session` always leaves `session` populated; this arm
        // exists only so the match stays total for the borrow checker.
        return rx.recv();
    };
    seed_or_scan(active, request.resolved, &request.needle);
    active.nucleo.pattern.reparse(
        0,
        &request.needle,
        CaseMatching::Smart,
        Normalization::Smart,
        false,
    );
    match stream_until_preempted(active, request.generation, rx, tx) {
        Some(preempting) => Ok(preempting),
        None => rx.recv(),
    }
}

/// Rebuilds `session` from scratch when `source` no longer matches the
/// cached one, leaving an already-matching session untouched so the corpus
/// it already gathered survives.
fn ensure_session(session: &mut Option<Session>, source: &Source) {
    let stale = session.as_ref().map(|active| &active.source) != Some(source);
    if stale {
        *session = Some(Session::new(source.clone()));
    }
}

/// Applies this request's corpus update: a `Buffers` seed (pre-gathered by
/// `view-engine`, since only it can speak RPC -- see `Effect::PickerQuery`'s
/// `resolved` field doc) replaces the cached corpus outright; a `Files`
/// source starts its background walk at most once per session; a
/// `LiveGrep` source re-walks and re-searches on every call, cancelling
/// whatever scan it started for the previous query (see
/// [`restart_live_grep`] -- `Effect::PickerQuery`'s doc names this as the
/// one source that never reuses a cached corpus).
fn seed_or_scan(active: &mut Session, resolved: Option<Vec<PickerItem>>, needle: &str) {
    if let Some(items) = resolved {
        seed_resolved(active, items);
        return;
    }
    match &active.source {
        Source::Files { root } => {
            if !active.scan_started.swap(true, Ordering::AcqRel) {
                let _handle = sources::spawn_file_scan(
                    root.clone(),
                    active.nucleo.injector(),
                    active.cancel.clone(),
                );
            }
        }
        Source::LiveGrep { root } => restart_live_grep(active, root.clone(), needle),
        // `Buffers`, and any future `Source` variant `view-core` adds: this
        // module never gets ahead of that enum (it is `#[non_exhaustive]`
        // for exactly this reason) -- a genuinely new source's own scan is
        // added here explicitly, never inferred from a wildcard arm.
        _ => {}
    }
}

/// Cancels whatever `LiveGrep` scan is still populating `active`'s injector
/// for a prior query (this session's `cancel` flag is swapped for a fresh
/// one rather than reused, since a `LiveGrep` session's scan restarts on
/// every query -- unlike `Files`, whose single-scan-per-session shape lets
/// `Session::new`'s original flag live for the session's whole lifetime),
/// clears the matcher's snapshot, and starts a new scan for `needle`
/// against `root`. An empty `needle` starts no scan at all: searching every
/// line of every file for nothing is not a query, it is the entire
/// worktree's content, which is neither what a user typing a live-grep
/// query wants to see first nor a payload worth pushing through the
/// injector.
fn restart_live_grep(active: &mut Session, root: std::path::PathBuf, needle: &str) {
    active.cancel.store(true, Ordering::Release);
    active.cancel = Arc::new(AtomicBool::new(false));
    active.nucleo.restart(true);
    if needle.is_empty() {
        return;
    }
    let _handle = sources::spawn_live_grep_scan(
        root,
        needle.to_string(),
        active.nucleo.injector(),
        active.cancel.clone(),
    );
}

/// Replaces the cached instance's corpus wholesale with `items`, clearing
/// whatever it held before: `Source::Buffers`'s listed set can shrink
/// between two queries (a buffer closed), and a stale entry must not linger
/// in the ranked results.
fn seed_resolved(active: &mut Session, items: Vec<PickerItem>) {
    active.nucleo.restart(true);
    let injector = active.nucleo.injector();
    for item in items {
        injector.push(item, |item, cols| {
            cols[0] = item.label.as_str().into();
        });
    }
}

/// Ticks `active`'s matcher until nucleo reports no more in-flight work
/// (`Status::running` false) or a newer request preempts it, streaming one
/// `Msg::PickerResults` per tick that actually changed the ranked set. This
/// is what makes a keystroke against a still-running `Files` scan visible
/// before the scan thread exits: nucleo's `tick` matches whatever the
/// injector has produced so far, and its snapshot reflects that partial
/// corpus, not the finished one.
fn stream_until_preempted(
    active: &mut Session,
    generation: u64,
    rx: &Receiver<WorkerRequest>,
    tx: &SyncSender<Msg>,
) -> Option<WorkerRequest> {
    loop {
        let status = active.nucleo.tick(TICK_BUDGET_MS);
        if status.changed {
            send_results(active, generation, tx);
        }
        if !status.running {
            return None;
        }
        // a newer query or a close has already arrived: stop ticking the
        // superseded request and let the caller pick it up, rather than
        // spending the rest of this pass's budget on results
        // `PickerState::apply_results` will drop as stale anyway (or a
        // session about to be dropped regardless)
        if let Ok(next) = rx.try_recv() {
            return Some(next);
        }
    }
}

/// Copies the top-ranked entries off `active`'s current snapshot and sends
/// them as one `Msg::PickerResults`, tagged `generation`. A dropped
/// receiver (the runtime loop exiting) is not an error here: the worker
/// notices on its next `rx.recv()`, the same as every other unwired-channel
/// degrade in this workspace.
fn send_results(active: &mut Session, generation: u64, tx: &SyncSender<Msg>) {
    let items = build_results(active);
    let _ = tx.send(Msg::PickerResults { generation, items });
}

/// The pure half of [`send_results`]: ranks, converts nucleo's char-based
/// match indices into the byte offsets `PickerItem::indices` documents
/// (`nucleo_matcher::pattern::Pattern::indices` returns Unicode codepoint
/// offsets, but `view-core`'s span builder slices `label` by byte range),
/// and returns the ranked `Vec<PickerItem>` without touching `tx` -- kept
/// separate so this module's streaming test can drive it directly against a
/// synthetic corpus.
fn build_results(active: &mut Session) -> Vec<PickerItem> {
    let snapshot = active.nucleo.snapshot();
    let take = snapshot.matched_item_count().min(STREAM_ROWS);
    let column_pattern = snapshot.pattern().column_pattern(0);
    let mut matcher = nucleo::Matcher::default();
    let mut char_indices = Vec::new();
    let mut items = Vec::with_capacity(take as usize);
    for matched in snapshot.matched_items(0..take) {
        char_indices.clear();
        let _ = column_pattern.indices(
            matched.matcher_columns[0].slice(..),
            &mut matcher,
            &mut char_indices,
        );
        let mut item = matched.data.clone();
        item.indices = char_to_byte_offsets(&item.label, &char_indices);
        items.push(item);
    }
    items
}

/// Converts nucleo's char (Unicode codepoint) match indices into byte
/// offsets into `label`. `char_indices` need not arrive sorted; the
/// returned `Vec` is ascending by construction since it is built by a
/// single forward walk over `label`'s own `char_indices()`.
fn char_to_byte_offsets(label: &str, char_indices: &[u32]) -> Vec<u32> {
    if char_indices.is_empty() {
        return Vec::new();
    }
    let mut wanted: Vec<u32> = char_indices.to_vec();
    wanted.sort_unstable();
    let mut wanted = wanted.into_iter().peekable();
    let mut byte_offsets = Vec::with_capacity(char_indices.len());
    for (char_idx, (byte_idx, _)) in label.char_indices().enumerate() {
        while wanted.peek() == Some(&(char_idx as u32)) {
            byte_offsets.push(byte_idx as u32);
            wanted.next();
        }
        if wanted.peek().is_none() {
            break;
        }
    }
    byte_offsets
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn char_to_byte_offsets_shifts_past_multibyte_characters() {
        // "cafe" with a trailing accented e (2 bytes): a char index of 3
        // (the accented character) must map to byte 3, and any wanted index
        // past it would be off by one if this walked bytes instead of chars
        let offsets = char_to_byte_offsets("caf\u{e9}", &[0, 3]);
        assert_eq!(offsets, vec![0, 3]);
    }

    #[test]
    fn a_full_query_round_trip_streams_a_ranked_result() {
        let (req_tx, req_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::sync_channel(16);
        let _worker = spawn(req_rx, msg_tx);
        req_tx
            .send(WorkerRequest::Query(MatchRequest {
                generation: 1,
                needle: "buf".to_string(),
                source: Source::Buffers,
                resolved: Some(vec![
                    PickerItem::new("src/buffer.rs"),
                    PickerItem::new("README.md"),
                ]),
            }))
            .expect("worker channel closed");
        let msg = msg_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no PickerResults within 5s");
        match msg {
            Msg::PickerResults { generation, items } => {
                assert_eq!(generation, 1);
                assert!(items.iter().any(|item| item.label == "src/buffer.rs"));
            }
            other => panic!("expected Msg::PickerResults, got {other:?}"),
        }
    }

    /// The brief's falsifiable streaming check: a non-empty, ranked result
    /// set observed before a 1,000,000-entry scan thread has exited. Drives
    /// this module's own tick/stream loop (`stream_until_preempted`,
    /// `build_results`) directly against a synthetic, staggered producer
    /// instead of a literal on-disk fixture of a million files -- creating
    /// that many real files is not itself the property under test; nucleo's
    /// own tick/snapshot streaming contract is, layered under this module's
    /// generation tagging and byte-offset conversion. `sources::spawn_file_scan`
    /// (exercised by the `Source::Files` production path) feeds the same
    /// `Injector` this producer feeds directly.
    #[test]
    fn a_result_set_streams_before_a_million_entry_scan_finishes() {
        let mut session = Session::new(Source::Files {
            root: std::path::PathBuf::from("/synthetic"),
        });
        let injector = session.nucleo.injector();
        let producer = std::thread::spawn(move || {
            for i in 0..1_000_000u32 {
                injector.push(PickerItem::new(format!("file-{i}.rs")), |item, cols| {
                    cols[0] = item.label.as_str().into();
                });
                if i % 2_000 == 0 {
                    std::thread::sleep(Duration::from_micros(300));
                }
            }
        });
        session
            .nucleo
            .pattern
            .reparse(0, "file", CaseMatching::Smart, Normalization::Smart, false);

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut saw_results_while_running = false;
        loop {
            let status = session.nucleo.tick(TICK_BUDGET_MS);
            if status.changed {
                let items = build_results(&mut session);
                if !items.is_empty() && !producer.is_finished() {
                    saw_results_while_running = true;
                    break;
                }
            }
            if !status.running && producer.is_finished() {
                break;
            }
            if Instant::now() > deadline {
                break;
            }
        }
        assert!(
            saw_results_while_running,
            "expected a non-empty ranked result set before the 1,000,000-entry \
             scan thread exited"
        );
        producer.join().expect("producer thread panicked");
    }

    /// A real, on-disk tree under the workspace's own `target/tmp` (reached
    /// via `CARGO_MANIFEST_DIR` rather than the shared system temp
    /// directory, since `CARGO_TARGET_TMPDIR` is only set for
    /// integration-test/bench binaries, never for a `#[cfg(test)]` unit
    /// test inside a lib target -- see `build()` below) -- large enough
    /// that a walk cancelled shortly after it starts still has most of the
    /// tree left unvisited, so the disconfirm below stays accurate
    /// regardless of how fast disk I/O is on the host running it. Removed
    /// on drop so a failed run does not leave 20,000 files behind.
    struct CancelTestTree {
        root: std::path::PathBuf,
        total: u32,
    }

    impl CancelTestTree {
        fn build() -> Self {
            let nonce = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            // `CARGO_TARGET_TMPDIR` is integration-test-only (cargo never
            // populates it for a `#[cfg(test)]` unit test inside a lib
            // target), so this reaches the workspace's own `target/`
            // through `CARGO_MANIFEST_DIR` instead -- always set at compile
            // time, for every target, including under `cargo clippy`
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/tmp")
                .join(format!("picker-cancel-{nonce}"));
            let dirs = 200u32;
            let files_per_dir = 100u32;
            let mut total = 0u32;
            for d in 0..dirs {
                let dir = root.join(format!("d{d}"));
                std::fs::create_dir_all(&dir).expect("create synthetic cancel-test dir");
                for f in 0..files_per_dir {
                    std::fs::write(dir.join(format!("f{f}.rs")), [])
                        .expect("create synthetic cancel-test file");
                    total += 1;
                }
            }
            Self { root, total }
        }
    }

    impl Drop for CancelTestTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A falsifiable cancellation check: starts a real `Files` scan,
    /// replaces its session the way `run()` does on every request whose
    /// source no longer matches the cached one, and proves the old
    /// session's walker thread actually stopped rather than running on in
    /// the background pushing into an injector nothing reads anymore.
    /// Deterministic rather than a sleep-and-hope: `Injector::injected_items`
    /// only ever equals `tree.total` once the walk has visited every entry,
    /// so an observed count strictly below it, once the count has stopped
    /// growing, can only mean the walk exited early -- disabling the
    /// `cancel` check in `sources::spawn_file_scan` makes this fail by
    /// name, since the old session's walker would then run to completion
    /// and settle at exactly `tree.total`.
    #[test]
    fn replacing_a_session_cancels_its_files_scan_in_flight() {
        let tree = CancelTestTree::build();
        let mut session: Option<Session> = None;
        ensure_session(
            &mut session,
            &Source::Files {
                root: tree.root.clone(),
            },
        );
        let active = session
            .as_mut()
            .expect("ensure_session always populates session");
        let injector = active.nucleo.injector();
        seed_or_scan(active, None, "");

        let start_deadline = Instant::now() + Duration::from_secs(10);
        while injector.injected_items() == 0 {
            assert!(
                Instant::now() < start_deadline,
                "the scan never produced a single item"
            );
        }

        // the shape `run()` takes on every request whose source no longer
        // matches the cached one: replaces the session outright, which must
        // drop -- and so cancel -- the old one's still-running Files scan
        ensure_session(&mut session, &Source::Buffers);

        let settle_deadline = Instant::now() + Duration::from_secs(5);
        let mut last = injector.injected_items();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = injector.injected_items();
            if now == last {
                break;
            }
            last = now;
            assert!(
                Instant::now() < settle_deadline,
                "the injected item count never stopped growing after the \
                 session was replaced"
            );
        }
        assert!(
            last < tree.total,
            "expected the old session's scan to stop before walking the \
             whole {}-entry tree once its session was replaced, got {last} \
             items",
            tree.total
        );
    }

    /// The same falsifiable check as `replacing_a_session_cancels_its_
    /// files_scan_in_flight`, but for the close path (`Effect::PickerClose`
    /// -> `WorkerRequest::Close`) instead of a session replaced by a later,
    /// differently-sourced query: closing the picker overlay is the
    /// dominant real way a user abandons a huge scan, and unlike a
    /// replacement it may never be followed by another query at all, so it
    /// cannot rely on that path to eventually cancel the walker. Calls
    /// `close_session` directly rather than driving the real `spawn`/`run`
    /// channel loop, the same directness `replacing_a_session_cancels_its_
    /// files_scan_in_flight` uses -- and `run`'s own `WorkerRequest::Close`
    /// arm calls this exact function, so the test exercises production
    /// code rather than a look-alike. Disabling the `cancel` check in
    /// `sources::spawn_file_scan` makes this fail by name the same way, at
    /// exactly `tree.total`.
    #[test]
    fn closing_the_picker_cancels_its_files_scan_in_flight() {
        let tree = CancelTestTree::build();
        let mut session: Option<Session> = None;
        ensure_session(
            &mut session,
            &Source::Files {
                root: tree.root.clone(),
            },
        );
        let active = session
            .as_mut()
            .expect("ensure_session always populates session");
        let injector = active.nucleo.injector();
        seed_or_scan(active, None, "");

        let start_deadline = Instant::now() + Duration::from_secs(10);
        while injector.injected_items() == 0 {
            assert!(
                Instant::now() < start_deadline,
                "the scan never produced a single item"
            );
        }

        // the shape `run()` takes on a `WorkerRequest::Close`: drops the
        // session outright, which must cancel the old one's still-running
        // Files scan the same way a replacement does
        close_session(&mut session);
        assert!(
            session.is_none(),
            "close_session must leave no session behind"
        );

        let settle_deadline = Instant::now() + Duration::from_secs(5);
        let mut last = injector.injected_items();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = injector.injected_items();
            if now == last {
                break;
            }
            last = now;
            assert!(
                Instant::now() < settle_deadline,
                "the injected item count never stopped growing after the \
                 picker was closed"
            );
        }
        assert!(
            last < tree.total,
            "expected the scan to stop before walking the whole \
             {}-entry tree once the picker was closed, got {last} items",
            tree.total
        );
    }

    /// A real, on-disk tree whose every file's content matches the same
    /// needle (`"target"`), so a `LiveGrep` scan against it has plenty of
    /// matching lines still left to find when a follow-up query preempts it
    /// -- the content analogue of `CancelTestTree`'s "large enough that a
    /// cancel lands mid-walk" property, needed because `LiveGrep`'s scan
    /// must actually be reading and matching file content, not merely
    /// enumerating paths, for a cancellation mid-scan to be meaningful.
    struct GrepCancelTestTree {
        root: std::path::PathBuf,
    }

    impl GrepCancelTestTree {
        fn build() -> Self {
            let nonce = format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            );
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/tmp")
                .join(format!("picker-grep-cancel-{nonce}"));
            for d in 0..200u32 {
                let dir = root.join(format!("d{d}"));
                std::fs::create_dir_all(&dir).expect("create synthetic grep-cancel dir");
                for f in 0..20u32 {
                    let body = "one target line\nanother target line\nno match here\n";
                    std::fs::write(dir.join(format!("f{f}.rs")), body)
                        .expect("create synthetic grep-cancel file");
                }
            }
            Self { root }
        }
    }

    impl Drop for GrepCancelTestTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The same falsifiable cancellation shape
    /// `replacing_a_session_cancels_its_files_scan_in_flight` proves for
    /// `Files`, for `LiveGrep`'s own per-query restart instead of a
    /// session-replacement teardown: a second query against the *same*
    /// `LiveGrep` session must stop the first query's still-running scan,
    /// not let it keep pushing matches for a needle the picker no longer
    /// shows. Disabling the `cancel` check in `sources::spawn_live_grep_scan`
    /// makes this fail by name, the same way disabling it in
    /// `spawn_file_scan` makes the `Files` cancellation test fail.
    #[test]
    fn a_new_live_grep_query_cancels_the_previous_scan_in_flight() {
        let tree = GrepCancelTestTree::build();
        let mut session: Option<Session> = None;
        ensure_session(
            &mut session,
            &Source::LiveGrep {
                root: tree.root.clone(),
            },
        );
        let active = session
            .as_mut()
            .expect("ensure_session always populates session");
        seed_or_scan(active, None, "target");
        // obtained AFTER the scan starts, not before: `Nucleo::restart`
        // (which `seed_or_scan`'s `LiveGrep` arm calls on every query, see
        // `restart_live_grep`) disconnects any injector handle acquired
        // ahead of it from the instance the new scan actually pushes into
        // -- `seed_resolved`'s own restart-then-`injector()` order is the
        // same precedent.
        let injector = active.nucleo.injector();

        let start_deadline = Instant::now() + Duration::from_secs(10);
        while injector.injected_items() == 0 {
            assert!(
                Instant::now() < start_deadline,
                "the grep scan never produced a single match"
            );
        }

        // the shape `run()` takes on every subsequent query against a
        // session whose source is unchanged: `seed_or_scan` itself must
        // cancel the previous query's scan before starting the new one
        seed_or_scan(active, None, "no-such-needle-anywhere");

        let settle_deadline = Instant::now() + Duration::from_secs(5);
        let mut last = injector.injected_items();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = injector.injected_items();
            if now == last {
                break;
            }
            last = now;
            assert!(
                Instant::now() < settle_deadline,
                "the injected item count never stopped growing after the \
                 query changed"
            );
        }
        // 200 dirs * 20 files * 2 matching lines each = 8,000 possible
        // matches for "target"; the second query's needle matches nothing,
        // so any growth past this point can only be the stale first scan
        // still running
        assert!(
            last < 8_000,
            "expected the first query's scan to stop before matching every \
             line in the tree once a new query preempted it, got {last} items"
        );
    }

    /// Spec 3.1's picker-match row (keystroke -> first results painted,
    /// 100k resident entries, <= 16 ms) measured once here and recorded in
    /// the commit description; task 16 owns the paired, class-gated
    /// bench-suite version of this budget. "Resident" means the corpus is
    /// fully matched before the timer starts: the cost under measurement is
    /// one incremental re-match against an already-settled 100k-item
    /// snapshot, not the one-time cost of loading it.
    #[test]
    fn keystroke_to_first_results_at_100k_resident_entries() {
        let mut session = Session::new(Source::Buffers);
        let injector = session.nucleo.injector();
        for i in 0..100_000u32 {
            injector.push(
                PickerItem::new(format!("crates/view-core/src/native/picker_{i}.rs")),
                |item, cols| cols[0] = item.label.as_str().into(),
            );
        }
        session
            .nucleo
            .pattern
            .reparse(0, "", CaseMatching::Smart, Normalization::Smart, false);
        let settle_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            session.nucleo.tick(TICK_BUDGET_MS);
            if session.nucleo.snapshot().matched_item_count() == 100_000 {
                break;
            }
            assert!(
                Instant::now() < settle_deadline,
                "100k-entry corpus never fully settled"
            );
        }

        let start = Instant::now();
        session.nucleo.pattern.reparse(
            0,
            "picker_42",
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let items = loop {
            let status = session.nucleo.tick(TICK_BUDGET_MS);
            if status.changed {
                let items = build_results(&mut session);
                if !items.is_empty() {
                    break items;
                }
            }
            assert!(
                Instant::now() < deadline,
                "no results for the keystroke query within 5s"
            );
        };
        let elapsed = start.elapsed();
        assert!(items.iter().any(|item| item.label.contains("picker_42")));
        // a generous, debug-build-safe sanity ceiling, not the spec's real
        // 16 ms bar -- that number was measured once in release mode for
        // this commit's description (4.550806 ms) and is gated going
        // forward by task 16's paired, class-gated bench suite, not this
        // unit test; a debug build's unoptimized fuzzy match alone measured
        // 24.924149 ms here, which is why this ceiling is generous rather
        // than the product bar
        assert!(
            elapsed < Duration::from_secs(1),
            "keystroke -> first results at 100k resident took {elapsed:?}"
        );
    }
}
