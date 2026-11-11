//! The terminal of a run: a vt100 screen, and its rows as lines of the
//! view (spec 6.3).
use std::cell::Cell;
use std::num::NonZeroU16;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use crate::doc::{DIM, escaped};

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
    parser: Parser,
    columns: u16,
    rows: u16,
    /// The last screen of a program that was still on the alternate
    /// screen when its run ended.
    last_screen: Option<Vec<Line<'static>>>,
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
    /// Whether the program turned on the application cursor mode, which
    /// changes what the arrows send. The reader sets it after each read, so
    /// that a key never waits for the terminal.
    application_cursor: AtomicBool,
    /// Whether the program turned on bracketed paste mode, which marks the
    /// start and the end of a paste.
    bracketed_paste: AtomicBool,
}

/// Rows of a terminal, as lines of the view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    pub lines: Vec<Line<'static>>,
    /// The line and the column of the cursor, if the program shows it.
    pub cursor: Option<(usize, u16)>,
    /// Whether the program was on the alternate screen, such as vim, less
    /// or top (spec 6.3).
    pub alternate: bool,
    /// Whether the run ended on the alternate screen, which stepbook then
    /// left: the program did not leave it.
    pub ended_on_alternate: bool,
}

/// What the main thread takes of a terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Want {
    /// The last rows, under the block. With `cursor`, they go down to the
    /// row of the cursor at least.
    Last { cursor: bool },
    /// Each row of the screen, with the cursor: the full-screen view of a
    /// running block.
    Screen,
    /// The scrollback and the screen, without the blank rows at the end:
    /// the full-screen view after the run.
    All,
}

#[derive(Default)]
struct Exchange {
    asked: Option<Want>,
    made: Option<(Want, Rows)>,
}

impl Shared {
    pub fn new(columns: u16, rows: u16) -> Shared {
        Shared {
            terminal: Mutex::new(Terminal::new(columns, rows)),
            size: AtomicU32::new(u32::from(columns) << 16 | u32::from(rows)),
            exchange: Mutex::default(),
            application_cursor: AtomicBool::new(false),
            bracketed_paste: AtomicBool::new(false),
        }
    }

    /// Reads the output of the run, then makes the rows that the main
    /// thread asked for meanwhile. It waits for the lock. It returns the
    /// answers to the queries of the program.
    pub fn process(&self, bytes: &[u8]) -> Vec<u8> {
        let terminal = self.terminal.lock().unwrap_or_else(PoisonError::into_inner);
        let mut terminal = self.sized(terminal);
        let answers = terminal.process(bytes);
        let screen = terminal.parser.screen();
        (self.application_cursor).store(screen.application_cursor(), Ordering::Relaxed);
        (self.bracketed_paste).store(screen.bracketed_paste(), Ordering::Relaxed);
        let asked = self.exchange().asked.take();
        if let Some(want) = asked {
            let rows = terminal.rows(want);
            self.exchange().made = Some((want, rows));
        }
        answers
    }

    /// Ends the terminal of a run that has ended: see `Terminal::end`.
    pub fn end(&self) {
        let terminal = self.terminal.lock().unwrap_or_else(PoisonError::into_inner);
        self.sized(terminal).end();
    }

    pub fn resize(&self, columns: u16, rows: u16) {
        let size = u32::from(columns) << 16 | u32::from(rows);
        self.size.store(size, Ordering::Relaxed);
    }

    /// The rows of the terminal. While the reader holds it, the rows that
    /// the reader made since the last call, if any: the reader makes them
    /// after its next read.
    pub fn try_rows(&self, want: Want) -> Option<Rows> {
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

    pub fn application_cursor(&self) -> bool {
        self.application_cursor.load(Ordering::Relaxed)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste.load(Ordering::Relaxed)
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

type Parser = vt100::Parser<Answers>;

fn parser(columns: u16, rows: u16) -> Parser {
    Parser::new_with_callbacks(size(rows), size(columns), SCROLLBACK, Answers::default())
}

/// The answers of the terminal to the queries of a program, which programs
/// such as fzf wait for: its status and the position of its cursor (DSR),
/// and its device attributes (DA1). They hold only numbers, never text from
/// the program, which could write keys for it. Other queries, such as
/// those of the colors (OSC 10 and 11), get no answer: most programs send
/// DA1 after them, and stop waiting at its answer.
#[derive(Default)]
pub struct Answers(Vec<u8>);

impl vt100::Callbacks for Answers {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first: Option<u8>,
        second: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if (first, second) != (None, None) {
            return;
        }
        match (c, params) {
            ('n', [[5]]) => self.0.extend(b"\x1b[0n"),
            ('n', [[6]]) => {
                // After a character in the last column, vt100 keeps the
                // cursor past it, until the next character wraps.
                let (row, column) = screen.cursor_position();
                let column = column.min(screen.size().1.get() - 1);
                let answer = format!("\x1b[{};{}R", row + 1, column + 1);
                self.0.extend(answer.as_bytes());
            }
            // A VT100 with the advanced video option, as tmux and screen
            // answer.
            ('c', [] | [[0]]) => self.0.extend(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

impl Terminal {
    pub fn new(columns: u16, rows: u16) -> Terminal {
        Terminal {
            parser: parser(columns, rows),
            columns,
            rows,
            last_screen: None,
        }
    }

    /// Runs `f` on the parser. vt100 can panic on the output of a run: 18
    /// crafted bytes made it unwrap a None. The panic must not end stepbook
    /// and the other runs: the terminal starts again, with a row that says
    /// so, and `f` gives nothing.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut Parser) -> T) -> Option<T> {
        IN_VT100.set(true);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(&mut self.parser)));
        IN_VT100.set(false);
        if result.is_err() {
            self.parser = parser(self.columns, self.rows);
            self.parser.process(LOST);
        }
        result.ok()
    }

    /// Reads the output of a run, and returns the answers to its queries.
    /// vt100 keeps the start of a UTF-8 character that ends in the next
    /// read (upmd issue K18).
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.guarded(|parser| parser.process(bytes));
        std::mem::take(&mut self.parser.callbacks_mut().0)
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        (self.columns, self.rows) = (columns, rows);
        self.guarded(|parser| parser.set_size(size(rows), size(columns)));
    }

    pub fn rows(&mut self, want: Want) -> Rows {
        let read = |parser: &mut Parser| {
            let alternate = parser.screen().alternate_screen();
            let (lines, cursor) = match want {
                Want::Last { cursor } => last_rows(parser.screen_mut(), cursor),
                Want::Screen => screen_rows(parser.screen()),
                Want::All => (all_rows(parser.screen_mut()), None),
            };
            Rows {
                lines,
                cursor,
                alternate,
                ended_on_alternate: false,
            }
        };
        let mut rows = match self.guarded(read) {
            Some(rows) => rows,
            None => read(&mut self.parser),
        };
        if let Some(last_screen) = &self.last_screen {
            rows.ended_on_alternate = true;
            if want == Want::All {
                let mark = "[stepbook: the last screen of the full-screen program]";
                rows.lines.push(Line::styled(mark, DIM));
                rows.lines.extend(last_screen.iter().cloned());
            }
        }
        rows
    }

    /// Leaves the alternate screen at the end of a run, as `reset` would:
    /// vt100 gives no access to the main screen and its scrollback while
    /// the alternate screen shows. The last screen of the program, where a
    /// crash may have left its message, comes after them in `Want::All`.
    pub fn end(&mut self) {
        if !self.parser.screen().alternate_screen() {
            return;
        }
        let mut lines = self.rows(Want::Screen).lines;
        while lines.last().is_some_and(|line| line.spans.is_empty()) {
            lines.pop();
        }
        self.process(b"\x1b[?1049l");
        self.last_screen = Some(lines);
    }
}

/// Lines, and the line and the column of the cursor if it shows.
type Lines = (Vec<Line<'static>>, Option<(usize, u16)>);

/// Each row of the screen, and the cursor if the program shows it.
fn screen_rows(screen: &vt100::Screen) -> Lines {
    let (rows, columns) = screen.size();
    let lines = (0..rows.get()).map(|row| line(screen, row, columns.get()));
    let (row, column) = screen.cursor_position();
    let cursor = (!screen.hide_cursor()).then_some((usize::from(row), column));
    (lines.collect(), cursor)
}

/// The lines of the scrollback, then the rows of the screen, without the
/// blank rows at the end.
fn all_rows(screen: &mut vt100::Screen) -> Vec<Line<'static>> {
    let mut lines = scrollback(screen, screen.scrollback_len());
    lines.extend(screen_rows(screen).0);
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines
}

/// The last `count` lines of the scrollback, at most its length. With an
/// offset of n, row r of the screen shows the n-th line above the screen,
/// plus r. Each change of the offset walks the scrollback, so the offset
/// moves by a screen at a time: 5,000 changes took 50 ms.
fn scrollback(screen: &mut vt100::Screen, count: usize) -> Vec<Line<'static>> {
    let (rows, columns) = screen.size();
    let (rows, columns) = (usize::from(rows.get()), columns.get());
    let mut lines = Vec::with_capacity(count);
    let mut offset = count;
    while offset > 0 {
        screen.set_scrollback(offset);
        let shown = rows.min(offset);
        lines.extend((0..shown).map(|row| line(screen, row as u16, columns)));
        offset -= shown;
    }
    screen.set_scrollback(0);
    lines
}

/// The last `OUTPUT_ROWS` rows of the terminal, scrollback included,
/// without the blank rows at the end. If `cursor`, the rows go down to the
/// row of the cursor at least, where a running program writes next, and
/// they give the cursor.
fn last_rows(screen: &mut vt100::Screen, cursor: bool) -> Lines {
    let (rows, columns) = screen.size();
    let (rows, columns) = (rows.get(), columns.get());
    let mut end = (0..rows)
        .rev()
        .find(|&row| (0..columns).any(|column| screen.cell(row, column).is_some_and(shows)))
        .map_or(0, |row| row + 1);
    let (cursor_row, cursor_column) = screen.cursor_position();
    if cursor {
        end = end.max(cursor_row + 1);
    }
    let start = end.saturating_sub(OUTPUT_ROWS as u16);
    // The cursor can be above the last rows, under the text that ends them.
    let shown = cursor && !screen.hide_cursor() && cursor_row >= start;
    let mut cursor = shown.then(|| (usize::from(cursor_row - start), cursor_column));
    let mut lines: Vec<_> = (start..end).map(|row| line(screen, row, columns)).collect();
    if lines.len() < OUTPUT_ROWS {
        // The scrollback shows above the screen.
        let above = (OUTPUT_ROWS - lines.len()).min(screen.scrollback_len());
        let mut all = scrollback(screen, above);
        if let Some((row, _)) = &mut cursor {
            *row += all.len();
        }
        all.append(&mut lines);
        lines = all;
    }
    (lines, cursor)
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

/// The bytes that xterm sends for a key (spec 7.2), or none. In the
/// application cursor mode, which a program turns on with `ESC[?1h`, the
/// arrows, Home and End send `ESC O` instead of `ESC [`. A key with
/// modifiers sends their sum plus 1 as a parameter: 2 for shift, 3 for alt,
/// 5 for ctrl.
pub fn key_bytes(key: &KeyEvent, application_cursor: bool) -> Vec<u8> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let modifiers = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    // A key that sends a letter after ESC [ or ESC O: ESC [ 1 ; m X with
    // modifiers.
    let letter = |letter: char, prefix: &str| match modifiers {
        1 => format!("\x1b{prefix}{letter}"),
        m => format!("\x1b[1;{m}{letter}"),
    };
    let cursor = if application_cursor { "O" } else { "[" };
    // A key that sends ESC [ n ~, with ESC [ n ; m ~ for modifiers.
    let tilde = |n: u8| match modifiers {
        1 => format!("\x1b[{n}~"),
        m => format!("\x1b[{n};{m}~"),
    };
    let text = match key.code {
        KeyCode::Char(c) => {
            let mut bytes = match ctrl.then(|| control(c)).flatten() {
                Some(byte) => vec![byte],
                None => c.to_string().into_bytes(),
            };
            if alt {
                bytes.insert(0, 0x1b);
            }
            return bytes;
        }
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Null => "\0".into(),
        KeyCode::Up => letter('A', cursor),
        KeyCode::Down => letter('B', cursor),
        KeyCode::Right => letter('C', cursor),
        KeyCode::Left => letter('D', cursor),
        KeyCode::Home => letter('H', cursor),
        KeyCode::End => letter('F', cursor),
        KeyCode::F(n @ 1..=4) => letter(char::from(b'P' + n - 1), "O"),
        KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]),
        KeyCode::Insert => tilde(2),
        KeyCode::Delete => tilde(3),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        _ => return Vec::new(),
    };
    let mut bytes = text.into_bytes();
    // Alt before a key of one byte sends ESC first, as for a character.
    if alt && bytes.len() == 1 {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// The bytes of a paste (spec 7.2). If the program turned bracketed paste
/// on, they come between `ESC[200~` and `ESC[201~`, without the ESC
/// characters of the text: an `ESC[201~` in it would end the paste early,
/// and the program would take the rest for keys.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let text = text.replace('\x1b', "");
    [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()
}

/// The byte of a character with ctrl, as xterm sends it, if it has one.
/// crossterm reads the bytes 0x1c to 0x1f as ctrl with 4 to 7, and 0 as
/// ctrl with a space.
fn control(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_lowercase() as u8 - b'a' + 1,
        ' ' | '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
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
        markup(&terminal.rows(Want::Last { cursor }).lines)
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
        assert_eq!(terminal.rows(LAST).lines.len(), 1);
    }

    #[test]
    fn the_last_rows_of_a_running_block_give_the_cursor_that_the_program_shows() {
        let mut terminal = after(b"a\r\nPassword: ", 80, 24);
        assert_eq!(
            terminal.rows(Want::Last { cursor: true }).cursor,
            Some((1, 10))
        );
        assert_eq!(terminal.rows(LAST).cursor, None);
        // Above the screen, the scrollback lines count.
        let mut terminal = after(format!("{}ab", seq(30)).as_bytes(), 80, 5);
        assert_eq!(
            terminal.rows(Want::Last { cursor: true }).cursor,
            Some((14, 2))
        );
        // Under the first 15 rows of the screen.
        let mut terminal = after(format!("{}ab", seq(20)).as_bytes(), 80, 24);
        assert_eq!(
            terminal.rows(Want::Last { cursor: true }).cursor,
            Some((14, 2))
        );
        // A program can hide the cursor, or move it above the last rows.
        terminal.process(b"\x1b[?25l");
        assert_eq!(terminal.rows(Want::Last { cursor: true }).cursor, None);
        let mut terminal = after(format!("{}\x1b[H", seq(20)).as_bytes(), 80, 24);
        assert_eq!(terminal.rows(Want::Last { cursor: true }).cursor, None);
    }

    #[test]
    fn keys_send_what_xterm_sends() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let shift = KeyModifiers::SHIFT;
        let cases: &[(KeyEvent, bool, &[u8])] = &[
            (key(KeyCode::Char('a'), none), false, b"a"),
            (key(KeyCode::Char('A'), shift), false, b"A"),
            (
                key(KeyCode::Char('\u{e9}'), none),
                false,
                "\u{e9}".as_bytes(),
            ),
            (key(KeyCode::Char('c'), ctrl), false, b"\x03"),
            (key(KeyCode::Char('C'), ctrl | shift), false, b"\x03"),
            (key(KeyCode::Char(' '), ctrl), false, b"\x00"),
            (key(KeyCode::Char('4'), ctrl), false, b"\x1c"),
            (key(KeyCode::Char('7'), ctrl), false, b"\x1f"),
            (key(KeyCode::Char('1'), ctrl), false, b"1"),
            (key(KeyCode::Char('x'), alt), false, b"\x1bx"),
            (key(KeyCode::Char('x'), alt | ctrl), false, b"\x1b\x18"),
            (key(KeyCode::Enter, none), false, b"\r"),
            (key(KeyCode::Enter, alt), false, b"\x1b\r"),
            (key(KeyCode::Tab, none), false, b"\t"),
            (key(KeyCode::BackTab, shift), false, b"\x1b[Z"),
            (key(KeyCode::Backspace, none), false, b"\x7f"),
            (key(KeyCode::Esc, none), false, b"\x1b"),
            (key(KeyCode::Up, none), false, b"\x1b[A"),
            (key(KeyCode::Up, none), true, b"\x1bOA"),
            (key(KeyCode::Down, ctrl), true, b"\x1b[1;5B"),
            (key(KeyCode::Right, shift), false, b"\x1b[1;2C"),
            (key(KeyCode::Left, alt), false, b"\x1b[1;3D"),
            (key(KeyCode::Home, none), false, b"\x1b[H"),
            (key(KeyCode::End, none), true, b"\x1bOF"),
            (key(KeyCode::End, ctrl | shift), false, b"\x1b[1;6F"),
            (key(KeyCode::F(1), none), false, b"\x1bOP"),
            (key(KeyCode::F(4), none), true, b"\x1bOS"),
            (key(KeyCode::F(2), ctrl), false, b"\x1b[1;5Q"),
            (key(KeyCode::F(5), none), false, b"\x1b[15~"),
            (key(KeyCode::F(6), none), false, b"\x1b[17~"),
            (key(KeyCode::F(10), none), false, b"\x1b[21~"),
            (key(KeyCode::F(11), none), false, b"\x1b[23~"),
            (key(KeyCode::F(12), shift), false, b"\x1b[24;2~"),
            (key(KeyCode::F(13), none), false, b""),
            (key(KeyCode::Insert, none), false, b"\x1b[2~"),
            (
                key(KeyCode::Delete, alt | ctrl | shift),
                false,
                b"\x1b[3;8~",
            ),
            (key(KeyCode::PageUp, none), false, b"\x1b[5~"),
            (key(KeyCode::PageDown, none), false, b"\x1b[6~"),
            (key(KeyCode::CapsLock, none), false, b""),
        ];
        for (key, application_cursor, bytes) in cases {
            assert_eq!(
                key_bytes(key, *application_cursor),
                *bytes,
                "{key:?} {application_cursor}"
            );
        }
    }

    #[test]
    fn ctrl_gives_the_control_byte_of_each_character_that_has_one() {
        let cases = [
            ('a', 1),
            ('z', 26),
            ('@', 0),
            ('2', 0),
            ('[', 0x1b),
            ('3', 0x1b),
            ('\\', 0x1c),
            (']', 0x1d),
            ('5', 0x1d),
            ('^', 0x1e),
            ('~', 0x1e),
            ('6', 0x1e),
            ('_', 0x1f),
            ('/', 0x1f),
            ('?', 0x7f),
            ('8', 0x7f),
        ];
        for (c, byte) in cases {
            assert_eq!(control(c), Some(byte), "{c}");
        }
        assert_eq!(control('9'), None);
    }

    #[test]
    fn the_terminal_answers_the_queries_of_its_status_cursor_and_attributes() {
        let mut terminal = Terminal::new(80, 24);
        let answers = terminal.process(b"\r\nab\x1b[5n\x1b[6n\x1b[c\x1b[0c");
        assert_eq!(answers, b"\x1b[0n\x1b[2;3R\x1b[?1;2c\x1b[?1;2c");
        // Each read gives its own answers.
        assert_eq!(terminal.process(b"\x1b[6n"), b"\x1b[2;3R");
        // After the last column, the cursor stays in it.
        let mut narrow = Terminal::new(10, 3);
        assert_eq!(narrow.process(b"0123456789\x1b[6n"), b"\x1b[1;10R");
        // Other queries get no answer: DA2, DECXCPR, the colors, the
        // window title.
        let other = terminal.process(b"\x1b[>c\x1b[?6n\x1b[1c\x1b[7n\x1b]11;?\x07\x1b[21t");
        assert_eq!(other, b"");
    }

    #[test]
    fn the_shared_terminal_tells_the_modes_of_the_program() {
        let shared = Shared::new(80, 24);
        let modes = |shared: &Shared| (shared.application_cursor(), shared.bracketed_paste());
        assert_eq!(modes(&shared), (false, false));
        shared.process(b"\x1b[?1h");
        assert_eq!(modes(&shared), (true, false));
        shared.process(b"\x1b[?2004h\x1b[?1l");
        assert_eq!(modes(&shared), (false, true));
        shared.process(b"\x1b[?2004l");
        assert_eq!(modes(&shared), (false, false));
    }

    #[test]
    fn a_bracketed_paste_has_its_marks_and_no_escape_of_its_own() {
        assert_eq!(paste_bytes("a\x1b[201~b\r", false), b"a\x1b[201~b\r");
        assert_eq!(paste_bytes("ab\r", true), b"\x1b[200~ab\r\x1b[201~");
        assert_eq!(
            paste_bytes("a\x1b[201~b\x1b", true),
            b"\x1b[200~a[201~b\x1b[201~"
        );
    }

    #[test]
    fn rows_tell_whether_the_program_is_on_the_alternate_screen() {
        let mut terminal = after(b"a\x1b[?1049hb", 80, 24);
        for want in [LAST, Want::Screen, Want::All] {
            assert!(terminal.rows(want).alternate, "{want:?}");
        }
        terminal.process(b"\x1b[?1049l");
        let rows = terminal.rows(LAST);
        assert_eq!((markup(&rows.lines).as_str(), rows.alternate), ("a", false));
    }

    #[test]
    fn a_run_that_ends_on_the_alternate_screen_shows_both_screens() {
        let mut terminal = after(b"before\r\n\x1b[?1049hon-alt\r\n", 20, 4);
        terminal.end();
        let rows = terminal.rows(LAST);
        let state = (rows.alternate, rows.ended_on_alternate);
        assert_eq!(
            (markup(&rows.lines).as_str(), state),
            ("before", (false, true))
        );
        let all = markup(&terminal.rows(Want::All).lines);
        let mark = "«d:[stepbook: the last screen of the full-screen program]»";
        assert_eq!(all, format!("before\n{mark}\non-alt"));
        // On the main screen, the end changes nothing.
        let mut terminal = after(b"main", 20, 4);
        terminal.end();
        let rows = terminal.rows(Want::All);
        assert_eq!(
            (markup(&rows.lines).as_str(), rows.ended_on_alternate),
            ("main", false)
        );
    }

    #[test]
    fn the_screen_rows_give_each_row_and_the_cursor() {
        let mut terminal = after(format!("{}x\r\ny", seq(6)).as_bytes(), 80, 4);
        let rows = terminal.rows(Want::Screen);
        assert_eq!(markup(&rows.lines), "5\n6\nx\ny");
        assert_eq!(rows.cursor, Some((3, 1)));
        let mut terminal = after(b"a\x1b[?25l", 80, 3);
        let rows = terminal.rows(Want::Screen);
        assert_eq!((markup(&rows.lines).as_str(), rows.cursor), ("a\n\n", None));
    }

    #[test]
    fn all_rows_give_the_scrollback_and_the_screen_without_the_blank_rows_at_the_end() {
        let mut terminal = after(
            format!("{}\r\n\x1b[41m \x1b[0m\r\n", seq(20)).as_bytes(),
            80,
            5,
        );
        let rows = terminal.rows(Want::All);
        let numbers: Vec<_> = (1..=20).map(|i| i.to_string()).collect();
        let expected = format!("{}\n\n«bg=red: »", numbers.join("\n"));
        assert_eq!((markup(&rows.lines), rows.cursor), (expected, None));
        assert_eq!(terminal.parser.screen().scrollback(), 0);
        // A scrollback that is not a whole number of screens.
        let mut terminal = after(seq(10).as_bytes(), 80, 4);
        let numbers: Vec<_> = (1..=10).map(|i| i.to_string()).collect();
        assert_eq!(markup(&terminal.rows(Want::All).lines), numbers.join("\n"));
        // The scrollback keeps 5,000 lines.
        let mut terminal = after(seq(5_010).as_bytes(), 80, 5);
        let rows = terminal.rows(Want::All);
        assert_eq!(rows.lines.len(), 5_004);
        assert_eq!(markup(&rows.lines[..1]), "7");
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
        let rows = terminal.rows(LAST).lines;
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
        assert_eq!(markup(&rows.lines), "a");
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
        let rows = shared.try_rows(LAST).map(|rows| markup(&rows.lines));
        assert_eq!(rows.as_deref(), Some("ab"));
        // Rows of another kind never show.
        assert_eq!(shared.try_rows(Want::Last { cursor: true }), None);
        drop(held);
        shared.process(b"c");
        let held = shared.terminal.lock().unwrap();
        let rows = shared.try_rows(Want::Last { cursor: true });
        assert_eq!(rows.map(|rows| markup(&rows.lines)).as_deref(), Some("abc"));
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
        assert_eq!(markup(&shared.try_rows(LAST).unwrap().lines), "ab");
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
                    Step::Read(bytes) => _ = terminal.process(&bytes),
                    Step::Resize(columns, rows) => terminal.resize(columns, rows),
                }
                terminal.rows(Want::Last { cursor: true });
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
            for line in terminal.rows(Want::Last { cursor: true }).lines {
                for span in &line.spans {
                    prop_assert!(!span.content.chars().any(needs_escape), "{:?}", span);
                }
            }
        }
    }
}
