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
