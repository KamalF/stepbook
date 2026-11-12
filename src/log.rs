//! The log of the runs (spec 12).
use std::collections::{BTreeMap, VecDeque};
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

/// The sections of the runs, as the log holds them (spec 12.1). One run
/// writes to the log at a time, so that lines of two runs never mix: the
/// others hold their text, and write their whole section when they end
/// (spec 12.2).
struct Sections<W> {
    out: W,
    runs: BTreeMap<u64, Section>,
    /// The run whose section the log holds now.
    writer: Option<u64>,
    /// The runs that wait for the log, in the order that they started.
    waiting: VecDeque<u64>,
    /// The section of another run came after the last line of the writer:
    /// its next line follows a CONTINUE line.
    interrupted: bool,
}

struct Section {
    name: String,
    cleaner: Cleaner,
    /// The text that waits for the log.
    held: String,
}

impl<W: Write> Sections<W> {
    fn new(out: W) -> Sections<W> {
        Sections {
            out,
            runs: BTreeMap::new(),
            writer: None,
            waiting: VecDeque::new(),
            interrupted: false,
        }
    }

    fn write(&mut self, message: Message) -> std::io::Result<()> {
        let (id, text, ended) = match message {
            Message::Start { id, name, time } => {
                let line = format!("=== START {name} {}\n", clock(time));
                let section = Section {
                    name,
                    cleaner: Cleaner::default(),
                    held: String::new(),
                };
                self.runs.insert(id, section);
                match self.writer {
                    None => self.writer = Some(id),
                    Some(_) => self.waiting.push_back(id),
                }
                (id, line, false)
            }
            Message::Output { id, bytes } => match self.runs.get_mut(&id) {
                Some(section) => (id, section.cleaner.lines(&bytes), false),
                None => return Ok(()),
            },
            Message::End { id, rc, time } => match self.runs.get_mut(&id) {
                Some(section) => {
                    let (name, time) = (&section.name, clock(time));
                    let end = format!("=== END {name} rc={rc} {time}\n");
                    (id, section.cleaner.finish() + &end, true)
                }
                None => return Ok(()),
            },
        };
        if self.writer != Some(id) {
            let section = self.runs.get_mut(&id).expect("the run has a section");
            section.held.push_str(&text);
            // Its whole section goes in at once: watch.sh sees its END
            // line without a wait for the run that writes.
            if ended {
                let held = std::mem::take(&mut section.held);
                self.runs.remove(&id);
                self.waiting.retain(|&run| run != id);
                self.interrupted = true;
                self.out.write_all(held.as_bytes())?;
            }
            return Ok(());
        }
        if !text.is_empty() && std::mem::take(&mut self.interrupted) {
            let name = &self.runs[&id].name;
            self.out
                .write_all(format!("=== CONTINUE {name}\n").as_bytes())?;
        }
        self.out.write_all(text.as_bytes())?;
        if ended {
            self.runs.remove(&id);
            self.writer = None;
            self.interrupted = false;
            // The run that has waited longest writes what it holds, then
            // goes on as it runs.
            if let Some(next) = self.waiting.pop_front() {
                let section = self
                    .runs
                    .get_mut(&next)
                    .expect("a run that waits has a section");
                self.writer = Some(next);
                self.out
                    .write_all(std::mem::take(&mut section.held).as_bytes())?;
            }
        }
        Ok(())
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

    /// The modes of the alternate screen, as vt100 reads them: 47 and 1049
    /// after a first intermediate `?`. The view and the log must agree on
    /// which output is full-screen.
    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _: bool, action: char) {
        if intermediates.first() != Some(&b'?') || !matches!(action, 'h' | 'l') {
            return;
        }
        if !params.iter().any(|param| matches!(param, [47 | 1049])) {
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

    /// A reset of the terminal (RIS) leaves the alternate screen, as in
    /// vt100: `reset` after a full-screen program.
    fn esc_dispatch(&mut self, intermediates: &[u8], _: bool, byte: u8) {
        if intermediates.is_empty() && byte == b'c' {
            self.alternate = false;
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
        // Other modes, 1047 among them, as vt100 reads it, and the same
        // numbers without "?", are not that screen.
        assert_eq!(
            clean(&[b"\x1b[?25hshown\n\x1b[1049hseen\n\x1b[?1047hthere\n"]),
            "shown\nseen\nthere\n"
        );
        // A reset of the terminal leaves the alternate screen.
        assert_eq!(
            clean(&[b"\x1b[?1049hvim\x1bcafter\n"]),
            format!("{FULL_SCREEN}after\n")
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
    fn one_run_writes_to_the_log_at_a_time() {
        let mut sections = Sections::new(Vec::new());
        let start = |id, seconds| Message::Start {
            id,
            name: format!("r{id}"),
            time: time(seconds),
        };
        let output = |id, text: &str| Message::Output {
            id,
            bytes: text.as_bytes().to_vec(),
        };
        let end = |id, seconds| Message::End {
            id,
            rc: 0,
            time: time(seconds),
        };
        let messages = [
            start(1, 1),
            start(2, 2),
            output(2, "b1\n"),
            output(1, "a1\n"),
            start(3, 3),
            end(2, 4),
            output(3, "c1\n"),
            output(1, "a2\n"),
            end(1, 5),
            output(3, "c2\n"),
            end(3, 6),
            start(4, 7),
        ];
        for message in messages {
            sections.write(message).unwrap();
        }
        // A run that ends while r1 writes goes in at once, and r1 goes on
        // after a CONTINUE line. Each run keeps the time it started at.
        let expected = "\
=== START r1 00:00:01Z
a1
=== START r2 00:00:02Z
b1
=== END r2 rc=0 00:00:04Z
=== CONTINUE r1
a2
=== END r1 rc=0 00:00:05Z
=== START r3 00:00:03Z
c1
c2
=== END r3 rc=0 00:00:06Z
=== START r4 00:00:07Z
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

        /// The log leaves out the output that the view shows as
        /// full-screen, and only that output.
        #[test]
        fn the_log_and_vt100_agree_on_the_alternate_screen(
            pieces in proptest::collection::vec(piece(), 0..16),
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
        ) {
            let output = pieces.concat();
            let mut cuts: Vec<usize> = cuts.iter().map(|cut| cut.index(output.len() + 1)).collect();
            cuts.push(output.len());
            cuts.sort_unstable();
            let size = std::num::NonZeroU16::MIN.saturating_add(9);
            let mut screen = vt100::Parser::new(size, size, 0);
            let mut cleaner = Cleaner::default();
            let mut start = 0;
            for cut in cuts {
                let read = &output[start..cut];
                start = cut;
                screen.process(read);
                cleaner.lines(read);
                prop_assert_eq!(cleaner.lines.alternate, screen.screen().alternate_screen());
            }
        }
    }

    /// A piece of output that can change the screen: a CSI sequence of a
    /// mode, an escape sequence, or text.
    fn piece() -> impl Strategy<Value = Vec<u8>> {
        let select = |items: &[&'static str]| prop::sample::select(items.to_vec());
        let params =
            proptest::collection::vec(select(&["47", "1047", "1049", "25", "47:1", ""]), 0..3);
        let csi = (
            select(&["", "?", ">"]),
            params,
            select(&["", "$", " "]),
            select(&["h", "l", "m"]),
        )
            .prop_map(|(private, params, middle, action)| {
                format!("\x1b[{private}{}{middle}{action}", params.join(";"))
            });
        let other = select(&[
            "\x1bc",
            "\x1b(c",
            "\x1b#c",
            "x\n",
            "\x1b]0;\x1b[?1049h\x07",
            "\x1b",
        ]);
        prop_oneof![csi, other.prop_map(String::from)].prop_map(String::into_bytes)
    }
}
