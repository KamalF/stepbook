use pulldown_cmark::{Alignment, HeadingLevel};
use ratatui::Frame;
use std::ops::Range;

use ratatui::layout::{Constraint, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::merge::MergeStrategy;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use unicode_width::UnicodeWidthChar;

use crate::doc::{Cell, DIM, Document, Item, Kind, WARNING, escaped};

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const ERROR: Style = Style::new().fg(Color::Red);
const BLOCK_GUTTER: Style = Style::new();
const SELECTED: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const GUTTER: &str = "▌ ";
/// The sidebar shows on terminals at least this wide.
const SIDEBAR_SCREEN_WIDTH: u16 = 80;

/// The state of the screen: the document, its file name and the
/// selection.
pub struct View {
    pub doc: Document,
    pub title: String,
    pub selected: Option<usize>,
    pub scroll: usize,
}

impl View {
    pub fn new(doc: Document, title: String) -> Self {
        let selected = (!doc.blocks.is_empty()).then_some(0);
        View {
            doc,
            title,
            selected,
            scroll: 0,
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        let (main, footer) = split_footer(frame.area());
        if let Some(sidebar) = self.sidebar_area(main) {
            self.draw_sidebar(frame, sidebar);
        }
        let area = self.document_area(main);
        let block = self.document_block();
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let page = layout(&self.doc, usize::from(inner.width), self.selected);
        let scroll = self
            .scroll
            .min(max_scroll(&page, usize::from(inner.height)));
        let visible = page.lines.iter().skip(scroll);
        for (y, line) in (inner.y..inner.bottom()).zip(visible) {
            frame.buffer_mut().set_line(inner.x, y, line, inner.width);
        }
        let line = Line::from(vec![
            Span::styled(" NORMAL ", Style::new().add_modifier(Modifier::REVERSED)),
            Span::raw("  ↑↓ move  q quit"),
        ]);
        frame.render_widget(line, footer);
    }

    /// The area of the sidebar, with the border that it shares with the
    /// document view, or `None` on a narrow screen.
    fn sidebar_area(&self, main: Rect) -> Option<Rect> {
        let width = self.sidebar_width();
        (main.width >= SIDEBAR_SCREEN_WIDTH).then_some(Rect {
            width: width + 1,
            ..main
        })
    }

    fn document_area(&self, main: Rect) -> Rect {
        match self.sidebar_area(main) {
            Some(sidebar) => Rect {
                x: main.x + sidebar.width - 1,
                width: main.width - sidebar.width + 1,
                ..main
            },
            None => main,
        }
    }

    fn document_block(&self) -> Block<'static> {
        let mut title = vec![Span::raw(" ")];
        title.extend(escaped(&self.title, Style::new()));
        title.push(Span::raw(" "));
        Block::bordered()
            .title(Line::from(title))
            .padding(Padding::left(1))
            .merge_borders(MergeStrategy::Exact)
    }

    /// The width of the sidebar with its left border: the longest label
    /// and 4 columns, between 12 and 30.
    fn sidebar_width(&self) -> u16 {
        let labels = (0..self.doc.blocks.len()).map(|i| Span::raw(self.doc.label(i)).width());
        let width = labels.max().unwrap_or(0) + 4;
        width.clamp(12, 30) as u16
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(" blocks ")
            .merge_borders(MergeStrategy::Exact);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = usize::from(inner.height);
        let first = match self.selected {
            Some(i) if i >= rows => i + 1 - rows,
            _ => 0,
        };
        let label_width = usize::from(inner.width).saturating_sub(3);
        for (y, i) in (inner.y..inner.bottom()).zip(first..self.doc.blocks.len()) {
            let (marker, style) = if self.selected == Some(i) {
                (Span::styled("▶", SELECTED), SELECTED)
            } else {
                (Span::raw(" "), Style::new())
            };
            let glyph = match self.doc.blocks[i].problem {
                Some(_) => Span::styled("!", ERROR),
                None => Span::raw(" "),
            };
            let mut spans = vec![marker, glyph, Span::raw(" ")];
            spans.extend(truncate(
                &[Span::styled(self.doc.label(i), style)],
                label_width,
            ));
            frame
                .buffer_mut()
                .set_line(inner.x, y, &Line::from(spans), inner.width);
        }
    }

    /// The page and the height of the document view on a screen of
    /// `size`.
    fn page(&self, size: Size) -> (Page, usize) {
        let (main, _) = split_footer(Rect::from((Position::ORIGIN, size)));
        let inner = self.document_block().inner(self.document_area(main));
        let page = layout(&self.doc, usize::from(inner.width), self.selected);
        (page, usize::from(inner.height))
    }

    /// Moves the selection by `step` blocks, and scrolls to show it.
    pub fn select_by(&mut self, step: isize, size: Size) {
        if let Some(i) = self.selected {
            let last = self.doc.blocks.len() - 1;
            self.select(i.saturating_add_signed(step).min(last), size);
        }
    }

    pub fn select_first(&mut self, size: Size) {
        match self.doc.blocks.is_empty() {
            true => self.scroll = 0,
            false => self.select(0, size),
        }
    }

    pub fn select_last(&mut self, size: Size) {
        match self.doc.blocks.len() {
            0 => {
                let (page, height) = self.page(size);
                self.scroll = max_scroll(&page, height);
            }
            n => self.select(n - 1, size),
        }
    }

    fn select(&mut self, i: usize, size: Size) {
        self.selected = Some(i);
        let (page, height) = self.page(size);
        let scroll = self.scroll.min(max_scroll(&page, height));
        self.scroll = scroll_to(scroll, page.tops[i], &page.blocks[i], height);
    }

    /// Scrolls one page down (`pages` = 1) or up (-1). The selection moves
    /// to the first block whose info line is in view, else to the first
    /// block in view.
    pub fn scroll_pages(&mut self, pages: isize, size: Size) {
        let (page, height) = self.page(size);
        let max = max_scroll(&page, height);
        let scroll = self.scroll.min(max);
        self.scroll = scroll
            .saturating_add_signed(pages * height as isize)
            .min(max);
        let view = self.scroll..self.scroll + height;
        let blocks = &page.blocks;
        let in_view = blocks.iter().position(|b| view.contains(&b.start));
        let overlap = || {
            blocks
                .iter()
                .position(|b| b.start < view.end && b.end > view.start)
        };
        if let Some(i) = in_view.or_else(overlap) {
            self.selected = Some(i);
        }
    }
}

fn split_footer(area: Rect) -> (Rect, Rect) {
    let [main, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    (main, footer)
}

/// The largest scroll: the last line of the page at the bottom of the view.
fn max_scroll(page: &Page, height: usize) -> usize {
    page.lines.len().saturating_sub(height)
}

/// The scroll closest to `scroll` that shows the lines from `top` to the
/// end of `block`. If they do not fit in `height`, it shows `top`, or the
/// first line of the block if `top` is too far above it.
fn scroll_to(scroll: usize, top: usize, block: &Range<usize>, height: usize) -> usize {
    if block.end - top > height {
        if block.start - top < height {
            top
        } else {
            block.start
        }
    } else if top < scroll {
        top
    } else if block.end > scroll + height {
        block.end - height
    } else {
        scroll
    }
}

/// The document laid out at a width.
pub struct Page {
    pub lines: Vec<Line<'static>>,
    /// The lines of each block: its info line, then its code.
    pub blocks: Vec<Range<usize>>,
    /// The first line of the paragraph above each block, or the first line
    /// of the block if no paragraph is right above it.
    pub tops: Vec<usize>,
}

/// The lines of the document view at `width` columns. The selected block
/// gets a highlighted gutter.
pub fn layout(doc: &Document, width: usize, selected: Option<usize>) -> Page {
    let mut lines = Vec::new();
    let mut blocks = vec![0..0; doc.blocks.len()];
    let mut tops = vec![0; doc.blocks.len()];
    // The first line of the last item that is not blank, if it is text.
    let mut paragraph = None;
    for item in &doc.items {
        let start = lines.len();
        let prefix_width = item.rest.iter().map(Span::width).sum::<usize>();
        let width = width.saturating_sub(prefix_width).max(1);
        let body = match &item.kind {
            Kind::Text(text) => wrap(text, width),
            Kind::Heading(level, text) => {
                let mut body = wrap(text, width);
                let rule = match level {
                    HeadingLevel::H1 => "━",
                    HeadingLevel::H2 => "─",
                    _ => "",
                };
                if !rule.is_empty() {
                    body.push(Line::from(Span::styled(rule.repeat(width), DIM)));
                }
                body
            }
            Kind::Table(aligns, rows) => table(aligns, rows, width),
            Kind::Code { lines, block } => code(doc, lines, *block, selected, width),
            Kind::Rule => vec![Line::from(Span::styled("─".repeat(width), DIM))],
            Kind::Blank => vec![Line::default()],
        };
        lines.extend(prefixed(item, body));
        match item.kind {
            Kind::Code { block: Some(i), .. } => {
                blocks[i] = start..lines.len();
                tops[i] = paragraph.unwrap_or(start);
            }
            Kind::Blank => continue,
            _ => {}
        }
        paragraph = matches!(item.kind, Kind::Text(_)).then_some(start);
    }
    Page {
        lines,
        blocks,
        tops,
    }
}

/// Puts the item's prefixes before the lines of its body.
fn prefixed(item: &Item, body: Vec<Line<'static>>) -> impl Iterator<Item = Line<'static>> {
    body.into_iter().enumerate().map(|(i, line)| {
        let prefix = if i == 0 { &item.first } else { &item.rest };
        let mut spans = prefix.clone();
        spans.extend(line.spans);
        Line::from(spans)
    })
}

/// Draws a code block with a gutter. A block also gets an info line: its
/// label, its problem and its warnings. Long lines wrap at the width, so
/// that the view shows all the code that runs.
fn code(
    doc: &Document,
    lines: &[Vec<Span<'static>>],
    block: Option<usize>,
    selected: Option<usize>,
    width: usize,
) -> Vec<Line<'static>> {
    let style = match block {
        None => DIM,
        Some(_) if block == selected => SELECTED,
        Some(_) => BLOCK_GUTTER,
    };
    let gutter = Span::styled(GUTTER, style);
    let width = width.saturating_sub(GUTTER.chars().count()).max(1);
    let mut out = Vec::new();
    if let Some(i) = block {
        let mut info = vec![Span::styled(doc.label(i), BOLD)];
        if let Some(problem) = doc.blocks[i].problem {
            info.extend([Span::raw("  "), Span::styled(problem, ERROR)]);
        }
        let warnings = &doc.blocks[i].warnings;
        if !warnings.is_empty() {
            info.push(Span::raw("  "));
            info.extend(escaped(&warnings.join("; "), WARNING));
        }
        out.push(Line::from(
            [vec![gutter.clone()], truncate(&info, width)].concat(),
        ));
    }
    for line in lines {
        for part in hard_wrap(line, width) {
            out.push(Line::from([vec![gutter.clone()], part].concat()));
        }
    }
    out
}

/// Draws a table with box-drawing borders. If the table is wider than
/// `width`, the widest columns shrink, and their cells end with `…`.
fn table(aligns: &[Alignment], rows: &[Vec<Cell>], width: usize) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1; columns];
    for row in rows {
        for (cell, w) in row.iter().zip(&mut widths) {
            *w = (*w).max(cell.iter().map(Span::width).sum());
        }
    }
    // Each column takes a border and a space on each side of its text.
    let room = width.saturating_sub(3 * columns + 1);
    while widths.iter().sum::<usize>() > room {
        let widest = widths.iter_mut().max().unwrap();
        if *widest == 1 {
            break;
        }
        *widest -= 1;
    }
    let border = |left: &str, middle: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line::from(Span::styled(
            format!("{left}{}{right}", parts.join(middle)),
            DIM,
        ))
    };
    let mut lines = vec![border("┌", "┬", "┐")];
    for (i, row) in rows.iter().enumerate() {
        let mut spans = vec![Span::styled("│", DIM)];
        for (column, w) in widths.iter().enumerate() {
            let mut cell = truncate(row.get(column).map_or(&[][..], Vec::as_slice), *w);
            if i == 0 {
                cell = cell
                    .into_iter()
                    .map(|s| s.patch_style(Modifier::BOLD))
                    .collect();
            }
            let gap = w - cell.iter().map(Span::width).sum::<usize>();
            let before = match aligns.get(column) {
                Some(Alignment::Center) => gap / 2,
                Some(Alignment::Right) => gap,
                _ => 0,
            };
            spans.push(Span::raw(" ".repeat(before + 1)));
            spans.extend(cell);
            spans.push(Span::raw(" ".repeat(gap - before + 1)));
            spans.push(Span::styled("│", DIM));
        }
        lines.push(Line::from(spans));
        if i == 0 {
            lines.push(border("├", "┼", "┤"));
        }
    }
    lines.push(border("└", "┴", "┘"));
    lines
}

/// Cuts styled text to `width` columns. Cut text ends with `…`.
fn truncate(text: &[Span], width: usize) -> Vec<Span<'static>> {
    let mut chars: Vec<Styled> = text
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    if chars.iter().map(|(c, _)| char_width(*c)).sum::<usize>() > width {
        let mut used = 0;
        let keep = chars
            .iter()
            .take_while(|(c, _)| {
                used += char_width(*c);
                used < width
            })
            .count();
        let style = chars.get(keep).map_or_else(Style::new, |(_, s)| *s);
        chars.truncate(keep);
        chars.push(('…', style));
    }
    join_styles(chars)
}

/// Wraps styled text at `width` columns, at any character.
fn hard_wrap(text: &[Span], width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut used = 0;
    for (c, style) in text
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
    {
        let w = char_width(c);
        if used + w > width && !line.is_empty() {
            lines.push(join_styles(std::mem::take(&mut line)));
            used = 0;
        }
        line.push((c, style));
        used += w;
    }
    lines.push(join_styles(line));
    lines
}

/// Word-wraps styled text to `width` columns. A `\n` starts a new line.
/// A word longer than the width breaks at the width.
fn wrap(text: &[Span], width: usize) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper {
        width,
        lines: Vec::new(),
        line: Vec::new(),
        line_width: 0,
        word: Vec::new(),
        word_width: 0,
        spaces: Vec::new(),
    };
    for span in text {
        for c in span.content.chars() {
            match c {
                '\n' => {
                    wrapper.end_word();
                    wrapper.spaces.clear();
                    wrapper.break_line();
                }
                ' ' => {
                    wrapper.end_word();
                    wrapper.spaces.push((c, span.style));
                }
                _ => {
                    wrapper.word.push((c, span.style));
                    wrapper.word_width += char_width(c);
                }
            }
        }
    }
    wrapper.end_word();
    wrapper.break_line();
    wrapper.lines
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

type Styled = (char, Style);

struct Wrapper {
    width: usize,
    lines: Vec<Line<'static>>,
    line: Vec<Styled>,
    line_width: usize,
    word: Vec<Styled>,
    word_width: usize,
    /// The spaces before the word. They are dropped at a line break.
    spaces: Vec<Styled>,
}

impl Wrapper {
    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let spaces = std::mem::take(&mut self.spaces);
        let word = std::mem::take(&mut self.word);
        let word_width = std::mem::take(&mut self.word_width);
        if self.line_width + spaces.len() + word_width <= self.width {
            self.line_width += spaces.len();
            self.line.extend(spaces);
        } else if !self.line.is_empty() {
            self.break_line();
        }
        for (c, style) in word {
            let w = char_width(c);
            if self.line_width + w > self.width && !self.line.is_empty() {
                self.break_line();
            }
            self.line.push((c, style));
            self.line_width += w;
        }
    }

    fn break_line(&mut self) {
        let chars = std::mem::take(&mut self.line);
        self.line_width = 0;
        self.lines.push(Line::from(join_styles(chars)));
    }
}

/// Groups characters of the same style into spans.
fn join_styles(chars: Vec<Styled>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(span) if span.style == style => span.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::parse;
    use insta::assert_snapshot;
    use proptest::prelude::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The lines as text, with each styled span as «style:text». Trailing
    /// spaces are trimmed.
    fn markup(lines: &[Line]) -> String {
        let lines: Vec<String> = lines
            .iter()
            .map(|line| {
                let mut out = String::new();
                for span in &line.spans {
                    let tags = style_tags(line.style.patch(span.style));
                    if tags.is_empty() {
                        out.push_str(&span.content);
                    } else {
                        out.push_str(&format!("«{tags}:{}»", span.content));
                    }
                }
                out.trim_end().to_string()
            })
            .collect();
        lines.join("\n")
    }

    fn style_tags(style: Style) -> String {
        let mut tags = Vec::new();
        if let Some(fg) = style.fg.filter(|c| *c != Color::Reset) {
            tags.push(fg.to_string().to_lowercase());
        }
        let modifiers = [
            (Modifier::BOLD, "b"),
            (Modifier::DIM, "d"),
            (Modifier::ITALIC, "i"),
            (Modifier::UNDERLINED, "u"),
            (Modifier::CROSSED_OUT, "s"),
            (Modifier::REVERSED, "r"),
        ];
        for (modifier, tag) in modifiers {
            if style.add_modifier.contains(modifier) {
                tags.push(tag.to_string());
            }
        }
        tags.join(",")
    }

    fn render(markdown: &str, width: usize) -> String {
        markup(&layout(&parse(markdown), width, None).lines)
    }

    #[test]
    fn paragraphs_wrap_at_word_boundaries() {
        let text = "The quick brown fox\njumps over the lazy dog.\n\nSecond paragraph.";
        assert_snapshot!(render(text, 16), @r"
        The quick brown
        fox jumps over
        the lazy dog.

        Second
        paragraph.
        ");
    }

    #[test]
    fn a_hard_break_starts_a_new_line() {
        assert_snapshot!(render("one  \ntwo\\\nthree", 40), @r"
        one
        two
        three
        ");
    }

    #[test]
    fn a_word_longer_than_the_width_breaks_at_the_width() {
        assert_snapshot!(render("ab abcdefghij cd", 4), @r"
        ab
        abcd
        efgh
        ij
        cd
        ");
    }

    #[test]
    fn wide_characters_count_two_columns() {
        assert_snapshot!(render("日本語 日本", 6), @r"
        日本語
        日本
        ");
    }

    #[test]
    fn headings_are_bold_and_h1_and_h2_have_a_rule() {
        assert_snapshot!(render("# One\n## Two\n### Three\ntext", 8), @r"
        «b:One»
        «d:━━━━━━━━»

        «b:Two»
        «d:────────»

        «b:Three»

        text
        ");
    }

    #[test]
    fn lists_indent_two_columns_per_level() {
        let text = "- one\n  - two\n    - three\n- four\n\nafter";
        assert_snapshot!(render(text, 40), @"
        • one
          ◦ two
            • three
        • four

        after
        ");
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        assert_snapshot!(render("- one two three\n\n1. four five six", 10), @"
        • one two
          three

        1. four
           five
           six
        ");
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let text = "9. nine\n10. ten\n    1. nested\n11. eleven";
        assert_snapshot!(render(text, 40), @"
        9. nine
        10. ten
            1. nested
        11. eleven
        ");
    }

    #[test]
    fn task_items_show_their_box() {
        assert_snapshot!(render("- [ ] todo\n- [x] done", 40), @"
        • [ ] todo
        • [x] done
        ");
    }

    #[test]
    fn loose_lists_keep_a_blank_line_between_items() {
        assert_snapshot!(render("- one\n\n  more\n\n- two\n", 40), @"
        • one

          more

        • two
        ");
    }

    #[test]
    fn blockquotes_have_a_bar_per_level() {
        let text = "> one two three\n>\n> > nested\n>\n> - item\n\nafter";
        assert_snapshot!(render(text, 12), @"
        «d:│ »one two
        «d:│ »three
        «d:│»
        «d:│ »«d:│ »nested
        «d:│»
        «d:│ »• item

        after
        ");
    }

    #[test]
    fn prefixes_keep_the_innermost_10_levels() {
        let quotes = render(&format!("{} x", ">".repeat(12)), 80);
        assert_eq!(quotes, format!("{}x", "«d:│ »".repeat(10)));
        let list: String = (0..12)
            .map(|i| format!("{}- {i}\n", "  ".repeat(i)))
            .collect();
        let last = render(&list, 80).lines().last().unwrap().to_string();
        assert_eq!(last, format!("{}◦ 11", "  ".repeat(9)));
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let quotes = layout(&parse(&format!("{} x\n", ">".repeat(100_000))), 80, None);
        assert_eq!(quotes.lines.len(), 1);
        let list: String = (0..1_000)
            .map(|i| format!("{}- a\n", "  ".repeat(i)))
            .collect();
        assert_eq!(layout(&parse(&list), 80, None).lines.len(), 1_000);
    }

    #[test]
    fn tables_have_borders_a_bold_header_and_aligned_columns() {
        let text = "| Left | Center | Right |\n|:--|:-:|--:|\n| a | b | c |\n| long cell | `x` |";
        assert_snapshot!(render(text, 60), @"
        «d:┌───────────┬────────┬───────┐»
        «d:│» «b:Left»      «d:│» «b:Center» «d:│» «b:Right» «d:│»
        «d:├───────────┼────────┼───────┤»
        «d:│» a         «d:│»   b    «d:│»     c «d:│»
        «d:│» long cell «d:│»   «cyan:x»    «d:│»       «d:│»
        «d:└───────────┴────────┴───────┘»
        ");
    }

    #[test]
    fn wide_tables_shrink_their_widest_columns() {
        let text = "| key | a long value here |\n|---|---|\n| k | 日本語のテキスト |";
        assert_snapshot!(render(text, 20), @"
        «d:┌─────┬────────────┐»
        «d:│» «b:key» «d:│» «b:a long va…» «d:│»
        «d:├─────┼────────────┤»
        «d:│» k   «d:│» 日本語の…  «d:│»
        «d:└─────┴────────────┘»
        ");
    }

    #[test]
    fn blocks_show_an_info_line_and_a_gutter() {
        let text = "```bash [name:check]\nls\n```\n\n```sh\n\ncd /\n```";
        assert_snapshot!(render(text, 40), @"
        ▌ «b:check»
        ▌ «blue:ls»

        ▌ «b:#2»
        ▌
        ▌ «blue:cd» /
        ");
    }

    #[test]
    fn bash_code_is_highlighted_with_terminal_colors() {
        let text =
            "```bash\nexport A=\"x $HOME\" # note\nif [ -n 1 ]; then ls -la | grep x; fi\n```";
        assert_snapshot!(render(text, 80), @r#"
        ▌ «b:#1»
        ▌ «magenta:export» «cyan:A»=«green:"x »«cyan:$HOME»«green:"» «d:# note»
        ▌ «magenta:if» «blue:[» -n 1 «blue:]»«magenta:;» «magenta:then» «blue:ls» -la «magenta:|» «blue:grep» x«magenta:;» «magenta:fi»
        "#);
    }

    #[test]
    fn other_code_blocks_have_a_dim_gutter_and_no_highlighting() {
        assert_snapshot!(render("```python\nif x: 'a'\n```\n\n    ls '$x'", 40), @"
        «d:▌ »if x: 'a'

        «d:▌ »ls '$x'
        ");
    }

    #[test]
    fn long_code_lines_wrap_so_that_all_the_code_shows() {
        assert_snapshot!(render("```bash\nrm -rf build/output/folder\n```", 12), @"
        ▌ «b:#1»
        ▌ «blue:rm» -rf bui
        ▌ ld/output/
        ▌ folder
        ");
    }

    #[test]
    fn info_lines_show_problems_and_warnings() {
        let text = "```bash [name:a x:1]\n```\n```bash [name:a]\n```\n```bash [name:\"b c\"]\n```";
        assert_snapshot!(render(text, 60), @"
        ▌ «b:a»  «red:duplicate name»  «yellow:unknown attribute 'x'»

        ▌ «b:a»  «red:duplicate name»

        ▌ «b:#3»  «yellow:invalid name 'b c'»
        ");
    }

    #[test]
    fn code_in_a_list_keeps_the_list_indent() {
        assert_snapshot!(render("- step:\n\n  ```bash [name:s]\n  ls\n  ```", 40), @"
        • step:

          ▌ «b:s»
          ▌ «blue:ls»
        ");
    }

    #[test]
    fn thematic_breaks_are_dim_rules() {
        assert_snapshot!(render("a\n\n***\n\n> b\n>\n> ---", 10), @"
        a

        «d:──────────»

        «d:│ »b
        «d:│»
        «d:│ »«d:────────»
        ");
    }

    #[test]
    fn raw_html_shows_dimmed_as_source() {
        assert_snapshot!(render("<div>\n  <b>x</b>\n</div>\n\nan <i>inline</i> tag", 40), @"
        «d:<div>»
        «d:  <b>x</b>»
        «d:</div>»

        an «d:<i>»inline«d:</i>» tag
        ");
    }

    #[test]
    fn images_show_their_alt_text() {
        assert_snapshot!(render("see ![a *chart*](c.png) here", 40), @"see [image: a «i:chart»] here");
    }

    #[test]
    fn frontmatter_shows_dimmed_before_the_document() {
        assert_snapshot!(render("---\ntitle: x\n---\n# T", 20), @"
        «d:---»
        «d:title: x»
        «d:---»

        «b:T»
        «d:━━━━━━━━━━━━━━━━━━━━»
        ");
    }

    #[test]
    fn control_characters_in_code_show_escaped_with_a_warning() {
        let text = "```bash\necho \u{1b}[31mred\u{7f} \u{9b}x\n```";
        assert_snapshot!(render(text, 60), @"
        ▌ «b:#1»  «yellow:control characters in code»
        ▌ «blue:echo» «yellow:^[»«magenta:[»31mred«yellow:^?» «yellow:<9b>»x
        ");
    }

    #[test]
    fn control_characters_in_text_show_escaped() {
        let text = "# T\u{1b}\n\na\u{1b}]52;c;eA==\u{7}b \u{85}c\n\n| \u{1b} |\n|---|";
        assert_snapshot!(render(text, 60), @"
        «b:T»«yellow,b:^[»
        «d:━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━»

        a«yellow:^[»]52;c;eA==«yellow:^G»b «yellow:<85>»c

        «d:┌────┐»
        «d:│» «yellow,b:^[» «d:│»
        «d:├────┤»
        «d:└────┘»
        ");
    }

    #[test]
    fn control_characters_in_attributes_show_escaped() {
        assert_snapshot!(render("```bash [name:\"x\u{1b}y\"]\n```", 60), @"▌ «b:#1»  «yellow:invalid name 'x^[y'»");
    }

    #[test]
    fn tabs_in_code_expand_to_8_columns() {
        assert_snapshot!(render("```\na\tb\n\tc\n```", 40), @"
        «d:▌ »a       b
        «d:▌ »        c
        ");
    }

    fn markdown() -> impl Strategy<Value = String> {
        let parts = prop_oneof![
            any::<String>(),
            Just("\n".to_string()),
            Just("```bash [name:\"a\u{1b}\"]\n".to_string()),
            Just("```\n".to_string()),
            Just("> ".to_string()),
            Just("- ".to_string()),
            Just("| a | b |\n|---|---|\n".to_string()),
            Just("# ".to_string()),
            Just("<b>".to_string()),
            Just("\t".to_string()),
            Just("\u{1b}[2K\r".to_string()),
            Just("\u{9b}".to_string()),
            Just("---\n".to_string()),
            Just("    ".to_string()),
            Just("![a](b)".to_string()),
        ];
        prop::collection::vec(parts, 0..20).prop_map(|parts| parts.concat())
    }

    proptest! {
        #[test]
        fn no_control_character_reaches_the_view(text in markdown(), width in 1..60usize) {
            for line in layout(&parse(&text), width, None).lines {
                for span in &line.spans {
                    prop_assert!(!span.content.chars().any(char::is_control), "{:?}", span);
                }
            }
        }
    }

    /// The screen of `view` at `width` by `height`, in the format of
    /// `markup`.
    fn screen_with_footer(view: &View, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let lines: Vec<Line> = (0..height)
            .map(|y| {
                let cells = (0..width).map(|x| &buffer[(x, y)]);
                let chars = cells.flat_map(|c| c.symbol().chars().map(|ch| (ch, c.style())));
                Line::from(join_styles(chars.collect()))
            })
            .collect();
        markup(&lines)
    }

    /// The screen without its footer, which changes with each milestone.
    fn screen(view: &View, width: u16, height: u16) -> String {
        let screen = screen_with_footer(view, width, height);
        screen.rsplit_once('\n').unwrap().0.to_string()
    }

    fn view(markdown: &str) -> View {
        View::new(parse(markdown), "run.md".into())
    }

    const BLOCKS: &str = "# Restart\n\nWhy: apply the config.\n\n```bash [name:check]\nls\n```\n\n```bash [name:restart]\nsystemctl restart api\n```\n\n```bash\necho\n```\n\n```bash [name:check]\n```\n";

    #[test]
    fn the_screen_has_a_sidebar_a_document_view_and_a_footer() {
        assert_snapshot!(screen_with_footer(&view(BLOCKS), 80, 16), @"
        ┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐
        │«cyan,b:▶»«red:!» «cyan,b:check»   │ «b:Restart»                                                          │
        │   restart │ «d:━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━»│
        │   #3      │                                                                  │
        │ «red:!» check   │ Why: apply the config.                                           │
        │           │                                                                  │
        │           │ «cyan,b:▌ »«b:check»  «red:duplicate name»                                          │
        │           │ «cyan,b:▌ »«blue:ls»                                                             │
        │           │                                                                  │
        │           │ ▌ «b:restart»                                                        │
        │           │ ▌ «blue:systemctl» restart api                                          │
        │           │                                                                  │
        │           │ ▌ «b:#3»                                                             │
        │           │ ▌ «blue:echo»                                                           │
        └───────────┴──────────────────────────────────────────────────────────────────┘
        «r: NORMAL »  ↑↓ move  q quit
        ");
    }

    #[test]
    fn the_selected_block_has_a_highlighted_gutter_and_sidebar_row() {
        let mut view = view(BLOCKS);
        view.selected = Some(1);
        assert_snapshot!(screen(&view, 80, 13), @"
        ┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐
        │ «red:!» check   │ «b:Restart»                                                          │
        │«cyan,b:▶»  «cyan,b:restart» │ «d:━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━»│
        │   #3      │                                                                  │
        │ «red:!» check   │ Why: apply the config.                                           │
        │           │                                                                  │
        │           │ ▌ «b:check»  «red:duplicate name»                                          │
        │           │ ▌ «blue:ls»                                                             │
        │           │                                                                  │
        │           │ «cyan,b:▌ »«b:restart»                                                        │
        │           │ «cyan,b:▌ »«blue:systemctl» restart api                                          │
        └───────────┴──────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_sidebar_hides_under_80_columns() {
        assert_snapshot!(screen(&view(BLOCKS), 79, 8), @"
        ┌ run.md ─────────────────────────────────────────────────────────────────────┐
        │ «b:Restart»                                                                     │
        │ «d:━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━»│
        │                                                                             │
        │ Why: apply the config.                                                      │
        │                                                                             │
        └─────────────────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_sidebar_width_follows_the_longest_name_up_to_30() {
        let long = format!("```bash [name:{}]\n```\n", "n".repeat(40));
        let rows = |markdown: &str| {
            screen(&view(markdown), 80, 3)
                .lines()
                .next()
                .unwrap()
                .to_string()
        };
        assert_snapshot!(rows("```bash [name:a]\n```\n"), @"┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐");
        assert_snapshot!(rows("```bash [name:abcdefghijklmnop]\n```\n"), @"┌ blocks ───────────┬ run.md ──────────────────────────────────────────────────┐");
        assert_snapshot!(screen(&view(&long), 80, 4), @"
        ┌ blocks ─────────────────────┬ run.md ────────────────────────────────────────┐
        │«cyan,b:▶»  «cyan,b:nnnnnnnnnnnnnnnnnnnnnnnnn…»│ «cyan,b:▌ »«b:nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn»     │
        └─────────────────────────────┴────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_sidebar_scrolls_to_keep_the_selection_visible() {
        let mut view = view(&"```bash\n```\n".repeat(10));
        view.selected = Some(7);
        assert_snapshot!(screen(&view, 80, 6), @"
        ┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐
        │   #6      │ ▌ «b:#1»                                                             │
        │   #7      │                                                                  │
        │«cyan,b:▶»  «cyan,b:#8»      │ ▌ «b:#2»                                                             │
        └───────────┴──────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_file_name_shows_control_characters_escaped() {
        let view = View::new(parse(""), "a\u{1b}]52;c;x\u{7}\nb.md".into());
        assert_snapshot!(screen(&view, 40, 3), @"
        ┌ a«yellow:^[»]52;c;x«yellow:^G^J»b.md ──────────────────┐
        └──────────────────────────────────────┘
        ");
    }

    #[test]
    fn scroll_to_moves_as_little_as_possible() {
        // (scroll, top, block, height) -> scroll
        let cases = [
            ((5, 6, 8..10, 10), 5),
            ((5, 2, 3..6, 10), 2),
            ((0, 12, 14..20, 10), 10),
            ((0, 12, 14..30, 10), 12),
            ((0, 2, 14..30, 10), 14),
        ];
        for ((scroll, top, block, height), expected) in cases {
            let found = scroll_to(scroll, top, &block, height);
            assert_eq!(found, expected, "{scroll} {top} {block:?} {height}");
        }
    }

    /// Four steps of 9 lines: a heading and its rule, a blank line, a
    /// paragraph, a blank line, a block of 3 lines and a blank line. Step
    /// i starts at line 9 * i, and its block at line 9 * i + 5. The view
    /// shows 6 lines.
    fn steps() -> View {
        let step =
            |i| format!("## Step {i}\n\nWhy: step {i}.\n\n```bash [name:s{i}]\nls\nls\n```\n\n");
        view(&(1..=4).map(step).collect::<String>())
    }

    const SMALL: Size = Size::new(60, 9);

    fn position(view: &View) -> (Option<usize>, usize) {
        (view.selected, view.scroll)
    }

    #[test]
    fn moving_down_selects_the_next_block_and_shows_its_paragraph() {
        let mut view = steps();
        view.select_by(1, SMALL);
        assert_eq!(position(&view), (Some(1), 11));
        assert_snapshot!(screen(&view, 60, 9), @"
        ┌ run.md ──────────────────────────────────────────────────┐
        │                                                          │
        │ Why: step 2.                                             │
        │                                                          │
        │ «cyan,b:▌ »«b:s2»                                                     │
        │ «cyan,b:▌ »«blue:ls»                                                     │
        │ «cyan,b:▌ »«blue:ls»                                                     │
        └──────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn moving_up_shows_the_paragraph_above_the_block() {
        let mut view = steps();
        view.scroll = 20;
        view.select_by(-1, SMALL);
        assert_eq!(position(&view), (Some(0), 3));
    }

    #[test]
    fn moving_stops_at_the_first_and_the_last_block() {
        let mut view = steps();
        view.select_by(-1, SMALL);
        assert_eq!(view.selected, Some(0));
        view.select_by(9, SMALL);
        assert_eq!(view.selected, Some(3));
    }

    #[test]
    fn home_and_end_select_the_first_and_the_last_block() {
        let mut view = steps();
        view.select_last(SMALL);
        assert_eq!(position(&view), (Some(3), 29));
        view.select_first(SMALL);
        assert_eq!(position(&view), (Some(0), 3));
    }

    #[test]
    fn page_keys_scroll_one_page_and_select_the_first_block_in_view() {
        let mut view = steps();
        // Lines 6 to 11: no info line, and the code of block 0.
        view.scroll_pages(1, SMALL);
        assert_eq!(position(&view), (Some(0), 6));
        view.scroll_pages(1, SMALL);
        assert_eq!(position(&view), (Some(1), 12));
        view.scroll_pages(9, SMALL);
        assert_eq!(position(&view), (Some(3), 29));
        view.scroll_pages(-9, SMALL);
        assert_eq!(position(&view), (Some(0), 0));
    }

    #[test]
    fn without_blocks_home_and_end_scroll_to_the_top_and_the_bottom() {
        let mut view = view(&"text\n\n".repeat(20));
        view.select_last(SMALL);
        // 20 lines of text and 19 blank lines, in a view of 6 lines.
        assert_eq!(position(&view), (None, 33));
        view.select_first(SMALL);
        assert_eq!(position(&view), (None, 0));
    }

    #[test]
    #[ignore = "reads the runbooks of ~/dev/claude/runbooks, or of $STEPBOOK_RUNBOOKS"]
    fn each_runbook_opens_without_a_panic() {
        let dir = std::env::var_os("STEPBOOK_RUNBOOKS").map_or_else(
            || std::path::Path::new(&std::env::var_os("HOME").unwrap()).join("dev/claude/runbooks"),
            std::path::PathBuf::from,
        );
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
            for (width, height) in [(20, 5), (79, 24), (120, 40)] {
                let size = Size::new(width, height);
                let mut view = View::new(parse(&text), path.display().to_string());
                screen_with_footer(&view, width, height);
                view.select_last(size);
                view.scroll_pages(-1, size);
                view.select_by(-1, size);
                screen_with_footer(&view, width, height);
            }
            count += 1;
        }
        println!("{count} runbooks in {}", dir.display());
        assert!(count > 0);
    }

    #[test]
    fn inline_styles_use_terminal_styles() {
        let text = "*em* **strong** ~~gone~~ `code` [link](http://x) ***both***";
        assert_snapshot!(render(text, 60), @"«i:em» «b:strong» «s:gone» «cyan:code» «u:link» «b,i:both»");
    }
}
