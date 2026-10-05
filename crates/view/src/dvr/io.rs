//! The DVR's file work, on a thread of its own: hashing the files the
//! session showed, writing clips and reading them back.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::Arc;

use view_core::hash::{fnv1a_extend, FNV_OFFSET};
use view_core::msg::Msg;
use view_core::native::dvr::{DvrIoReply, Marker};
use view_proc::writer::BackgroundWriter;
use view_tui::dvr::{FrameRing, RingSnapshot};

use super::clip::{self, Inputs};
use crate::wake::LoopSender;

/// The jobs queued ahead of the one the thread is doing.
const QUEUE: usize = 8;

/// One piece of file work.
pub(crate) enum IoJob {
    /// Hash these files, each the first time it is seen.
    Baseline(Vec<String>),
    /// Hash every baselined file again and reply which changed.
    DiskCheck,
    /// Write a clip.
    Export(Export),
    /// Read the clip at `path` into a ring holding a recording bound of
    /// `max_bytes`, and hand the ring to the loop.
    Play {
        /// The clip's file.
        path: PathBuf,
        /// The recording bound the ring is read with.
        max_bytes: usize,
    },
}

/// What an export writes, and where.
pub(crate) struct Export {
    /// The file to create.
    pub(crate) path: PathBuf,
    /// The frames, shared with the ring until the clip is written.
    pub(crate) frames: RingSnapshot,
    /// The input log's records.
    pub(crate) inputs: Inputs,
    /// The marks on the timeline.
    pub(crate) markers: Vec<(u64, Marker)>,
    /// The frame ranges a branch abandoned.
    pub(crate) dead: Vec<RangeInclusive<u64>>,
    /// Held until the reply is sent, so the loop knows an export is open.
    pub(crate) held: Arc<()>,
    /// Set when view quits: the writer stops, removes its part file and
    /// publishes nothing.
    pub(crate) cancel: Arc<AtomicBool>,
    /// Signalled once the clip is published or has failed.
    pub(crate) done: SyncSender<()>,
}

/// Starts the thread. `remote` sessions show files of another host, which
/// the thread never hashes. A clip read is sent on `clips` ahead of its
/// reply.
///
/// # Errors
///
/// Returns the error the host refused the thread with.
pub(crate) fn start(
    msg: LoopSender,
    remote: bool,
    clips: Sender<FrameRing>,
) -> io::Result<BackgroundWriter<IoJob, Infallible>> {
    let mut io = Io {
        baselines: HashMap::new(),
        remote,
        msg,
        clips,
    };
    BackgroundWriter::start("dvr-io", QUEUE, move |job| {
        io.apply(job);
        Ok(())
    })
}

/// The thread's state: each baselined file's hash, `None` while it could
/// not be read.
struct Io {
    baselines: HashMap<String, Option<u64>>,
    remote: bool,
    msg: LoopSender,
    clips: Sender<FrameRing>,
}

impl Io {
    fn apply(&mut self, job: IoJob) {
        let reply = match job {
            IoJob::Baseline(paths) => {
                if !self.remote {
                    for path in paths {
                        let hash = hash_file(&path);
                        self.baselines.entry(path).or_insert(hash);
                    }
                }
                return;
            }
            IoJob::DiskCheck => self.disk_check(),
            IoJob::Export(export) => write(export),
            IoJob::Play { path, max_bytes } => match read_clip(&path, max_bytes) {
                Ok((ring, left_out)) => {
                    // a loop that has quit takes no frames and no reply
                    if self.clips.send(ring).is_err() {
                        return;
                    }
                    DvrIoReply::ClipLoaded {
                        path: path.display().to_string(),
                        left_out,
                    }
                }
                Err(reason) => DvrIoReply::Failed {
                    verb: "play",
                    reason,
                },
            },
        };
        let _ = self.msg.send(Msg::DvrIo(reply));
    }

    fn disk_check(&self) -> DvrIoReply {
        if self.remote {
            return DvrIoReply::DiskChecked {
                changed: Vec::new(),
                unverifiable: true,
            };
        }
        let mut changed: Vec<String> = self
            .baselines
            .iter()
            .filter(|(path, hash)| hash_file(path) != **hash)
            .map(|(path, _)| path.clone())
            .collect();
        changed.sort();
        DvrIoReply::DiskChecked {
            changed,
            unverifiable: false,
        }
    }
}

/// The frames of the clip at `path`, read into a ring holding a recording
/// bound of `max_bytes`, with how many of its oldest frames that bound left
/// out, or why they could not be read, naming the path.
fn read_clip(path: &Path, max_bytes: usize) -> Result<(FrameRing, usize), String> {
    let shown = path.display();
    let file = view_native::picker::preview::open_regular_file(path)
        .map_err(|e| format!("{shown}: {e}"))?;
    let clip = clip::read::decode(&mut io::BufReader::new(file), max_bytes)
        .map_err(|e| format!("{shown}: {e}"))?;
    crate::vlog::log_with("dvr", || {
        format!(
            "play {shown} dropped={} left_out={} inputs={} dropped_inputs={} marks={} dead={}",
            clip.dropped,
            clip.left_out,
            clip.inputs.len(),
            clip.dropped_inputs,
            clip.markers.len(),
            clip.dead.len()
        )
    });
    if clip.ring.newest().is_none() {
        return Err(format!("{shown}: the clip holds no frame"));
    }
    Ok((clip.ring, clip.left_out))
}

/// The FNV-1a hash of the file at `path`, read in pieces. `None` when it
/// cannot be read.
fn hash_file(path: &str) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut hash = FNV_OFFSET;
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Some(hash),
            Ok(n) => hash = fnv1a_extend(hash, buf.get(..n)?),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// The file a clip bound for `path` is written to before it is published
/// there: `.NAME.<pid>.part` beside it, so two processes writing one name
/// never share it.
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "clip".into(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.part", std::process::id()))
}

/// Writes the clip `export` describes to a file that must not exist yet.
/// The export's frames and hold are released before the reply is made.
pub(super) fn write(export: Export) -> DvrIoReply {
    write_paced(
        export,
        || {},
        || {},
        |part, path| std::fs::hard_link(part, path),
    )
}

/// The error a cancelled export stops with.
fn cancelled() -> io::Error {
    io::Error::other("view quit before the clip was written")
}

/// The part file's writer, which fails once the export is cancelled, so a
/// clip cancelled mid-encode stops at its next buffer flush.
struct Cancellable<'a> {
    file: File,
    cancel: &'a AtomicBool,
}

impl Write for Cancellable<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Publishes the synced `part` under `path` unless a file holds that name.
/// A hard link fails on an existing name in the same step that creates it.
/// Where the filesystem has no hard links, the part is renamed by
/// [`rename_noreplace`]. Any other link error is returned as it is.
fn publish(
    part: &Path,
    path: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    match link(part, path) {
        Ok(()) => {}
        Err(e) if lacks_hard_links(&e) => rename_noreplace(part, path)?,
        Err(e) => return Err(e),
    }
    // a power loss can otherwise take the new name back
    if let Some(dir) = path.parent() {
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Whether a failed hard link says the filesystem has none: Linux answers
/// EPERM on vfat and exFAT and EOPNOTSUPP elsewhere, and macOS answers
/// ENOTSUP on FAT and exFAT.
fn lacks_hard_links(e: &io::Error) -> bool {
    // Windows FAT answers ERROR_INVALID_FUNCTION and a share
    // ERROR_NOT_SUPPORTED, which std maps to no kind of its own
    let windows = cfg!(windows) && matches!(e.raw_os_error(), Some(1 | 50));
    // std gives macOS's ENOTSUP no kind; only its EOPNOTSUPP is Unsupported
    #[cfg(unix)]
    let notsup = e.raw_os_error() == Some(libc::ENOTSUP);
    #[cfg(not(unix))]
    let notsup = false;
    windows
        || notsup
        || matches!(
            e.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        )
}

/// Renames `part` to `path` in one step that fails on an existing name.
/// A filesystem refusing the flag gets [`rename_checked`].
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn rename_noreplace(part: &Path, path: &Path) -> io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    rename_or_check(part, path, |part, path| {
        renameat_with(CWD, part, CWD, path, RenameFlags::NOREPLACE)
    })
}

/// [`rename_noreplace`] through `rename`, falling to [`rename_checked`]
/// where the filesystem refuses the flag.
#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn rename_or_check(
    part: &Path,
    path: &Path,
    rename: impl FnOnce(&Path, &Path) -> rustix::io::Result<()>,
) -> io::Result<()> {
    use rustix::io::Errno;
    match rename(part, path) {
        // Linux answers EINVAL, macOS ENOTSUP and a kernel older than
        // renameat2 ENOSYS
        Err(Errno::INVAL | Errno::NOTSUP | Errno::NOSYS) => rename_checked(part, path),
        other => other.map_err(io::Error::from),
    }
}

/// Renames `part` to `path` in one step that fails on an existing name.
#[cfg(windows)]
fn rename_noreplace(part: &Path, path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let (from, to) = (wide(part), wide(path));
    // SAFETY: both pointers are NUL-terminated buffers that outlive the
    // call, which retains neither. Flags 0 leave out
    // MOVEFILE_REPLACE_EXISTING, so an existing name is refused.
    #[allow(unsafe_code)]
    let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames `part` to `path` unless the name is taken; this target has no
/// rename that refuses an existing name.
#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
fn rename_noreplace(part: &Path, path: &Path) -> io::Result<()> {
    rename_checked(part, path)
}

/// Renames `part` to `path` unless the name is taken, a symlink included.
/// A file created between the check and the rename is replaced: on unix
/// targets other than Linux and Apple's, and on a Linux or macOS
/// filesystem that refuses a rename with no replace.
#[cfg(not(windows))]
fn rename_checked(part: &Path, path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::rename(part, path)
}

/// [`write`], running `created` once the part file exists and before the
/// encode, and `synced` once the whole clip sits synced under its part
/// name, ahead of `link`, which publishes it.
fn write_paced(
    export: Export,
    created: impl FnOnce(),
    synced: impl FnOnce(),
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> DvrIoReply {
    let Export {
        path,
        frames,
        inputs,
        markers,
        dead,
        held,
        cancel,
        done,
    } = export;
    let shown = path.display().to_string();
    let part = part_path(&path);
    let stop = || {
        if cancel.load(Ordering::SeqCst) {
            Err(cancelled())
        } else {
            Ok(())
        }
    };
    let written = stop()
        .and_then(|()| path.try_exists())
        .and_then(|exists| {
            if exists {
                Err(io::Error::from(io::ErrorKind::AlreadyExists))
            } else {
                // a part under this pid is one a killed process left, and
                // the person is told its name
                File::create_new(&part).map_err(|e| match e.kind() {
                    io::ErrorKind::AlreadyExists => {
                        io::Error::other(format!("{} already exists", part.display()))
                    }
                    _ => e,
                })
            }
        })
        .and_then(|file| {
            let written = (|| -> io::Result<usize> {
                stop()?;
                created();
                let mut out = BufWriter::new(Cancellable {
                    file,
                    cancel: &cancel,
                });
                let cut = clip::encode(&mut out, &frames, &inputs, &markers, &dead)?;
                out.into_inner()
                    .map_err(io::IntoInnerError::into_error)?
                    .file
                    .sync_all()?;
                synced();
                stop()?;
                publish(&part, &path, link)?;
                Ok(cut)
            })();
            // only a part this export created is removed, and once the
            // clip is published the part is a second name for it
            let _ = std::fs::remove_file(&part);
            written
        });
    drop((frames, held));
    let _ = done.try_send(());
    match written {
        Ok(cut) => DvrIoReply::Exported { path: shown, cut },
        Err(e) => DvrIoReply::Failed {
            verb: "export",
            reason: if e.kind() == io::ErrorKind::AlreadyExists {
                format!("{shown} already exists, left as it is")
            } else {
                format!("{shown}: {e}")
            },
        },
    }
}

/// The names `dir` holds, sorted, leaving out the `._` sidecar macOS
/// writes beside every file on a volume with no extended attributes (FAT,
/// exFAT).
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) fn listed(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with("._"))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::mpsc;

    use view_test_support::ScratchDir;
    use view_tui::dvr::{FrameRing, RingBuilder};

    use super::*;

    fn io(remote: bool) -> (Io, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::sync_channel(8);
        let io = Io {
            baselines: HashMap::new(),
            remote,
            msg: LoopSender::new(tx),
            clips: mpsc::channel().0,
        };
        (io, rx)
    }

    fn reply(rx: &mpsc::Receiver<Msg>) -> DvrIoReply {
        match rx.try_recv().unwrap() {
            Msg::DvrIo(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn disk_check_names_a_changed_baseline() {
        let dir = ScratchDir::new("dvr-disk-check").unwrap();
        let path = |name: &str| dir.join(name).display().to_string();
        std::fs::write(path("kept.rs"), "fn a() {}").unwrap();
        std::fs::write(path("changed.rs"), "fn b() {}").unwrap();
        let (mut io, rx) = io(false);
        io.apply(IoJob::Baseline(vec![
            path("kept.rs"),
            path("changed.rs"),
            path("created.rs"),
        ]));
        std::fs::write(path("changed.rs"), "fn c() {}").unwrap();
        // a second sight of a file keeps its first baseline
        io.apply(IoJob::Baseline(vec![path("changed.rs")]));
        std::fs::write(path("created.rs"), "new").unwrap();
        io.apply(IoJob::DiskCheck);
        assert_eq!(
            reply(&rx),
            DvrIoReply::DiskChecked {
                changed: vec![path("changed.rs"), path("created.rs")],
                unverifiable: false,
            }
        );
    }

    #[test]
    fn a_remote_session_reports_the_disk_unverifiable() {
        let dir = ScratchDir::new("dvr-remote-disk").unwrap();
        let file = dir.join("a.rs").display().to_string();
        std::fs::write(&file, "one").unwrap();
        let (mut io, rx) = io(true);
        io.apply(IoJob::Baseline(vec![file.clone()]));
        assert!(io.baselines.is_empty(), "nothing on this host is read");
        std::fs::write(&file, "two").unwrap();
        io.apply(IoJob::DiskCheck);
        assert_eq!(
            reply(&rx),
            DvrIoReply::DiskChecked {
                changed: Vec::new(),
                unverifiable: true,
            }
        );
    }

    fn one_frame() -> FrameRing {
        let mut builder = RingBuilder::new(1 << 20);
        builder
            .push_key(0, (3, 1), None, std::iter::empty())
            .unwrap();
        builder.finish()
    }

    fn export(ring: &mut FrameRing, path: PathBuf, held: &Arc<()>) -> IoJob {
        IoJob::Export(Export {
            path,
            frames: ring.snapshot().unwrap(),
            inputs: Inputs::default(),
            markers: Vec::new(),
            dead: Vec::new(),
            held: Arc::clone(held),
            cancel: Arc::new(AtomicBool::new(false)),
            done: mpsc::sync_channel(1).0,
        })
    }

    fn job(ring: &mut FrameRing, path: &Path) -> Export {
        let IoJob::Export(job) = export(ring, path.to_path_buf(), &Arc::new(())) else {
            panic!("an export job")
        };
        job
    }

    /// A ring of one frame whose cell holds `symbol`, so two clips of
    /// different rings tell apart.
    fn frame_of(symbol: &str) -> FrameRing {
        let mut builder = RingBuilder::new(1 << 20);
        let cell = view_tui::dvr::CellView::new(0, 0, symbol, [0; 3], 0);
        builder.push_key(0, (3, 1), None, [cell]).unwrap();
        builder.finish()
    }

    fn clip_bytes(ring: &mut FrameRing) -> Vec<u8> {
        let mut out = Vec::new();
        let frames = ring.snapshot().unwrap();
        clip::encode(&mut out, &frames, &Inputs::default(), &[], &[]).unwrap();
        out
    }

    fn left_in(dir: &ScratchDir) -> Vec<String> {
        listed(dir.path())
    }

    fn hard_link(part: &Path, path: &Path) -> io::Result<()> {
        std::fs::hard_link(part, path)
    }

    /// [`write_paced`] with the production link and `synced` alone.
    fn paced(export: Export, synced: impl FnOnce()) -> DvrIoReply {
        write_paced(export, || {}, synced, hard_link)
    }

    fn refused(path: &Path) -> DvrIoReply {
        DvrIoReply::Failed {
            verb: "export",
            reason: format!("{} already exists, left as it is", path.display()),
        }
    }

    /// Two writers in one process share its pid, so the second is refused
    /// by the part name the first holds.
    #[test]
    fn two_writers_to_one_name_leave_one_whole_clip_and_one_refusal() {
        let dir = ScratchDir::new("dvr-export-two").unwrap();
        let path = dir.join("a.vdvr");
        let (mut first, mut second) = (frame_of("1"), frame_of("2"));
        let want = clip_bytes(&mut first);
        let mut inner = None;
        let outer = paced(job(&mut first, &path), || {
            inner = Some(write(job(&mut second, &path)));
        });
        assert!(matches!(outer, DvrIoReply::Exported { .. }), "{outer:?}");
        let Some(DvrIoReply::Failed { reason, .. }) = &inner else {
            panic!("{inner:?}")
        };
        let held = format!(".a.vdvr.{}.part already exists", std::process::id());
        assert!(reason.ends_with(&held), "{reason}");
        assert_eq!(std::fs::read(&path).unwrap(), want, "the first clip, whole");
        assert_eq!(left_in(&dir), ["a.vdvr"]);
    }

    /// Two processes write two part names, and the one that links second
    /// is refused at the link, the first clip left whole.
    #[test]
    fn a_clip_linked_first_by_another_process_is_left_whole() {
        let dir = ScratchDir::new("dvr-export-two-parts").unwrap();
        let path = dir.join("a.vdvr");
        let (mut ours, mut theirs) = (frame_of("1"), frame_of("2"));
        let want = clip_bytes(&mut theirs);
        let other = dir.join(".a.vdvr.0.part");
        std::fs::write(&other, &want).unwrap();
        let probe = dir.join("probe");
        match std::fs::hard_link(&other, &probe) {
            Err(e) if lacks_hard_links(&e) => {
                eprintln!("skipped: this volume refuses a hard link ({e})");
                return;
            }
            linked => linked.unwrap(),
        }
        std::fs::remove_file(&probe).unwrap();
        let reply = paced(job(&mut ours, &path), || {
            publish(&other, &path, hard_link).unwrap();
        });
        assert_eq!(reply, refused(&path));
        assert_eq!(std::fs::read(&path).unwrap(), want, "the other clip, whole");
        assert_eq!(left_in(&dir), [".a.vdvr.0.part", "a.vdvr"]);
    }

    /// A filesystem with no hard links publishes by a rename that still
    /// refuses a taken name.
    #[test]
    fn with_no_hard_links_a_clip_is_renamed_into_place_and_a_taken_name_kept() {
        let dir = ScratchDir::new("dvr-export-no-links").unwrap();
        let path = dir.join("a.vdvr");
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::Unsupported));
        let mut ring = frame_of("1");
        let want = clip_bytes(&mut ring);
        let reply = write_paced(job(&mut ring, &path), || {}, || {}, unsupported);
        assert!(matches!(reply, DvrIoReply::Exported { .. }), "{reply:?}");
        assert_eq!(std::fs::read(&path).unwrap(), want, "the clip, whole");
        assert_eq!(left_in(&dir), ["a.vdvr"]);

        let taken = dir.join("b.vdvr");
        let plant = || std::fs::write(&taken, b"planted").unwrap();
        let reply = write_paced(job(&mut ring, &taken), || {}, plant, unsupported);
        assert_eq!(reply, refused(&taken));
        assert_eq!(std::fs::read(&taken).unwrap(), b"planted");
        assert_eq!(left_in(&dir), ["a.vdvr", "b.vdvr"]);
    }

    /// A link that fails for any reason but a missing hard-link operation
    /// is the export's own error, and nothing is renamed into place.
    #[test]
    fn a_link_that_fails_on_a_full_disk_publishes_nothing() {
        let dir = ScratchDir::new("dvr-export-full").unwrap();
        let path = dir.join("a.vdvr");
        let full = |_: &Path, _: &Path| Err(io::Error::from(io::ErrorKind::StorageFull));
        let mut ring = one_frame();
        let reply = write_paced(job(&mut ring, &path), || {}, || {}, full);
        let want = io::Error::from(io::ErrorKind::StorageFull);
        assert_eq!(
            reply,
            DvrIoReply::Failed {
                verb: "export",
                reason: format!("{}: {want}", path.display()),
            }
        );
        assert!(left_in(&dir).is_empty(), "{:?}", left_in(&dir));
    }

    /// A cancel landing once the part exists stops the encode at its first
    /// write, so the clip is never synced.
    #[test]
    fn a_cancel_during_the_encode_stops_it_before_the_clip_is_synced() {
        let dir = ScratchDir::new("dvr-export-cancel-encode").unwrap();
        let path = dir.join("a.vdvr");
        let mut ring = one_frame();
        let job = job(&mut ring, &path);
        let cancel = Arc::clone(&job.cancel);
        let mut synced = false;
        let reply = write_paced(
            job,
            || cancel.store(true, Ordering::SeqCst),
            || synced = true,
            hard_link,
        );
        assert!(matches!(reply, DvrIoReply::Failed { .. }), "{reply:?}");
        assert!(!synced, "the encode ran past the cancel");
        assert!(left_in(&dir).is_empty(), "{:?}", left_in(&dir));
    }

    /// A link refused for want of hard links is told from every other
    /// failure by the raw code each host answers with.
    #[test]
    fn a_filesystem_without_hard_links_is_told_by_its_raw_error() {
        #[cfg(unix)]
        assert!(lacks_hard_links(&io::Error::from_raw_os_error(
            libc::ENOTSUP
        )));
        #[cfg(windows)]
        for code in [1, 50] {
            let e = io::Error::from_raw_os_error(code);
            assert!(lacks_hard_links(&e), "{code}");
        }
        let full = io::Error::from(io::ErrorKind::StorageFull);
        assert!(!lacks_hard_links(&full));
    }

    /// A filesystem refusing the no-replace flag, by any of the codes
    /// Linux, macOS and an old kernel answer with, gets the checked rename.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_refused_no_replace_flag_falls_to_the_checked_rename() {
        use rustix::io::Errno;
        let dir = ScratchDir::new("dvr-export-flag-refused").unwrap();
        for errno in [Errno::INVAL, Errno::NOTSUP, Errno::NOSYS] {
            let (part, path) = (dir.join("p"), dir.join("a.vdvr"));
            std::fs::write(&part, b"clip").unwrap();
            let renamed = rename_or_check(&part, &path, |_, _| Err(errno));
            assert!(renamed.is_ok(), "{errno:?}: {renamed:?}");
            assert_eq!(std::fs::read(&path).unwrap(), b"clip", "{errno:?}");
            std::fs::remove_file(&path).unwrap();
        }
        let part = dir.join("p");
        std::fs::write(&part, b"clip").unwrap();
        let taken = rename_or_check(&part, &dir.join("b"), |_, _| Err(Errno::EXIST));
        assert_eq!(taken.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(part.exists() && !dir.join("b").exists());
    }

    /// A dangling symlink holds its name: neither rename replaces it.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_at_the_name_is_left_as_it_is() {
        let dir = ScratchDir::new("dvr-export-dangling").unwrap();
        let (part, path) = (dir.join("p"), dir.join("b.vdvr"));
        std::fs::write(&part, b"clip").unwrap();
        std::os::unix::fs::symlink("nowhere", &path).unwrap();
        let noreplace = rename_noreplace(&part, &path).map_err(|e| e.kind());
        let checked = rename_checked(&part, &path).map_err(|e| e.kind());
        for (how, got) in [("noreplace", noreplace), ("checked", checked)] {
            assert_eq!(got, Err(io::ErrorKind::AlreadyExists), "{how}");
        }
        let link = std::fs::symlink_metadata(&path).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(std::fs::read(&part).unwrap(), b"clip");
    }

    #[test]
    fn a_file_created_during_the_encode_is_left_as_it_is() {
        let dir = ScratchDir::new("dvr-export-planted").unwrap();
        let path = dir.join("a.vdvr");
        let mut ring = one_frame();
        let reply = paced(job(&mut ring, &path), || {
            std::fs::write(&path, b"planted").unwrap();
        });
        assert_eq!(
            reply,
            DvrIoReply::Failed {
                verb: "export",
                reason: format!("{} already exists, left as it is", path.display()),
            }
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"planted");
        assert_eq!(left_in(&dir), ["a.vdvr"]);
    }

    #[test]
    fn a_cancelled_export_publishes_nothing_and_leaves_no_part() {
        let dir = ScratchDir::new("dvr-export-cancel").unwrap();
        let path = dir.join("a.vdvr");
        let mut ring = one_frame();
        let job = job(&mut ring, &path);
        let cancel = Arc::clone(&job.cancel);
        let reply = paced(job, || cancel.store(true, Ordering::SeqCst));
        assert!(matches!(reply, DvrIoReply::Failed { .. }), "{reply:?}");
        assert!(left_in(&dir).is_empty(), "{:?}", left_in(&dir));

        let cancel = AtomicBool::new(false);
        let mut out = Cancellable {
            file: File::create(dir.join("w")).unwrap(),
            cancel: &cancel,
        };
        out.write_all(b"one").unwrap();
        cancel.store(true, Ordering::SeqCst);
        assert!(out.write_all(b"two").is_err(), "a cancelled encode stops");
    }

    #[test]
    fn an_export_writes_its_clip_and_never_overwrites_a_file() {
        let dir = ScratchDir::new("dvr-export").unwrap();
        let path = dir.join("a.vdvr");
        let shown = path.display().to_string();
        let (mut io, rx) = io(false);
        let mut ring = one_frame();
        let held = Arc::new(());
        io.apply(export(&mut ring, path.clone(), &held));
        assert_eq!(Arc::strong_count(&held), 1, "released before the reply");
        assert_eq!(
            reply(&rx),
            DvrIoReply::Exported {
                path: shown.clone(),
                cut: 0,
            }
        );
        let written = std::fs::read(&path).unwrap();
        assert_eq!(&written[..10], b"VIEWDVR\0\x01\x00");
        io.apply(export(&mut ring, path.clone(), &held));
        assert_eq!(
            reply(&rx),
            DvrIoReply::Failed {
                verb: "export",
                reason: format!("{shown} already exists, left as it is"),
            }
        );
        assert_eq!(std::fs::read(&path).unwrap(), written);
        let missing = dir.join("no-such-dir").join("b.vdvr");
        io.apply(export(&mut ring, missing, &held));
        assert!(matches!(reply(&rx), DvrIoReply::Failed { .. }));
        assert_eq!(left_in(&dir).len(), 1);
    }

    #[test]
    fn a_clip_appears_under_its_name_only_once_it_is_whole() {
        let dir = ScratchDir::new("dvr-export-part").unwrap();
        let path = dir.join("a  b.vdvr");
        let part = part_path(&path);
        assert_eq!(
            part,
            dir.join(format!(".a  b.vdvr.{}.part", std::process::id()))
        );
        let mut ring = one_frame();
        let held = Arc::new(());
        let IoJob::Export(job) = export(&mut ring, path.clone(), &held) else {
            panic!("an export job")
        };
        let mut looked = false;
        let reply = paced(job, || {
            looked = true;
            assert!(!path.exists(), "the clip is named before it is renamed");
            let whole = std::fs::read(&part).unwrap();
            assert!(
                clip::read::decode(&mut whole.as_slice(), 1 << 20).is_ok(),
                "the part file holds the whole clip, end record included"
            );
        });
        assert!(looked);
        assert!(matches!(reply, DvrIoReply::Exported { .. }), "{reply:?}");
        assert!(path.exists() && !part.exists());
    }
}
