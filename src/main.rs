mod doc;
mod run;
mod view;

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{DisableLineWrap, EnableLineWrap};

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
                Some(path) => log = Some(set_once(log, PathBuf::from(path))?),
                None => return Err("--log needs a PATH".into()),
            },
            // The bytes of the argument, not its text: a path need not be
            // UTF-8.
            _ => match arg.as_bytes().strip_prefix(b"--log=") {
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
            eprintln!("stepbook: {message}\n{USAGE}\nFor more information, run 'stepbook --help'.");
            return ExitCode::from(2);
        }
    };
    match open(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("stepbook: {}", doc::escaped_text(&message));
            ExitCode::FAILURE
        }
    }
}

fn open(args: &Args) -> Result<(), String> {
    let text = read_runbook(&args.file)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    if run::find_in_path("bash", &path).is_none() {
        return Err("cannot find bash on the PATH".into());
    }
    if !std::io::stdout().is_terminal() {
        return Err("run stepbook in a terminal: its output is not a terminal".into());
    }
    let doc = doc::parse(&text);
    let mut terminal =
        set_up_terminal().map_err(|e| format!("cannot set up the terminal: {}", io_message(&e)))?;
    let title = args.file.file_name().unwrap_or(args.file.as_os_str());
    let mut view = view::View::new(doc, title.to_string_lossy().into_owned());
    let result = event_loop(&mut terminal, &mut view);
    restore_terminal();
    result.map_err(|e| format!("terminal error: {}", io_message(&e)))
}

/// Enters raw mode and the alternate screen, and turns autowrap off: a
/// character that the terminal draws wider than ratatui counts must not
/// wrap into the row below and move what is there. The panic hooks
/// restore all three.
fn set_up_terminal() -> std::io::Result<DefaultTerminal> {
    // try_init installs a panic hook that leaves raw mode and the
    // alternate screen. If it fails, it may have turned on raw mode.
    let terminal = ratatui::try_init().inspect_err(|_| restore_terminal())?;
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), EnableLineWrap);
        ratatui_hook(info);
    }));
    execute!(std::io::stdout(), DisableLineWrap).inspect_err(|_| restore_terminal())?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = execute!(std::io::stdout(), EnableLineWrap);
    ratatui::restore();
}

/// Runbooks are a few KB: the largest of ~/dev/claude/runbooks had 22 KB
/// on 2026-11-09. The view lays out the whole file on each frame.
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

fn event_loop(terminal: &mut DefaultTerminal, view: &mut view::View) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| view.draw(frame))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        let size = terminal.size()?;
        match action(key) {
            Some(Action::Quit) => return Ok(()),
            _ if view::too_small(size) => {}
            Some(Action::Select(step)) => view.select_by(step, size),
            Some(Action::First) => view.select_first(size),
            Some(Action::Last) => view.select_last(size),
            Some(Action::Page(pages)) => view.scroll_pages(pages, size),
            None => {}
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
}

fn action(key: KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('c') if ctrl => Action::Quit,
        KeyCode::Down | KeyCode::Char('j') => Action::Select(1),
        KeyCode::Up | KeyCode::Char('k') => Action::Select(-1),
        KeyCode::Home => Action::First,
        KeyCode::End => Action::Last,
        KeyCode::PageDown => Action::Page(1),
        KeyCode::PageUp => Action::Page(-1),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (key(KeyCode::Char('x')), None),
        ];
        for (key, expected) in cases {
            assert_eq!(action(key), expected, "{key:?}");
        }
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        key.kind = KeyEventKind::Release;
        assert_eq!(action(key), None);
    }

    #[test]
    fn io_messages_lose_the_os_error_number() {
        let error = std::io::Error::from_raw_os_error(2);
        assert_eq!(io_message(&error), "No such file or directory");
    }
}
