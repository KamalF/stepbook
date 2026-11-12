//! The log of the runs (spec 12).
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::SystemTime;

use crossbeam_channel::Sender;

/// The line that stands for the output of a run on the alternate screen.
const FULL_SCREEN: &str = "[full-screen program output not logged]\n";

/// What the main thread sends to the thread of the log.
pub enum Message {
    /// Run `id`, of the block named `name`, started at `time`.
    Start {
        id: u64,
        name: String,
        time: SystemTime,
    },
    Output {
        id: u64,
        bytes: Vec<u8>,
    },
    /// Run `id` ended with exit code `rc` at `time`.
    End {
        id: u64,
        rc: i32,
        time: SystemTime,
    },
}

/// Opens the log at `path` to append to it, and creates it with mode 0600
/// (spec 12.1).
pub fn open(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true).mode(0o600).open(path)
}

/// Starts the thread that writes the log to `file`, and returns the sender
/// of its messages. The thread ends once the sender is dropped, and all the
/// messages are written. It calls `failed` on the first write that fails.
pub fn spawn(
    file: File,
    mut failed: impl FnMut(std::io::Error) + Send + 'static,
) -> (Sender<Message>, JoinHandle<()>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let thread = std::thread::spawn(move || {
        let mut sections = Sections::new(file);
        let mut ok = true;
        for message in receiver {
            if let Err(error) = sections.write(message)
                && std::mem::take(&mut ok)
            {
                failed(error);
            }
        }
    });
    (sender, thread)
}

/// The sections of the runs, as the log holds them (spec 12.1).
struct Sections<W> {
    out: W,
    runs: BTreeMap<u64, Section>,
}

struct Section {
    name: String,
    cleaner: Cleaner,
}

impl<W: Write> Sections<W> {
    fn new(out: W) -> Sections<W> {
        Sections {
            out,
            runs: BTreeMap::new(),
        }
    }

    fn write(&mut self, message: Message) -> std::io::Result<()> {
        let text = match message {
            Message::Start { id, name, time } => {
                let line = format!("=== START {name} {}\n", clock(time));
                let cleaner = Cleaner::default();
                self.runs.insert(id, Section { name, cleaner });
                line
            }
            Message::Output { id, bytes } => match self.runs.get_mut(&id) {
                Some(section) => section.cleaner.lines(&bytes),
                None => return Ok(()),
            },
            Message::End { id, rc, time } => match self.runs.remove(&id) {
                Some(mut section) => {
                    let (name, time) = (&section.name, clock(time));
                    let end = format!("=== END {name} rc={rc} {time}\n");
                    section.cleaner.finish() + &end
                }
                None => return Ok(()),
            },
        };
        self.out.write_all(text.as_bytes())
    }
}

/// The time of day of `time`, in UTC: `13:00:30Z` (spec 12.1).
fn clock(time: SystemTime) -> String {
    let since = time.duration_since(SystemTime::UNIX_EPOCH);
    let seconds = since.map_or(0, |since| since.as_secs()) % 86_400;
    let (hours, minutes) = (seconds / 3600, seconds / 60 % 60);
    format!("{hours:02}:{minutes:02}:{:02}Z", seconds % 60)
}

/// Turns the output of a run into the lines of its log (spec 12.1): no
/// escape sequences, LF for CRLF, and U+FFFD for bytes that are not
/// UTF-8. A carriage return alone drops the partial line, as a terminal
/// overwrites it: a progress bar logs only its last state. The parser
/// keeps its state from one read to the next, so a character or a
/// sequence split across two reads comes out whole.
#[derive(Default)]
pub struct Cleaner {
    parser: vte::Parser,
    lines: Lines,
}

#[derive(Default)]
struct Lines {
    /// The lines that ended, each with its line feed.
    done: String,
    /// The line that has not ended yet.
    line: String,
    /// A carriage return came last: the next character starts the line
    /// again, and a line feed ends it as it is.
    carriage_return: bool,
    /// The program is on the alternate screen, whose output the log leaves
    /// out (spec 12.1).
    alternate: bool,
    /// The log has the line that stands for that output.
    marked: bool,
}

impl vte::Perform for Lines {
    fn print(&mut self, c: char) {
        // A C1 control character encoded in UTF-8, which some terminals
        // still act on.
        if self.alternate || c.is_control() && c != '\t' {
            return;
        }
        if std::mem::take(&mut self.carriage_return) {
            self.line.clear();
        }
        self.line.push(c);
    }

    fn execute(&mut self, byte: u8) {
        if self.alternate {
            return;
        }
        match byte {
            b'\n' => {
                self.carriage_return = false;
                self.done.push_str(&self.line);
                self.done.push('\n');
                self.line.clear();
            }
            b'\r' => self.carriage_return = true,
            b'\t' => self.print('\t'),
            // Other control characters move the cursor or ring the bell:
            // the log has no use for them.
            _ => {}
        }
    }

    /// The modes of the alternate screen that vt100 knows: 47, 1047 and
    /// 1049.
    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _: bool, action: char) {
        let screen = params
            .iter()
            .any(|param| matches!(param, [47 | 1047 | 1049]));
        if intermediates != b"?" || !screen || !matches!(action, 'h' | 'l') {
            return;
        }
        self.alternate = action == 'h';
        if self.alternate && !std::mem::replace(&mut self.marked, true) {
            if !std::mem::take(&mut self.carriage_return) && !self.line.is_empty() {
                self.done.push_str(&self.line);
                self.done.push('\n');
            }
            self.line.clear();
            self.done.push_str(FULL_SCREEN);
        }
    }
}

impl Cleaner {
    /// The lines of the log that `bytes` end. The text of the line that has
    /// not ended stays for later.
    pub fn lines(&mut self, bytes: &[u8]) -> String {
        self.parser.advance(&mut self.lines, bytes);
        std::mem::take(&mut self.lines.done)
    }

    /// At the end of the run: the line that has not ended, with a line
    /// feed, so that the line after it starts on a line of its own.
    pub fn finish(&mut self) -> String {
        let line = std::mem::take(&mut self.lines.line);
        let dropped = std::mem::take(&mut self.lines.carriage_return);
        match line.is_empty() || dropped {
            true => String::new(),
            false => line + "\n",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn clean(reads: &[&[u8]]) -> String {
        let mut cleaner = Cleaner::default();
        let mut text: String = reads.iter().map(|read| cleaner.lines(read)).collect();
        text.push_str(&cleaner.finish());
        text
    }

    #[test]
    fn escape_sequences_vanish() {
        assert_eq!(clean(&[b"\x1b[1;31mred\x1b[0m\n"]), "red\n");
        assert_eq!(
            clean(&[b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\b\n"]),
            "ab\n"
        );
        assert_eq!(clean(&[b"\x1b[?25l\x1b(Bc\x07\n"]), "c\n");
    }

    #[test]
    fn crlf_is_a_line_feed_and_a_carriage_return_alone_drops_the_line() {
        assert_eq!(clean(&[b"a\r\nb\r\n"]), "a\nb\n");
        assert_eq!(clean(&[b" 10%\r 50%\r100%\n"]), "100%\n");
        // Across two reads.
        assert_eq!(clean(&[b"a\r", b"\nb\n"]), "a\nb\n");
        assert_eq!(clean(&[b"old\r", b"new\n"]), "new\n");
    }

    #[test]
    fn bytes_that_are_not_utf_8_are_a_replacement_character() {
        assert_eq!(clean(&[b"a\xffb\n"]), "a\u{fffd}b\n");
    }

    #[test]
    fn a_character_split_across_two_reads_is_whole() {
        let bytes = "é€\n".as_bytes();
        assert_eq!(clean(&[&bytes[..1], &bytes[1..3], &bytes[3..]]), "é€\n");
    }

    #[test]
    fn the_line_that_has_not_ended_comes_at_the_finish() {
        let mut cleaner = Cleaner::default();
        assert_eq!(cleaner.lines(b"a\nprompt: "), "a\n");
        assert_eq!(cleaner.finish(), "prompt: \n");
        assert_eq!(cleaner.finish(), "");
        // A line that a carriage return dropped does not come back.
        assert_eq!(clean(&[b"working...\r"]), "");
    }

    #[test]
    fn tabs_stay_and_other_control_characters_vanish() {
        assert_eq!(clean(&[b"a\tb\x07\x08c\x00\n"]), "a\tbc\n");
        // U+009B, a CSI of 8 bits, encoded in UTF-8.
        assert_eq!(clean(&["a\u{9b}31mb\n".as_bytes()]), "a31mb\n");
    }

    #[test]
    fn output_on_the_alternate_screen_is_a_line_once_per_run() {
        let reads: [&[u8]; 3] = [
            b"before\n\x1b[?1049hvim\nstuff\x1b[?1049l",
            b"after\n\x1b[?47hmore\x1b[?47l",
            b"end\n",
        ];
        let expected = format!("before\n{FULL_SCREEN}after\nend\n");
        assert_eq!(clean(&reads), expected);
        // A line that has not ended comes before the mark.
        assert_eq!(
            clean(&[b"$ vim\x1b[?1049h"]),
            format!("$ vim\n{FULL_SCREEN}")
        );
    }

    fn time(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
    }

    #[test]
    fn the_time_of_a_line_is_utc() {
        assert_eq!(clock(time(13 * 3600 + 30)), "13:00:30Z");
        assert_eq!(
            clock(time(86_400 * 365 + 23 * 3600 + 59 * 60 + 59)),
            "23:59:59Z"
        );
    }

    #[test]
    fn a_run_writes_a_section_between_its_start_and_end_lines() {
        let mut sections = Sections::new(Vec::new());
        let start = |id, name: &str| Message::Start {
            id,
            name: name.into(),
            time: time(10),
        };
        let output = |id, bytes: &[u8]| Message::Output {
            id,
            bytes: bytes.to_vec(),
        };
        let messages = [
            start(1, "check"),
            output(1, b"\x1b[32mok\x1b[0m\r\npartial"),
            Message::End {
                id: 1,
                rc: 3,
                time: time(12),
            },
            // Output of a run that the log does not know, after its end.
            output(1, b"late\n"),
            start(2, "#2"),
            Message::End {
                id: 2,
                rc: 130,
                time: time(3600),
            },
        ];
        for message in messages {
            sections.write(message).unwrap();
        }
        let expected = "\
=== START check 00:00:10Z
ok
partial
=== END check rc=3 00:00:12Z
=== START #2 00:00:10Z
=== END #2 rc=130 01:00:00Z
";
        assert_eq!(String::from_utf8(sections.out).unwrap(), expected);
    }

    #[test]
    fn the_log_is_a_file_of_mode_0600_that_runs_append_to() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("run.log");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (sender, thread) = spawn(open(&path).unwrap(), |error| panic!("{error}"));
        let start = Message::Start {
            id: 1,
            name: "a".into(),
            time: time(0),
        };
        sender.send(start).unwrap();
        drop(sender);
        thread.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "old\n=== START a 00:00:00Z\n");
        // A log that does not exist yet gets mode 0600.
        let new = tmp.path().join("new.log");
        open(&new).unwrap();
        let mode = std::fs::metadata(&new).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    proptest! {
        #[test]
        fn the_log_holds_no_control_character_but_tab_and_line_feed(
            reads in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..8),
        ) {
            let reads: Vec<&[u8]> = reads.iter().map(Vec::as_slice).collect();
            let text = clean(&reads);
            let bad = text.chars().find(|c| c.is_control() && !matches!(c, '\t' | '\n'));
            prop_assert_eq!(bad, None);
            prop_assert!(text.is_empty() || text.ends_with('\n'));
        }
    }
}
