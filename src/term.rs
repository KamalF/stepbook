//! The terminal of a run: a vt100 screen, and its rows as lines of the
//! view (spec 6.3).
use std::cell::Cell;
use std::num::NonZeroU16;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use crate::doc::escaped;

/// The lines of scrollback of a run (spec 6.3).
const SCROLLBACK: usize = 5_000;

/// The rows of output under a block (spec 6.3).
const OUTPUT_ROWS: usize = 15;

/// What the terminal of a run shows after vt100 failed on its output.
const LOST: &[u8] = b"[stepbook: the terminal of this run failed, and lost the output above]\r\n";

thread_local! {
    /// Whether vt100 runs on this thread, under `catch_unwind`.
    static IN_VT100: Cell<bool> = const { Cell::new(false) };
}

/// Whether a panic comes from vt100, which `Terminal` catches, on the
/// reader thread of a run or on the main thread: the panic hook must then
/// neither print nor end stepbook.
pub fn in_vt100() -> bool {
    IN_VT100.get()
}

/// The screen of a run. Program output reaches the outer terminal only
/// through it, as text and styles: never as escape sequences (spec 6.4).
pub struct Terminal {
    parser: vt100::Parser,
    columns: u16,
    rows: u16,
}

/// The terminal of a run, between the reader thread of the run, which
/// feeds it, and the main thread, which shows it. vt100 can take seconds on
/// crafted output, such as a repeat of a character 65,535 times or a
/// scroll of the whole screen: only the run that prints it waits, since
/// the main thread only tries the lock.
pub struct Shared {
    terminal: Mutex<Terminal>,
    /// The size for the terminal, as columns << 16 | rows. The thread that
    /// next holds the lock gives it to the terminal.
    size: AtomicU32,
    /// The rows that the main thread asked for while the reader held the
    /// terminal, and the rows that the reader made for it. During a flood
    /// of output, the reader holds the terminal at almost each try of the
    /// main thread, whose rows would stay old for seconds.
    exchange: Mutex<Exchange>,
}

/// What the main thread takes of a terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Want {
    /// The last rows, under the block. With `cursor`, they go down to the
    /// row of the cursor at least.
    Last { cursor: bool },
}

#[derive(Default)]
struct Exchange {
    asked: Option<Want>,
    made: Option<(Want, Vec<Line<'static>>)>,
}

impl Shared {
    pub fn new(columns: u16, rows: u16) -> Shared {
        Shared {
            terminal: Mutex::new(Terminal::new(columns, rows)),
            size: AtomicU32::new(u32::from(columns) << 16 | u32::from(rows)),
            exchange: Mutex::default(),
        }
    }

    /// Reads the output of the run, then makes the rows that the main
    /// thread asked for meanwhile. It waits for the lock.
    pub fn process(&self, bytes: &[u8]) {
        let terminal = self.terminal.lock().unwrap_or_else(PoisonError::into_inner);
        let mut terminal = self.sized(terminal);
        terminal.process(bytes);
        let asked = self.exchange().asked.take();
        if let Some(want) = asked {
            let rows = terminal.rows(want);
            self.exchange().made = Some((want, rows));
        }
    }

    pub fn resize(&self, columns: u16, rows: u16) {
        let size = u32::from(columns) << 16 | u32::from(rows);
        self.size.store(size, Ordering::Relaxed);
    }

    /// The rows of the terminal. While the reader holds it, the rows that
    /// the reader made since the last call, if any: the reader makes them
    /// after its next read.
    pub fn try_rows(&self, want: Want) -> Option<Vec<Line<'static>>> {
        let terminal = match self.terminal.try_lock() {
            Ok(terminal) => terminal,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                let mut exchange = self.exchange();
                exchange.asked = Some(want);
                let made = exchange.made.take().filter(|(made, _)| *made == want);
                return made.map(|(_, rows)| rows);
            }
        };
        let mut terminal = self.sized(terminal);
        // Rows that the reader made before are older than these.
        *self.exchange() = Exchange::default();
        Some(terminal.rows(want))
    }

    fn exchange(&self) -> MutexGuard<'_, Exchange> {
        self.exchange.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sized<'a>(&self, mut terminal: MutexGuard<'a, Terminal>) -> MutexGuard<'a, Terminal> {
        let size = self.size.load(Ordering::Relaxed);
        let (columns, rows) = ((size >> 16) as u16, size as u16);
        if (columns, rows) != (terminal.columns, terminal.rows) {
            terminal.resize(columns, rows);
        }
        terminal
    }
}

fn size(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

fn parser(columns: u16, rows: u16) -> vt100::Parser {
    vt100::Parser::new(size(rows), size(columns), SCROLLBACK)
}

impl Terminal {
    pub fn new(columns: u16, rows: u16) -> Terminal {
        Terminal {
            parser: parser(columns, rows),
            columns,
            rows,
        }
    }

    /// Runs `f` on the parser. vt100 can panic on the output of a run: 18
    /// crafted bytes made it unwrap a None. The panic must not end stepbook
    /// and the other runs: the terminal starts again, with a row that says
    /// so, and `f` gives nothing.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut vt100::Parser) -> T) -> Option<T> {
        IN_VT100.set(true);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut self.parser)));
        IN_VT100.set(false);
        if result.is_err() {
            self.parser = parser(self.columns, self.rows);
            self.parser.process(LOST);
        }
        result.ok()
    }

    /// Reads the output of a run. vt100 keeps the start of a UTF-8
    /// character that ends in the next read (upmd issue K18).
    pub fn process(&mut self, bytes: &[u8]) {
        self.guarded(|parser| parser.process(bytes));
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        (self.columns, self.rows) = (columns, rows);
        self.guarded(|parser| parser.set_size(size(rows), size(columns)));
    }

    pub fn rows(&mut self, want: Want) -> Vec<Line<'static>> {
        let Want::Last { cursor } = want;
        self.last_rows(cursor)
    }

    /// The last `OUTPUT_ROWS` rows of the terminal, scrollback included,
    /// without the blank rows at the end. If `cursor`, the rows go down to
    /// the row of the cursor at least: a running program writes there next.
    pub fn last_rows(&mut self, cursor: bool) -> Vec<Line<'static>> {
        match self.guarded(|parser| last_rows(parser, cursor)) {
            Some(rows) => rows,
            None => last_rows(&mut self.parser, cursor),
        }
    }
}

/// The last `OUTPUT_ROWS` rows of the terminal: see `Terminal::last_rows`.
fn last_rows(parser: &mut vt100::Parser, cursor: bool) -> Vec<Line<'static>> {
    let screen = parser.screen();
    let (rows, columns) = screen.size();
    let (rows, columns) = (rows.get(), columns.get());
    let mut end = (0..rows)
        .rev()
        .find(|&row| (0..columns).any(|column| screen.cell(row, column).is_some_and(shows)))
        .map_or(0, |row| row + 1);
    if cursor {
        end = end.max(screen.cursor_position().0 + 1);
    }
    let start = end.saturating_sub(OUTPUT_ROWS as u16);
    let mut lines: Vec<_> = (start..end).map(|row| line(screen, row, columns)).collect();
    if lines.len() < OUTPUT_ROWS {
        // The scrollback shows above the screen. With an offset of n,
        // the first row of the screen shows the n-th line above it.
        let screen = parser.screen_mut();
        let above = (OUTPUT_ROWS - lines.len()).min(screen.scrollback_len());
        let mut all = Vec::with_capacity(OUTPUT_ROWS);
        for offset in (1..=above).rev() {
            screen.set_scrollback(offset);
            all.push(line(screen, 0, columns));
        }
        screen.set_scrollback(0);
        all.append(&mut lines);
        lines = all;
    }
    lines
}

/// Whether a cell shows: text other than a space, or a style, such as a
/// background. A program can write spaces over a line to clear it.
fn shows(cell: &vt100::Cell) -> bool {
    (cell.has_contents() && cell.contents() != " ") || style(cell) != Style::new()
}

/// A row as a line, without its blank cells at the end. Control and bidi
/// characters show escaped (spec 6.5).
fn line(screen: &vt100::Screen, row: u16, columns: u16) -> Line<'static> {
    let cells: Vec<_> = (0..columns)
        .filter_map(|column| screen.cell(row, column))
        .filter(|cell| !cell.is_wide_continuation())
        .collect();
    let end = cells
        .iter()
        .rposition(|cell| shows(cell))
        .map_or(0, |i| i + 1);
    // One escape per run of cells of one style: one per cell would
    // allocate for each.
    let mut spans = Vec::new();
    for run in cells[..end].chunk_by(|a, b| style(a) == style(b)) {
        let text: String = (run.iter())
            .map(|cell| {
                if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                }
            })
            .collect();
        spans.extend(escaped(&text, style(run[0])));
    }
    Line::from(spans)
}

fn style(cell: &vt100::Cell) -> Style {
    let mut style = Style::new();
    if let Some(color) = color(cell.fgcolor()) {
        style = style.fg(color);
    }
    if let Some(color) = color(cell.bgcolor()) {
        style = style.bg(color);
    }
    let modifiers = [
        (cell.bold(), Modifier::BOLD),
        (cell.dim(), Modifier::DIM),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ];
    for (on, modifier) in modifiers {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

/// The program's colors, as a terminal shows them (spec 6.3). The first 16
/// indexed colors are the palette's named colors.
fn color(color: vt100::Color) -> Option<Color> {
    const NAMED: [Color; 16] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(
            NAMED
                .get(usize::from(i))
                .copied()
                .unwrap_or(Color::Indexed(i)),
        ),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::needs_escape;
    use crate::view::tests::markup;
    use insta::assert_snapshot;
    use proptest::prelude::*;

    fn rows(terminal: &mut Terminal, cursor: bool) -> String {
        markup(&terminal.last_rows(cursor))
    }

    fn after(bytes: &[u8], columns: u16, rows: u16) -> Terminal {
        let mut terminal = Terminal::new(columns, rows);
        terminal.process(bytes);
        terminal
    }

    fn seq(n: usize) -> String {
        (1..=n).map(|i| format!("{i}\r\n")).collect()
    }

    #[test]
    fn a_utf_8_character_split_across_two_reads_shows_whole() {
        let mut terminal = Terminal::new(80, 24);
        terminal.process(b"price: \xe2\x82");
        terminal.process(b"\xac 5");
        assert_eq!(rows(&mut terminal, false), "price: € 5");
    }

    #[test]
    fn the_last_15_rows_show_without_the_blank_rows_at_the_end() {
        let mut terminal = after(seq(30).as_bytes(), 80, 24);
        let numbers = |range: std::ops::RangeInclusive<usize>| {
            range.map(|i| i.to_string()).collect::<Vec<_>>().join("\n")
        };
        assert_eq!(rows(&mut terminal, false), numbers(16..=30));
        // A running block keeps the row of its cursor.
        assert_eq!(rows(&mut terminal, true), numbers(17..=30) + "\n");
        let mut terminal = after(b"a\r\n\r\nb\r\n\r\n\r\n", 80, 24);
        assert_eq!(rows(&mut terminal, false), "a\n\nb");
        // A row of spaces is blank too.
        let mut terminal = after(b"a\r\n\r     \r\n\r\n", 80, 24);
        assert_eq!(terminal.last_rows(false).len(), 1);
    }

    #[test]
    fn the_last_rows_take_the_scrollback_above_a_short_screen() {
        let mut terminal = after(seq(30).as_bytes(), 80, 5);
        let expected: Vec<_> = (16..=30).map(|i| i.to_string()).collect();
        assert_eq!(rows(&mut terminal, false), expected.join("\n"));
        assert_eq!(terminal.parser.screen().scrollback(), 0);
        // Without enough scrollback, all the rows show.
        let mut terminal = after(seq(6).as_bytes(), 80, 5);
        assert_eq!(rows(&mut terminal, false), "1\n2\n3\n4\n5\n6");
    }

    #[test]
    fn output_keeps_the_colors_and_styles_of_the_program() {
        let bytes = b"\x1b[1;31mred\x1b[0m \x1b[38;5;200;2mx\x1b[0m\x1b[48;2;1;2;3;7my\x1b[0m \x1b[4;3;94mz";
        assert_snapshot!(rows(&mut after(bytes, 80, 24), false), @"«red,b:red» «200,d:x»«bg=#010203,r:y» «lightblue,i,u:z»");
    }

    #[test]
    fn a_row_ends_at_its_last_text_or_background() {
        let mut terminal = after(b"a\r\nb\x1b[41m  \x1b[0m", 80, 24);
        let rows = terminal.last_rows(false);
        let widths: Vec<_> = rows.iter().map(Line::width).collect();
        assert_eq!(widths, [1, 3]);
    }

    #[test]
    fn wide_characters_take_one_span() {
        assert_eq!(rows(&mut after("中a文".as_bytes(), 80, 24), false), "中a文");
    }

    #[test]
    fn bidi_controls_show_escaped_and_c1_controls_and_invalid_bytes_vanish() {
        let mut bytes = "a\u{202e}b \u{9b}31m ".as_bytes().to_vec();
        bytes.extend(b"x\xff\xfey");
        assert_snapshot!(rows(&mut after(&bytes, 80, 24), false), @"a«yellow:<202e>»b 31m xy");
    }

    #[test]
    fn a_new_terminal_and_a_resize_set_the_size_of_the_screen() {
        assert_eq!(
            rows(&mut after(b"0123456789abc", 10, 3), false),
            "0123456789\nabc"
        );
        let mut terminal = Terminal::new(80, 24);
        terminal.resize(10, 3);
        terminal.process(b"0123456789abc");
        assert_eq!(rows(&mut terminal, false), "0123456789\nabc");
    }

    #[test]
    fn the_panic_hook_tells_a_failure_of_vt100_from_other_panics() {
        let mut terminal = Terminal::new(80, 24);
        assert_eq!(terminal.guarded(|_| in_vt100()), Some(true));
        assert!(!in_vt100());
    }

    const LAST: Want = Want::Last { cursor: false };

    #[test]
    fn the_shared_terminal_gives_no_rows_while_the_reader_holds_it() {
        let shared = Shared::new(80, 24);
        shared.process(b"a");
        let held = shared.terminal.lock().unwrap();
        assert_eq!(shared.try_rows(LAST), None);
        drop(held);
        shared.resize(1, 3);
        let rows = shared.try_rows(LAST).unwrap();
        assert_eq!(markup(&rows), "a");
        assert_eq!(shared.terminal.lock().unwrap().columns, 1);
    }

    #[test]
    fn the_reader_makes_the_rows_that_the_view_asked_for_while_it_held_them() {
        let shared = Shared::new(80, 24);
        shared.process(b"a");
        let held = shared.terminal.lock().unwrap();
        assert_eq!(shared.try_rows(LAST), None);
        drop(held);
        shared.process(b"b");
        let held = shared.terminal.lock().unwrap();
        let rows = shared.try_rows(LAST).map(|rows| markup(&rows));
        assert_eq!(rows.as_deref(), Some("ab"));
        // Rows of another kind never show.
        assert_eq!(shared.try_rows(Want::Last { cursor: true }), None);
        drop(held);
        shared.process(b"c");
        let held = shared.terminal.lock().unwrap();
        let rows = shared.try_rows(Want::Last { cursor: true });
        assert_eq!(rows.map(|rows| markup(&rows)).as_deref(), Some("abc"));
        drop(held);
    }

    #[test]
    fn rows_that_the_reader_made_never_show_after_newer_rows() {
        let shared = Shared::new(80, 24);
        let held = shared.terminal.lock().unwrap();
        assert_eq!(shared.try_rows(LAST), None);
        drop(held);
        shared.process(b"a");
        shared.process(b"b");
        assert_eq!(markup(&shared.try_rows(LAST).unwrap()), "ab");
        let held = shared.terminal.lock().unwrap();
        assert_eq!(shared.try_rows(LAST), None);
        drop(held);
    }

    /// The length of the window title that vt100 gets.
    #[derive(Default)]
    struct TitleLength(usize);

    impl vt100::Callbacks for TitleLength {
        fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
            self.0 = title.len();
        }
    }

    #[test]
    fn vt100_keeps_at_most_1_kb_of_an_operating_system_command() {
        // With its std feature, vte keeps each byte of an OSC that never
        // ends: a log with a stray ESC ] made the memory grow without end.
        let mut parser =
            vt100::Parser::new_with_callbacks(size(24), size(80), 0, TitleLength::default());
        parser.process(b"\x1b]2;");
        parser.process(&[b'x'; 100_000]);
        parser.process(b"\x07after");
        assert!(parser.callbacks().0 <= 1024, "{}", parser.callbacks().0);
        assert_eq!(parser.screen().contents(), "after");
    }

    #[test]
    fn a_failure_of_vt100_loses_the_output_and_ends_nothing() {
        // vt100 0.18.1 unwraps a None on these 18 bytes, at any size.
        let mut terminal = after(b"a\r\n", 80, 24);
        terminal.process(b"\x1b[999;999H8\xef\xb8\x8f\x1b[A\x1b[999Gx");
        terminal.process(b"b");
        let lost = String::from_utf8_lossy(LOST).trim_end().to_string();
        assert_eq!(rows(&mut terminal, false), format!("{lost}\nb"));
    }

    /// A read of a run, or a resize of its PTY.
    #[derive(Clone, Debug)]
    enum Step {
        Read(Vec<u8>),
        Resize(u16, u16),
    }

    fn steps() -> impl Strategy<Value = Vec<Step>> {
        // Escape sequences that move the cursor and widen characters make
        // the failures of vt100 likely.
        let pieces = prop_oneof![
            prop::collection::vec(any::<u8>(), 0..20),
            Just(b"\x1b[999;999H".to_vec()),
            Just(b"\x1b[A".to_vec()),
            Just(b"\x1b[999G".to_vec()),
            Just("\u{fe0f}".as_bytes().to_vec()),
            Just("\u{4e2d}".as_bytes().to_vec()),
            Just(b"8".to_vec()),
        ];
        let read = prop::collection::vec(pieces, 1..8).prop_map(|p| Step::Read(p.concat()));
        let resize = (0..12u16, 0..6u16).prop_map(|(c, r)| Step::Resize(c, r));
        prop::collection::vec(prop_oneof![3 => read, 1 => resize], 0..12)
    }

    proptest! {
        // A panic of vt100 that escapes would end stepbook and its runs.
        #[test]
        fn no_output_or_resize_makes_the_terminal_panic(
            steps in steps(),
            (columns, rows) in (1..12u16, 1..6u16),
        ) {
            let mut terminal = Terminal::new(columns, rows);
            for step in steps {
                match step {
                    Step::Read(bytes) => terminal.process(&bytes),
                    Step::Resize(columns, rows) => terminal.resize(columns, rows),
                }
                terminal.last_rows(true);
            }
        }

        #[test]
        fn no_control_character_of_a_run_reaches_the_view(
            reads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..40), 0..8),
            text in "[\\u{0}-\\u{a0}\\u{200e}\\u{202a}-\\u{202e}\\u{2066}-\\u{2069}a ]{0,30}",
        ) {
            let mut terminal = Terminal::new(20, 4);
            for read in &reads {
                terminal.process(read);
            }
            terminal.process(text.as_bytes());
            for line in terminal.last_rows(true) {
                for span in &line.spans {
                    prop_assert!(!span.content.chars().any(needs_escape), "{:?}", span);
                }
            }
        }
    }
}
