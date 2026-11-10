//! Runs: the workspace, the session state, and the PTY of each run.
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use crossbeam_channel::{Receiver, Sender};
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal};
use rustix::pty::OpenptFlags;
use rustix::termios::Winsize;

use crate::io_message;

/// The first executable file called `name` in the directories of `path`.
pub fn find_in_path(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|file| {
            file.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The directory of the run files, which only the user can read (spec
/// 8.3): the scripts can hold secrets. Dropping it deletes it.
pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    /// Creates `stepbook-<pid>` in `parent`. It fails if the directory
    /// exists: in a shared temp dir, another user could have made it.
    pub fn create(parent: &Path) -> std::io::Result<Workspace> {
        let dir = parent.join(format!("stepbook-{}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Workspace { dir })
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

/// `$XDG_RUNTIME_DIR` if it is an absolute path, else the temp dir.
pub fn workspace_parent(runtime_dir: Option<OsString>) -> PathBuf {
    runtime_dir
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
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
    next_id: u64,
}

/// The variables that a run gets besides the session (spec 8.2).
pub struct Env<'a> {
    /// The absolute path of the runbook.
    pub file: &'a Path,
    /// The absolute path of the log.
    pub log: Option<&'a Path>,
    pub block: &'a str,
}

/// A run in its own PTY. Its reader thread sends its output, then the exit
/// status, so that the end of a run never comes before its output.
pub struct Run {
    pub events: Receiver<Event>,
    /// The process of the wrapper, the leader of the process group.
    pid: u32,
    master: File,
    /// The state that the run started with.
    start: State,
    script: PathBuf,
    state_file: PathBuf,
}

pub enum Event {
    Output(Vec<u8>),
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

impl Session {
    pub fn new(workspace: Workspace, bash: PathBuf, state: State) -> std::io::Result<Session> {
        let wrapper = workspace.write("wrapper.sh", WRAPPER.as_bytes())?;
        Ok(Session {
            origin: state.dir.clone(),
            state,
            bash,
            workspace,
            wrapper,
            next_id: 1,
        })
    }

    pub fn workspace(&self) -> &Path {
        self.workspace.dir()
    }

    /// Starts a run of `code` in the session directory, in a new PTY of
    /// `columns` by `rows`. If the session directory no longer exists, the
    /// run does not start, and the session goes back to the directory that
    /// stepbook started in: never to `$HOME` (spec 8.2).
    pub fn start(&mut self, code: &str, env: &Env, columns: u16, rows: u16) -> Result<Run, String> {
        if !self.state.dir.is_dir() {
            let dir = std::mem::replace(&mut self.state.dir, self.origin.clone());
            return Err(format!(
                "cannot start: directory {} no longer exists",
                dir.display()
            ));
        }
        let fail = |e: std::io::Error| format!("cannot start: {}", io_message(&e));
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
        let run_files = [&script, &state_file];
        let (master, child) = (self.spawn(&run_files, &start, env, columns, rows))
            .inspect_err(|_| run_files.iter().for_each(|f| _ = std::fs::remove_file(f)))
            .map_err(fail)?;
        let reader = master.try_clone().map_err(fail)?;
        let (sender, events) = crossbeam_channel::bounded(OUTPUT_MESSAGES);
        let pid = child.id();
        std::thread::spawn(move || read_run(reader, child, sender));
        Ok(Run {
            events,
            pid,
            master,
            start,
            script,
            state_file,
        })
    }

    /// Spawns the wrapper with the files of a run, in a new PTY.
    fn spawn(
        &self,
        run_files: &[&PathBuf; 2],
        start: &State,
        env: &Env,
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
            .env_clear()
            .envs(&start.vars)
            .env("STEPBOOK_FILE", env.file)
            .env("STEPBOOK_LOG", env.log.unwrap_or(Path::new("")))
            .env("STEPBOOK_BLOCK", env.block)
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
        let end = std::fs::read(&run.state_file).ok();
        let _ = std::fs::remove_file(&run.script);
        let _ = std::fs::remove_file(&run.state_file);
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

    /// Sends a signal to the processes of the run: the wrapper's process
    /// group. A process that has left it, with setsid for one, does not get
    /// it. The run may have ended already.
    pub fn signal(&self, signal: Signal) {
        if let Some(pid) = Pid::from_raw(self.pid as i32) {
            let _ = rustix::process::kill_process_group(pid, signal);
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

/// Sends the output of a run until the PTY ends, then its exit status. On
/// Linux, a read fails with EIO once the run and its children have closed
/// the PTY, after the last output.
fn read_run(mut reader: File, mut child: Child, sender: Sender<Event>) {
    let mut buffer = vec![0; READ_SIZE];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if sender.send(Event::Output(buffer[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let status = child.wait().expect("the child of a run is waited for once");
    let status = match status.code() {
        Some(code) => Status::Code(code),
        None => Status::Signal(status.signal().unwrap_or(0)),
    };
    let _ = sender.send(Event::Exit(status));
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
        assert_eq!(
            workspace.dir(),
            tmp.path().join(format!("stepbook-{}", std::process::id()))
        );
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
        let dir = tmp.path().join(format!("stepbook-{}", std::process::id()));
        std::os::unix::fs::symlink(tmp.path(), &dir).unwrap();
        assert!(Workspace::create(tmp.path()).is_err());
    }

    #[test]
    fn the_workspace_goes_in_the_runtime_dir_if_it_is_absolute() {
        let temp = std::env::temp_dir();
        assert_eq!(
            workspace_parent(Some("/run/user/1".into())),
            Path::new("/run/user/1")
        );
        assert_eq!(workspace_parent(Some("run".into())), temp);
        assert_eq!(workspace_parent(Some("".into())), temp);
        assert_eq!(workspace_parent(None), temp);
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
        Session::new(workspace, bash, State::new(tmp.into(), vars)).unwrap()
    }

    fn env() -> Env<'static> {
        Env {
            file: Path::new("/r/run.md"),
            log: Some(Path::new("/r/run.log")),
            block: "check",
        }
    }

    /// The output of a run without its carriage returns, and its status.
    fn wait(run: &Run) -> (String, Status) {
        let mut output = Vec::new();
        loop {
            match run.events.recv().unwrap() {
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
        let run = session.start(code, &env(), 80, 24).unwrap();
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
    fn trap_keeps_its_other_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        let code = "trap 'echo a' EXIT; trap - EXIT\ntrap 'echo b' 0; trap EXIT\n\
            trap 'echo int' INT; trap -p INT\n( trap 'echo sub' EXIT; true )\n\
            echo \"$(trap 'echo cmd' EXIT)\"\ntrap -- 'echo c' exit\n";
        let (output, status, saved) = run(&mut session, code);
        let expected = "trap -- 'echo int' SIGINT\nsub\ncmd\nc\n";
        assert_eq!(
            (output.as_str(), status, saved),
            (expected, Status::Code(0), true)
        );
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
    fn a_removed_directory_fails_the_run_and_resets_the_session_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        run(&mut session, "mkdir gone; cd gone\n");
        fs::remove_dir(tmp.path().join("gone")).unwrap();
        let error = session.start("pwd\n", &env(), 80, 24).err();
        let message = format!(
            "cannot start: directory {} no longer exists",
            tmp.path().join("gone").display()
        );
        assert_eq!(error, Some(message));
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
            match run.events.recv().unwrap() {
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
            .start("export A=1\necho ready\nsleep 5\n", &env(), 80, 24)
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
            .start("export A=1; echo ready; read\n", &env(), 80, 24)
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
            "stat -c %a {0}/run-1.sh {0}/run-1.state {0}/wrapper.sh\n",
            workspace.display()
        );
        let (output, _, _) = run(&mut session, &code);
        assert_eq!(output, "600\n600\n600\n");
        let mut files: Vec<_> = fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, ["wrapper.sh"]);
    }

    #[test]
    fn a_run_that_cannot_start_leaves_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut session = session(tmp.path());
        session.bash = tmp.path().join("no-bash");
        let error = session.start("ls\n", &env(), 80, 24).err();
        assert_eq!(
            error.as_deref(),
            Some("cannot start: No such file or directory")
        );
        let files: Vec<_> = fs::read_dir(session.workspace())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, ["wrapper.sh"]);
    }
}
