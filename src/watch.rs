//! The watcher of the runbook, which reloads it when it changes (spec 10).
use std::ffi::OsString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};

/// How long the file must stay as it is after a change before a reload:
/// an editor can write it in several steps.
pub const SETTLE: Duration = Duration::from_millis(300);

/// Watches the directory of `file`, an absolute path, and calls `changed`
/// once the file has stayed as it is for `SETTLE` after a change. The
/// watch is on the directory, because editors often replace a file with a
/// rename. Through a link, the watch is on the file that it names, which
/// a write through the link changes. The thread ends when `changed`
/// returns false.
///
/// `pending` merges the changes that the main thread has not taken yet:
/// the watcher sets it before it calls `changed`, and calls it again only
/// once the main thread has cleared it, before it reads the file. A file
/// that another writer changes without end must not queue reloads ahead
/// of the keys.
pub fn spawn(
    file: &Path,
    pending: Arc<AtomicBool>,
    mut changed: impl FnMut() -> bool + Send + 'static,
) -> std::io::Result<()> {
    let file = &std::fs::canonicalize(file)?;
    let dir = file.parent().unwrap_or(file);
    let name: OsString = file.file_name().unwrap_or_default().to_owned();
    let fd = inotify::init(CreateFlags::CLOEXEC | CreateFlags::NONBLOCK)?;
    // A write ends with CLOSE_WRITE; a rename onto the name is MOVED_TO.
    let flags = WatchFlags::CLOSE_WRITE | WatchFlags::MOVED_TO | WatchFlags::ONLYDIR;
    inotify::add_watch(&fd, dir, flags)?;
    std::thread::spawn(move || {
        let mut buffer = [MaybeUninit::uninit(); 4096];
        // When the file has settled, after a change.
        let mut settled: Option<Instant> = None;
        loop {
            let left = settled.map(|at| at.saturating_duration_since(Instant::now()));
            let timeout = left.map(|left| Timespec::try_from(left).unwrap_or_default());
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(0) => {
                    settled = None;
                    if !pending.swap(true, Ordering::AcqRel) && !changed() {
                        return;
                    }
                    continue;
                }
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            }
            let mut reader = inotify::Reader::new(&fd, &mut buffer);
            loop {
                match reader.next() {
                    Ok(event) => {
                        let ours = event.file_name().map(|n| n.to_bytes()) == Some(name.as_bytes());
                        // The kernel dropped events: one of them may be ours.
                        let lost = event.events().contains(ReadFlags::QUEUE_OVERFLOW);
                        if ours || lost {
                            settled = Some(Instant::now() + SETTLE);
                        }
                    }
                    Err(rustix::io::Errno::AGAIN) => break,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return,
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::RecvTimeoutError;

    /// A watch on `run.md` in a new directory, the channel of its
    /// changes, and the flag of a change that waits.
    fn watched() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        crossbeam_channel::Receiver<()>,
        Arc<AtomicBool>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("run.md");
        std::fs::write(&file, "a\n").unwrap();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let pending = Arc::new(AtomicBool::new(false));
        spawn(&file, pending.clone(), move || sender.send(()).is_ok()).unwrap();
        (tmp, file, receiver, pending)
    }

    const WAIT: Duration = Duration::from_secs(5);
    /// Long enough for a change that should not come: twice the settle.
    const NONE: Duration = Duration::from_millis(600);

    #[test]
    fn a_write_of_the_file_is_a_change_once_it_settles() {
        let (_tmp, file, changes, pending) = watched();
        let start = Instant::now();
        for text in ["b\n", "c\n", "d\n"] {
            std::fs::write(&file, text).unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(changes.recv_timeout(WAIT), Ok(()));
        assert!(start.elapsed() >= SETTLE + Duration::from_millis(100));
        // Three writes in a row are one change.
        pending.store(false, Ordering::Release);
        assert_eq!(changes.recv_timeout(NONE), Err(RecvTimeoutError::Timeout));
    }

    #[test]
    fn a_change_that_waits_for_the_main_thread_covers_the_next_ones() {
        let (_tmp, file, changes, pending) = watched();
        std::fs::write(&file, "b\n").unwrap();
        assert_eq!(changes.recv_timeout(WAIT), Ok(()));
        std::fs::write(&file, "c\n").unwrap();
        assert_eq!(changes.recv_timeout(NONE), Err(RecvTimeoutError::Timeout));
        // Once the main thread takes the change, the next one comes.
        pending.store(false, Ordering::Release);
        std::fs::write(&file, "d\n").unwrap();
        assert_eq!(changes.recv_timeout(WAIT), Ok(()));
    }

    #[test]
    fn a_rename_onto_the_file_is_a_change() {
        let (tmp, file, changes, _pending) = watched();
        let new = tmp.path().join(".run.md.swp");
        std::fs::write(&new, "b\n").unwrap();
        assert_eq!(changes.recv_timeout(NONE), Err(RecvTimeoutError::Timeout));
        std::fs::rename(&new, &file).unwrap();
        assert_eq!(changes.recv_timeout(WAIT), Ok(()));
    }

    #[test]
    fn a_write_through_a_link_is_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real.md");
        let link = tmp.path().join("run.md");
        std::fs::write(&target, "a\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (sender, changes) = crossbeam_channel::unbounded();
        let pending = Arc::new(AtomicBool::new(false));
        spawn(&link, pending, move || sender.send(()).is_ok()).unwrap();
        std::fs::write(&link, "b\n").unwrap();
        assert_eq!(changes.recv_timeout(WAIT), Ok(()));
    }

    #[test]
    fn other_files_of_the_directory_are_no_change() {
        let (tmp, _file, changes, _pending) = watched();
        std::fs::write(tmp.path().join("run.log"), "log\n").unwrap();
        std::fs::write(tmp.path().join("run.md.bak"), "a\n").unwrap();
        assert_eq!(changes.recv_timeout(NONE), Err(RecvTimeoutError::Timeout));
    }
}
