#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the layout commit of milestone 1 uses it")
)]
mod doc;
mod run;
mod view;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

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
            _ => match text.strip_prefix("--log=") {
                Some(path) => log = Some(set_once(log, PathBuf::from(path))?),
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
            eprintln!("stepbook: {message}\n{USAGE}\nFor more information, run 'stepbook --help'.");
            return ExitCode::from(2);
        }
    };
    match open(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("stepbook: {message}");
            ExitCode::FAILURE
        }
    }
}

fn open(args: &Args) -> Result<(), String> {
    let bytes = std::fs::read(&args.file)
        .map_err(|e| format!("cannot read {}: {}", args.file.display(), io_message(&e)))?;
    let _text = String::from_utf8_lossy(&bytes);
    let path = std::env::var_os("PATH").unwrap_or_default();
    if run::find_in_path("bash", &path).is_none() {
        return Err("cannot find bash on the PATH".into());
    }
    if !std::io::stdout().is_terminal() {
        return Err("run stepbook in a terminal: its output is not a terminal".into());
    }
    // try_init also installs a panic hook that restores the terminal.
    let mut terminal = ratatui::try_init()
        .map_err(|e| format!("cannot set up the terminal: {}", io_message(&e)))?;
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result.map_err(|e| format!("terminal error: {}", io_message(&e)))
}

fn event_loop(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    loop {
        terminal.draw(view::draw)?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') if ctrl => return Ok(()),
                _ => {}
            }
        }
    }
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
    fn help_and_version_win_over_other_arguments() {
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
    fn io_messages_lose_the_os_error_number() {
        let error = std::io::Error::from_raw_os_error(2);
        assert_eq!(io_message(&error), "No such file or directory");
    }
}
