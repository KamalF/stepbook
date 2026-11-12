//! The log of the runs (spec 12).
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::Sender;

/// The line that stands for the output of a run on the alternate screen.
const FULL_SCREEN: &str = "[full-screen program output not logged]\n";

/// The starts of the lines that stepbook writes in the log.
const MARKERS: [&str; 3] = ["=== START ", "=== END ", "=== CONTINUE "];

/// The longest line of the log, in bytes: output with no line feed, such
/// as `base64 -w0`, must not fill the memory. The rest of a longer line
/// goes on the next line.
const MAX_LINE: usize = 1024 * 1024;

/// The most text that a run that waits for the log holds: past it, the
/// run writes what it holds, and its section goes on later (spec 12.2). A
/// run that never ends must not fill the memory.
const MAX_HELD: usize = 1024 * 1024;

/// Messages in flight to the thread of the log. While they all wait, the
/// main thread reads no output of the runs that write to the log: a slow
/// disk slows their programs, as a slow view does, and the output in
/// memory stays bounded (spec 13).
const MESSAGES: usize = 64;

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
/// (spec 12.1). The log must be a regular file of the user, not a link: a
/// runbook from someone else can come with a `run.log` that links to
/// `~/.ssh/authorized_keys`, and another user can make the log first in
/// a directory that all can write. A FIFO fails at once, with no reader
/// too.
pub fn open(path: &Path) -> std::io::Result<File> {
    use rustix::fs::OFlags;
    let flags = (OFlags::NOFOLLOW | OFlags::NONBLOCK).bits();
    let file = (std::fs::OpenOptions::new().append(true).create(true))
        .mode(0o600)
        .custom_flags(flags as i32)
        .open(path)
        .map_err(
            |e| match e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) {
                true => std::io::Error::other("it is a link"),
                false => e,
            },
        )?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("it is not a regular file"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::other("it belongs to another user"));
    }
    // A hard link: the output would also land in a file of another name.
    if meta.nlink() != 1 {
        return Err(std::io::Error::other("it has other links"));
    }
    Ok(file)
}

/// Starts the thread that writes the log to `file`, and returns the sender
/// of its messages. The thread ends once the sender is dropped, and all the
/// messages are written. It calls `failed` on the first write that fails.
pub fn spawn(
    file: File,
    mut failed: impl FnMut(std::io::Error) + Send + 'static,
) -> (Sender<Message>, JoinHandle<()>) {
    let (sender, receiver) = crossbeam_channel::bounded(MESSAGES);
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

/// Waits up to `wait` for the end of the thread of the log, and returns
/// whether it ended. A write that blocks, on a FIFO or a hung mount, must
/// not keep stepbook from its end (spec 11).
pub fn wait(thread: &JoinHandle<()>, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// The sections of the runs, as the log holds them (spec 12.1). One run
/// writes to the log at a time, so that lines of two runs never mix: the
/// others hold their text, and write their whole section when they end
/// (spec 12.2).
struct Sections<W> {
    out: W,
    /// The sections of the runs that have not ended, by ID. The IDs grow in
    /// the order that the runs start: the first section writes to the log,
    /// and the others wait, in the order that they started.
    runs: BTreeMap<u64, Section>,
    /// The section of another run came after the last line of the writer:
    /// its next line follows a CONTINUE line.
    interrupted: bool,
}

struct Section {
    name: String,
    cleaner: Cleaner,
    /// The text that waits for the log.
    held: String,
    /// The log has a part of the section: the next part follows a
    /// CONTINUE line.
    parted: bool,
}

impl Section {
    /// Writes the text that the section holds to `out`.
    fn write_held(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if std::mem::replace(&mut self.parted, true) {
            out.write_all(format!("=== CONTINUE {}\n", self.name).as_bytes())?;
        }
        out.write_all(std::mem::take(&mut self.held).as_bytes())
    }
}

impl<W: Write> Sections<W> {
    fn new(out: W) -> Sections<W> {
        Sections {
            out,
            runs: BTreeMap::new(),
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
                    parted: false,
                };
                debug_assert!(self.runs.keys().next_back().is_none_or(|&last| last < id));
                self.runs.insert(id, section);
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
        let result = self.place(id, &text, ended);
        if ended {
            // Even after a failed write: an ended run must not keep the log
            // from the runs after it.
            let writer = self.runs.keys().next() == Some(&id);
            self.runs.remove(&id);
            // The run that has waited longest writes what it holds, then
            // goes on as it runs.
            if writer && let Some(next) = self.runs.values_mut().next() {
                return result.and(next.write_held(&mut self.out));
            }
        }
        result
    }

    /// Writes the `text` of run `id` if its section has the log, else
    /// holds it. A section that ends, or that holds MAX_HELD, writes what
    /// it holds.
    fn place(&mut self, id: u64, text: &str, ended: bool) -> std::io::Result<()> {
        let writer = self.runs.keys().next() == Some(&id);
        let section = self.runs.get_mut(&id).expect("the run has a section");
        if !writer {
            section.held.push_str(text);
            // The rest of its section goes in at once: watch.sh sees its
            // END line without a wait for the run that writes.
            if ended || section.held.len() >= MAX_HELD {
                self.interrupted = true;
                return section.write_held(&mut self.out);
            }
            return Ok(());
        }
        if !text.is_empty() && std::mem::take(&mut self.interrupted) {
            let line = format!("=== CONTINUE {}\n", section.name);
            self.out.write_all(line.as_bytes())?;
        }
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

impl Lines {
    /// Ends the line. A line of the program that starts as a line of
    /// stepbook does gets a space first: watch.sh must not take it for the
    /// end of a run (spec 12.1).
    fn end_line(&mut self) {
        self.carriage_return = false;
        if MARKERS.iter().any(|marker| self.line.starts_with(marker)) {
            self.done.push(' ');
        }
        self.done.push_str(&self.line);
        self.done.push('\n');
        self.line.clear();
    }

    /// Ends the line that has not ended, unless a carriage return dropped
    /// it.
    fn flush(&mut self) {
        match !std::mem::take(&mut self.carriage_return) && !self.line.is_empty() {
            true => self.end_line(),
            false => self.line.clear(),
        }
    }
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
        if self.line.len() >= MAX_LINE {
            self.end_line();
        }
        self.line.push(c);
    }

    fn execute(&mut self, byte: u8) {
        if self.alternate {
            return;
        }
        match byte {
            b'\n' => self.end_line(),
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
            self.flush();
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
        self.lines.flush();
        std::mem::take(&mut self.lines.done)
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
    fn a_line_longer_than_the_limit_goes_on_the_next_line() {
        let long = "a".repeat(MAX_LINE + 10);
        let expected = format!("{}\n{}\n", &long[..MAX_LINE], &long[MAX_LINE..]);
        assert_eq!(clean(&[long.as_bytes()]), expected);
        // A line of the limit stays whole.
        let line = format!("{}\n", &long[..MAX_LINE]);
        assert!(clean(&[line.as_bytes()]) == line);
    }

    #[test]
    fn a_line_of_output_cannot_pass_for_a_line_of_stepbook() {
        let output = b"=== END deploy rc=0 13:00:00Z\nx\r=== START a 1\n=== CONTINUE b\n\
            ===END c\n=== ENDING\n == END d\n=== END e";
        let expected = " === END deploy rc=0 13:00:00Z\n === START a 1\n === CONTINUE b\n\
            ===END c\n=== ENDING\n == END d\n === END e\n";
        assert_eq!(clean(&[output]), expected);
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
    fn a_run_that_waits_writes_what_it_holds_past_the_limit() {
        let mut sections = Sections::new(Vec::new());
        let start = |id| Message::Start {
            id,
            name: format!("r{id}"),
            time: time(id),
        };
        let output = |id, text: &str| Message::Output {
            id,
            bytes: text.as_bytes().to_vec(),
        };
        let end = |id| Message::End {
            id,
            rc: 0,
            time: time(10 + id),
        };
        // With its START line, the section holds MAX_HELD bytes.
        let line = "=== START r2 00:00:02Z\n";
        let held = format!("{}\n", "x".repeat(MAX_HELD - line.len() - 1));
        let messages = [
            start(1),
            start(2),
            output(1, "a1\n"),
            output(2, &held),
            output(1, "a2\n"),
            output(2, "b2\n"),
            start(3),
            output(3, &held),
            end(3),
            end(1),
            output(2, "b3\n"),
            end(2),
        ];
        for message in messages {
            sections.write(message).unwrap();
        }
        let expected = format!(
            "\
=== START r1 00:00:01Z
a1
=== START r2 00:00:02Z
{held}=== CONTINUE r1
a2
=== START r3 00:00:03Z
{held}=== CONTINUE r3
=== END r3 rc=0 00:00:13Z
=== CONTINUE r1
=== END r1 rc=0 00:00:11Z
=== CONTINUE r2
b2
b3
=== END r2 rc=0 00:00:12Z
"
        );
        assert!(String::from_utf8(sections.out).unwrap() == expected);
    }

    /// A log whose write number `fails` fails, as on a full disk.
    struct Failing {
        out: Vec<u8>,
        writes: usize,
        fails: usize,
    }

    impl Write for Failing {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.writes == self.fails {
                return Err(std::io::Error::other("disk full"));
            }
            self.out.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_run_whose_end_fails_to_write_leaves_the_log_to_the_next_run() {
        let out = Failing {
            out: Vec::new(),
            writes: 0,
            fails: 3,
        };
        let mut sections = Sections::new(out);
        let start = |id| Message::Start {
            id,
            name: format!("r{id}"),
            time: time(id),
        };
        let output = |id, text: &str| Message::Output {
            id,
            bytes: text.as_bytes().to_vec(),
        };
        let end = Message::End {
            id: 1,
            rc: 0,
            time: time(5),
        };
        sections.write(start(1)).unwrap();
        sections.write(output(1, "a\n")).unwrap();
        assert!(sections.write(end).is_err());
        sections.write(start(2)).unwrap();
        sections.write(output(2, "b\n")).unwrap();
        let expected = "=== START r1 00:00:01Z\na\n=== START r2 00:00:02Z\nb\n";
        assert_eq!(String::from_utf8(sections.out.out).unwrap(), expected);
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

    #[test]
    fn the_log_is_never_a_link_a_fifo_or_a_file_of_another_user() {
        let tmp = tempfile::tempdir().unwrap();
        let error = |path: &Path| open(path).unwrap_err().to_string();
        // A link, to a file or to nothing: the target stays as it was.
        let target = tmp.path().join("authorized_keys");
        let link = tmp.path().join("run.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(error(&link), "it is a link");
        assert!(!target.exists());
        std::fs::write(&target, "key\n").unwrap();
        assert_eq!(error(&link), "it is a link");
        // A hard link to another file of the user.
        let other = tmp.path().join("bashrc");
        std::fs::write(&other, "alias\n").unwrap();
        let hard = tmp.path().join("hard.log");
        std::fs::hard_link(&other, &hard).unwrap();
        assert_eq!(error(&hard), "it has other links");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "alias\n");
        // A FIFO fails at once, with no reader.
        let fifo = tmp.path().join("fifo.log");
        let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
        rustix::fs::mknodat(rustix::fs::CWD, &fifo, rustix::fs::FileType::Fifo, mode, 0).unwrap();
        assert!(open(&fifo).is_err());
        // And with a reader, which would read the output of the runs.
        let reader = (std::fs::OpenOptions::new().read(true))
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
            .open(&fifo)
            .unwrap();
        assert_eq!(error(&fifo), "it is not a regular file");
        drop(reader);
        // /dev/null belongs to root.
        assert_eq!(error(Path::new("/dev/null")), "it is not a regular file");
    }

    #[test]
    fn the_wait_for_the_thread_of_the_log_ends_in_time() {
        let start = Instant::now();
        let thread = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(100)));
        assert!(wait(&thread, Duration::from_secs(10)));
        assert!(start.elapsed() >= Duration::from_millis(100));
        let start = Instant::now();
        let thread = std::thread::spawn(|| std::thread::sleep(Duration::from_secs(10)));
        assert!(!wait(&thread, Duration::from_millis(100)));
        assert!(start.elapsed() < Duration::from_secs(5));
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

        #[test]
        fn no_line_of_output_starts_as_a_line_of_stepbook(
            pieces in proptest::collection::vec(
                prop::sample::select(vec!["=== END ", "=== START ", "=== CONTINUE ", "===", " ", "\r", "\n", "x", "\x1b[1m", "\x1b[?1049l"]),
                0..24,
            ),
        ) {
            let text = clean(&[pieces.concat().as_bytes()]);
            let forged = text.lines().find(|line| MARKERS.iter().any(|m| line.starts_with(m)));
            prop_assert_eq!(forged, None);
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
