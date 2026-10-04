//! The DVR's file work, on a thread of its own: hashing the files the
//! session showed, and writing clips.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::File;
use std::io::{self, BufWriter, Read};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use view_core::hash::{fnv1a_extend, FNV_OFFSET};
use view_core::msg::Msg;
use view_core::native::dvr::{DvrIoReply, Marker};
use view_proc::writer::BackgroundWriter;
use view_tui::dvr::RingSnapshot;

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
}

/// Starts the thread. `remote` sessions show files of another host, which
/// the thread never reads.
///
/// # Errors
///
/// Returns the error the host refused the thread with.
pub(crate) fn start(
    msg: LoopSender,
    remote: bool,
) -> io::Result<BackgroundWriter<IoJob, Infallible>> {
    let mut io = Io {
        baselines: HashMap::new(),
        remote,
        msg,
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

/// The file a clip bound for `path` is written to before it is renamed
/// there: `.NAME.part` beside it.
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "clip".into(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.part"))
}

/// Writes the clip `export` describes to a file that must not exist yet.
/// The export's frames and hold are released before the reply is made.
fn write(export: Export) -> DvrIoReply {
    write_paced(export, || {})
}

/// [`write`], running `synced` once the whole clip sits synced under its
/// part name, ahead of the rename that publishes it.
fn write_paced(export: Export, synced: impl FnOnce()) -> DvrIoReply {
    let Export {
        path,
        frames,
        inputs,
        markers,
        dead,
        held,
    } = export;
    let shown = path.display().to_string();
    let part = part_path(&path);
    let written = (|| -> io::Result<usize> {
        if path.try_exists()? {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let mut out = BufWriter::new(File::create(&part)?);
        let cut = clip::encode(&mut out, &frames, &inputs, &markers, &dead)?;
        out.into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .sync_all()?;
        synced();
        std::fs::rename(&part, &path)?;
        Ok(cut)
    })();
    // nothing is left to remove once the rename has published the clip
    let _ = std::fs::remove_file(&part);
    drop((frames, held));
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
        })
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
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_clip_appears_under_its_name_only_once_it_is_whole() {
        let dir = ScratchDir::new("dvr-export-part").unwrap();
        let path = dir.join("a  b.vdvr");
        let part = part_path(&path);
        assert_eq!(part, dir.join(".a  b.vdvr.part"));
        let mut ring = one_frame();
        let held = Arc::new(());
        let IoJob::Export(job) = export(&mut ring, path.clone(), &held) else {
            panic!("an export job")
        };
        let mut looked = false;
        let reply = write_paced(job, || {
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
