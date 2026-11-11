//! Runs: the workspace, the session state, and the PTY of each run.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, PoisonError};

use crossbeam_channel::{Receiver, Sender};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{CWD, FileType, Mode, OFlags};
use rustix::process::{Pid, PidfdFlags, Signal, WaitId, WaitIdOptions};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

use crate::{io_message, term};

/// The first executable file called `name` in the directories of `path`,
/// as an absolute path: each run starts in its own directory, where a
/// relative path would name another file.
pub fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    let found = std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })?;
    Some(std::path::absolute(&found).unwrap_or(found))
}

/// The directory of the run files, which only the user can read (spec
/// 8.3): the scripts can hold secrets. Dropping it deletes it.
pub struct Workspace {
    dir: PathBuf,
    /// The directory, open and locked. systemd-tmpfiles never deletes a
    /// locked directory as old (tmpfiles.d(5)). While it is open, no other
    /// directory can get its inode.
    lock: File,
}

impl Workspace {
    /// Creates `stepbook-<pid>` in `parent`. It fails if the directory
    /// exists: in a shared temp dir, another user could have made it. It
    /// also fails if another user could replace it in `parent`.
    pub fn create(parent: &Path) -> std::io::Result<Workspace> {
        check_parent(parent)?;
        let dir = Workspace::dir_in(parent);
        DirBuilder::new().mode(0o700).create(&dir)?;
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let lock = rustix::fs::open(&dir, flags, Mode::empty())
            .inspect_err(|_| _ = std::fs::remove_dir(&dir))?;
        let lock = File::from(lock);
        // Without the lock, the check before each run still holds.
        let _ = lock.lock_shared();
        Ok(Workspace { dir, lock })
    }

    /// Reads the file `name` of the workspace, through the open directory:
    /// if the path names another directory now, the file still comes from
    /// the workspace. A link fails, and a FIFO, which a block can make in
    /// place of the file, gives no data instead of blocking the open.
    fn read(&self, name: &OsStr) -> std::io::Result<Vec<u8>> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = rustix::fs::openat(&self.lock, name, flags, Mode::empty())?;
        let mut bytes = Vec::new();
        File::from(file).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Fails if the workspace is no longer the directory that stepbook
    /// made. After a program deletes it, another user can make it again,
    /// with a wrapper of their own.
    fn check(&self) -> std::io::Result<()> {
        let ours = self.lock.metadata()?;
        let now = std::fs::symlink_metadata(&self.dir).ok();
        if now.is_some_and(|now| (now.dev(), now.ino()) == (ours.dev(), ours.ino())) {
            return Ok(());
        }
        let dir = self.dir.display();
        Err(std::io::Error::other(format!(
            "the workspace {dir} was deleted or replaced"
        )))
    }

    /// The path of the workspace in `parent`.
    pub fn dir_in(parent: &Path) -> PathBuf {
        parent.join(format!("stepbook-{}", std::process::id()))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes a new file that only the user can read.
    fn write(&self, name: &str, contents: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.dir.join(name);
        let mut file = (OpenOptions::new().write(true).create_new(true))
            .mode(0o600)
            .open(&path)?;
        file.write_all(contents)?;
        Ok(path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The directory that stepbook starts in: `$PWD`, as the shell gave it,
/// if it names the current directory, else the physical path. Through a
/// link, the runs then start in the path that the user typed.
pub fn start_dir(current: PathBuf, pwd: Option<OsString>) -> PathBuf {
    let same = |pwd: &PathBuf| {
        let (Ok(a), Ok(b)) = (std::fs::metadata(pwd), std::fs::metadata(&current)) else {
            return false;
        };
        pwd.is_absolute() && (a.dev(), a.ino()) == (b.dev(), b.ino())
    };
    pwd.map(PathBuf::from).filter(same).unwrap_or(current)
}

/// Fails if another user could replace the workspace in `parent`. Only
/// the owner of a directory and root can rename what it holds, and, if
/// the directory lacks the sticky bit, each user who can write to it.
fn check_parent(parent: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(parent)?;
    let parent = parent.display();
    if meta.uid() != 0 && meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::other(format!(
            "{parent} belongs to another user"
        )));
    }
    if meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0 {
        return Err(std::io::Error::other(format!(
            "other users can write to {parent}, which has no sticky bit"
        )));
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR` if it is an absolute path to a directory of the
/// user, else the temp dir, made absolute. After su, `$XDG_RUNTIME_DIR`
/// can still name the directory of the first user.
pub fn workspace_parent(runtime_dir: Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    let euid = rustix::process::geteuid().as_raw();
    let dir = runtime_dir.map(PathBuf::from).filter(|dir| {
        dir.is_absolute() && std::fs::metadata(dir).is_ok_and(|meta| meta.uid() == euid)
    });
    let dir = dir.unwrap_or(temp_dir);
    // The runs open their files from the session directory.
    std::path::absolute(&dir).unwrap_or(dir)
}

/// The exported variables and the working directory that a run starts
/// with (spec 9).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub dir: PathBuf,
    pub vars: BTreeMap<OsString, OsString>,
}

/// The variables that bash sets on its own, and those of stepbook.
fn is_excluded(name: &OsStr) -> bool {
    matches!(name.as_bytes(), b"_" | b"SHLVL" | b"PWD" | b"OLDPWD")
        || name.as_bytes().starts_with(b"STEPBOOK_")
}

/// Whether bash can hold a variable of this name. bash passes the others
/// from its environment to its children, but a script cannot change them,
/// and `compgen -e` does not list them.
fn is_identifier(name: &OsStr) -> bool {
    let name = name.as_bytes();
    name.first().is_some_and(|c| !c.is_ascii_digit())
        && name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

impl State {
    pub fn new(dir: PathBuf, vars: impl IntoIterator<Item = (OsString, OsString)>) -> State {
        let vars = vars.into_iter().filter(|(name, _)| !is_excluded(name));
        State {
            dir,
            vars: vars.collect(),
        }
    }

    /// Reads a state file (spec 9.2): entries that end with a NUL byte, the
    /// directory first, then each variable as `NAME=value`, then an empty
    /// entry. Without the empty entry, the file is partial: the wrapper was
    /// killed while it wrote it, and the missing variables would count as
    /// unset.
    pub fn parse(bytes: &[u8]) -> Option<State> {
        let mut entries = bytes.strip_suffix(b"\0\0")?.split(|&b| b == 0);
        let dir = PathBuf::from(OsStr::from_bytes(entries.next()?));
        let mut vars = Vec::new();
        for entry in entries {
            let equals = entry.iter().position(|&b| b == b'=')?;
            let (name, value) = (&entry[..equals], &entry[equals + 1..]);
            vars.push((
                OsStr::from_bytes(name).into(),
                OsStr::from_bytes(value).into(),
            ));
        }
        Some(State::new(dir, vars))
    }

    /// Applies what a run changed from `start` to `end` (spec 9.3). The
    /// session may have changed since `start`, by another run: only the
    /// changes of this run apply.
    pub fn apply(&mut self, start: &State, end: &State) {
        for (name, value) in &end.vars {
            if start.vars.get(name) != Some(value) {
                self.vars.insert(name.clone(), value.clone());
            }
        }
        for name in start.vars.keys() {
            if !end.vars.contains_key(name) && is_identifier(name) {
                self.vars.remove(name);
            }
        }
        // A block can unset PWD, or set it to a relative path.
        if end.dir != start.dir && end.dir.is_absolute() {
            self.dir = end.dir.clone();
        }
    }
}

const WRAPPER: &str = include_str!("wrapper.sh");

/// Where runs start, and the session state that they share (spec 8.2, 9).
pub struct Session {
    pub state: State,
    /// The directory that stepbook started in. The session goes back to it
    /// if its directory no longer exists.
    origin: PathBuf,
    bash: PathBuf,
    workspace: Workspace,
    wrapper: PathBuf,
    timer: PathBuf,
    /// The absolute paths of the runbook and of the log, for the runs
    /// (spec 8.2).
    file: PathBuf,
    log: Option<PathBuf>,
    next_id: u64,
}

/// Why a run did not start.
#[derive(Debug, PartialEq)]
pub enum StartError {
    /// The session directory no longer exists. The session went back to the
    /// directory that stepbook started in.
    Gone(PathBuf),
    Failed(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            StartError::Gone(dir) => {
                write!(
                    f,
                    "cannot start: directory {} no longer exists",
                    dir.display()
                )
            }
            StartError::Failed(reason) => write!(f, "cannot start: {reason}"),
        }
    }
}

/// A run in its own PTY. Its reader thread feeds its terminal, and sends
/// its output, then the exit status, so that the end of a run never comes
/// before its output.
pub struct Run {
    pub events: Receiver<Event>,
    /// The terminal that the reader thread feeds with the output.
    pub screen: Arc<term::Shared>,
    /// The process of the wrapper, the leader of the process group.
    pid: Pid,
    master: File,
    /// The state that the run started with.
    start: State,
    script: PathBuf,
    state_file: PathBuf,
    env_file: PathBuf,
}

pub enum Event {
    Output(
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "the log of milestone 4 writes the output")
        )]
        Vec<u8>,
    ),
    Exit(Status),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Code(i32),
    Signal(i32),
}

impl Status {
    /// The exit code, as a shell reports it: 128 + N for signal N.
    pub fn rc(self) -> i32 {
        match self {
            Status::Code(code) => code,
            Status::Signal(signal) => 128 + signal,
        }
    }
}

/// Output messages in flight per run. When they are all waiting, the
/// reader stops reading, and the program waits on its writes: a slow
/// view slows the program, and loses no output.
const OUTPUT_MESSAGES: usize = 64;
const READ_SIZE: usize = 32 * 1024;

/// A variable whose NAME=value takes at least this many bytes goes to the
/// wrapper in a file: in the environment of exec, one of over 128 KiB
/// (MAX_ARG_STRLEN) fails the start of every run.
const LARGE: usize = 4096;

/// The wrappers not reaped yet, which lead the sessions of the runs: those
/// that have not ended, and those whose background jobs go on. Children of
/// stepbook that came with an exec, such as a process that a launcher
/// started before "exec stepbook", are not runs.
static RUNS: Mutex<Vec<Pid>> = Mutex::new(Vec::new());

impl Session {
    pub fn new(
        workspace: Workspace,
        bash: PathBuf,
        state: State,
        file: PathBuf,
        log: Option<PathBuf>,
    ) -> std::io::Result<Session> {
        let wrapper = workspace.write("wrapper.sh", WRAPPER.as_bytes())?;
        // A FIFO with no writer, which the wrapper reads with a timeout, to
        // wait without a program on the PATH.
        let timer = workspace.dir().join("timer");
        rustix::fs::mknodat(CWD, &timer, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0)?;
        Ok(Session {
            origin: state.dir.clone(),
            state,
            bash,
            workspace,
            wrapper,
            timer,
            file,
            log,
            next_id: 1,
        })
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.dir()
    }

    /// Starts a run of the code of `block` in the session directory, in a
    /// new PTY of `columns` by `rows`. If the session directory no longer
    /// exists, the run does not start, and the session goes back to the
    /// directory that stepbook started in: never to `$HOME` (spec 8.2).
    pub fn start(
        &mut self,
        code: &str,
        block: &str,
        columns: u16,
        rows: u16,
    ) -> Result<Run, StartError> {
        if !self.state.dir.is_dir() {
            let dir = std::mem::replace(&mut self.state.dir, self.origin.clone());
            return Err(StartError::Gone(dir));
        }
        let fail = |e: std::io::Error| StartError::Failed(io_message(&e));
        self.workspace.check().map_err(fail)?;
        let id = self.next_id;
        self.next_id += 1;
        let script = (self.workspace)
            .write(&format!("run-{id}.sh"), code.as_bytes())
            .map_err(fail)?;
        // Made here, so that it has the mode of the workspace files: the
        // wrapper truncates it.
        let state_file = (self.workspace)
            .write(&format!("run-{id}.state"), b"")
            .map_err(fail)?;
        let mut start = self.state.clone();
        start.vars.insert("TERM".into(), "xterm-256color".into());
        let (large, small): (Vec<_>, Vec<_>) = (start.vars.iter())
            .partition(|(name, value)| name.len() + value.len() >= LARGE && is_identifier(name));
        let mut large_vars = Vec::new();
        for (name, value) in large {
            large_vars.extend([name.as_bytes(), b"=", value.as_bytes(), b"\0"].concat());
        }
        let env_file = (self.workspace)
            .write(&format!("run-{id}.env"), &large_vars)
            .inspect_err(|_| {
                [&script, &state_file]
                    .iter()
                    .for_each(|f| _ = std::fs::remove_file(f))
            })
            .map_err(fail)?;
        let run_files = [&script, &state_file, &env_file];
        let (master, child) = (self.spawn(&run_files, small, block, columns, rows))
            .inspect_err(|_| run_files.iter().for_each(|f| _ = std::fs::remove_file(f)))
            .map_err(fail)?;
        let reader = master.try_clone().map_err(fail)?;
        let (sender, events) = crossbeam_channel::bounded(OUTPUT_MESSAGES);
        let pid = Pid::from_child(&child);
        RUNS.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pid);
        let screen = Arc::new(term::Shared::new(columns, rows));
        let feed = screen.clone();
        std::thread::spawn(move || read_run(reader, child, sender, &feed));
        Ok(Run {
            events,
            screen,
            pid,
            master,
            start,
            script,
            state_file,
            env_file,
        })
    }

    /// Spawns the wrapper with the files of a run, in a new PTY.
    fn spawn(
        &self,
        run_files: &[&PathBuf; 3],
        vars: Vec<(&OsString, &OsString)>,
        block: &str,
        columns: u16,
        rows: u16,
    ) -> std::io::Result<(File, Child)> {
        let (master, slave) = open_pty(columns, rows)?;
        let mut command = Command::new(&self.bash);
        command
            .arg("--noprofile")
            .arg("--norc")
            .arg(&self.wrapper)
            .args(run_files)
            .arg(&self.timer)
            .env_clear()
            .envs(vars)
            // bash keeps a PWD that names its directory: through a link,
            // the run starts in the path that the last cd gave, as in a
            // terminal, and not in the physical path.
            .env("PWD", &self.state.dir)
            .env("STEPBOOK_FILE", &self.file)
            .env("STEPBOOK_LOG", self.log.as_deref().unwrap_or(Path::new("")))
            .env("STEPBOOK_BLOCK", block)
            .current_dir(&self.state.dir)
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave);
        // SAFETY: the closure runs in the child between fork and exec, where
        // only async-signal-safe calls are allowed. setsid and ioctl are
        // plain system calls, and the closure allocates nothing.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                // The PTY is on fd 0 now: it becomes the controlling
                // terminal of the new session, so that ctrl-c and the
                // window size reach the run.
                rustix::process::ioctl_tiocsctty(BorrowedFd::borrow_raw(0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        // Drops the parent's copies of the PTY's other side: while they are
        // open, the reader never sees the end of the output.
        drop(command);
        Ok((master, child))
    }

    /// Applies the state that a run saved, and deletes the files of the run.
    /// Returns whether the run saved its state: it does not after SIGKILL,
    /// or after exec (spec 9.2).
    pub fn finish(&mut self, run: &Run) -> bool {
        let name = run.state_file.file_name().unwrap_or_default();
        let end = self.workspace.read(name).ok();
        for file in [&run.script, &run.state_file, &run.env_file] {
            let _ = std::fs::remove_file(file);
        }
        match end.as_deref().and_then(State::parse) {
            Some(end) => {
                self.state.apply(&run.start, &end);
                true
            }
            None => false,
        }
    }
}

impl Run {
    /// Writes to the program, as keys would.
    #[cfg_attr(not(test), expect(dead_code, reason = "input mode, in milestone 3"))]
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        (&self.master).write_all(bytes)
    }

    /// Sends a signal to the processes of the run: those of the session of
    /// the wrapper. timeout and set -m put their children in process groups
    /// of their own, but in the same session. A process that has left the
    /// session, with setsid, does not get it: that is the way to keep a job
    /// alive after stepbook. The run may have ended already.
    pub fn signal(&self, signal: Signal) {
        for pid in session_processes(self.pid) {
            let _ = rustix::process::kill_process(pid, signal);
        }
    }

    /// Sets the size of the PTY. The kernel sends SIGWINCH to the program.
    pub fn resize(&self, columns: u16, rows: u16) -> std::io::Result<()> {
        rustix::termios::tcsetwinsize(&self.master, winsize(columns, rows))?;
        Ok(())
    }
}

fn winsize(columns: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A process, from /proc.
struct Process {
    pid: Pid,
    session: i32,
    /// Ended, but not yet reaped by its parent.
    zombie: bool,
}

/// The processes of the system.
fn processes() -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let process = |pid: i32| parse_stat(pid, &std::fs::read(format!("/proc/{pid}/stat")).ok()?);
    let pids = entries.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok());
    pids.filter_map(process).collect()
}

/// A process from its /proc/PID/stat.
fn parse_stat(pid: i32, stat: &[u8]) -> Option<Process> {
    // The name of the program, in parentheses, can hold any byte, and the
    // kernel cuts it at 15 bytes, inside a UTF-8 character too. The fields
    // after it are ASCII.
    let after = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    let mut fields = std::str::from_utf8(after).ok()?.split_whitespace();
    let state = fields.next()?;
    let session = fields.nth(2)?.parse().ok()?;
    // A process whose main thread has ended shows Z while its other threads
    // run: it has ended only with no thread left.
    let threads: u32 = fields.nth(13)?.parse().ok()?;
    Some(Process {
        pid: Pid::from_raw(pid)?,
        session,
        zombie: state == "Z" && threads <= 1,
    })
}

/// The processes of a session. Without /proc, only its leader.
fn session_processes(leader: Pid) -> Vec<Pid> {
    let all = processes();
    if all.is_empty() {
        return vec![leader];
    }
    let members = all.into_iter().filter(|p| p.session == leader.as_raw_pid());
    members.map(|p| p.pid).collect()
}

/// Ends the runs that have not ended, and the background jobs of those that
/// have, before stepbook ends and deletes the workspace: the sessions of the
/// wrappers in `RUNS`. A wrapper that outlived stepbook would write its
/// state where the workspace was, maybe in a directory that another user
/// made there.
pub fn end_runs() {
    // Held only around a push or a retain, which cannot panic: the lock
    // cannot block for long, nor be held by the thread of a panic.
    let runs: Vec<i32> = (RUNS.lock().unwrap_or_else(PoisonError::into_inner).iter())
        .map(|pid| pid.as_raw_pid())
        .collect();
    end_sessions(&runs);
}

/// Sends SIGHUP to the processes of the sessions `runs`, as the close of
/// their PTYs would, then SIGKILL after up to 0.2 s. The close itself
/// reaches only the wrapper and the process group in the foreground:
/// timeout and set -m give a child a process group of its own.
fn end_sessions(runs: &[i32]) {
    let members = || {
        let alive = (processes().into_iter()).filter(|p| !p.zombie && runs.contains(&p.session));
        alive.map(|p| p.pid).collect::<Vec<_>>()
    };
    let signal = |pids: Vec<Pid>, signal| {
        for pid in pids {
            let _ = rustix::process::kill_process(pid, signal);
        }
    };
    signal(members(), Signal::HUP);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    while !members().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    signal(members(), Signal::KILL);
}

/// Opens a PTY of `columns` by `rows`: its side for stepbook, and its
/// side for the run.
fn open_pty(columns: u16, rows: u16) -> std::io::Result<(File, OwnedFd)> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = rustix::pty::openpt(flags)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let flags = OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC;
    let slave = rustix::fs::open(name.as_c_str(), flags, Mode::empty())?;
    rustix::termios::tcsetwinsize(&master, winsize(columns, rows))?;
    Ok((File::from(master), slave))
}

/// Sends the output of a run, then its exit status. The run ends when the
/// wrapper exits: a background job, such as one under setsid, can keep the
/// PTY open long after its block, and the block would stay running, with
/// its state held back. After the exit, the reader takes only the output
/// that waits in the PTY. On Linux, a read fails with EIO once every
/// process has closed the PTY.
fn read_run(reader: File, mut child: Child, sender: Sender<Event>, screen: &term::Shared) {
    let exit = rustix::process::pidfd_open(Pid::from_child(&child), PidfdFlags::empty()).ok();
    let mut buffer = vec![0; READ_SIZE];
    // The reads after the exit: a job that writes without a pause must
    // not hold the end back.
    let mut left = None;
    while left != Some(0) {
        let now = Timespec::default();
        let mut fds = vec![PollFd::new(&reader, PollFlags::IN)];
        if let (None, Some(exit)) = (left, &exit) {
            fds.push(PollFd::new(exit, PollFlags::IN));
        }
        match rustix::event::poll(&mut fds, left.map(|_| &now)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }
        if fds.get(1).is_some_and(|fd| !fd.revents().is_empty()) {
            left = Some(16);
        }
        if fds[0].revents().is_empty() {
            continue;
        }
        left = left.map(|left: u32| left - 1);
        match (&reader).read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                screen.process(&buffer[..n]);
                if sender.send(Event::Output(buffer[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    // The PTY can end first: a script that moves its stdio off the PTY goes
    // on with no terminal. The run ends when the wrapper exits. NOWAIT
    // leaves the wrapper a zombie, for wait_for_jobs.
    let pid = Pid::from_child(&child);
    let status = loop {
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        match rustix::process::waitid(WaitId::Pid(pid), options) {
            Ok(Some(status)) => break status,
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => panic!("cannot wait for the wrapper of a run: {e}"),
        }
    };
    let status = match status.terminating_signal() {
        Some(signal) => Status::Signal(signal),
        None => Status::Code(status.exit_status().unwrap_or(0)),
    };
    let _ = sender.send(Event::Exit(status));
    wait_for_jobs(&reader, pid, &mut buffer);
    let _ = child.wait();
    RUNS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|&run| run != pid);
}

/// How often `wait_for_jobs` checks the session of a run.
const JOB_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits while the background jobs of a run go on: they outlive their
/// run, until stepbook ends (spec 8.2, 11). Meanwhile, it reads their
/// output and drops it: once its buffer is full, a write to a PTY that
/// nobody reads would block. The wrapper stays a zombie until then: its pid
/// names their session, and no other session can take it before
/// `end_runs`.
fn wait_for_jobs(reader: &File, wrapper: Pid, buffer: &mut [u8]) {
    let mut open = true;
    let jobs = || {
        let session = |p: &Process| p.session == wrapper.as_raw_pid() && !p.zombie;
        processes().iter().any(session)
    };
    while jobs() {
        let deadline = std::time::Instant::now() + JOB_CHECK;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            if !open {
                std::thread::sleep(left);
                break;
            }
            let timeout = Timespec::try_from(left).unwrap_or_default();
            let mut fds = [PollFd::new(reader, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(0) | Err(rustix::io::Errno::INTR) => {}
                // Once the jobs close the PTY, a read fails with EIO.
                Ok(_) => match (&*reader).read(buffer) {
                    Ok(0) => open = false,
                    Ok(_) => {}
                    Err(e) => open = e.kind() == std::io::ErrorKind::Interrupted,
                },
                Err(_) => open = false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::fs;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn only_the_user_can_read_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(tmp.path()).unwrap();
        assert_eq!(workspace.dir(), Workspace::dir_in(tmp.path()));
        assert_eq!(mode(workspace.dir()), 0o700);
    }

    #[test]
    fn dropping_the_workspace_deletes_it_with_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(tmp.path()).unwrap();
        fs::write(workspace.dir().join("run-1.sh"), "ls\n").unwrap();
        let dir = workspace.dir().to_owned();
        drop(workspace);
        assert!(!dir.exists());
    }

    #[test]
    fn the_workspace_must_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = Workspace::dir_in(tmp.path());
        std::os::unix::fs::symlink(tmp.path(), &dir).unwrap();
        assert!(Workspace::create(tmp.path()).is_err());
    }

    #[test]
    fn a_parent_that_others_can_write_to_needs_the_sticky_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let set_mode = |mode| fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode));
        set_mode(0o777).unwrap();
        let error = Workspace::create(tmp.path()).err().unwrap();
        let expected = format!(
            "other users can write to {}, which has no sticky bit",
            tmp.path().display()
        );
        assert_eq!(error.to_string(), expected);
        assert!(!Workspace::dir_in(tmp.path()).exists());
        set_mode(0o1777).unwrap();
        assert!(Workspace::create(tmp.path()).is_ok());
    }

    #[test]
    fn the_workspace_stays_locked_against_systemd_tmpfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = Workspace::create(tmp.path()).unwrap();
        // systemd-tmpfiles takes an exclusive lock before it deletes.
        let other = File::open(workspace.dir()).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
    }

    #[test]
    fn a_run_keeps_the_path_of_a_cd_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("real/proj")).unwrap();
        std::os::unix::fs::symlink("real/proj", tmp.path().join("lnk")).unwrap();
        let mut session = session(tmp.path());
        run(&mut session, "cd lnk\n");
        let lnk = tmp.path().join("lnk");
        assert_eq!(session.state.dir, lnk);
        // A run that does not cd changes no directory.
        assert_eq!(run(&mut session, "pwd\n").0, format!("{}\n", lnk.display()));
        assert_eq!(session.state.dir, lnk);
        let (output, _, _) = run(&mut session, "cd ..; pwd\n");
        assert_eq!(output, format!("{}\n", tmp.path().display()));
    }

    #[test]
    fn stepbook_starts_in_the_pwd_that_names_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", tmp.path().join("lnk")).unwrap();
        let real = tmp.path().join("real");
        let lnk = tmp.path().join("lnk");
        assert_eq!(start_dir(real.clone(), Some(lnk.clone().into())), lnk);
        for pwd in [
            tmp.path().into(),
            OsString::from("lnk"),
            OsString::from("/no/such"),
        ] {
            assert_eq!(start_dir(real.clone(), Some(pwd)), real);
        }
        assert_eq!(start_dir(real.clone(), None), real);
    }

    #[test]
    fn the_workspace_goes_in_the_runtime_dir_if_it_is_absolute_and_of_the_user() {
        let parent = |runtime: Option<&str>, temp: &str| {
            workspace_parent(runtime.map(OsString::from), temp.into())
        };
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().to_str().unwrap();
        assert_eq!(parent(Some(runtime), "/tmp"), tmp.path());
        // After su, the directory of another user.
        assert_eq!(parent(Some("/"), "/tmp"), Path::new("/tmp"));
        assert_eq!(parent(Some("/nowhere"), "/tmp"), Path::new("/tmp"));
        assert_eq!(parent(Some("run"), "/tmp"), Path::new("/tmp"));
        assert_eq!(parent(Some(""), "/tmp"), Path::new("/tmp"));
        assert_eq!(parent(None, "/tmp"), Path::new("/tmp"));
        // A run opens its files after its cd.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(parent(None, "tmp"), cwd.join("tmp"));
    }

    fn make_file(dir: &std::path::Path, mode: u32) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let file = dir.join("bash");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        file
    }

    #[test]
    fn finds_the_first_executable_file_on_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let not_exec = tmp.path().join("a");
        let folder = tmp.path().join("b");
        let exec = tmp.path().join("c");
        make_file(&not_exec, 0o644);
        fs::create_dir_all(folder.join("bash")).unwrap();
        let found = make_file(&exec, 0o755);
        make_file(&tmp.path().join("d"), 0o755);
        let path =
            std::env::join_paths([&not_exec, &folder, &exec, &tmp.path().join("d")]).unwrap();
        assert_eq!(find_in_path("bash", &path), Some(found));
    }

    #[test]
    fn a_relative_directory_of_the_path_gives_an_absolute_path() {
        let tmp = tempfile::tempdir_in(".").unwrap();
        let name = tmp.path().file_name().unwrap();
        make_file(tmp.path(), 0o755);
        let path = PathBuf::from(name);
        let expected = std::env::current_dir().unwrap().join(name).join("bash");
        assert_eq!(find_in_path("bash", path.as_os_str()), Some(expected));
    }

    #[test]
    fn finds_nothing_on_an_empty_path() {
        assert_eq!(find_in_path("bash", OsStr::new("")), None);
    }

    fn state(dir: &str, vars: &[(&str, &str)]) -> State {
        let vars = vars
            .iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)));
        State::new(dir.into(), vars)
    }

    /// The bytes that the wrapper writes for a state.
    fn file(state: &State) -> Vec<u8> {
        let mut bytes = state.dir.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        for (name, value) in &state.vars {
            bytes.extend([name.as_bytes(), b"=", value.as_bytes(), b"\0"].concat());
        }
        bytes.push(0);
        bytes
    }

    #[test]
    fn a_state_file_gives_the_directory_and_the_variables() {
        let bytes = b"/tmp/a\nb\0A=1\0B=x=y\0C=\0\0";
        let expected = state("/tmp/a\nb", &[("A", "1"), ("B", "x=y"), ("C", "")]);
        assert_eq!(State::parse(bytes), Some(expected));
        assert_eq!(State::parse(b"\0\0"), Some(state("", &[])));
    }

    #[test]
    fn a_partial_state_file_gives_no_state() {
        for bytes in [
            &b""[..],
            b"/tmp\0",
            b"/tmp\0A=1\0",
            b"/tmp\0A=1",
            b"/tmp\0A\0\0",
        ] {
            assert_eq!(State::parse(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn excluded_variables_never_enter_the_state() {
        let names = ["_", "SHLVL", "PWD", "OLDPWD", "STEPBOOK_FILE", "STEPBOOK_"];
        let vars: Vec<_> = names
            .iter()
            .map(|n| (*n, "1"))
            .chain([("KEPT", "1")])
            .collect();
        let kept = state("/", &[("KEPT", "1")]);
        assert_eq!(state("/", &vars), kept);
        let unfiltered = State {
            dir: "/".into(),
            vars: (vars.iter().map(|(n, v)| (n.into(), v.into()))).collect(),
        };
        assert_eq!(State::parse(&file(&unfiltered)), Some(kept));
    }

    #[test]
    fn a_run_applies_only_its_own_changes() {
        let start = state("/a", &[("SET", "1"), ("UNSET", "1"), ("KEPT", "1")]);
        // Another run changed KEPT and the directory since the start.
        let mut session = state("/b", &[("SET", "1"), ("UNSET", "1"), ("KEPT", "2")]);
        let end = state("/a", &[("SET", "2"), ("KEPT", "1"), ("NEW", "1")]);
        session.apply(&start, &end);
        assert_eq!(
            session,
            state("/b", &[("SET", "2"), ("KEPT", "2"), ("NEW", "1")])
        );
    }

    #[test]
    fn the_run_that_ends_last_wins() {
        let start = state("/a", &[("X", "0")]);
        let mut session = start.clone();
        session.apply(&start, &state("/b", &[("X", "1")]));
        session.apply(&start, &state("/c", &[("X", "2")]));
        assert_eq!(session, state("/c", &[("X", "2")]));
    }

    #[test]
    fn variables_that_bash_cannot_hold_are_never_unset() {
        let start = state("/a", &[("a-b", "1"), ("1A", "1"), ("", "1"), ("A_1", "1")]);
        let mut session = start.clone();
        session.apply(&start, &state("/a", &[]));
        assert_eq!(
            session,
            state("/a", &[("a-b", "1"), ("1A", "1"), ("", "1")])
        );
    }

    #[test]
    fn an_empty_or_relative_directory_changes_nothing() {
        let start = state("/a", &[]);
        let mut session = start.clone();
        session.apply(&start, &state("", &[]));
        session.apply(&start, &state("b", &[]));
        assert_eq!(session.dir, Path::new("/a"));
    }

    fn name() -> impl Strategy<Value = OsString> {
        "[A-Z_][A-Z0-9_]{0,3}".prop_filter_map("excluded", |n| {
            let n = OsString::from(n);
            (!is_excluded(&n)).then_some(n)
        })
    }

    fn value() -> impl Strategy<Value = OsString> {
        let bytes = prop::collection::vec(1..=255u8, 0..12);
        bytes.prop_map(|b| OsStr::from_bytes(&b).to_owned())
    }

    fn any_state() -> impl Strategy<Value = State> {
        let dir = prop_oneof![Just("/a"), Just("/a\nb=c"), Just("/\u{e9}")];
        let vars = prop::collection::btree_map(name(), value(), 0..6);
        (dir, vars).prop_map(|(dir, vars)| State {
            dir: dir.into(),
            vars,
        })
    }

    proptest! {
        #[test]
        fn any_state_survives_the_state_file(state in any_state()) {
            prop_assert_eq!(State::parse(&file(&state)), Some(state));
        }

        #[test]
        fn applying_a_run_to_its_start_gives_its_end(start in any_state(), end in any_state()) {
            let mut session = start.clone();
            session.apply(&start, &end);
            prop_assert_eq!(session, end);
        }
    }

    /// A session in a temporary directory, with only PATH and X=1.
    fn session(tmp: &Path) -> Session {
        let workspace = Workspace::create(tmp).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        let bash = find_in_path("bash", &path).unwrap();
        let vars = [("PATH".into(), path), ("X".into(), "1".into())];
        let state = State::new(tmp.into(), vars);
        let (file, log) = ("/r/run.md".into(), Some("/r/run.log".into()));
        Session::new(workspace, bash, state, file, log).unwrap()
    }

    /// A run in a test takes less: a bug that loses an event fails the
    /// test, and does not hang it.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// The output of a run without its carriage returns, and its status.
    fn wait(run: &Run) -> (String, Status) {
        let mut output = Vec::new();
        loop {
            match run.events.recv_timeout(WAIT).unwrap() {
                Event::Output(bytes) => output.extend(bytes),
                Event::Exit(status) => {
                    let output = String::from_utf8_lossy(&output).replace('\r', "");
                    return (output, status);
                }
            }
        }
    }

    /// Runs `code` to its end, and applies its state.
    fn run(session: &mut Session, code: &str) -> (String, Status, bool) {
        let run = session.start(code, "check", 80, 24).unwrap();
        let (output, status) = wait(&run);
        (output, status, session.finish(&run))
    }

    fn var(session: &Session, name: &str) -> Option<String> {
        let value = session.state.vars.get(OsStr::new(name));
        value.map(|v| v.to_string_lossy().into_owned())
    }

    #[test]
    fn export_unset_and_cd_carry_over_to_the_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "export A=1; unset X; mkdir sub; cd sub\n";
        assert_eq!(
            run(&mut session, code),
            (String::new(), Status::Code(0), true)
        );
        let code = "echo \"$A ${X-unset} $PWD\"\n";
        let sub = tmp.path().join("sub");
        let (output, _, _) = run(&mut session, code);
        assert_eq!(output, format!("1 unset {}\n", sub.display()));
        assert_eq!(session.state.dir, sub);
    }

    #[test]
    fn a_run_gets_the_session_and_the_variables_of_stepbook() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "compgen -e | tr '\\n' ' '; echo; echo \"$TERM|$STEPBOOK_FILE|$STEPBOOK_LOG|$STEPBOOK_BLOCK|$X\"; pwd\n";
        let (output, _, _) = run(&mut session, code);
        let expected = format!(
            "PATH PWD SHLVL STEPBOOK_BLOCK STEPBOOK_FILE STEPBOOK_LOG TERM X \nxterm-256color|/r/run.md|/r/run.log|check|1\n{}\n",
            tmp.path().display()
        );
        assert_eq!(output, expected);
        // TERM is not a change of the run.
        assert_eq!(var(&session, "TERM"), None);
    }

    #[test]
    fn exit_saves_the_state_and_gives_the_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let (_, status, saved) = run(&mut session, "export A=1\nexit 3\nexport A=2\n");
        assert_eq!((status.rc(), saved), (3, true));
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
    }

    #[test]
    fn a_failure_under_set_e_saves_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let (_, status, saved) = run(&mut session, "set -e\nexport A=1\nfalse\nexport A=2\n");
        assert_eq!((status, saved), (Status::Code(1), true));
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
    }

    #[test]
    fn the_exit_trap_of_a_script_runs_and_the_state_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "trap 'echo \"trap $?\"; export B=2' EXIT\nexport A=1\nexit 4\n";
        let (output, status, saved) = run(&mut session, code);
        assert_eq!(
            (output.as_str(), status, saved),
            ("trap 4\n", Status::Code(4), true)
        );
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
        assert_eq!(var(&session, "B").as_deref(), Some("2"));
    }

    #[test]
    fn exit_without_a_status_in_the_handler_keeps_the_status_of_the_script() {
        // As in bash: the status from before the trap, but in a "( )"
        // subshell the status of its last command.
        let codes = [
            (
                "set -e\ntrap 'echo cleanup; exit' EXIT\nfalse\n",
                "cleanup\n",
                1,
            ),
            ("trap 'echo cleanup; exit' EXIT\nexit 3\n", "cleanup\n", 3),
            ("trap '(true; exit) || echo bad' EXIT\nexit 4\n", "", 4),
            (
                "trap 'x=$(false; exit); echo \"comsub $?\"' EXIT\nexit 4\n",
                "comsub 4\n",
                4,
            ),
        ];
        for (code, expected, rc) in codes {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let expected = (expected.into(), Status::Code(rc), true);
            assert_eq!(run(&mut session, code), expected, "{code}");
        }
    }

    #[test]
    fn the_locals_of_a_function_stay_out_of_the_state() {
        // bash runs the EXIT trap in the function that called exit, or that
        // a signal stopped.
        let codes = [
            "f() { local X=local PATH=/nowhere; local -x ONLY=1; export A=1; exit 1; }\nf\n",
            "f() { local X=local; g; }\ng() { local +x PATH; export A=1; kill -INT $$; }\nf\n",
            "shopt -s localvar_unset\nf() { local X=local; g; }\ng() { local PATH; export A=1; exit 2; }\nf\n",
        ];
        for code in codes {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let path = var(&session, "PATH");
            assert!(run(&mut session, code).2, "{code}");
            assert_eq!(var(&session, "A").as_deref(), Some("1"), "{code}");
            assert_eq!(var(&session, "X").as_deref(), Some("1"), "{code}");
            assert_eq!(var(&session, "PATH"), path, "{code}");
            assert_eq!(var(&session, "ONLY"), None, "{code}");
        }
    }

    #[test]
    fn a_debug_trap_that_functions_inherit_never_stops_the_wrapper() {
        for option in ["set -T", "shopt -s extdebug"] {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let code = format!(
                "{option}\ntrap 'echo \"+ $BASH_COMMAND\"' DEBUG\n\
                trap 'echo cleanup' EXIT\nexport A=1\n"
            );
            let (output, status, saved) = run(&mut session, &code);
            assert_eq!((status, saved), (Status::Code(0), true), "{option}");
            assert!(output.ends_with("cleanup\n"), "{option}: {output}");
            let names = session.state.vars.keys().map(|n| n.to_string_lossy());
            assert_eq!(names.collect::<Vec<_>>(), ["A", "PATH", "X"], "{option}");
        }
    }

    #[test]
    fn a_run_gets_no_file_descriptor_of_stepbook() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let runs: Vec<_> = (0..2)
            .map(|_| {
                let run = session
                    .start("echo ready; read\n", "check", 80, 24)
                    .unwrap();
                wait_for(&run, "ready");
                run
            })
            .collect();
        // The second run started while the PTY of the first was open. bash
        // keeps the wrapper on fd 255.
        let fds = fs::read_dir(format!("/proc/{}/fd", runs[1].pid.as_raw_pid())).unwrap();
        let mut fds: Vec<_> = fds.map(|fd| fd.unwrap().file_name()).collect();
        fds.sort();
        assert_eq!(fds, ["0", "1", "2", "255"]);
        for run in &runs {
            run.write(b"\n").unwrap();
            wait(run);
        }
    }

    #[test]
    fn posix_mode_never_stops_the_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let (_, _, saved) = run(&mut session, "export POSIXLY_CORRECT=1\n");
        assert!(saved);
        // The block runs in POSIX mode, with the variable as it was.
        let code = "shopt -oq posix && echo posix\nprintenv POSIXLY_CORRECT\n";
        assert_eq!(
            run(&mut session, code),
            ("posix\n1\n".into(), Status::Code(0), true)
        );
        assert_eq!(var(&session, "POSIXLY_CORRECT").as_deref(), Some("1"));
        assert!(run(&mut session, "unset POSIXLY_CORRECT\n").2);
        assert_eq!(
            run(&mut session, "shopt -oq posix || echo bash\n").0,
            "bash\n"
        );
    }

    #[test]
    fn set_x_never_shows_the_wrapper_or_the_variables() {
        // In the script, or turned on again by its handler.
        let codes = [
            "set -x\ntrap 'echo bye' EXIT\ntrue\n",
            "cleanup() { set -x; echo bye; }\ntrap cleanup EXIT\n",
            "trap 'set -x; echo bye; exit 0' EXIT\n",
        ];
        for code in codes {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let code = format!("export SECRET=s3cret\n{code}");
            let (output, _, saved) = run(&mut session, &code);
            assert!(saved, "{code}");
            assert!(
                !output.contains("s3cret") && !output.contains("__stepbook"),
                "{code}: {output}"
            );
            assert!(output.contains("bye\n"), "{code}: {output}");
        }
    }

    #[test]
    fn set_a_keeps_the_variables_of_the_wrapper_out_of_the_state() {
        // bash sets BASH_TRAPSIG in a trap, and exports it under set -a.
        let codes: [(&str, &[&str]); 3] = [
            ("set -a; trap 'exit 4' EXIT", &["PATH", "X"]),
            ("set -a; set -e; false", &["PATH", "X"]),
            ("set -a\ntrap 'Y=2' EXIT\nX=3\n", &["PATH", "X", "Y"]),
        ];
        for (code, expected) in codes {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            assert!(run(&mut session, code).2, "{code}");
            let names = session.state.vars.keys().map(|n| n.to_string_lossy());
            assert_eq!(names.collect::<Vec<_>>(), expected, "{code}");
        }
    }

    #[test]
    fn a_handler_can_chain_the_handler_that_trap_shows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "trap 'echo a' EXIT\n\
            trap \"echo b; $(trap -p EXIT | sed \"s/^trap -- '//; s/' EXIT$//\")\" EXIT\n";
        let (output, _, saved) = run(&mut session, code);
        assert_eq!((output.as_str(), saved), ("b\na\n", true));
    }

    #[test]
    fn trap_keeps_its_other_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "trap 'echo a' EXIT; trap - EXIT\ntrap 'echo b' 0; trap EXIT\n\
            trap 'echo int' INT; trap -p INT\n( trap 'echo sub' EXIT; true )\n\
            echo \"$(trap 'echo cmd' EXIT)\"\ntrap -- 'echo c' exit\n\
            trap -p EXIT; trap\n";
        let (output, status, saved) = run(&mut session, code);
        let expected = "trap -- 'echo int' SIGINT\nsub\ncmd\n\
            trap -- 'echo c' EXIT\ntrap -- 'echo c' EXIT\ntrap -- 'echo int' SIGINT\nc\n";
        assert_eq!(
            (output.as_str(), status, saved),
            (expected, Status::Code(0), true)
        );
    }

    #[test]
    fn a_subshell_can_set_the_handler_of_the_script() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "trap 'echo p' EXIT\n( trap 'echo p' EXIT; echo sub )\n\
            echo \"[$(trap 'echo p' EXIT; echo cmd)]\"\n( trap -p EXIT )\n";
        let (output, status, saved) = run(&mut session, code);
        let expected = "sub\np\n[cmd\np]\ntrap -- 'echo p' EXIT\np\n";
        assert_eq!(
            (output.as_str(), status, saved),
            (expected, Status::Code(0), true)
        );
    }

    #[test]
    fn the_wrapper_writes_the_state_only_into_the_file_of_stepbook() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // As when the workspace is gone, and another user made it again.
        let run = session
            .start("rm \"$__stepbook_state\"\nexport A=1\n", "check", 80, 24)
            .unwrap();
        wait(&run);
        assert!(!run.state_file.exists());
        assert!(!session.finish(&run));
        // Or a link in its place.
        let code = "ln -sf \"$PWD/other\" \"$__stepbook_state\"\nexport A=1\n";
        let run = session.start(code, "check", 80, 24).unwrap();
        fs::write(tmp.path().join("other"), "kept").unwrap();
        wait(&run);
        assert_eq!(
            fs::read_to_string(tmp.path().join("other")).unwrap(),
            "kept"
        );
        assert!(!session.finish(&run));
        // Or a link in place of the workspace, to a directory with a file of
        // that name. The drain makes no timer there either.
        let code = "ws=${__stepbook_state%/*}; mv \"$ws\" moved; mkdir dir\n\
            : >\"dir/${__stepbook_state##*/}\"; ln -s \"$PWD/dir\" \"$ws\"\nexport A=1\n";
        let run = session.start(code, "check", 80, 24).unwrap();
        wait(&run);
        let dir = tmp.path().join("dir");
        assert_eq!(fs::read_to_string(dir.join("run-3.state")).unwrap(), "");
        assert!(!dir.join("timer").exists());
        assert!(!session.finish(&run));
    }

    #[test]
    fn stepbook_reads_the_state_only_from_its_workspace() {
        let codes = [
            // As when another user made the workspace again during the run.
            "ws=${__stepbook_state%/*}; mv \"$ws\" moved; mkdir \"$ws\"\n\
            : >\"$__stepbook_state\"\nexport A=1\n",
            // A FIFO in place of the state file would block the read.
            "rm \"$__stepbook_state\"; mkfifo \"$__stepbook_state\"\nexport A=1\n",
        ];
        for code in codes {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let run = session.start(code, "check", 80, 24).unwrap();
            wait(&run);
            assert!(!session.finish(&run), "{code}");
            assert_eq!(var(&session, "A"), None, "{code}");
        }
    }

    #[test]
    fn a_block_can_unset_pwd_under_set_u() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let expected = (String::new(), Status::Code(0), true);
        assert_eq!(run(&mut session, "set -u\nunset PWD\n"), expected);
        assert_eq!(session.state.dir, tmp.path());
    }

    #[test]
    fn the_output_of_a_process_substitution_ends_before_the_run() {
        // The logging header of the old runbooks (spec 12.3): when the
        // wrapper exits, the kernel sends SIGHUP to its tee. Under set -f
        // too, and with a handler that ends with exit.
        let handlers = [
            "trap 'echo \"=== END b rc=$?\"' EXIT",
            "set -f\ntrap 'echo \"=== END b rc=$?\"' EXIT",
            "trap 'rc=$?; echo \"=== END b rc=$rc\"; exit $rc' EXIT",
        ];
        for handler in handlers {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let code = format!("exec > >(tee -a \"$PWD/log\") 2>&1\n{handler}\nseq 1 20000\n");
            let started = std::time::Instant::now();
            let (output, status, saved) = run(&mut session, &code);
            assert_eq!((status, saved), (Status::Code(0), true), "{handler}");
            let end = &output[output.len().saturating_sub(50)..];
            assert!(
                output.ends_with("\n20000\n=== END b rc=0\n"),
                "{handler}: {end}"
            );
            let log = fs::read_to_string(tmp.path().join("log")).unwrap();
            assert_eq!(log.lines().count(), 20001, "{handler}");
            // The tee read the end of its input: the wrapper did not wait
            // its whole second.
            assert!(
                started.elapsed() < std::time::Duration::from_millis(900),
                "{handler}"
            );
        }
    }

    #[test]
    fn the_end_of_a_run_fires_no_trap_and_needs_no_option_of_the_script() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // An ERR trap under set -E, set -f and POSIX mode act on the lines
        // of the wrapper after the script; a background job makes the
        // wrapper wait.
        let code = "set -Eeuo pipefail\ntrap 'echo err >> \"$PWD/errs\"' ERR\n\
            set -f\nsleep 0.3 &\necho deployed\n";
        assert_eq!(
            run(&mut session, code),
            ("deployed\n".into(), Status::Code(0), true)
        );
        assert!(!tmp.path().join("errs").exists());
        // Without set -E, bash puts the ERR trap back when the functions of
        // the wrapper return.
        let code = "set -e\ntrap 'echo err >> \"$PWD/errs\"; exit 1' ERR\necho deployed\n";
        assert_eq!(
            run(&mut session, code),
            ("deployed\n".into(), Status::Code(0), true)
        );
        assert!(!tmp.path().join("errs").exists());
        let code = "set -o posix\nset -f\ntrue\n";
        assert_eq!(
            run(&mut session, code),
            (String::new(), Status::Code(0), true)
        );
    }

    #[test]
    fn a_variable_too_large_for_exec_goes_to_the_next_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "export BIG=$(printf '%200000s' x)\n";
        assert!(run(&mut session, code).2);
        // As in a terminal, a program fails to start with it, not bash.
        let code = "echo ${#BIG}\nunset BIG\n";
        assert_eq!(
            run(&mut session, code),
            ("200000\n".into(), Status::Code(0), true)
        );
        assert_eq!(var(&session, "BIG"), None);
        assert_eq!(run(&mut session, "printenv X\n").0, "1\n");
    }

    #[test]
    fn an_exported_ifs_keeps_its_value() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        assert!(run(&mut session, "export IFS=,\n").2);
        assert_eq!(var(&session, "IFS").as_deref(), Some(","));
    }

    #[test]
    fn a_bash_without_compgen_saves_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // As a bash built without programmable completion.
        fs::write(tmp.path().join("env.sh"), "enable -n compgen\n").unwrap();
        let env = tmp.path().join("env.sh").into_os_string();
        session.state.vars.insert("BASH_ENV".into(), env);
        let (_, _, saved) = run(&mut session, "unset X\n");
        assert!(!saved);
        assert_eq!(var(&session, "X").as_deref(), Some("1"));
    }

    #[test]
    fn a_variable_named_as_an_array_with_another_case_stays_under_nocasematch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        session.state.vars.insert("HOSTS".into(), "a b".into());
        assert!(run(&mut session, "shopt -s nocasematch\nhosts=(a b)\n").2);
        assert_eq!(var(&session, "HOSTS").as_deref(), Some("a b"));
    }

    #[test]
    fn exported_arrays_stay_out_of_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "export ARR=(a b) B=1\ndeclare -Ax M=([k]=v)\n";
        assert!(run(&mut session, code).2);
        let names = session.state.vars.keys().map(|n| n.to_string_lossy());
        assert_eq!(names.collect::<Vec<_>>(), ["B", "PATH", "X"]);
    }

    #[test]
    fn values_with_newlines_quotes_and_escapes_survive_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "export A=$'a\\nb' B=\"it's \\\"q\\\"\" C=$'\\e[31m\\\\' D=\nmkdir $'d\\nE=1'; cd $'d\\nE=1'\n";
        let (_, _, saved) = run(&mut session, code);
        assert!(saved);
        assert_eq!(var(&session, "A").as_deref(), Some("a\nb"));
        assert_eq!(var(&session, "B").as_deref(), Some("it's \"q\""));
        assert_eq!(var(&session, "C").as_deref(), Some("\u{1b}[31m\\"));
        assert_eq!(var(&session, "D").as_deref(), Some(""));
        assert_eq!(var(&session, "E"), None);
        assert_eq!(session.state.dir, tmp.path().join("d\nE=1"));
        let code = "[ \"$A\" = $'a\\nb' ] && [ \"$C\" = $'\\e[31m\\\\' ] && echo same\n";
        assert_eq!(run(&mut session, code).0, "same\n");
    }

    #[test]
    fn a_replaced_workspace_fails_the_run() {
        // As when a program deleted it, and another user made it again,
        // with a wrapper of their own.
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let workspace = session.workspace().to_owned();
        fs::remove_dir_all(&workspace).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(workspace.join("wrapper.sh"), "touch replaced\n").unwrap();
        let error = session.start("true\n", "check", 80, 24).err();
        let reason = format!(
            "the workspace {} was deleted or replaced",
            workspace.display()
        );
        assert_eq!(error, Some(StartError::Failed(reason)));
        assert!(!tmp.path().join("replaced").exists());
        // No run file went into the other directory.
        assert_eq!(fs::read_dir(&workspace).unwrap().count(), 1);
    }

    #[test]
    fn a_removed_directory_fails_the_run_and_resets_the_session_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        run(&mut session, "mkdir gone; cd gone\n");
        fs::remove_dir(tmp.path().join("gone")).unwrap();
        let error = session.start("pwd\n", "check", 80, 24).err();
        assert_eq!(error, Some(StartError::Gone(tmp.path().join("gone"))));
        assert_eq!(session.state.dir, tmp.path());
        assert_eq!(
            run(&mut session, "pwd\n").0,
            format!("{}\n", tmp.path().display())
        );
    }

    /// Waits for `text` in the output of a run, and returns the output.
    fn wait_for(run: &Run, text: &str) -> String {
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(text) {
            match run.events.recv_timeout(WAIT).unwrap() {
                Event::Output(bytes) => output.extend(bytes),
                Event::Exit(_) => panic!("the run ended before {text:?}"),
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    #[test]
    fn ctrl_c_ends_the_run_with_rc_130_and_saves_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let run = session
            .start("export A=1\necho ready\nsleep 5\n", "check", 80, 24)
            .unwrap();
        wait_for(&run, "ready");
        run.write(b"\x03").unwrap();
        let (_, status) = wait(&run);
        assert_eq!((status, status.rc()), (Status::Signal(2), 130));
        assert!(session.finish(&run));
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
    }

    #[test]
    fn a_run_that_replaces_its_shell_saves_no_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let (_, status, saved) = run(&mut session, "export A=1\nexec true\n");
        assert_eq!((status, saved), (Status::Code(0), false));
        assert_eq!(var(&session, "A"), None);
    }

    #[test]
    fn two_runs_at_the_same_time_keep_each_other_s_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let first = session
            .start("export A=1; echo ready; read\n", "check", 80, 24)
            .unwrap();
        wait_for(&first, "ready");
        let (_, _, saved) = run(&mut session, "export B=2; unset X\n");
        assert!(saved);
        first.write(b"\n").unwrap();
        wait(&first);
        assert!(session.finish(&first));
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
        assert_eq!(var(&session, "B").as_deref(), Some("2"));
        assert_eq!(var(&session, "X"), None);
    }

    #[test]
    fn a_run_ends_when_its_shell_exits_though_a_job_keeps_the_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let started = std::time::Instant::now();
        let code = "export A=1\nsetsid sleep 3 &\necho started\n";
        assert_eq!(
            run(&mut session, code),
            ("started\n".into(), Status::Code(0), true)
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
    }

    #[test]
    fn a_signal_reaches_the_processes_that_timeout_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // timeout puts itself and its child in a process group of its own.
        let run = session
            .start("echo ready; timeout 30 sleep 30\n", "check", 80, 24)
            .unwrap();
        wait_for(&run, "ready");
        until_session_has(&run, 3);
        run.signal(Signal::KILL);
        wait(&run);
        assert_eq!(session_of(&run), []);
    }

    #[test]
    fn a_script_runs_to_its_end_after_it_leaves_the_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "exec </dev/null >/dev/null 2>&1\nexport A=1\nsleep 0.3\ntouch marker\n";
        assert_eq!(
            run(&mut session, code),
            (String::new(), Status::Code(0), true)
        );
        assert!(tmp.path().join("marker").exists());
        assert_eq!(var(&session, "A").as_deref(), Some("1"));
    }

    #[test]
    fn a_process_with_a_name_cut_inside_a_character_is_in_its_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // The kernel keeps the first 15 bytes of the name: "a", 4 Japanese
        // characters of 3 bytes, and 2 bytes of the 5th.
        let name = tmp.path().join("aデプロイスクリプト");
        fs::write(&name, "#!/bin/sh\nsleep 30\ntrue\n").unwrap();
        fs::set_permissions(&name, fs::Permissions::from_mode(0o755)).unwrap();
        let code = format!("set -m\n'{}' &\necho ready\nwait\n", name.display());
        let run = session.start(&code, "check", 80, 24).unwrap();
        wait_for(&run, "ready");
        // The wrapper, the script and its sleep.
        until_session_has(&run, 3);
        run.signal(Signal::KILL);
        wait(&run);
        assert_eq!(session_of(&run), []);
    }

    #[test]
    fn a_background_job_outlives_its_run_until_stepbook_ends() {
        // A child of the wrapper, and a job that its parent left. Both are
        // in the process group of the wrapper, which the kernel hangs up
        // when the wrapper exits. The job writes after the run, more than
        // the buffer of a PTY holds.
        let job = "(sleep 1.2; seq 1 200000; echo $? > \"$PWD/seq\"; sleep 30)";
        for code in [format!("{job} &\n"), format!("({job} &)\n")] {
            let tmp = tempfile::tempdir().unwrap();
            let mut session = session(tmp.path());
            let run = session.start(&code, "check", 80, 24).unwrap();
            assert_eq!(wait(&run), (String::new(), Status::Code(0)), "{code}");
            let seq = tmp.path().join("seq");
            let deadline = std::time::Instant::now() + WAIT;
            while !fs::read_to_string(&seq).is_ok_and(|s| s.ends_with('\n')) {
                assert!(std::time::Instant::now() < deadline, "{code}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(fs::read_to_string(&seq).unwrap(), "0\n", "{code}");
            assert!(RUNS.lock().unwrap().contains(&run.pid), "{code}");
            end_sessions(&[run.pid.as_raw_pid()]);
            assert_eq!(session_of(&run), [], "{code}");
            // The reader reaps the wrapper once its jobs end.
            while RUNS.lock().unwrap().contains(&run.pid) {
                assert!(std::time::Instant::now() < deadline, "{code}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn a_run_ends_when_the_terminal_is_in_the_hands_of_another_group() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        // A bash with job control that dies leaves the PTY to its group: the
        // wrapper must not wait for its turn.
        let code = "(exec -c /proc/self/exe -m -c 'kill -KILL $$') 2>/dev/tty\n\
            sleep 30 &\necho started\n";
        let run = session.start(code, "check", 80, 24).unwrap();
        let (output, status) = wait(&run);
        assert_eq!(status, Status::Code(0));
        assert!(output.ends_with("started\n"), "{output}");
        end_sessions(&[run.pid.as_raw_pid()]);
        assert_eq!(session_of(&run), []);
    }

    #[test]
    fn a_process_whose_main_thread_ended_is_not_a_zombie_while_a_thread_runs() {
        // pid (name) state ppid pgrp session tty tpgid, 11 fields, then
        // the number of threads. The name can hold ") ".
        let stat = |state: &str, threads: u32| {
            format!(
                "42 (a) Z 1 (b) {state} 1 7 7 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 {threads} 0 9\n"
            )
        };
        let parse = |state, threads| {
            let process = parse_stat(42, stat(state, threads).as_bytes()).unwrap();
            (process.session, process.zombie)
        };
        assert_eq!(parse("Z", 1), (7, true));
        assert_eq!(parse("Z", 2), (7, false));
        assert_eq!(parse("S", 1), (7, false));
    }

    /// Waits until a run has `count` processes, which a loaded machine
    /// can fork late.
    fn until_session_has(run: &Run, count: usize) {
        let deadline = std::time::Instant::now() + WAIT;
        while session_of(run).len() != count {
            assert!(
                std::time::Instant::now() < deadline,
                "{:?}",
                session_of(run)
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The processes of a run, without the zombies that wait for their
    /// parent.
    fn session_of(run: &Run) -> Vec<Pid> {
        let session = |p: &Process| p.session == run.pid.as_raw_pid() && !p.zombie;
        processes()
            .into_iter()
            .filter(session)
            .map(|p| p.pid)
            .collect()
    }

    #[test]
    fn a_run_ends_after_all_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let (output, _, _) = run(&mut session, "seq 1 50000\n");
        assert_eq!(output.lines().count(), 50000);
        assert!(output.ends_with("\n50000\n"));
    }

    #[test]
    fn the_files_of_a_run_are_private_and_go_away_at_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let workspace = session.workspace().to_owned();
        let code = format!(
            "stat -c %a {0}/run-1.sh {0}/run-1.state {0}/run-1.env {0}/wrapper.sh {0}/timer\n",
            workspace.display()
        );
        let (output, _, _) = run(&mut session, &code);
        assert_eq!(output, "600\n600\n600\n600\n600\n");
        let mut files: Vec<_> = fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, ["timer", "wrapper.sh"]);
    }

    #[test]
    fn a_run_that_cannot_start_leaves_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        session.bash = tmp.path().join("no-bash");
        let error = session.start("ls\n", "check", 80, 24).err();
        let failed = StartError::Failed("No such file or directory".into());
        assert_eq!(error, Some(failed));
        let files: Vec<_> = fs::read_dir(session.workspace())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["timer", "wrapper.sh"]);
    }
}
