//! The log of the runs (spec 12).

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
}

impl vte::Perform for Lines {
    fn print(&mut self, c: char) {
        // A C1 control character encoded in UTF-8, which some terminals
        // still act on.
        if c.is_control() && c != '\t' {
            return;
        }
        if std::mem::take(&mut self.carriage_return) {
            self.line.clear();
        }
        self.line.push(c);
    }

    fn execute(&mut self, byte: u8) {
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
        assert_eq!(clean(&[b"\x1b[?1049h\x1b(Bc\x07\n"]), "c\n");
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
