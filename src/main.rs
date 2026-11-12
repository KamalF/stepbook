mod doc;
mod run;
mod term;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Select, Sender};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EnableLineWrap,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Size;
use ratatui::{DefaultTerminal, Terminal};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::process::Signal;
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM};

const USAGE: &str = "Usage: stepbook [--log PATH | --no-log] FILE";

const HELP: &str = "\
stepbook shows a Markdown runbook and runs its bash blocks one at a time.

Usage: stepbook [--log PATH | --no-log] FILE

Arguments:
  FILE           The Markdown runbook.

Options:
  --log PATH     Write the log to PATH. The default is FILE with the extension .log.
  --no-log       Write no log.
  -h, --help     Print this help.
  -V, --version  Print the version.";

#[derive(Debug, PartialEq)]
enum Command {
    Open(Args),
    Help,
    Version,
}

#[derive(Debug, PartialEq)]
struct Args {
    file: PathBuf,
    log: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut file = None;
    let mut log = None;
    let mut no_log = false;
    let mut args = args.into_iter();
    let mut options_end = false;
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if options_end || !text.starts_with('-') || text == "-" {
            if file.is_some() {
                return Err(format!("unexpected argument '{text}'"));
            }
            file = Some(PathBuf::from(arg));
            continue;
        }
        match &*text {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--" => options_end = true,
            "--no-log" => no_log = true,
            "--log" => match args.next() {
                Some(path) if !path.is_empty() => {
                    log = Some(set_once(log, PathBuf::from(path))?);
                }
                _ => return Err("--log needs a PATH".into()),
            },
            // The bytes of the argument, not its text: a path need not be
            // UTF-8.
            _ => match arg.as_bytes().strip_prefix(b"--log=") {
                Some([]) => return Err("--log needs a PATH".into()),
                Some(path) => log = Some(set_once(log, PathBuf::from(OsStr::from_bytes(path)))?),
                None => return Err(format!("unknown option '{text}'")),
            },
        }
    }
    let Some(file) = file else {
        return Err("the FILE argument is missing".into());
    };
    let log = match (log, no_log) {
        (Some(_), true) => return Err("use --log or --no-log, not both".into()),
        (Some(path), false) => Some(path),
        (None, true) => None,
        (None, false) => Some(file.with_extension("log")),
    };
    Ok(Command::Open(Args { file, log }))
}

fn set_once(old: Option<PathBuf>, new: PathBuf) -> Result<PathBuf, String> {
    match old {
        Some(_) => Err("--log is given twice".into()),
        None => Ok(new),
    }
}

/// The text of an I/O error, without its "(os error N)" suffix.
fn io_message(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(end) => text[..end].to_string(),
        None => text,
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Open(args)) => args,
        Ok(Command::Help) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Version) => {
            println!("stepbook {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            let message = doc::escaped_text(&message);
            print_error(&format!(
                "stepbook: {message}\n{USAGE}\nFor more information, run 'stepbook --help'."
            ));
            return ExitCode::from(2);
        }
    };
    match open(&args) {
        Ok(Exit::Quit) => ExitCode::SUCCESS,
        Ok(Exit::Signal(signal)) => {
            // Dies of the signal, as it would have without the cleanup.
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            ExitCode::FAILURE
        }
        Err(message) => {
            print_error(&format!("stepbook: {}", doc::escaped_text(&message)));
            ExitCode::FAILURE
        }
    }
}

/// Writes an error on stderr. eprintln would panic if stderr fails, as
/// after a SIGHUP, when the terminal is gone.
fn print_error(text: &str) {
    let _ = writeln!(std::io::stderr(), "{text}");
}

/// How the event loop ends.
#[derive(Debug, PartialEq)]
enum Exit {
    Quit,
    /// SIGTERM, SIGHUP, SIGINT or SIGQUIT.
    Signal(i32),
}

fn open(args: &Args) -> Result<Exit, String> {
    let text = read_runbook(&args.file)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(bash) = run::find_in_path("bash", &path) else {
        return Err("cannot find bash on the PATH".into());
    };
    let dir = std::env::current_dir()
        .map_err(|e| format!("cannot read the current directory: {}", io_message(&e)))?;
    let dir = run::start_dir(dir, std::env::var_os("PWD"));
    if !std::io::stdout().is_terminal() {
        return Err("run stepbook in a terminal: its output is not a terminal".into());
    }
    let doc = doc::parse(&text);
    let (sender, control) = crossbeam_channel::unbounded();
    // Before the workspace exists, so that these signals always delete it,
    // with the state files of the runs. In raw mode, ctrl-c and ctrl-\ are
    // keys: SIGINT and SIGQUIT come only from kill.
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])
        .map_err(|e| format!("cannot handle signals: {}", io_message(&e)))?;
    // With SIGCHLD ignored, which a parent can leave through exec, the
    // kernel reaps the wrappers on its own: their exit status would be lost,
    // and their pid free before the hang-up of their session. Any handler
    // stops that.
    signal_hook::flag::register(SIGCHLD, Arc::default())
        .map_err(|e| format!("cannot handle signals: {}", io_message(&e)))?;
    let signal_sender = sender.clone();
    std::thread::spawn(move || {
        for signal in signals.forever() {
            let _ = signal_sender.send(Control::Signal(signal));
        }
    });
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let parent = run::workspace_parent(runtime_dir, std::env::temp_dir());
    let fail = |e: std::io::Error| {
        format!(
            "cannot create the workspace in {}: {}",
            parent.display(),
            io_message(&e)
        )
    };
    let workspace = run::Workspace::create(&parent).map_err(fail)?;
    // The runs get absolute paths (spec 8.2).
    let file = dir.join(&args.file);
    let log = args.log.as_ref().map(|log| dir.join(log));
    let state = run::State::new(dir, std::env::vars_os());
    let session = run::Session::new(workspace, bash, state, file, log).map_err(fail)?;
    let mut terminal = set_up_terminal(session.workspace())
        .map_err(|e| format!("cannot set up the terminal: {}", io_message(&e)))?;
    let background = terminal_input()
        .ok()
        .and_then(|input| query_background(input.as_fd(), &mut std::io::stdout(), QUERY_WAIT));
    let title = args.file.file_name().unwrap_or(args.file.as_os_str());
    let mut view = view::View::new(doc, title.to_string_lossy().into_owned());
    view.background = background;
    let mut app = App::new(view, session, crossbeam_channel::tick(TICK));
    std::thread::spawn(move || read_terminal(sender));
    let result = event_loop(&mut terminal, &mut app, &control);
    run::end_runs();
    restore_terminal();
    // The drop of a ratatui terminal shows the cursor, and reports a
    // failure with eprintln, which panics when the terminal is gone, as
    // after a SIGHUP: stepbook would end with 101, not die of the signal.
    // restore_terminal shows the cursor.
    std::mem::forget(terminal);
    result.map_err(|e| format!("terminal error: {}", io_message(&e)))
}

/// Enters raw mode and the alternate screen, and turns autowrap off: a
/// character that the terminal draws wider than ratatui counts must not
/// wrap into the row below and move what is there. Bracketed paste is on,
/// so that a paste never arrives as keys: a line feed in it would run the
/// selected block.
///
/// The panic hook deletes the workspace, restores the terminal, and ends
/// stepbook: after a panic in another thread, the main thread would go on
/// drawing on the restored terminal. A panic of vt100 is the exception:
/// term.rs catches it. stepbook sets up the terminal itself:
/// the panic hook of ratatui reports a failed restore with eprintln, which
/// panics again when the terminal is gone, and the process aborts.
fn set_up_terminal(workspace: &Path) -> std::io::Result<DefaultTerminal> {
    let default_hook = std::panic::take_hook();
    let workspace = workspace.to_owned();
    std::panic::set_hook(Box::new(move |info| {
        // term.rs catches a panic of vt100, and the run goes on.
        if term::in_vt100() {
            return;
        }
        run::end_runs();
        let _ = std::fs::remove_dir_all(&workspace);
        restore_terminal();
        default_hook(info);
        std::process::exit(101);
    }));
    let set_up = || {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            DisableLineWrap,
            EnableBracketedPaste
        )?;
        Terminal::new(CrosstermBackend::new(stdout))
    };
    set_up().inspect_err(|_| restore_terminal())
}

/// The input of the terminal, where crossterm reads the keys: stdin if it
/// is a terminal, else /dev/tty.
fn terminal_input() -> std::io::Result<OwnedFd> {
    let stdin = std::io::stdin();
    match stdin.is_terminal() {
        true => stdin.as_fd().try_clone_to_owned(),
        false => Ok(std::fs::File::open("/dev/tty")?.into()),
    }
}

/// How long stepbook waits for the answers of the terminal at startup. A
/// terminal answers the query of its device attributes at once: the wait
/// ends there.
const QUERY_WAIT: Duration = Duration::from_millis(500);

/// The most bytes that stepbook keeps while it waits for the answers. The
/// bytes of a paste do not count.
const MAX_ANSWERS: usize = 4096;

const PASTE_START: [u8; 6] = *b"\x1b[200~";
const PASTE_END: [u8; 6] = *b"\x1b[201~";

/// How long the query waits for the next byte of a paste, or of the start
/// of one, past the end of its own wait: crossterm would read the rest of
/// the paste as keys. A terminal that never ends a paste must not hold the
/// startup forever.
const PASTE_WAIT: Duration = Duration::from_secs(5);

/// Asks the terminal for its background color (OSC 11), then for its
/// device attributes (DA1), and returns the color, as red, green and blue
/// (spec 5.3). A terminal answers in order, and every terminal answers DA1:
/// one that does not know OSC 11 gives no color, and stepbook waits no
/// longer. The keys typed before the answers are lost: they would arrive
/// mixed with them.
///
/// It reads a byte at a time, so that it never takes the start of a paste
/// after the answers. A paste that starts before them, it drops to its
/// end: crossterm would read the rest of the paste as keys, and a line
/// feed in it would run the selected block (spec 7.1).
fn query_background(
    input: BorrowedFd,
    output: &mut impl Write,
    wait: Duration,
) -> Option<(u8, u8, u8)> {
    output.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    output.flush().ok()?;
    let end = Instant::now() + wait;
    let mut answers = Vec::new();
    // While a paste goes on, its last bytes.
    let mut paste: Option<[u8; 6]> = None;
    loop {
        let answered = answers.last() == Some(&b'c') && device_attributes(&answers).is_some();
        let pasting =
            paste.is_some() || (1..PASTE_START.len()).any(|n| answers.ends_with(&PASTE_START[..n]));
        if !pasting && (answered || answers.len() >= MAX_ANSWERS) {
            break;
        }
        let left = match pasting {
            true => PASTE_WAIT,
            false => end.saturating_duration_since(Instant::now()),
        };
        let Some(byte) = read_byte(input, left) else {
            break;
        };
        match &mut paste {
            Some(window) => {
                window.rotate_left(1);
                let [.., last] = window;
                *last = byte;
                if *window == PASTE_END {
                    paste = None;
                }
            }
            None => {
                answers.push(byte);
                if answers.ends_with(&PASTE_START) {
                    paste = Some([0; 6]);
                }
            }
        }
    }
    parse_background(&answers)
}

/// The next byte of `input`, if one comes within `wait`.
fn read_byte(input: BorrowedFd, wait: Duration) -> Option<u8> {
    let timeout = Timespec::try_from(wait).unwrap_or_default();
    let mut fds = [PollFd::new(&input, PollFlags::IN)];
    loop {
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return None,
            Ok(_) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => return None,
        }
    }
    let mut byte = [0];
    match rustix::io::read(input, &mut byte) {
        Ok(1) => Some(byte[0]),
        _ => None,
    }
}

/// Where the answer to DA1 starts in `bytes`, if they hold all of it:
/// `ESC [ ?`, then digits and semicolons, then `c`.
fn device_attributes(bytes: &[u8]) -> Option<usize> {
    (0..bytes.len()).find(|&i| {
        let Some(rest) = bytes[i..].strip_prefix(b"\x1b[?") else {
            return false;
        };
        let end = rest.iter().position(|b| !b.is_ascii_digit() && *b != b';');
        end.is_some_and(|end| rest[end] == b'c')
    })
}

/// The color in the answer to OSC 11 in `bytes`, before the answer to DA1:
/// `ESC ] 11 ; rgb:R/G/B`, ended by BEL or by `ESC \`, where R, G and B
/// have 1 to 4 hex digits each.
fn parse_background(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let bytes = &bytes[..device_attributes(bytes).unwrap_or(bytes.len())];
    let start = bytes.windows(5).position(|w| w == b"\x1b]11;")? + 5;
    let rest = &bytes[start..];
    let end = rest.iter().position(|&b| b == 0x07 || b == 0x1b)?;
    let text = std::str::from_utf8(&rest[..end]).ok()?;
    let mut values = text.strip_prefix("rgb:")?.split('/').map(|hex| {
        let ok = (1..=4).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        let value = u32::from_str_radix(hex, 16).ok().filter(|_| ok)?;
        // A value of n digits is a fraction of 16^n - 1.
        let max = (1u32 << (4 * hex.len())) - 1;
        u8::try_from((value * 255 + max / 2) / max).ok()
    });
    let rgb = (values.next()??, values.next()??, values.next()??);
    values.next().is_none().then_some(rgb)
}

/// Ignores errors: after a SIGHUP, the terminal is gone.
fn restore_terminal() {
    let _ = execute!(
        std::io::stdout(),
        EndSynchronizedUpdate,
        EnableLineWrap,
        DisableBracketedPaste,
        Show
    );
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

/// Runbooks are a few KB: the largest of ~/dev/claude/runbooks had 22 KB
/// on 2026-11-09. The view lays out the whole file at each width.
const MAX_FILE: u64 = 1024 * 1024;

/// Reads a runbook, which must be a regular file of at most `MAX_FILE`
/// bytes: a link to /dev/zero would fill the memory, and a FIFO would
/// block the open. Its lines must nest at most `doc::MAX_NESTING` levels.
fn read_runbook(path: &Path) -> Result<String, String> {
    let fail = |reason: String| format!("cannot read {}: {reason}", path.display());
    let metadata = std::fs::metadata(path).map_err(|e| fail(io_message(&e)))?;
    if !metadata.is_file() {
        return Err(fail("not a regular file".into()));
    }
    let file = std::fs::File::open(path).map_err(|e| fail(io_message(&e)))?;
    let mut bytes = Vec::new();
    (file.take(MAX_FILE + 1).read_to_end(&mut bytes)).map_err(|e| fail(io_message(&e)))?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(fail("the file is larger than 1 MB".into()));
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if doc::nesting(&text) > doc::MAX_NESTING {
        return Err(fail(
            "a line nests more than 1,000 levels of quotes, lists or indent".into(),
        ));
    }
    Ok(text)
}

/// What the event loop reacts to. The main thread owns all the state,
/// and the other threads only send these (spec 13).
enum Control {
    Terminal(std::io::Result<Event>),
    Signal(i32),
}

/// Sends the terminal events, until the first error.
fn read_terminal(control: Sender<Control>) {
    loop {
        let event = event::read();
        let failed = event.is_err();
        if control.send(Control::Terminal(event)).is_err() || failed {
            return;
        }
    }
}

/// A block that has run: its terminal, and its run while it runs.
struct Ran {
    screen: Arc<term::Shared>,
    run: Option<run::Run>,
    started: Instant,
    ended: Option<Instant>,
}

/// The state that the event loop owns.
struct App {
    view: view::View,
    session: run::Session,
    /// The blocks that have run, by index. A crafted runbook can hold 80,000
    /// blocks, and the terminal of a run takes about 700 bytes.
    ran: BTreeMap<usize, Ran>,
    /// The blocks whose output changed since the last frame: their rows are
    /// made once a frame, not once a read of the PTY.
    changed: BTreeSet<usize>,
    /// While blocks run, a tick updates their time and blinks their glyph.
    ticks: Receiver<Instant>,
    /// The question in the footer, which the next key answers.
    question: Option<Question>,
    /// When the notification in the footer ends.
    notice_until: Option<Instant>,
    /// Until when keys do nothing, after stepbook stopped sending them to
    /// a program on its own (spec 7.2).
    paused_until: Option<Instant>,
    /// While stepbook stops the runs to quit: when it kills those left.
    stopping: Option<Instant>,
}

/// A question in the footer (spec 7.4).
#[derive(Debug, PartialEq)]
enum Question {
    /// Re-run block i.
    Rerun(usize),
    /// Stop the runs and quit.
    Quit,
    /// Send a paste of several lines to the program of block i, which did
    /// not turn on bracketed paste (spec 7.2).
    Paste(usize, Vec<u8>),
}

/// How long the runs have to end after SIGHUP, when stepbook quits (spec
/// 11).
const STOP_WAIT: Duration = Duration::from_secs(2);

/// How long a notification shows (spec 6.1).
const NOTICE: Duration = Duration::from_secs(3);

/// How long keys do nothing after stepbook stopped sending them to a
/// program on its own (spec 7.2).
const PAUSE: Duration = Duration::from_secs(1);

/// An event of the loop: a control event, or a message from the run of a
/// block.
enum Incoming {
    Control(Control),
    Tick,
    Run(usize, run::Event),
}

/// The blink of the glyph of a running block, and the update of its time.
const TICK: Duration = Duration::from_millis(500);

/// After output, the loop reads the next events for this long before it
/// draws: a program that prints a lot redraws the view at most this often.
const FRAME: Duration = Duration::from_millis(16);

impl App {
    fn new(view: view::View, session: run::Session, ticks: Receiver<Instant>) -> App {
        App {
            view,
            session,
            ran: BTreeMap::new(),
            changed: BTreeSet::new(),
            ticks,
            question: None,
            notice_until: None,
            paused_until: None,
            stopping: None,
        }
    }

    /// The next event, a control event first, until `deadline`, or with no
    /// end. An error means that the thread that reads the keys stopped.
    fn next(
        &self,
        control: &Receiver<Control>,
        deadline: Option<Instant>,
    ) -> Option<Result<Incoming, RecvError>> {
        if let Ok(event) = control.try_recv() {
            return Some(Ok(Incoming::Control(event)));
        }
        let runs: Vec<_> = self.runs().collect();
        // Not biased among the runs, so that no run's output waits for
        // another's.
        let mut select = Select::new();
        select.recv(control);
        // Without runs, the tick would only wake the loop. With runs, it
        // comes right after the control channel.
        if !runs.is_empty() {
            select.recv(&self.ticks);
        }
        for (_, run) in &runs {
            select.recv(&run.events);
        }
        let operation = match deadline {
            Some(deadline) => select.select_deadline(deadline).ok()?,
            None => select.select(),
        };
        Some(match operation.index() {
            0 => operation.recv(control).map(Incoming::Control),
            1 => {
                let _ = operation.recv(&self.ticks);
                Ok(Incoming::Tick)
            }
            n => {
                let (i, run) = runs[n - 2];
                let event = operation.recv(&run.events);
                let event = event.expect("a reader sends the exit status before it stops");
                Ok(Incoming::Run(i, event))
            }
        })
    }

    /// The blocks that run, with their runs.
    fn runs(&self) -> impl Iterator<Item = (usize, &run::Run)> {
        (self.ran.iter()).filter_map(|(&i, ran)| Some((i, ran.run.as_ref()?)))
    }

    /// When the loop must wake up with no event: at the end of the
    /// notification, at the end of the pause of the keys, and when the
    /// runs that stop get SIGKILL.
    fn deadline(&self) -> Option<Instant> {
        [self.notice_until, self.paused_until, self.stopping]
            .into_iter()
            .flatten()
            .min()
    }

    /// Ends the notification and the pause of the keys after their time.
    fn expire(&mut self) {
        let now = Instant::now();
        if self.notice_until.is_some_and(|until| now >= until) {
            self.notice_until = None;
            self.view.notice = None;
        }
        if self.paused_until.is_some_and(|until| now >= until) {
            self.paused_until = None;
        }
    }

    /// Makes the keys do nothing for a moment, after stepbook stopped
    /// sending them to the program of block `i` on its own: the run ended,
    /// or the program left the alternate screen. Keys typed for the
    /// program would act on the document: a second ctrl-c would quit
    /// stepbook, and Down then Enter would run another block (spec 7.2).
    fn pause(&mut self, i: usize) {
        let until = Instant::now() + PAUSE;
        self.paused_until = Some(until);
        let label = self.view.doc.label(i);
        self.view.notice = Some(format!("keys off for 1 s: {label} no longer takes them"));
        self.notice_until = Some(until);
    }

    /// Applies a terminal event, and returns how the loop ends, if it does.
    fn handle(&mut self, event: &Event, size: Size) -> Option<Exit> {
        if let Event::Resize(..) = event {
            self.resize(size);
        }
        let paused = self
            .paused_until
            .is_some_and(|until| Instant::now() < until);
        if self.stopping.is_some() || paused && matches!(event, Event::Key(_)) {
            return None;
        }
        if let Some(question) = self.question.take() {
            match event {
                // On a screen too small for the view, the question does not
                // show, and no key answers it (spec 6.1).
                Event::Key(key) if key.kind == KeyEventKind::Press && !view::too_small(size) => {
                    self.view.question = None;
                    let yes = key.code == KeyCode::Char('y') && key.modifiers.is_empty();
                    match question {
                        // As Enter, y runs no code off the screen (spec 7.1):
                        // after a resize, it shows the block and asks again.
                        Question::Rerun(i) if yes => match self.view.reveal(size) {
                            true => self.start(i, size),
                            false => self.ask_rerun(i),
                        },
                        Question::Quit if yes => self.stop_runs(),
                        Question::Paste(i, bytes) if yes => {
                            if let Some(run) = self.ran.get(&i).and_then(|ran| ran.run.as_ref()) {
                                run.write(&bytes);
                            }
                        }
                        Question::Rerun(_) | Question::Quit | Question::Paste(..) => {}
                    }
                }
                _ => self.question = Some(question),
            }
            return None;
        }
        if let Some(i) = self.view.input_block() {
            if !self.send(i, event, size) {
                self.leave(size);
            }
            return None;
        }
        if let view::Mode::Full(_) = self.view.mode {
            self.browse(event, size);
            return None;
        }
        match action(event, size)? {
            Action::Quit => match self.runs().count() {
                0 => return Some(Exit::Quit),
                1 => self.ask(Question::Quit, "1 block running. Stop it and quit?".into()),
                n => self.ask(
                    Question::Quit,
                    format!("{n} blocks running. Stop them and quit?"),
                ),
            },
            Action::Select(step) => self.view.select_by(step, size),
            Action::First => self.view.select_first(size),
            Action::Last => self.view.select_last(size),
            Action::Page(pages) => self.view.scroll_pages(pages, size),
            Action::Run => {
                if self.view.reveal(size)
                    && let Some(i) = self.view.selected
                {
                    self.enter(i, size);
                }
            }
            Action::Input => {
                let i = self.view.selected?;
                match (self.view.running(i), self.just_ended(i)) {
                    (true, _) => self.enter_input(i, size),
                    // The run ended as i came: the keys after it were
                    // typed for the program.
                    (false, true) => self.pause(i),
                    (false, false) => self.notify("not running: ↵ runs it".into()),
                }
            }
            Action::Output => {
                let i = self.view.selected?;
                if !self.ran.contains_key(&i) {
                    self.notify("no output: ↵ runs it".into());
                    return None;
                }
                self.open_full(i, size, view::Back::Normal);
                if self.just_ended(i) {
                    self.pause(i);
                }
            }
        }
        None
    }

    /// Sends a key or a paste to the run of block `i`, in input mode or in
    /// its full-screen view, and returns whether to go on. `ctrl-o` leaves,
    /// and so does any key or paste on a screen too small for the view,
    /// which does not show what they do (spec 6.1, 7.2).
    /// A paste of lines to a program that did not turn on bracketed paste
    /// waits for a `y` in the footer: each line would run.
    /// stepbook keeps bracketed paste on in the outer terminal, which then
    /// takes every paste for safe and gives no warning of its own.
    fn send(&mut self, i: usize, event: &Event, size: Size) -> bool {
        let Some(run) = self.ran.get(&i).and_then(|ran| ran.run.as_ref()) else {
            return true;
        };
        let mut lines = 0;
        let bytes = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if is_ctrl_o(key) {
                    return false;
                }
                term::key_bytes(key, run.screen.application_cursor())
            }
            Event::Paste(text) => {
                let bracketed = run.screen.bracketed_paste();
                // A control character runs a line too: readline takes
                // ctrl-o for Enter, as it does a line feed.
                if !bracketed && text.chars().any(|c| c.is_control() && c != '\t') {
                    let parts = text.split(['\r', '\n']);
                    lines = parts.filter(|line| !line.is_empty()).count().max(1);
                }
                term::paste_bytes(text, bracketed)
            }
            _ => return true,
        };
        if view::too_small(size) {
            return false;
        }
        if lines == 0 {
            run.write(&bytes);
            return true;
        }
        let lines = match lines {
            1 => "1 line".to_string(),
            n => format!("{n} lines"),
        };
        let text = format!("Paste {lines} to {}?", self.view.doc.label(i));
        self.ask(Question::Paste(i, bytes), text);
        true
    }

    /// Whether the run of block `i` ended less than the pause of the keys
    /// ago.
    fn just_ended(&self, i: usize) -> bool {
        let ended = self.ran.get(&i).and_then(|ran| ran.ended);
        ended.is_some_and(|ended| ended.elapsed() < PAUSE)
    }

    /// The keys of the full-screen view after the run: they scroll the
    /// scrollback as they scroll the document view, or go back (spec 7.3).
    fn browse(&mut self, event: &Event, size: Size) {
        let view::Mode::Full(full) = &self.view.mode else {
            return;
        };
        let page = self.view.pty_size(full.block, size).1 as isize;
        let back = matches!(event, Event::Key(key) if key.kind == KeyEventKind::Press
            && (key.code == KeyCode::Esc || is_ctrl_o(key)));
        let lines = match action(event, size) {
            Some(Action::Select(step)) => -step,
            Some(Action::Page(pages)) => -pages * page,
            Some(Action::First) => isize::MAX,
            Some(Action::Last) => isize::MIN,
            Some(Action::Quit | Action::Output) => return self.leave(size),
            _ if back => return self.leave(size),
            _ => return,
        };
        self.view.scroll_full(lines, size);
    }

    /// Sends the keys to the program of block `i`: in input mode, or in its
    /// full-screen view while the program is on the alternate screen, which
    /// only that view shows (spec 6.3, 7.2).
    fn enter_input(&mut self, i: usize, size: Size) {
        match self.view.alternate(i) {
            true => self.open_full(i, size, view::Back::Input),
            false => {
                self.view.mode = view::Mode::Input(i);
                self.view.show_output_end(i, size);
            }
        }
    }

    /// Opens the full-screen view of block `i`. While the block runs, its
    /// PTY takes the screen without the footer (spec 7.3). If its program
    /// is on the alternate screen, the view goes `back` when it leaves it
    /// (spec 6.3).
    fn open_full(&mut self, i: usize, size: Size, back: view::Back) {
        self.view.mode = view::Mode::Full(view::Full {
            block: i,
            rows: term::Rows::default(),
            scroll: 0,
            back: self.view.alternate(i).then_some(back),
        });
        self.resize_pty(i, size);
        self.changed.insert(i);
    }

    /// Goes back to the document view. A running block gets its PTY of
    /// the document view back.
    fn leave(&mut self, size: Size) {
        let mode = std::mem::replace(&mut self.view.mode, view::Mode::Normal);
        if let view::Mode::Full(full) = mode {
            self.resize_pty(full.block, size);
            self.changed.insert(full.block);
        }
    }

    /// Gives block `i` a PTY of its size, if it runs. The kernel sends
    /// SIGWINCH to the program. On a screen too small for the view, the PTY
    /// keeps its size until a resize: the layout of a crafted file at a
    /// tiny width costs too much (spec 6.1).
    fn resize_pty(&self, i: usize, size: Size) {
        let Some(ran) = self.ran.get(&i) else {
            return;
        };
        if view::too_small(size) {
            return;
        }
        if let Some(run) = &ran.run {
            let (columns, rows) = self.view.pty_size(i, size);
            let _ = run.resize(columns, rows);
            ran.screen.resize(columns, rows);
        }
    }

    /// Whether a key of the document view acts on what the screen shows:
    /// `Enter`, `i`, `o`, or the answer to a question. In input mode and
    /// in the full-screen view, keys go to the program, in order.
    fn acts(&self, event: &Event, size: Size) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        let acts = self.view.mode == view::Mode::Normal
            && matches!(
                action(event, size),
                Some(Action::Run | Action::Input | Action::Output)
            );
        key.kind == KeyEventKind::Press && (self.question.is_some() || acts)
    }

    /// What `Enter` does on block `i`, by its state (spec 8.1).
    fn enter(&mut self, i: usize, size: Size) {
        if let Some(problem) = self.view.doc.blocks[i].problem {
            self.notify(format!("cannot run: {problem}"));
            return;
        }
        match self.view.status(i) {
            None => self.start(i, size),
            Some(view::Status::Running(_)) => {
                self.notify("already running: i to type, o to view".into());
            }
            Some(_) => self.ask_rerun(i),
        }
    }

    fn ask_rerun(&mut self, i: usize) {
        let label = self.view.doc.label(i);
        self.ask(Question::Rerun(i), format!("Re-run {label}?"));
    }

    fn ask(&mut self, question: Question, text: String) {
        self.view.question = Some(text);
        self.question = Some(question);
    }

    /// Sends SIGHUP to the runs, as the close of their PTY would, before
    /// stepbook quits (spec 11).
    fn stop_runs(&mut self) {
        for (_, run) in self.runs() {
            run.signal(Signal::HUP);
        }
        self.stopping = Some(Instant::now() + STOP_WAIT);
        self.notify("stopping the blocks that run".into());
    }

    /// Whether stepbook can quit, after `stop_runs`: once no run is left,
    /// or after the wait, when it sends SIGKILL to the runs left.
    fn stopped(&self) -> bool {
        let Some(until) = self.stopping else {
            return false;
        };
        if Instant::now() >= until {
            for (_, run) in self.runs() {
                run.signal(Signal::KILL);
            }
            return true;
        }
        self.runs().next().is_none()
    }

    /// Shows a notification in the footer for 3 seconds.
    fn notify(&mut self, text: String) {
        self.view.notice = Some(text);
        self.notice_until = Some(Instant::now() + NOTICE);
    }

    /// Starts a run of block `i`. `enter` never starts a block that runs:
    /// a block never runs twice at a time (upmd issue K5).
    fn start(&mut self, i: usize, size: Size) {
        debug_assert!(!self.ran.get(&i).is_some_and(|ran| ran.run.is_some()));
        let block = &self.view.doc.blocks[i];
        let (columns, rows) = self.view.pty_size(i, size);
        let label = self.view.doc.label(i);
        let status = match self.session.start(&block.code, &label, columns, rows) {
            Ok(run) => {
                let screen = run.screen.clone();
                let started = Instant::now();
                let run = Some(run);
                self.ran.insert(
                    i,
                    Ran {
                        screen,
                        run,
                        started,
                        ended: None,
                    },
                );
                view::Status::Running(Duration::ZERO)
            }
            Err(error) => {
                self.ran.remove(&i);
                // The session directory no longer existed (spec 8.2).
                if let run::StartError::Gone(_) = error {
                    let dir = self.session.state.dir.display().to_string();
                    self.notify(format!("session directory reset to {dir}"));
                }
                view::Status::NotStarted(error.to_string())
            }
        };
        self.view.set_status(i, status);
        let _ = self.view.set_alternate(i, false);
        self.view.set_output(i, term::Rows::default(), size);
    }

    /// Reads a message from the run of block `i`. Its rows are made before
    /// the next frame.
    fn read(&mut self, i: usize, event: run::Event) {
        let ran = self.ran.get_mut(&i).expect("a message comes from a run");
        match event {
            run::Event::Output(_) => {}
            run::Event::Exit(status) => {
                let run = ran.run.take().expect("a run sends its exit status once");
                ran.ended = Some(Instant::now());
                // A paste for the program has nowhere to go.
                if matches!(self.question, Some(Question::Paste(j, _)) if j == i) {
                    self.question = None;
                    self.view.question = None;
                }
                let saved = self.session.finish(&run);
                let input = self.view.input_block() == Some(i);
                let took = ran.started.elapsed();
                let ended = view::Status::Ended {
                    status,
                    took,
                    saved,
                };
                self.view.set_status(i, ended);
                // The keys no longer go to the program. In the full-screen
                // view, they now scroll the output.
                if input {
                    if self.view.mode == view::Mode::Input(i) {
                        self.view.mode = view::Mode::Normal;
                    }
                    self.pause(i);
                }
            }
        }
        self.changed.insert(i);
    }

    /// Makes the rows of the output that changed (spec 6.3). A terminal that
    /// its reader holds gives its rows at a later frame: the reader sends an
    /// event after it.
    fn refresh(&mut self, size: Size) {
        // The blocks whose terminal the reader held: their rows wait for a
        // later frame, as do those that the full-screen view hides.
        let mut later = BTreeSet::new();
        let mut hidden = BTreeSet::new();
        // A switch of the view adds its block again: its frame then shows
        // the rows of the new view.
        while let Some(i) = self.changed.pop_first() {
            let Some(ran) = self.ran.get(&i) else {
                continue;
            };
            let full = match &self.view.mode {
                view::Mode::Full(full) if full.block == i => true,
                // The full-screen view of another block hides these rows.
                view::Mode::Full(_) => {
                    hidden.insert(i);
                    continue;
                }
                _ => false,
            };
            let want = match (full, ran.run.is_some()) {
                (true, true) => term::Want::Screen,
                (true, false) => term::Want::All,
                (false, cursor) => term::Want::Last { cursor },
            };
            let Some(rows) = ran.screen.try_rows(want) else {
                later.insert(i);
                continue;
            };
            let switched = self.view.set_alternate(i, rows.alternate);
            let rows_ended_on_alternate = rows.ended_on_alternate;
            match &mut self.view.mode {
                view::Mode::Full(full) => full.rows = rows,
                // Under the block, the rows from before the switch stay:
                // a full-screen program shows in the full-screen view.
                _ if rows.alternate => {}
                _ => self.view.set_output(i, rows, size),
            }
            // stepbook leaves the alternate screen of a run that ended on
            // it: its full-screen view stays, with the last screen.
            if switched && !rows_ended_on_alternate {
                self.switch(i, size);
                if !matches!(self.view.mode, view::Mode::Full(_)) {
                    self.changed.append(&mut hidden);
                }
            }
        }
        later.append(&mut hidden);
        self.changed = later;
    }

    /// Closes the full-screen view that opened on the alternate screen when
    /// the program leaves it, and opens the view of the block in input mode
    /// when its program enters it (spec 6.3). Only `i` and `o` make the
    /// keys go to a program.
    fn switch(&mut self, i: usize, size: Size) {
        let (alternate, running) = (self.view.alternate(i), self.view.running(i));
        match &self.view.mode {
            // A view that opened on the alternate screen closes when the
            // program leaves it. refresh holds back the rows of the blocks
            // that the view does not show.
            view::Mode::Full(full) => {
                debug_assert_eq!(full.block, i);
                let Some(back) = full.back else {
                    return;
                };
                self.leave(size);
                match back == view::Back::Input && running {
                    true => self.enter_input(i, size),
                    false => self.pause(i),
                }
            }
            // The keys already go to the program: its view opens.
            view::Mode::Input(j) if *j == i && alternate => self.enter_input(i, size),
            _ => {}
        }
    }

    /// Updates the time of each running block, and blinks its glyph.
    fn tick(&mut self) {
        self.view.blink = !self.view.blink;
        for (&i, ran) in &self.ran {
            if ran.run.is_some() {
                let status = view::Status::Running(ran.started.elapsed());
                self.view.set_status(i, status);
            }
        }
    }

    /// Gives each running block a PTY of its new size (spec 6.3).
    fn resize(&mut self, size: Size) {
        let running: Vec<_> = self.runs().map(|(i, _)| i).collect();
        for i in running {
            self.resize_pty(i, size);
            self.changed.insert(i);
        }
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    control: &Receiver<Control>,
) -> std::io::Result<Exit> {
    let mut size = terminal.size()?;
    // A key that waits for the next frame.
    let mut waiting = None;
    loop {
        app.refresh(size);
        // A terminal can draw a grapheme of prose wider than ratatui counts
        // it, and shift the rest of its row. ratatui sends only the cells
        // that changed, so the shift would stay on later frames, where a
        // line of code can take that row. Each frame clears the screen and
        // draws every cell, in a synchronized update that does not flicker.
        queue!(
            terminal.backend_mut(),
            BeginSynchronizedUpdate,
            Clear(ClearType::All)
        )?;
        terminal.swap_buffers();
        terminal.draw(|frame| app.view.draw(frame))?;
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
        // The events of one frame: after a key, those that wait; after
        // output, those of the next FRAME. A deadline of the app also ends
        // the frame. crossbeam returns a ready event even after the
        // deadline, so the loop checks the end of the frame itself: else a
        // block that prints without a pause would let no frame end.
        let mut frame = None;
        if let Some(event) = waiting.take() {
            if let Some(exit) = app.handle(&event, size) {
                return Ok(exit);
            }
            frame = Some(Instant::now());
        }
        loop {
            app.expire();
            if app.stopped() {
                return Ok(Exit::Quit);
            }
            if frame.is_some_and(|frame| Instant::now() >= frame) {
                break;
            }
            let deadline = [frame, app.deadline()].into_iter().flatten().min();
            let Some(incoming) = app.next(control, deadline) else {
                frame.get_or_insert(Instant::now());
                continue;
            };
            let Ok(incoming) = incoming else {
                let error = "the thread that reads the keys stopped";
                return Err(std::io::Error::other(error));
            };
            let wait = match incoming {
                Incoming::Control(Control::Terminal(event)) => {
                    let event = event?;
                    if let Event::Resize(columns, rows) = event {
                        size = Size::new(columns, rows);
                    }
                    // Such a key acts on what the screen shows: after other
                    // events, it waits for the frame that shows them (spec
                    // 7.1).
                    if frame.is_some() && app.acts(&event, size) {
                        waiting = Some(event);
                        break;
                    }
                    if let Some(exit) = app.handle(&event, size) {
                        return Ok(exit);
                    }
                    Duration::ZERO
                }
                Incoming::Control(Control::Signal(signal)) => return Ok(Exit::Signal(signal)),
                Incoming::Tick => {
                    app.tick();
                    Duration::ZERO
                }
                Incoming::Run(i, event) => {
                    app.read(i, event);
                    FRAME
                }
            };
            frame.get_or_insert(Instant::now() + wait);
        }
    }
}

/// Whether a key is ctrl-o, which leaves input mode and the full-screen
/// view (spec 7.2, 7.3).
fn is_ctrl_o(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('o') && key.modifiers == KeyModifiers::CONTROL
}

/// What a key does in the document view (spec 7.1).
#[derive(Debug, PartialEq)]
enum Action {
    Quit,
    /// Select the block this many blocks away.
    Select(isize),
    First,
    Last,
    /// Scroll this many pages.
    Page(isize),
    /// Run the selected block, if the view shows it (spec 7.1).
    Run,
    /// Enter input mode, if the selected block runs (spec 7.2).
    Input,
    /// Open the full-screen view of the selected block, if it has run
    /// (spec 7.3).
    Output,
}

/// On a screen too small for the view, only the keys that quit work. A
/// paste does nothing.
fn action(event: &Event, size: Size) -> Option<Action> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let action = match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('c') if ctrl => Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => Action::Select(1),
        KeyCode::Up | KeyCode::Char('k') => Action::Select(-1),
        KeyCode::Home => Action::First,
        KeyCode::End => Action::Last,
        KeyCode::PageDown => Action::Page(1),
        KeyCode::PageUp => Action::Page(-1),
        KeyCode::Enter => Action::Run,
        KeyCode::Char('i') => Action::Input,
        // ctrl-o leaves input mode and the full-screen view: twice, it
        // would go back to them, and send the next keys to the program.
        KeyCode::Char('o') if !ctrl => Action::Output,
        _ => return None,
    };
    (action == Action::Quit || !view::too_small(size)).then_some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size::new(80, 24);

    /// The answer of `query_background` to the bytes that the terminal
    /// sends, and the bytes that it writes.
    fn background(answers: &[u8], wait: Duration) -> (Option<(u8, u8, u8)>, Vec<u8>) {
        let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
        terminal.write_all(answers).unwrap();
        let mut output = Vec::new();
        let color = query_background(input.as_fd(), &mut output, wait);
        (color, output)
    }

    const DA1: &[u8] = b"\x1b[?62;22c";

    #[test]
    fn the_background_comes_from_the_answer_to_osc_11_before_da1() {
        let answers = [b"\x1b]11;rgb:2828/2828/2828\x1b\\".as_slice(), DA1].concat();
        let (color, output) = background(&answers, Duration::from_secs(5));
        assert_eq!(color, Some((40, 40, 40)));
        assert_eq!(output, b"\x1b]11;?\x1b\\\x1b[c");
        // Keys typed ahead come before the answers, and are lost. A "c" in
        // them does not end the wait.
        let answers = [b"jc\x1b]11;rgb:ff/8c/0\x07".as_slice(), DA1].concat();
        assert_eq!(
            background(&answers, Duration::from_secs(5)).0,
            Some((255, 140, 0))
        );
    }

    #[test]
    fn the_wait_takes_answers_that_come_later() {
        let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
        let answer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            terminal.write_all(b"\x1b]11;rgb:0/0/0\x07").unwrap();
            std::thread::sleep(Duration::from_millis(50));
            terminal.write_all(DA1).unwrap();
            terminal
        });
        let color = query_background(input.as_fd(), &mut Vec::new(), Duration::from_secs(5));
        assert_eq!(color, Some((0, 0, 0)));
        answer.join().unwrap();
    }

    /// The bytes left in `input`.
    fn rest(input: &std::os::unix::net::UnixStream) -> Vec<u8> {
        std::iter::from_fn(|| read_byte(input.as_fd(), Duration::from_millis(10))).collect()
    }

    #[test]
    fn a_paste_after_the_answers_stays_whole_for_the_keys() {
        let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
        let paste = b"\x1b[200~y\ri\x1b[201~";
        terminal
            .write_all(&[b"\x1b]11;rgb:0/0/0\x07", DA1, paste].concat())
            .unwrap();
        let color = query_background(input.as_fd(), &mut Vec::new(), Duration::from_secs(5));
        assert_eq!(color, Some((0, 0, 0)));
        assert_eq!(rest(&input), paste);
    }

    #[test]
    fn a_paste_before_the_answers_is_dropped_to_its_end() {
        let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
        // Longer than the bound of the answers, which comes after it.
        let paste = [b"\x1b[200~".as_slice(), &[b'y'; 5000], b"\ri\x1b[201~"].concat();
        terminal
            .write_all(&[&paste, b"\x1b]11;rgb:0/0/0\x07".as_slice(), DA1].concat())
            .unwrap();
        let color = query_background(input.as_fd(), &mut Vec::new(), Duration::from_secs(5));
        assert_eq!(color, Some((0, 0, 0)));
        assert_eq!(rest(&input), b"");
        // A paste that starts at the bound of the answers.
        let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
        let typed = [&[b'x'; MAX_ANSWERS - 6][..], b"\x1b[200~y\ri\x1b[201~"].concat();
        terminal.write_all(&typed).unwrap();
        let wait = Duration::from_millis(50);
        assert_eq!(query_background(input.as_fd(), &mut Vec::new(), wait), None);
        assert_eq!(rest(&input), b"");
        // A paste that pauses for longer than the wait, and pastes whose
        // start comes in two parts across the end of the wait.
        let pastes: [&[&[u8]]; 3] = [
            &[b"\x1b[200~y", b"\r", b"i\x1b[201~"],
            &[b"\x1b[20", b"0~y\ri\x1b[201~"],
            &[b"\x1b[200", b"~y\ri\x1b[201~"],
        ];
        for parts in pastes {
            let (mut terminal, input) = std::os::unix::net::UnixStream::pair().unwrap();
            // The first part comes before the end of the wait, the others
            // after it.
            terminal.write_all(parts[0]).unwrap();
            let later: Vec<Vec<u8>> = parts[1..].iter().map(|part| part.to_vec()).collect();
            let typing = std::thread::spawn(move || {
                for part in later {
                    std::thread::sleep(Duration::from_millis(100));
                    terminal.write_all(&part).unwrap();
                }
                terminal
            });
            let wait = Duration::from_millis(50);
            assert_eq!(query_background(input.as_fd(), &mut Vec::new(), wait), None);
            let _terminal = typing.join().unwrap();
            assert_eq!(rest(&input), b"");
        }
    }

    #[test]
    fn without_an_answer_to_osc_11_the_wait_ends_with_da1() {
        let start = Instant::now();
        assert_eq!(background(DA1, Duration::from_secs(10)).0, None);
        assert!(start.elapsed() < Duration::from_secs(5));
        // A terminal that answers nothing gives no color at the end of the
        // wait.
        assert_eq!(background(b"", Duration::from_millis(50)).0, None);
    }

    #[test]
    fn the_answer_to_osc_11_has_1_to_4_hex_digits_per_color() {
        let parse = |answer: &str| parse_background(format!("\x1b]11;{answer}\x07").as_bytes());
        assert_eq!(parse("rgb:f/8/0"), Some((255, 136, 0)));
        assert_eq!(parse("rgb:fff/800/000"), Some((255, 128, 0)));
        assert_eq!(parse("rgb:FFFF/0000/7fff"), Some((255, 0, 127)));
        for answer in [
            "rgb:28/28",
            "rgb:28/28/28/28",
            "rgb:12345/0/0",
            "rgb:/0/0",
            "rgb:+f/0/0",
            "rgb:zz/0/0",
            "rgba:28/28/28/ff",
            "#282828",
        ] {
            assert_eq!(parse(answer), None, "{answer}");
        }
        // An OSC 11 after the answer to DA1 is not an answer.
        assert_eq!(
            parse_background(&[DA1, b"\x1b]11;rgb:0/0/0\x07"].concat()),
            None
        );
    }

    proptest::proptest! {
        #[test]
        fn any_bytes_parse_without_a_panic(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..64)) {
            let _ = parse_background(&bytes);
            let answer = [b"\x1b]11;rgb:".as_slice(), &bytes, b"\x07"].concat();
            let _ = parse_background(&answer);
        }
    }

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    fn open(file: &str, log: Option<&str>) -> Result<Command, String> {
        Ok(Command::Open(Args {
            file: file.into(),
            log: log.map(PathBuf::from),
        }))
    }

    #[test]
    fn the_log_defaults_to_the_file_with_the_extension_log() {
        assert_eq!(parse(&["a/run.md"]), open("a/run.md", Some("a/run.log")));
        assert_eq!(parse(&["run"]), open("run", Some("run.log")));
    }

    #[test]
    fn log_options_set_or_remove_the_log() {
        assert_eq!(
            parse(&["--log", "x.txt", "r.md"]),
            open("r.md", Some("x.txt"))
        );
        assert_eq!(parse(&["r.md", "--log=x.txt"]), open("r.md", Some("x.txt")));
        assert_eq!(parse(&["--no-log", "r.md"]), open("r.md", None));
    }

    #[test]
    fn log_paths_keep_bytes_that_are_not_utf_8() {
        let path = OsStr::from_bytes(b"/tmp/\xff.log");
        let joined = OsStr::from_bytes(b"--log=/tmp/\xff.log");
        for args in [vec![OsStr::new("--log"), path], vec![joined]] {
            let args = args
                .into_iter()
                .chain([OsStr::new("r.md")])
                .map(OsString::from);
            let Ok(Command::Open(parsed)) = parse_args(args) else {
                panic!("not parsed");
            };
            assert_eq!(parsed.log.as_deref(), Some(std::path::Path::new(path)));
        }
    }

    #[test]
    fn help_and_version_win_over_the_arguments_after_them() {
        assert_eq!(
            parse(&["r.md", "--bad", "-h"]),
            Err("unknown option '--bad'".into())
        );
        assert_eq!(parse(&["r.md", "--help", "--bad"]), Ok(Command::Help));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--version", "r.md"]), Ok(Command::Version));
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        assert_eq!(parse(&["--", "-r.md"]), open("-r.md", Some("-r.log")));
        assert_eq!(parse(&["-"]), open("-", Some("-.log")));
    }

    #[test]
    fn invalid_arguments_are_errors() {
        let cases: &[(&[&str], &str)] = &[
            (&[], "the FILE argument is missing"),
            (&["--no-log"], "the FILE argument is missing"),
            (&["a.md", "b.md"], "unexpected argument 'b.md'"),
            (&["-x", "a.md"], "unknown option '-x'"),
            (&["a.md", "--log"], "--log needs a PATH"),
            (&["a.md", "--log", ""], "--log needs a PATH"),
            (&["a.md", "--log="], "--log needs a PATH"),
            (
                &["--log", "x", "--log", "y", "a.md"],
                "--log is given twice",
            ),
            (
                &["--log", "x", "--no-log", "a.md"],
                "use --log or --no-log, not both",
            ),
        ];
        for (args, error) in cases {
            assert_eq!(parse(args), Err(error.to_string()), "{args:?}");
        }
    }

    #[test]
    fn keys_map_to_the_actions_of_the_document_view() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let cases = [
            (key(KeyCode::Char('q')), Some(Action::Quit)),
            (ctrl('c'), Some(Action::Quit)),
            (key(KeyCode::Char('c')), None),
            (key(KeyCode::Char('j')), Some(Action::Select(1))),
            (key(KeyCode::Down), Some(Action::Select(1))),
            (key(KeyCode::Char('k')), Some(Action::Select(-1))),
            (key(KeyCode::Up), Some(Action::Select(-1))),
            (key(KeyCode::Home), Some(Action::First)),
            (key(KeyCode::End), Some(Action::Last)),
            (key(KeyCode::PageDown), Some(Action::Page(1))),
            (key(KeyCode::PageUp), Some(Action::Page(-1))),
            (key(KeyCode::Enter), Some(Action::Run)),
            (key(KeyCode::Char('i')), Some(Action::Input)),
            (key(KeyCode::Char('o')), Some(Action::Output)),
            (ctrl('o'), None),
            (key(KeyCode::Char('x')), None),
        ];
        for (key, expected) in cases {
            assert_eq!(action(&Event::Key(key), SIZE), expected, "{key:?}");
        }
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        assert_eq!(action(&Event::Key(key), SIZE), None);
    }

    #[test]
    fn a_paste_does_nothing_in_the_document_view() {
        assert_eq!(action(&Event::Paste("\r\n".into()), SIZE), None);
    }

    #[test]
    fn a_screen_too_small_for_the_view_takes_only_the_keys_that_quit() {
        let key = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        for size in [Size::new(39, 8), Size::new(40, 7)] {
            assert_eq!(action(&key('q'), size), Some(Action::Quit));
            assert_eq!(action(&key('j'), size), None);
        }
        assert_eq!(action(&key('j'), Size::new(40, 8)), Some(Action::Select(1)));
    }

    #[test]
    fn io_messages_lose_the_os_error_number() {
        let error = std::io::Error::from_raw_os_error(2);
        assert_eq!(io_message(&error), "No such file or directory");
    }

    fn app(markdown: &str, tmp: &Path) -> App {
        let workspace = run::Workspace::create(tmp).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        let bash = run::find_in_path("bash", &path).unwrap();
        let state = run::State::new(tmp.into(), [("PATH".into(), path)]);
        let session = run::Session::new(workspace, bash, state, tmp.join("run.md"), None);
        let view = view::View::new(doc::parse(markdown), "run.md".into());
        App::new(view, session.unwrap(), crossbeam_channel::never())
    }

    fn enter(app: &mut App, size: Size) {
        let key = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.handle(&key, size), None);
    }

    fn key(app: &mut App, code: KeyCode) {
        let key = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(app.handle(&key, SIZE), None);
    }

    #[test]
    fn enter_on_a_block_that_has_run_asks_before_it_runs_it_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash [name:count]\necho run >> runs\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        enter(&mut app, SIZE);
        assert_eq!(app.view.question.as_deref(), Some("Re-run count?"));
        // A resize is no answer, and every key but y says no.
        assert_eq!(app.handle(&Event::Resize(80, 24), SIZE), None);
        assert!(app.question.is_some());
        // A screen too small for the view does not show the question.
        let y = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(app.handle(&y, Size::new(30, 6)), None);
        assert!(app.question.is_some() && !running(&app));
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.question.as_ref(), app.view.question.as_ref()),
            (None, None)
        );
        assert!(!running(&app));
        enter(&mut app, SIZE);
        key(&mut app, KeyCode::Char('y'));
        assert_eq!(app.view.output(0), []);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("runs")).unwrap(),
            "run\nrun\n"
        );
    }

    #[test]
    fn a_key_release_answers_no_question() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        enter(&mut app, SIZE);
        let mut release = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle(&Event::Key(release), SIZE), None);
        assert_eq!(app.question, Some(Question::Rerun(0)));
    }

    #[test]
    fn y_shows_a_block_that_a_resize_hid_and_asks_again() {
        let tmp = tempfile::tempdir().unwrap();
        let steps = "Step.\n\n".repeat(30);
        let mut app = app(&format!("{steps}```bash\ntrue\n```\n"), tmp.path());
        app.view.select_last(SIZE);
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        enter(&mut app, SIZE);
        // The view scrolls back to the top, as after a resize.
        app.view.scroll = 0;
        key(&mut app, KeyCode::Char('y'));
        assert!(app.question.is_some() && !running(&app) && app.view.scroll > 0);
        key(&mut app, KeyCode::Char('y'));
        assert!(app.question.is_none() && running(&app));
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn enter_on_a_block_off_the_screen_only_shows_it() {
        let tmp = tempfile::tempdir().unwrap();
        let steps = "Step.\n\n".repeat(30);
        let mut app = app(&format!("{steps}```bash\ntrue\n```\n"), tmp.path());
        assert_eq!((app.view.selected, app.view.scroll), (Some(0), 0));
        enter(&mut app, SIZE);
        assert!(!running(&app) && app.view.scroll > 0);
        enter(&mut app, SIZE);
        assert!(running(&app));
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn enter_and_the_answers_act_on_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        let press = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.acts(&press(KeyCode::Enter), SIZE));
        assert!(!app.acts(&press(KeyCode::Char('j')), SIZE));
        assert!(!app.acts(&Event::Resize(80, 24), SIZE));
        app.question = Some(Question::Quit);
        assert!(app.acts(&press(KeyCode::Char('j')), SIZE));
        let mut release = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!app.acts(&Event::Key(release), SIZE));
    }

    #[test]
    fn enter_says_why_a_block_does_not_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash [name:a]\nread\n```\n\n```bash [name:b]\n```\n\n```bash [name:b]\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        enter(&mut app, SIZE);
        assert_eq!(
            app.view.notice.as_deref(),
            Some("already running: i to type, o to view")
        );
        key(&mut app, KeyCode::Down);
        enter(&mut app, SIZE);
        assert_eq!(
            app.view.notice.as_deref(),
            Some("cannot run: duplicate name")
        );
        // The notification ends after 3 seconds, when the loop wakes up.
        let until = app.notice_until.unwrap();
        assert!(until > Instant::now() + Duration::from_millis(2_900));
        assert_eq!(app.deadline(), Some(until));
        app.expire();
        assert!(app.view.notice.is_some());
        app.notice_until = Some(Instant::now());
        app.expire();
        assert_eq!((app.view.notice.as_ref(), app.deadline()), (None, None));
        let ran = &app.ran[&0];
        ran.run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn input_mode_sends_the_keys_to_the_program_until_its_run_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash\nread -rs -p 'Password: ' pw\necho\necho \"got $pw\"\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("Password:"));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.view.mode, view::Mode::Input(0));
        // The keys of the document view go to the program too, o
        // included: only ctrl-o leaves.
        for c in "qj ox".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(output(&app, 0), "Password:\ngot qj ox");
        assert_eq!(app.view.mode, view::Mode::Normal);
    }

    #[test]
    fn ctrl_o_leaves_input_mode_and_i_needs_a_running_block() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\n```\n", tmp.path());
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.view.mode, view::Mode::Normal);
        assert_eq!(app.view.notice.as_deref(), Some("not running: ↵ runs it"));
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.handle(&ctrl('o'), SIZE), None);
        assert_eq!(app.view.mode, view::Mode::Normal);
        // On a screen too small for the view, a key leaves input mode, and
        // goes nowhere.
        key(&mut app, KeyCode::Char('i'));
        let small = Size::new(30, 6);
        assert_eq!(app.handle(&Event::Resize(30, 6), small), None);
        let enter_key = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.handle(&enter_key, small), None);
        assert_eq!(app.view.mode, view::Mode::Normal);
        std::thread::sleep(Duration::from_millis(100));
        assert!(running(&app));
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn the_arrows_follow_the_application_cursor_mode_of_the_program() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash\nprintf '\\e[?1h'\necho ready\nIFS= read -rsn3 k\nprintf '%q\\n' \"$k\"\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        key(&mut app, KeyCode::Up);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(output(&app, 0), "ready\n$'\\EOA'");
    }

    #[test]
    fn a_paste_in_input_mode_reaches_the_program_between_the_marks_it_asked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash\nprintf '\\e[?2004h'\necho ready\nIFS= read -rsN15 k\nprintf '%q\\n' \"$k\"\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.handle(&Event::Paste("a\tb".into()), SIZE), None);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(output(&app, 0), "ready\n$'\\E[200~a\\tb\\E[201~'");
    }

    fn full(app: &App) -> &view::Full {
        match &app.view.mode {
            view::Mode::Full(full) => full,
            mode => panic!("not in the full-screen view: {mode:?}"),
        }
    }

    /// Makes the rows of the full-screen view until `done`. While the
    /// reader of the run holds its terminal, the rows come later.
    fn until_full_rows(app: &mut App, done: impl Fn(&[Line]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&full(app).rows.lines) {
            assert!(Instant::now() < deadline, "no rows");
            app.refresh(SIZE);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_full_screen_view_of_a_running_block_has_the_size_of_the_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\necho ready\nread\nstty size\nread\nstty size\n```\n";
        let mut app = app(block, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('o'));
        // The rows of the screen, while the block runs.
        until_full_rows(&mut app, |lines| lines.len() == 23);
        // The keys go to the program.
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| {
            markup(&full(app).rows.lines).contains("23 80")
        });
        // ctrl-o gives the PTY its size in the document view back.
        assert_eq!(app.handle(&ctrl('o'), SIZE), None);
        assert_eq!(app.view.mode, view::Mode::Normal);
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
        let (columns, rows) = app.view.pty_size(0, SIZE);
        assert_eq!(
            output(&app, 0).lines().last(),
            Some(&*format!("{rows} {columns}"))
        );
    }

    #[test]
    fn a_resize_gives_the_block_of_the_full_screen_view_the_new_screen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\nstty size\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('o'));
        let size = Size::new(100, 30);
        assert_eq!(app.handle(&Event::Resize(100, 30), size), None);
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, size, |app| !running(app));
        // After the run, the view stays, with the scrollback and the screen.
        assert_eq!(markup(&full(&app).rows.lines), "ready\n\n29 100");
    }

    #[test]
    fn after_the_run_the_keys_of_the_full_screen_view_scroll_or_go_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\nseq 100\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        // Long after the end of the run, as the keys pause just after it.
        app.ran.get_mut(&0).unwrap().ended = Some(Instant::now() - Duration::from_secs(5));
        let scroll = |app: &mut App, code| {
            key(app, code);
            full(app).scroll
        };
        key(&mut app, KeyCode::Char('o'));
        app.refresh(SIZE);
        assert_eq!(full(&app).rows.lines.len(), 100);
        // The screen shows 23 lines: the scroll stops at line 1.
        assert_eq!(scroll(&mut app, KeyCode::Up), 1);
        assert_eq!(scroll(&mut app, KeyCode::Char('k')), 2);
        assert_eq!(scroll(&mut app, KeyCode::Char('j')), 1);
        assert_eq!(scroll(&mut app, KeyCode::PageUp), 24);
        assert_eq!(scroll(&mut app, KeyCode::PageDown), 1);
        assert_eq!(scroll(&mut app, KeyCode::Down), 0);
        assert_eq!(scroll(&mut app, KeyCode::Down), 0);
        assert_eq!(scroll(&mut app, KeyCode::Home), 77);
        assert_eq!(scroll(&mut app, KeyCode::Up), 77);
        assert_eq!(scroll(&mut app, KeyCode::End), 0);
        assert_eq!(scroll(&mut app, KeyCode::Char('c')), 0);
        let back = [
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ctrl('o'),
            ctrl('c'),
        ];
        for event in back {
            assert!(matches!(app.view.mode, view::Mode::Full(_)));
            assert_eq!(app.handle(&event, SIZE), None);
            assert_eq!(app.view.mode, view::Mode::Normal, "{event:?}");
            key(&mut app, KeyCode::Char('o'));
        }
    }

    /// Reads the messages of the runs until the mode of the view is
    /// `mode`, or until the full-screen view if `mode` is `None`.
    fn until_mode(app: &mut App, mode: Option<view::Mode>) {
        read_until(app, SIZE, |app| match &mode {
            Some(mode) => app.view.mode == *mode,
            None => matches!(app.view.mode, view::Mode::Full(_)),
        });
    }

    /// A block that waits for a line, enters the alternate screen, waits
    /// for a line, leaves it, then waits for a line that it prints.
    const FULL_SCREEN: &str = "```bash
echo before
read
printf '\\e[?1049h'
echo inside
read
printf '\\e[?1049l'
read x
echo \"got $x\"
```
";

    #[test]
    fn a_full_screen_program_shows_after_o_until_it_leaves() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(FULL_SCREEN, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("before"));
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| app.view.alternate(0));
        // The keys go to a program only after i or o. Under the block,
        // the rows from before the switch stay.
        assert_eq!(app.view.mode, view::Mode::Normal);
        let under = output(&app, 0);
        assert!(
            under.starts_with("before") && !under.contains("inside"),
            "{under}"
        );
        key(&mut app, KeyCode::Char('o'));
        until_full_rows(&mut app, |lines| markup(lines).starts_with("inside\n"));
        key(&mut app, KeyCode::Enter);
        until_mode(&mut app, Some(view::Mode::Normal));
        app.ran[&0].run.as_ref().unwrap().write(b"y\n");
        read_until(&mut app, SIZE, |app| !running(app));
        // The terminal echoes the lines that the block read.
        assert_eq!(output(&app, 0), "before\n\ny\ngot y");
    }

    #[test]
    fn from_input_mode_a_full_screen_program_goes_back_to_input_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(FULL_SCREEN, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("before"));
        key(&mut app, KeyCode::Char('i'));
        key(&mut app, KeyCode::Enter);
        until_mode(&mut app, None);
        key(&mut app, KeyCode::Enter);
        until_mode(&mut app, Some(view::Mode::Input(0)));
        key(&mut app, KeyCode::Char('y'));
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| !running(app));
        // The terminal echoes the lines that the keys typed.
        assert_eq!(output(&app, 0), "before\n\ny\ngot y");
    }

    #[test]
    fn after_ctrl_o_o_or_i_shows_the_full_screen_program_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(FULL_SCREEN, tmp.path());
        enter(&mut app, SIZE);
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| app.view.alternate(0));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(full(&app).back, Some(view::Back::Input));
        assert_eq!(app.handle(&ctrl('o'), SIZE), None);
        assert_eq!(app.view.mode, view::Mode::Normal);
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(full(&app).back, Some(view::Back::Normal));
        key(&mut app, KeyCode::Enter);
        until_mode(&mut app, Some(view::Mode::Normal));
        // Keys do nothing for a moment, until the pause ends.
        assert!(app.paused_until.is_some());
        app.paused_until = None;
        // Opened on the main screen, the view stays when the program
        // enters and leaves the alternate screen.
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(full(&app).back, None);
        app.ran[&0].run.as_ref().unwrap().write(b"y\n");
        read_until(&mut app, SIZE, |app| !running(app));
        assert!(matches!(app.view.mode, view::Mode::Full(_)));
    }

    #[test]
    fn a_switch_of_another_block_leaves_input_mode_as_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(&format!("{FULL_SCREEN}\n{FULL_SCREEN}"), tmp.path());
        enter(&mut app, SIZE);
        key(&mut app, KeyCode::Down);
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 1).contains("before"));
        let write = |app: &App, i, bytes: &[u8]| app.ran[&i].run.as_ref().unwrap().write(bytes);
        // The keys go to block 1, and block 0 enters, then leaves, the
        // alternate screen.
        key(&mut app, KeyCode::Char('i'));
        write(&app, 0, b"\n");
        read_until(&mut app, SIZE, |app| app.view.alternate(0));
        assert_eq!(app.view.mode, view::Mode::Input(1));
        write(&app, 0, b"\n");
        read_until(&mut app, SIZE, |app| !app.view.alternate(0));
        assert_eq!(app.view.mode, view::Mode::Input(1));
        write(&app, 0, b"y\n");
        write(&app, 1, b"\n\ny\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn after_a_run_ends_in_input_mode_keys_do_nothing_for_a_moment() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(app.view.mode, view::Mode::Normal);
        let notice = "keys off for 1 s: #1 no longer takes them";
        assert_eq!(app.view.notice.as_deref(), Some(notice));
        let until = app.paused_until.unwrap();
        assert!(until > Instant::now() + Duration::from_millis(900));
        assert_eq!(app.deadline(), Some(until));
        // A second ctrl-c, typed for the program, does not quit.
        assert_eq!(app.handle(&ctrl('c'), SIZE), None);
        app.paused_until = Some(Instant::now());
        app.expire();
        assert_eq!(app.paused_until, None);
        assert_eq!(app.handle(&ctrl('c'), SIZE), Some(Exit::Quit));
    }

    #[test]
    fn after_a_run_ends_in_its_full_screen_view_keys_do_nothing_for_a_moment() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('o'));
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| !running(app));
        key(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.view.mode, view::Mode::Full(_)));
        app.paused_until = None;
        key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.view.mode, view::Mode::Normal);
    }

    #[test]
    fn the_full_screen_view_of_a_run_that_ends_on_the_alternate_screen_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\necho before\nprintf '\\e[?1049h'\necho crash\nread\n```\n";
        let mut app = app(block, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| app.view.alternate(0));
        key(&mut app, KeyCode::Char('o'));
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
        let mark = "«d:[stepbook: the last screen of the full-screen program]»";
        let lines = markup(&full(&app).rows.lines);
        assert!(
            lines.starts_with(&format!("before\n{mark}\ncrash")),
            "{lines}"
        );
        // Back in the document view, the main screen shows under the block.
        app.paused_until = None;
        key(&mut app, KeyCode::Char('q'));
        app.refresh(SIZE);
        assert_eq!(output(&app, 0), "before");
    }

    #[test]
    fn the_rows_that_the_full_screen_view_hid_show_when_it_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let first = "```bash\necho a\nread\necho b\n```\n";
        let mut app = app(&format!("{first}\n{FULL_SCREEN}"), tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains('a'));
        key(&mut app, KeyCode::Down);
        enter(&mut app, SIZE);
        app.ran[&1].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| app.view.alternate(1));
        key(&mut app, KeyCode::Char('o'));
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !app.view.running(0));
        assert!(!output(&app, 0).contains('b'));
        // The refresh that closes the view makes the rows that it hid.
        app.ran[&1].run.as_ref().unwrap().write(b"\n");
        until_mode(&mut app, Some(view::Mode::Normal));
        assert_eq!(output(&app, 0), "a\n\nb");
        app.ran[&1].run.as_ref().unwrap().write(b"y\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn a_program_that_leaves_the_alternate_screen_on_a_tiny_screen_keeps_its_pty() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\necho before\nread\nprintf '\\e[?1049h'\nread\nprintf '\\e[?1049l'\nstty size\nread\n```\n";
        let mut app = app(block, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("before"));
        key(&mut app, KeyCode::Char('i'));
        key(&mut app, KeyCode::Enter);
        until_mode(&mut app, None);
        let tiny = Size::new(80, 3);
        assert_eq!(app.handle(&Event::Resize(80, 3), tiny), None);
        // The program leaves by itself. On a screen too small for the view,
        // nothing lays the document out: the PTY keeps the size of the
        // full-screen view.
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, tiny, |app| output(app, 0).contains("23 80"));
        assert_eq!(app.view.mode, view::Mode::Input(0));
        app.ran[&0].run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, tiny, |app| !running(app));
    }

    #[test]
    fn o_needs_a_block_that_has_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        key(&mut app, KeyCode::Char('o'));
        assert_eq!(app.view.mode, view::Mode::Normal);
        assert_eq!(app.view.notice.as_deref(), Some("no output: ↵ runs it"));
    }

    #[test]
    fn a_paste_of_several_lines_to_a_program_without_bracketed_paste_asks_first() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\nstty -echo\necho ready\nread a\nread b\necho \"$a+$b\"\n```\n";
        let mut app = app(block, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        let paste = |app: &mut App, text: &str| {
            assert_eq!(app.handle(&Event::Paste(text.into()), SIZE), None);
        };
        // One line with no line feed goes at once. A control character,
        // such as ctrl-o, which readline takes for Enter, asks first.
        paste(&mut app, "\x0fz");
        assert_eq!(app.view.question.as_deref(), Some("Paste 1 line to #1?"));
        key(&mut app, KeyCode::Char('n'));
        paste(&mut app, "x\t");
        paste(&mut app, "1\r2\r");
        let question = "Paste 2 lines to #1?";
        assert_eq!(app.view.question.as_deref(), Some(question));
        // The answer waits for the frame that shows the question.
        let y = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.acts(&y, SIZE));
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.view.mode, view::Mode::Input(0));
        paste(&mut app, "y\r");
        let question = "Paste 1 line to #1?";
        assert_eq!(app.view.question.as_deref(), Some(question));
        assert_eq!(app.handle(&y, SIZE), None);
        key(&mut app, KeyCode::Char('z'));
        key(&mut app, KeyCode::Enter);
        read_until(&mut app, SIZE, |app| !running(app));
        // The terminal draws the tab of the first line as spaces.
        let line = format!("x{}y+z", " ".repeat(7));
        assert!(output(&app, 0).ends_with(&line), "{}", output(&app, 0));
    }

    #[test]
    fn the_question_of_a_paste_ends_with_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\necho ready\nread -t 1\n```\n";
        let mut app = app(block, tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.handle(&Event::Paste("1\r2\r".into()), SIZE), None);
        assert!(app.question.is_some());
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!((app.question, app.view.question.as_ref()), (None, None));
    }

    #[test]
    fn i_or_o_just_after_the_end_of_the_run_pause_the_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.view.mode, view::Mode::Normal);
        assert!(app.paused_until.is_some());
        app.paused_until = None;
        key(&mut app, KeyCode::Char('o'));
        assert!(matches!(app.view.mode, view::Mode::Full(_)) && app.paused_until.is_some());
        app.paused_until = None;
        key(&mut app, KeyCode::Char('q'));
        // Long after the end, i only says why it does nothing.
        app.ran.get_mut(&0).unwrap().ended = Some(Instant::now() - Duration::from_secs(5));
        key(&mut app, KeyCode::Char('i'));
        assert_eq!(app.paused_until, None);
        assert_eq!(app.view.notice.as_deref(), Some("not running: ↵ runs it"));
    }

    #[test]
    fn enter_and_i_wait_for_the_frame_but_keys_of_input_mode_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        let press = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.acts(&press(KeyCode::Char('i')), SIZE));
        assert!(app.acts(&press(KeyCode::Char('o')), SIZE));
        app.view.mode = view::Mode::Input(0);
        assert!(!app.acts(&press(KeyCode::Enter), SIZE));
    }

    #[test]
    fn q_quits_at_once_without_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\ntrue\n```\n", tmp.path());
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.handle(&q, SIZE), Some(Exit::Quit));
    }

    #[test]
    fn quit_asks_then_stops_the_runs_with_sighup() {
        let tmp = tempfile::tempdir().unwrap();
        let block = "```bash\necho ready\nsleep 30\n```\n\n";
        let mut app = app(&block.repeat(2), tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('q'));
        assert_eq!(
            app.view.question.as_deref(),
            Some("1 block running. Stop it and quit?")
        );
        key(&mut app, KeyCode::Char('n'));
        key(&mut app, KeyCode::Down);
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 1).contains("ready"));
        key(&mut app, KeyCode::Char('q'));
        let question = "2 blocks running. Stop them and quit?";
        assert_eq!(app.view.question.as_deref(), Some(question));
        key(&mut app, KeyCode::Char('y'));
        assert!(!app.stopped());
        // Keys do nothing while the runs stop.
        key(&mut app, KeyCode::Up);
        assert_eq!(app.view.selected, Some(1));
        read_until(&mut app, SIZE, |app| !running(app));
        assert!(app.stopped());
        for i in 0..2 {
            let status = app.view.status(i);
            assert!(
                matches!(
                    status,
                    Some(view::Status::Ended {
                        status: run::Status::Signal(1),
                        ..
                    })
                ),
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_run_that_ignores_sighup_gets_sigkill_after_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash\ntrap '' HUP\necho ready\nsleep 30\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        key(&mut app, KeyCode::Char('q'));
        key(&mut app, KeyCode::Char('y'));
        std::thread::sleep(Duration::from_millis(200));
        assert!(running(&app));
        app.stopping = Some(Instant::now());
        assert!(app.stopped());
        read_until(&mut app, SIZE, |app| !running(app));
        let status = app.view.status(0);
        assert!(
            matches!(
                status,
                Some(view::Status::Ended {
                    status: run::Status::Signal(9),
                    ..
                })
            ),
            "{status:?}"
        );
    }

    fn running(app: &App) -> bool {
        app.runs().next().is_some()
    }

    /// Reads the messages of the runs until `done`.
    fn read_until(app: &mut App, size: Size, done: impl Fn(&App) -> bool) {
        let (_sender, control) = crossbeam_channel::unbounded();
        // A bug that loses an event fails the test, and does not hang it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(app) {
            let Some(Ok(Incoming::Run(i, event))) = app.next(&control, Some(deadline)) else {
                panic!("no message of a run");
            };
            app.read(i, event);
            app.refresh(size);
        }
    }

    fn output(app: &App, i: usize) -> String {
        markup(app.view.output(i))
    }

    use ratatui::text::Line;
    use view::tests::markup;

    #[test]
    fn enter_runs_the_selected_block_and_shows_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho hi\nexport A=1\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        assert_eq!(output(&app, 0), "hi");
        let a = app.session.state.vars.get(OsStr::new("A"));
        assert_eq!(a.map(|a| a.to_str().unwrap()), Some("1"));
        let Some(view::Status::Ended { status, saved, .. }) = app.view.status(0) else {
            panic!("not ended: {:?}", app.view.status(0));
        };
        assert_eq!((*status, *saved), (run::Status::Code(0), true));
    }

    #[test]
    fn a_run_shows_its_exit_code_and_whether_it_saved_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\nexec bash -c 'kill -INT $$'\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| !running(app));
        let Some(view::Status::Ended { status, saved, .. }) = app.view.status(0) else {
            panic!("not ended: {:?}", app.view.status(0));
        };
        assert_eq!((*status, *saved), (run::Status::Signal(2), false));
    }

    #[test]
    fn a_run_that_cannot_start_shows_why_and_can_run_again() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash
true
```
",
            tmp.path(),
        );
        app.session.state.dir = tmp.path().join("gone");
        enter(&mut app, SIZE);
        let reason = format!(
            "cannot start: directory {} no longer exists",
            tmp.path().join("gone").display()
        );
        assert_eq!(app.view.status(0), Some(&view::Status::NotStarted(reason)));
        let notice = format!("session directory reset to {}", tmp.path().display());
        assert_eq!(app.view.notice, Some(notice));
        enter(&mut app, SIZE);
        key(&mut app, KeyCode::Char('y'));
        read_until(&mut app, SIZE, |app| !running(app));
        assert!(matches!(
            app.view.status(0),
            Some(view::Status::Ended {
                status: run::Status::Code(0),
                ..
            })
        ));
    }

    #[test]
    fn a_tick_updates_the_running_blocks_and_blinks_their_glyph() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash
echo ready
read
```
",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        app.ran.get_mut(&0).unwrap().started -= Duration::from_secs(5);
        app.tick();
        assert!(!app.view.blink);
        let Some(view::Status::Running(took)) = app.view.status(0) else {
            panic!("not running: {:?}", app.view.status(0));
        };
        assert!(*took >= Duration::from_secs(5));
        let ran = &app.ran[&0];
        ran.run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn enter_on_a_running_block_starts_no_second_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        enter(&mut app, SIZE);
        assert!(!app.session.workspace().join("run-2.sh").exists());
        let ran = &app.ran[&0];
        ran.run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, SIZE, |app| !running(app));
    }

    #[test]
    fn a_block_that_cannot_run_does_not_start() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app(
            "```bash [name:a]\n```\n\n```bash [name:a]\n```\n",
            tmp.path(),
        );
        enter(&mut app, SIZE);
        assert!(app.ran.is_empty());
    }

    #[test]
    fn a_resize_gives_a_running_block_a_pty_of_its_new_size() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app("```bash\necho ready\nread\nstty size\n```\n", tmp.path());
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 0).contains("ready"));
        let size = Size::new(100, 30);
        assert_eq!(app.handle(&Event::Resize(100, 30), size), None);
        let ran = &app.ran[&0];
        ran.run.as_ref().unwrap().write(b"\n");
        read_until(&mut app, size, |app| !running(app));
        let (columns, rows) = app.view.pty_size(0, size);
        assert_eq!(
            output(&app, 0).lines().last(),
            Some(&*format!("{rows} {columns}"))
        );
    }
}
