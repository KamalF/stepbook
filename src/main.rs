mod doc;
mod run;
mod term;
mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Select, Sender};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, DisableLineWrap, EnableLineWrap,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Size;
use ratatui::{DefaultTerminal, Terminal};
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
    let title = args.file.file_name().unwrap_or(args.file.as_os_str());
    let view = view::View::new(doc, title.to_string_lossy().into_owned());
    let mut app = App::new(view, session, crossbeam_channel::tick(TICK));
    std::thread::spawn(move || read_terminal(sender));
    let result = event_loop(&mut terminal, &mut app, &control);
    run::end_runs();
    restore_terminal();
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

/// Ignores errors: after a SIGHUP, the terminal is gone.
fn restore_terminal() {
    let _ = execute!(
        std::io::stdout(),
        EndSynchronizedUpdate,
        EnableLineWrap,
        DisableBracketedPaste
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
}

/// How long the runs have to end after SIGHUP, when stepbook quits (spec
/// 11).
const STOP_WAIT: Duration = Duration::from_secs(2);

/// How long a notification shows (spec 6.1).
const NOTICE: Duration = Duration::from_secs(3);

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
    /// notification, and when the runs that stop get SIGKILL.
    fn deadline(&self) -> Option<Instant> {
        self.notice_until.into_iter().chain(self.stopping).min()
    }

    /// Ends the notification after its time.
    fn expire(&mut self) {
        if self
            .notice_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.notice_until = None;
            self.view.notice = None;
        }
    }

    /// Applies a terminal event, and returns how the loop ends, if it does.
    fn handle(&mut self, event: &Event, size: Size) -> Option<Exit> {
        if let Event::Resize(..) = event {
            self.resize(size);
        }
        if self.stopping.is_some() {
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
                        Question::Rerun(_) | Question::Quit => {}
                    }
                }
                _ => self.question = Some(question),
            }
            return None;
        }
        match action(event, size)? {
            Action::Quit => match self.runs().count() {
                0 => return Some(Exit::Quit),
                1 => self.ask(
                    Question::Quit,
                    "1 block running. Stop it and quit? (y/N)".into(),
                ),
                n => self.ask(
                    Question::Quit,
                    format!("{n} blocks running. Stop them and quit? (y/N)"),
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
        }
        None
    }

    /// Whether a key starts or stops runs: `Enter`, or the answer to a
    /// question.
    fn acts(&self, event: &Event, size: Size) -> bool {
        let Event::Key(key) = event else {
            return false;
        };
        let run = action(event, size) == Some(Action::Run);
        key.kind == KeyEventKind::Press && (self.question.is_some() || run)
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
        self.ask(Question::Rerun(i), format!("Re-run {label}? (y/N)"));
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
        self.view.set_output(i, Vec::new(), size);
    }

    /// Reads a message from the run of block `i`. Its rows are made before
    /// the next frame.
    fn read(&mut self, i: usize, event: run::Event) {
        let ran = self.ran.get_mut(&i).expect("a message comes from a run");
        match event {
            run::Event::Output(_) => {}
            run::Event::Exit(status) => {
                let run = ran.run.take().expect("a run sends its exit status once");
                let saved = self.session.finish(&run);
                let took = ran.started.elapsed();
                let ended = view::Status::Ended {
                    status,
                    took,
                    saved,
                };
                self.view.set_status(i, ended);
            }
        }
        self.changed.insert(i);
    }

    /// Makes the rows of the output that changed (spec 6.3). A terminal that
    /// its reader holds gives its rows at a later frame: the reader sends an
    /// event after it.
    fn refresh(&mut self, size: Size) {
        for i in std::mem::take(&mut self.changed) {
            let Some(ran) = self.ran.get(&i) else {
                continue;
            };
            let want = term::Want::Last {
                cursor: ran.run.is_some(),
            };
            match ran.screen.try_rows(want) {
                Some(lines) => self.view.set_output(i, lines, size),
                None => _ = self.changed.insert(i),
            }
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
        if view::too_small(size) {
            return;
        }
        for (&i, ran) in &mut self.ran {
            if let Some(run) = &ran.run {
                let (columns, rows) = self.view.pty_size(i, size);
                let _ = run.resize(columns, rows);
                ran.screen.resize(columns, rows);
                self.changed.insert(i);
            }
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
        _ => return None,
    };
    (action == Action::Quit || !view::too_small(size)).then_some(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    const SIZE: Size = Size::new(80, 24);

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
        assert_eq!(app.view.question.as_deref(), Some("Re-run count? (y/N)"));
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
        ran.run.as_ref().unwrap().write(b"\n").unwrap();
        read_until(&mut app, SIZE, |app| !running(app));
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
            Some("1 block running. Stop it and quit? (y/N)")
        );
        key(&mut app, KeyCode::Char('n'));
        key(&mut app, KeyCode::Down);
        enter(&mut app, SIZE);
        read_until(&mut app, SIZE, |app| output(app, 1).contains("ready"));
        key(&mut app, KeyCode::Char('q'));
        let question = "2 blocks running. Stop them and quit? (y/N)";
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
        view::tests::markup(app.view.output(i))
    }

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
        ran.run.as_ref().unwrap().write(b"\n").unwrap();
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
        ran.run.as_ref().unwrap().write(b"\n").unwrap();
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
        ran.run.as_ref().unwrap().write(b"\n").unwrap();
        read_until(&mut app, size, |app| !running(app));
        let (columns, rows) = app.view.pty_size(0, size);
        assert_eq!(
            output(&app, 0).lines().last(),
            Some(&*format!("{rows} {columns}"))
        );
    }
}
