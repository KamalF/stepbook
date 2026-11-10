//! The exit code of each kind of startup error. Under `cargo test`, the
//! output of stepbook is a pipe, not a terminal.

use std::process::{Command, Output};

fn stepbook(args: &[&str], path: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_stepbook"));
    command.args(args);
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().unwrap()
}

fn runbook() -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new().suffix(".md").tempfile().unwrap();
    std::fs::write(file.path(), "# Runbook\n").unwrap();
    file
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_exits_with_0() {
    let output = stepbook(&["--help"], None);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: stepbook"));
}

#[test]
fn invalid_arguments_exit_with_2() {
    let output = stepbook(&["--bad"], None);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).starts_with("stepbook: unknown option '--bad'\nUsage:"));
}

#[test]
fn an_unreadable_file_exits_with_1() {
    let output = stepbook(&["/nonexistent/run.md"], None);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "stepbook: cannot read /nonexistent/run.md: No such file or directory\n"
    );
}

#[test]
fn a_file_that_is_not_regular_or_larger_than_1_mb_exits_with_1() {
    let output = stepbook(&["/dev/zero"], None);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        stderr(&output),
        "stepbook: cannot read /dev/zero: not a regular file\n"
    );
    let big = runbook();
    std::fs::write(big.path(), vec![b'a'; 1024 * 1024 + 1]).unwrap();
    let output = stepbook(&[big.path().to_str().unwrap()], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).ends_with(": the file is larger than 1 MB\n"));
    // Exactly 1 MB passes the read, and stops at the check for a terminal.
    std::fs::write(big.path(), vec![b'a'; 1024 * 1024]).unwrap();
    let output = stepbook(&[big.path().to_str().unwrap()], None);
    assert!(stderr(&output).contains("run stepbook in a terminal"));
}

#[test]
fn a_file_that_nests_more_than_1000_levels_exits_with_1() {
    let file = runbook();
    // 600 KB that pulldown-cmark took 30 s to parse.
    let text = format!("{}x\n{}", "> ".repeat(100_000), "a\n".repeat(200_000));
    std::fs::write(file.path(), text).unwrap();
    let output = stepbook(&[file.path().to_str().unwrap()], None);
    assert_eq!(output.status.code(), Some(1));
    let message = ": a line nests more than 1,000 levels of quotes, lists or indent\n";
    assert!(stderr(&output).ends_with(message));
    // 1,000 quote markers pass the read, and stop at the check for a
    // terminal; 1,001 do not.
    std::fs::write(file.path(), format!("{} x\n", ">".repeat(1_000))).unwrap();
    let output = stepbook(&[file.path().to_str().unwrap()], None);
    assert!(stderr(&output).contains("run stepbook in a terminal"));
    std::fs::write(file.path(), format!("{} x\n", ">".repeat(1_001))).unwrap();
    let output = stepbook(&[file.path().to_str().unwrap()], None);
    assert!(stderr(&output).ends_with(message));
}

#[test]
fn bash_missing_from_the_path_exits_with_1() {
    let file = runbook();
    let output = stepbook(&[file.path().to_str().unwrap()], Some(""));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "stepbook: cannot find bash on the PATH\n");
}

#[test]
fn output_that_is_not_a_terminal_exits_with_1() {
    let file = runbook();
    let output = stepbook(&[file.path().to_str().unwrap()], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("run stepbook in a terminal"));
}

#[test]
fn error_messages_show_control_characters_escaped() {
    // A file name from a glob can hold an OSC 52 clipboard write.
    let name = "/nonexistent/a\u{1b}]52;c;eA==\u{7}\u{202e}.md";
    let output = stepbook(&[name], None);
    assert_eq!(
        stderr(&output),
        "stepbook: cannot read /nonexistent/a^[]52;c;eA==^G<202e>.md: No such file or directory\n"
    );
    let output = stepbook(&["a.md", name], None);
    assert!(stderr(&output).starts_with("stepbook: unexpected argument '/nonexistent/a^[]52"));
}
