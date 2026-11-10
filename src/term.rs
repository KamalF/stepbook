//! The terminal of a run: a vt100 screen, and its rows as lines of the
//! view (spec 6.3).
use std::num::NonZeroU16;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::doc::escaped;

/// The lines of scrollback of a run (spec 6.3).
const SCROLLBACK: usize = 5_000;

/// The rows of output under a block (spec 6.3).
pub const OUTPUT_ROWS: usize = 15;

/// The screen of a run. Program output reaches the outer terminal only
/// through it, as text and styles: never as escape sequences (spec 6.4).
pub struct Terminal {
    parser: vt100::Parser,
}

fn size(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

impl Terminal {
    pub fn new(columns: u16, rows: u16) -> Terminal {
        Terminal {
            parser: vt100::Parser::new(size(rows), size(columns), SCROLLBACK),
        }
    }

    /// Reads the output of a run. vt100 keeps the start of a UTF-8
    /// character that ends in the next read (upmd issue K18).
    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.parser.set_size(size(rows), size(columns));
    }

    /// The last `count` rows of the terminal, scrollback included, without
    /// the blank rows at the end. If `cursor`, the rows go down to the row
    /// of the cursor at least: a running program writes there next.
    pub fn last_rows(&mut self, count: usize, cursor: bool) -> Vec<Line<'static>> {
        let screen = self.parser.screen();
        let (rows, columns) = screen.size();
        let (rows, columns) = (rows.get(), columns.get());
        let mut end = (0..rows)
            .rev()
            .find(|&row| !is_blank(screen, row, columns))
            .map_or(0, |row| row + 1);
        if cursor {
            end = end.max(screen.cursor_position().0 + 1);
        }
        let mut lines: Vec<_> = (0..end).map(|row| line(screen, row, columns)).collect();
        if lines.len() < count {
            // The scrollback shows above the screen. With an offset of n,
            // the first row of the screen shows the n-th line above it.
            let screen = self.parser.screen_mut();
            let above = (count - lines.len()).min(screen.scrollback_len());
            let mut all = Vec::with_capacity(count);
            for offset in (1..=above).rev() {
                screen.set_scrollback(offset);
                all.push(line(screen, 0, columns));
            }
            screen.set_scrollback(0);
            all.append(&mut lines);
            lines = all;
        }
        lines.split_off(lines.len().saturating_sub(count))
    }
}

/// Whether a row shows nothing: no text, and no background.
fn is_blank(screen: &vt100::Screen, row: u16, columns: u16) -> bool {
    (0..columns).all(|column| {
        screen
            .cell(row, column)
            .is_none_or(|cell| !cell.has_contents() && style(cell) == Style::new())
    })
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
        .rposition(|cell| cell.has_contents() || style(cell) != Style::new())
        .map_or(0, |i| i + 1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for cell in &cells[..end] {
        let text = if cell.has_contents() {
            cell.contents()
        } else {
            " "
        };
        for span in escaped(text, style(cell)) {
            match spans.last_mut() {
                Some(last) if last.style == span.style => {
                    last.content.to_mut().push_str(&span.content)
                }
                _ => spans.push(span),
            }
        }
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
        markup(&terminal.last_rows(OUTPUT_ROWS, cursor))
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
    fn a_resize_changes_the_size_of_the_screen() {
        let mut terminal = Terminal::new(80, 24);
        terminal.resize(10, 3);
        terminal.process(b"0123456789abc");
        assert_eq!(rows(&mut terminal, false), "0123456789\nabc");
    }

    proptest! {
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
            for line in terminal.last_rows(15, true) {
                for span in &line.spans {
                    prop_assert!(!span.content.chars().any(needs_escape), "{:?}", span);
                }
            }
        }
    }
}
