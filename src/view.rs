use pulldown_cmark::{Alignment, HeadingLevel};
use ratatui::Frame;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use ratatui::buffer::CellWidth;
use ratatui::layout::{Constraint, Layout, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::merge::MergeStrategy;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};

use crate::doc::{Cell, DIM, Document, Item, Kind, WARNING, escaped};

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const ERROR: Style = Style::new().fg(Color::Red);
const BLOCK_GUTTER: Style = Style::new();
const SELECTED: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
/// "▐" has an East Asian width of N, so every terminal draws it 1 column
/// wide. "▌" has an ambiguous width: a terminal set to draw it wide
/// would shift the code after it.
const GUTTER: &str = "▐ ";
/// The second column of the gutter on a row that goes on from a wrapped
/// line of code or info line. Text never reaches the gutter, so it cannot
/// fake the mark.
const WRAP_MARK: &str = "↪";
/// The sidebar shows on terminals at least this wide.
const SIDEBAR_SCREEN_WIDTH: u16 = 80;

/// The state of the screen: the document, its file name and the
/// selection.
pub struct View {
    pub doc: Document,
    pub title: String,
    pub selected: Option<usize>,
    pub scroll: usize,
    /// The layout of the document at the last width: each key and each
    /// frame needs it, and 1 MB of tabs in code takes 0.9 s to lay out.
    page: RefCell<Option<Rc<Page>>>,
}

impl View {
    pub fn new(doc: Document, title: String) -> Self {
        let selected = (!doc.blocks.is_empty()).then_some(0);
        View {
            doc,
            title,
            selected,
            scroll: 0,
            page: RefCell::default(),
        }
    }

    pub fn draw(&self, frame: &mut Frame) {
        if too_small(frame.area().as_size()) {
            let message = "Enlarge the terminal to at least 40 columns and 8 rows.";
            let text = Paragraph::new(message).wrap(Wrap { trim: true });
            frame.render_widget(text, frame.area());
            return;
        }
        let (sidebar, document, footer) = self.areas(frame.area());
        if let Some(sidebar) = sidebar {
            self.draw_sidebar(frame, sidebar);
        }
        frame.render_widget(self.document_block(), document);
        let (page, text, scroll) = self.page(frame.area());
        let visible = page.lines.iter().enumerate().skip(scroll);
        for (y, (i, line)) in (text.y..text.bottom()).zip(visible) {
            let line = match self.selected {
                Some(block) if page.blocks[block].contains(&i) => {
                    let mut line = line.clone();
                    line.spans[page.gutters[block]].style = SELECTED;
                    line
                }
                _ => line.clone(),
            };
            frame.buffer_mut().set_line(text.x, y, &line, text.width);
        }
        let line = Line::from(vec![
            Span::styled(" NORMAL ", Style::new().add_modifier(Modifier::REVERSED)),
            Span::raw("  ↑↓ move  q quit"),
        ]);
        frame.render_widget(line, footer);
    }

    /// The areas of the sidebar (none on a narrow screen), of the document
    /// view and of the footer. The sidebar and the document view share the
    /// border between them.
    fn areas(&self, screen: Rect) -> (Option<Rect>, Rect, Rect) {
        let [main, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(screen);
        if main.width < SIDEBAR_SCREEN_WIDTH {
            return (None, main, footer);
        }
        let width = self.sidebar_width();
        let sidebar = Rect {
            width: width + 1,
            ..main
        };
        let document = Rect {
            x: main.x + width,
            width: main.width - width,
            ..main
        };
        (Some(sidebar), document, footer)
    }

    /// The page of the document view on `screen`, the area of its text, and
    /// the scroll, no more than the largest one. The keys and the drawing
    /// both use it, so that they agree on the height of the view.
    fn page(&self, screen: Rect) -> (Rc<Page>, Rect, usize) {
        let (_, document, _) = self.areas(screen);
        let text = self.document_block().inner(document);
        let width = usize::from(text.width);
        let mut cache = self.page.borrow_mut();
        let page = match &*cache {
            Some(page) if page.width == width => page.clone(),
            _ => cache.insert(Rc::new(layout(&self.doc, width))).clone(),
        };
        let scroll = self.scroll.min(max_scroll(&page, usize::from(text.height)));
        (page, text, scroll)
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
        let labels =
            (0..self.doc.blocks.len()).map(|i| text_width(&[Span::raw(self.doc.label(i))]));
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
                let (page, text, _) = self.page(screen(size));
                self.scroll = max_scroll(&page, usize::from(text.height));
            }
            n => self.select(n - 1, size),
        }
    }

    fn select(&mut self, i: usize, size: Size) {
        self.selected = Some(i);
        self.reveal(size);
    }

    /// Scrolls to show the selected block as `j` and `k` do, and returns
    /// whether the view showed it so already. `Enter` runs only such a
    /// block: after the startup or a resize, the selection can be off the
    /// screen.
    pub fn reveal(&mut self, size: Size) -> bool {
        let Some(i) = self.selected else {
            return false;
        };
        let (page, text, scroll) = self.page(screen(size));
        let height = usize::from(text.height);
        self.scroll = scroll_to(scroll, page.tops[i], &page.blocks[i], height);
        self.scroll == scroll
    }

    /// Scrolls one page down (`pages` = 1) or up (-1). The selection moves
    /// to the first block whose info line is in view, else to the first
    /// block in view, but never against the scroll: at the bottom, Page
    /// Down must not select the block above, which Enter would then run.
    pub fn scroll_pages(&mut self, pages: isize, size: Size) {
        let (page, text, scroll) = self.page(screen(size));
        let height = usize::from(text.height);
        self.scroll = scroll
            .saturating_add_signed(pages * height as isize)
            .min(max_scroll(&page, height));
        let view = self.scroll..self.scroll + height;
        let blocks = &page.blocks;
        let in_view = blocks.iter().position(|b| view.contains(&b.start));
        let overlap = || {
            blocks
                .iter()
                .position(|b| b.start < view.end && b.end > view.start)
        };
        if let Some(i) = in_view.or_else(overlap) {
            let backwards = self
                .selected
                .is_some_and(|s| (i as isize - s as isize) * pages < 0);
            if !backwards {
                self.selected = Some(i);
            }
        }
    }
}

/// The smallest screen that stepbook lays the document out on. Below it,
/// code under a list marker has no room, and the layout costs too much: a
/// file of 1 MB of tabs in code took 2.5 GB per frame at 6 columns.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 8;

/// Whether the screen is too small for the document. stepbook then shows
/// only a message, and the keys that move do nothing.
pub fn too_small(size: Size) -> bool {
    size.width < MIN_WIDTH || size.height < MIN_HEIGHT
}

fn screen(size: Size) -> Rect {
    Rect::from((Position::ORIGIN, size))
}

/// The largest scroll: the last line of the page at the bottom of the view.
fn max_scroll(page: &Page, height: usize) -> usize {
    page.lines.len().saturating_sub(height)
}

/// The scroll closest to `scroll` that shows the lines from `top` to the
/// end of `block`. If they do not fit in `height`, it shows the whole
/// block, with the end of the paragraph above it. A block taller than the
/// view shows from `top`, or from its first line if `top` is too far above.
fn scroll_to(scroll: usize, top: usize, block: &Range<usize>, height: usize) -> usize {
    if block.end - top > height {
        if block.len() <= height {
            block.end - height
        } else if block.start - top < height {
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
    pub width: usize,
    pub lines: Vec<Line<'static>>,
    /// The lines of each block: its info line, then its code.
    pub blocks: Vec<Range<usize>>,
    /// The first line of the paragraph above each block, or the first line
    /// of the block if no paragraph is right above it.
    pub tops: Vec<usize>,
    /// The index of the gutter's span in the lines of each block, after the
    /// prefixes. The view draws the gutter of the selected block in the
    /// style of the selection.
    pub gutters: Vec<usize>,
}

/// The lines of the document view at `width` columns.
pub fn layout(doc: &Document, width: usize) -> Page {
    let mut lines = Vec::new();
    let mut blocks = vec![0..0; doc.blocks.len()];
    let mut tops = vec![0; doc.blocks.len()];
    let mut gutters = vec![0; doc.blocks.len()];
    // The first line of the last item that is not blank, if it is text.
    let mut paragraph = None;
    for item in &doc.items {
        let start = lines.len();
        let (first, rest) = fitted_prefixes(item, width);
        let width = width.saturating_sub(text_width(rest)).max(1);
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
            Kind::Code { lines, block } => code(doc, lines, *block, width),
            Kind::Source(lines) => lines.iter().flat_map(|l| hard_wrap(l, width)).collect(),
            Kind::Rule => vec![Line::from(Span::styled("─".repeat(width), DIM))],
            Kind::Blank => vec![Line::default()],
        };
        lines.extend(prefixed(first, rest, body));
        match item.kind {
            Kind::Code { block: Some(i), .. } => {
                blocks[i] = start..lines.len();
                tops[i] = paragraph.unwrap_or(start);
                gutters[i] = rest.len();
            }
            Kind::Blank => continue,
            _ => {}
        }
        paragraph = matches!(item.kind, Kind::Text(_)).then_some(start);
    }
    Page {
        width,
        lines,
        blocks,
        tops,
        gutters,
    }
}

/// The prefixes of an item, without the outer levels that would make them
/// wider than half the view: the text keeps room, and the code of a block
/// stays in view. The innermost level always stays, with its marker. Each
/// level is one span in both prefixes.
fn fitted_prefixes(item: &Item, width: usize) -> (&[Span<'static>], &[Span<'static>]) {
    let mut outer = 0;
    while outer + 1 < item.rest.len() && text_width(&item.rest[outer..]) > width / 2 {
        outer += 1;
    }
    (&item.first[outer..], &item.rest[outer..])
}

/// Puts the prefixes before the lines of a body.
fn prefixed<'a>(
    first: &'a [Span<'static>],
    rest: &'a [Span<'static>],
    body: Vec<Line<'static>>,
) -> impl Iterator<Item = Line<'static>> + 'a {
    body.into_iter().enumerate().map(move |(i, line)| {
        let mut spans = if i == 0 { first } else { rest }.to_vec();
        spans.extend(line.spans);
        Line::from(spans)
    })
}

/// Draws a code block with a gutter. A block also gets an info line: its
/// label, its problem and its warnings. The info line and the code wrap
/// at the width, so that the view shows every warning and all the code
/// that runs.
fn code(
    doc: &Document,
    lines: &[Vec<Span<'static>>],
    block: Option<usize>,
    width: usize,
) -> Vec<Line<'static>> {
    let style = match block {
        None => DIM,
        Some(_) => BLOCK_GUTTER,
    };
    let gutter = Span::styled(GUTTER, style);
    // A row that goes on from a wrapped line has a mark in its gutter:
    // without it, a part of a line that starts with "#" would pass for a
    // comment, and a long name for a line of code.
    let wrapped = [Span::styled("▐", style), Span::styled(WRAP_MARK, DIM)];
    let gutter_of = |row: usize| {
        if row == 0 {
            vec![gutter.clone()]
        } else {
            wrapped.to_vec()
        }
    };
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
        for (row, line) in wrap(&info, width).into_iter().enumerate() {
            out.push(Line::from([gutter_of(row), line.spans].concat()));
        }
    }
    for line in lines {
        for (row, part) in hard_wrap(line, width).into_iter().enumerate() {
            out.push(Line::from(
                [gutter_of(row), mark_end_space(part.spans)].concat(),
            ));
        }
    }
    // An empty code block shows as a gutter, so that it, and the list
    // marker before it, stay in view.
    if out.is_empty() {
        out.push(Line::from(gutter));
    }
    out
}

/// The mark of a space that ends a row of code, which would not show: after
/// a "\\", bash runs the next line on its own, and a space at a wrap would
/// glue two words. "␣" has a width of 1 in every terminal, unlike "·".
const SPACE_MARK: &str = "␣";

fn mark_end_space(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    if let Some(last) = spans.last_mut()
        && last.content.ends_with(' ')
    {
        let style = last.style;
        last.content.to_mut().pop();
        if last.content.is_empty() {
            spans.pop();
        }
        spans.push(Span::styled(SPACE_MARK, style.patch(DIM)));
    }
    spans
}

/// Draws a table with box-drawing borders. If the table is wider than
/// `width`, the widest columns shrink, and their cells end with `…`.
fn table(aligns: &[Alignment], rows: &[Vec<Cell>], width: usize) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1; columns];
    for row in rows {
        for (cell, w) in row.iter().zip(&mut widths) {
            *w = (*w).max(text_width(cell));
        }
    }
    // Each column takes a border and a space on each side of its text.
    let room = width.saturating_sub(3 * columns + 1);
    if room < columns {
        widths.fill(1);
    } else {
        // No column keeps more than the room, which bounds the loop.
        let mut total = 0;
        for w in &mut widths {
            *w = (*w).min(room);
            total += *w;
        }
        while total > room {
            *widths.iter_mut().max().unwrap() -= 1;
            total -= 1;
        }
    }
    // The view would clip the columns that start past its width.
    let shown = (widths.iter())
        .scan(1, |start, w| Some(std::mem::replace(start, *start + w + 3)))
        .take_while(|&start| start <= width)
        .count();
    widths.truncate(shown);
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
            let gap = w - text_width(&cell);
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
    let mut kept: Vec<Glyph> = Vec::new();
    let mut used = 0;
    for glyph in glyphs(text).flatten() {
        if used + glyph.width <= width {
            used += glyph.width;
            kept.push(glyph);
            continue;
        }
        // Drop glyphs until the ellipsis fits. It takes the style of the
        // first glyph that does not show.
        let mut first_cut = glyph;
        while used + 1 > width
            && let Some(last) = kept.pop()
        {
            used -= last.width;
            first_cut = last;
        }
        if width > 0 {
            kept.push(Glyph {
                text: "…",
                style: first_cut.style,
                width: 1,
                source: usize::MAX,
            });
        }
        break;
    }
    join_styles(&kept)
}

/// Wraps styled text at `width` columns, at any grapheme.
fn hard_wrap(text: &[Span], width: usize) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper::new(width);
    for glyph in glyphs(text).flatten() {
        wrapper.push(glyph);
    }
    wrapper.finish()
}

/// Word-wraps styled text to `width` columns. A word longer than the
/// width breaks at the width.
fn wrap(text: &[Span], width: usize) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper::new(width);
    for glyph in glyphs(text) {
        match glyph {
            None => {
                wrapper.end_word();
                wrapper.spaces.clear();
                wrapper.break_line();
            }
            Some(glyph) if glyph.text == " " => {
                wrapper.end_word();
                // More spaces than the width never show: keep no more, so
                // that 1 MB of tabs does not fill the memory.
                if wrapper.spaces.len() < wrapper.width {
                    wrapper.spaces.push(glyph);
                }
            }
            Some(glyph) => wrapper.word.push(glyph),
        }
    }
    wrapper.finish()
}

/// A grapheme of styled text, with the columns that it takes on screen.
#[derive(Clone, Copy)]
struct Glyph<'a> {
    text: &'a str,
    style: Style,
    width: usize,
    /// The index of the span that the glyph comes from. Glyphs of two
    /// spans never merge: the merged text could form other graphemes, of
    /// another width, when the buffer splits it again.
    source: usize,
}

/// The graphemes of styled text, measured as the ratatui buffer measures
/// them. A hard line break, a span `\n` alone (see `doc::Kind::Text`),
/// gives `None`.
fn glyphs<'a>(text: &'a [Span<'a>]) -> impl Iterator<Item = Option<Glyph<'a>>> {
    text.iter().enumerate().flat_map(|(source, span)| {
        let hard_break = std::iter::once(None).filter(move |_| span.content == "\n");
        let glyphs = span.styled_graphemes(Style::new()).map(move |g| {
            Some(Glyph {
                text: g.symbol,
                style: g.style,
                width: usize::from(g.symbol.cell_width()),
                source,
            })
        });
        hard_break.chain(glyphs)
    })
}

/// The width of styled text on screen.
fn text_width(text: &[Span]) -> usize {
    glyphs(text).flatten().map(|g| g.width).sum()
}

#[derive(Default)]
struct Wrapper<'a> {
    width: usize,
    lines: Vec<Line<'static>>,
    line: Vec<Glyph<'a>>,
    line_width: usize,
    word: Vec<Glyph<'a>>,
    /// The spaces before the word. They are dropped at a line break.
    spaces: Vec<Glyph<'a>>,
}

impl<'a> Wrapper<'a> {
    fn new(width: usize) -> Self {
        Wrapper {
            width,
            ..Default::default()
        }
    }

    /// Adds a glyph to the line, after a line break if it would pass the
    /// width. A glyph wider than the width gets a line of its own.
    fn push(&mut self, glyph: Glyph<'a>) {
        if self.line_width + glyph.width > self.width && !self.line.is_empty() {
            self.break_line();
        }
        self.line_width += glyph.width;
        self.line.push(glyph);
    }

    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let spaces = std::mem::take(&mut self.spaces);
        let word = std::mem::take(&mut self.word);
        let word_width: usize = word.iter().map(|g| g.width).sum();
        if self.line_width + spaces.len() + word_width <= self.width {
            spaces.into_iter().for_each(|g| self.push(g));
        } else if !self.line.is_empty() {
            self.break_line();
        }
        word.into_iter().for_each(|g| self.push(g));
    }

    fn break_line(&mut self) {
        let glyphs = std::mem::take(&mut self.line);
        self.line_width = 0;
        self.lines.push(Line::from(join_styles(&glyphs)));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.end_word();
        self.break_line();
        self.lines
    }
}

/// Groups the glyphs of each span, and of each style, into spans.
fn join_styles(glyphs: &[Glyph]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut last_source = None;
    for glyph in glyphs {
        match spans.last_mut() {
            Some(span) if last_source == Some(glyph.source) && span.style == glyph.style => {
                span.content.to_mut().push_str(glyph.text);
            }
            _ => spans.push(Span::styled(glyph.text.to_string(), glyph.style)),
        }
        last_source = Some(glyph.source);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{escaped, needs_escape, parse};
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
                // Adjacent spans of one style look the same on screen.
                let mut runs: Vec<(Style, String)> = Vec::new();
                for span in &line.spans {
                    let style = line.style.patch(span.style);
                    match runs.last_mut() {
                        Some((last, text)) if *last == style => text.push_str(&span.content),
                        _ => runs.push((style, span.content.to_string())),
                    }
                }
                let mut out = String::new();
                for (style, text) in runs {
                    let tags = style_tags(style);
                    if tags.is_empty() {
                        out.push_str(&text);
                    } else {
                        out.push_str(&format!("«{tags}:{text}»"));
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
        markup(&layout(&parse(markdown), width).lines)
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
    fn a_list_after_a_list_starts_at_the_first_level() {
        assert_snapshot!(render("- a\n  - b\n\ntext\n\n- c", 40), @"
        • a
          ◦ b

        text

        • c
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
    fn an_empty_code_block_shows_its_gutter() {
        assert_snapshot!(render("1. a\n2. ```\n   ```\n3. c", 40), @"
        1. a
        2. «d:▐ »

        3. c
        ");
    }

    #[test]
    fn empty_items_keep_their_marker() {
        assert_snapshot!(render("1. a\n2.\n3. c\n\n- d\n-\n- e", 40), @"
        1. a
        2.
        3. c

        • d
        •
        • e
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
    fn a_block_at_the_end_of_a_tight_item_keeps_a_blank_line_after_it() {
        let text = "1. Do x:\n   ```bash\n   ls\n   ```\n2. Next\n\nafter";
        assert_snapshot!(render(text, 40), @"
        1. Do x:
           ▐ «b:#1»
           ▐ «blue:ls»

        2. Next

        after
        ");
    }

    #[test]
    fn a_loose_item_that_ends_with_a_list_keeps_its_blank_line() {
        assert_snapshot!(render("- a\n\n  - b\n\n- c", 40), @"
        • a

          ◦ b

        • c
        ");
    }

    #[test]
    fn a_quote_in_a_list_keeps_the_indent_on_its_blank_lines() {
        assert_snapshot!(render("- item\n\n  > a\n  >\n  > b", 40), @"
        • item

          «d:│ »a
          «d:│ »
          «d:│ »b
        ");
    }

    #[test]
    fn blockquotes_have_a_bar_per_level() {
        let text = "> one two three\n>\n> > nested\n>\n> - item\n\nafter";
        assert_snapshot!(render(text, 12), @"
        «d:│ »one two
        «d:│ »three
        «d:│ »
        «d:│ │ »nested
        «d:│ »
        «d:│ »• item

        after
        ");
    }

    #[test]
    fn prefixes_keep_the_innermost_10_levels() {
        let quotes = render(&format!("{} x", ">".repeat(12)), 80);
        assert_eq!(quotes, format!("«d:{}»x", "│ ".repeat(10)));
        let list: String = (0..12)
            .map(|i| format!("{}- {i}\n", "  ".repeat(i)))
            .collect();
        let last = render(&list, 80).lines().last().unwrap().to_string();
        assert_eq!(last, format!("{}◦ 11", "  ".repeat(9)));
    }

    #[test]
    fn prefixes_take_at_most_half_the_view() {
        // 10 levels of 11 columns: 110 columns of prefix.
        let mut text = String::new();
        for level in 0..10 {
            text += &format!("{}123456789. step\n\n", " ".repeat(11 * level));
        }
        let indent = " ".repeat(110);
        text += &format!("{indent}```bash [name:check]\n{indent}curl x | sh\n{indent}```\n");
        let page = layout(&parse(&text), 60);
        let code = markup(&page.lines[page.blocks[0].clone()]);
        // Two levels of 11 columns stay: at most 30 columns, half of 60.
        let expected = "▐ «b:check»\n▐ «blue:curl» x «magenta:|» «blue:sh»";
        assert_eq!(code, expected.replace("▐", &format!("{}▐", " ".repeat(22))));
    }

    #[test]
    fn fitted_prefixes_keep_the_innermost_level_and_half_the_view() {
        let item = |levels: usize| Item {
            first: vec![Span::raw("123456789. "); levels],
            rest: vec![Span::raw(" ".repeat(11)); levels],
            kind: Kind::Blank,
        };
        let kept = |levels, width| fitted_prefixes(&item(levels), width).1.len();
        // One level of 11 columns stays, even past half of 20 columns.
        assert_eq!(kept(1, 20), 1);
        // Two levels of 11 columns take exactly half of 44.
        assert_eq!(kept(2, 44), 2);
        assert_eq!(kept(2, 43), 1);
    }

    #[test]
    fn truncate_to_no_column_shows_nothing() {
        assert!(truncate(&[Span::raw("abc")], 0).is_empty());
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let quotes = layout(&parse(&format!("{} x\n", ">".repeat(100_000))), 80);
        assert_eq!(quotes.lines.len(), 1);
        let list: String = (0..1_000)
            .map(|i| format!("{}- a\n", "  ".repeat(i)))
            .collect();
        assert_eq!(layout(&parse(&list), 80).lines.len(), 1_000);
        // 10,000 nested emphasis spans, each with text inside.
        let open: String = (0..10_000).map(|i| ["*a ", "_a "][i % 2]).collect();
        let close: String = (0..10_000).rev().map(|i| [" a*", " a_"][i % 2]).collect();
        let emphasis = layout(&parse(&format!("{open}x{close}")), 80);
        let spans = emphasis.lines.iter().flat_map(|l| &l.spans);
        let x = spans.into_iter().find(|s| s.content.contains('x')).unwrap();
        assert!(x.style.add_modifier.contains(Modifier::ITALIC));
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
        ▐ «b:check»
        ▐ «blue:ls»

        ▐ «b:#2»
        ▐
        ▐ «blue:cd» /
        ");
    }

    #[test]
    fn bash_code_is_highlighted_with_terminal_colors() {
        let text =
            "```bash\nexport A=\"x $HOME\" # note\nif [ -n 1 ]; then ls -la | grep x; fi\n```";
        assert_snapshot!(render(text, 80), @r#"
        ▐ «b:#1»
        ▐ «magenta:export» «cyan:A»=«green:"x »«cyan:$HOME»«green:"» «i:# note»
        ▐ «magenta:if» «blue:[» -n 1 «blue:]»«magenta:;» «magenta:then» «blue:ls» -la «magenta:|» «blue:grep» x«magenta:;» «magenta:fi»
        "#);
    }

    #[test]
    fn a_block_with_a_long_line_is_not_highlighted() {
        let text = format!("```bash\nls\necho {}\n```", "a".repeat(1_000));
        let lines = markup(&layout(&parse(&text), 2_000).lines);
        assert_eq!(
            lines,
            format!("▐ «b:#1»\n▐ ls\n▐ echo {}", "a".repeat(1_000))
        );
    }

    #[test]
    fn a_carriage_return_before_a_line_feed_shows_in_plain_code() {
        // pulldown-cmark turns CR LF into LF, but CR CR LF keeps one CR.
        let text = "```\ncd /tmp/x\r\r\nrm -rf *\n```";
        assert_snapshot!(render(text, 40), @"
        «d:▐ »cd /tmp/x«yellow:^M»
        «d:▐ »rm -rf *
        ");
    }

    #[test]
    fn other_code_blocks_have_a_dim_gutter_and_no_highlighting() {
        assert_snapshot!(render("```python\nif x: 'a'\n```\n\n    ls '$x'", 40), @"
        «d:▐ »if x: 'a'

        «d:▐ »ls '$x'
        ");
    }

    #[test]
    fn a_long_info_line_wraps_so_that_its_warnings_show() {
        let text = "```bash [name:deploy-the-api-to-production x:1]\n\u{1b}[8m\n```";
        assert_snapshot!(render(text, 30), @"
        ▐ «b:deploy-the-api-to-production»
        ▐«d:↪»«yellow:unknown attribute 'x';»
        ▐«d:↪»«yellow:hidden characters in code»
        ▐ «yellow:^[»«blue:[8m»
        ");
    }

    #[test]
    fn long_code_lines_wrap_so_that_all_the_code_shows() {
        assert_snapshot!(render("```bash\nrm -rf build/output/folder\n```", 12), @"
        ▐ «b:#1»
        ▐ «blue:rm» -rf bui
        ▐«d:↪»ld/output/
        ▐«d:↪»folder
        ");
    }

    #[test]
    fn info_lines_show_problems_and_warnings() {
        let text = "```bash [name:a x:1]\n```\n```bash [name:a]\n```\n```bash [name:\"b c\"]\n```";
        assert_snapshot!(render(text, 60), @"
        ▐ «b:a»  «red:duplicate name»  «yellow:unknown attribute 'x'»

        ▐ «b:a»  «red:duplicate name»

        ▐ «b:#3»  «yellow:invalid name: use [A-Za-z0-9_.-]»
        ");
    }

    #[test]
    fn code_in_a_list_keeps_the_list_indent() {
        assert_snapshot!(render("- step:\n\n  ```bash [name:s]\n  ls\n  ```", 40), @"
        • step:

          ▐ «b:s»
          ▐ «blue:ls»
        ");
    }

    #[test]
    fn thematic_breaks_are_dim_rules() {
        assert_snapshot!(render("a\n\n***\n\n> b\n>\n> ---", 10), @"
        a

        «d:──────────»

        «d:│ »b
        «d:│ »
        «d:│ ────────»
        ");
    }

    #[test]
    fn a_line_of_an_html_block_wraps_at_any_character() {
        assert_snapshot!(render("<p>aaaa bbbb cccc</p>\n", 10), @"
        «d:<p>aaaa bb»
        «d:bb cccc</p»
        «d:>»
        ");
    }

    #[test]
    fn code_shows_the_characters_that_fonts_draw_as_a_blank_escaped() {
        assert_snapshot!(render("```bash\ntrue\u{1d159}#; ls\u{2800}x\n```", 50), @"
        ▐ «b:#1»  «yellow:hidden characters in code»
        ▐ «blue:true»«yellow:<1d159>»«blue:#»«magenta:;» «blue:ls»«yellow:<2800>»«blue:x»
        ");
    }

    #[test]
    fn raw_html_shows_dimmed_as_source() {
        let text = "text\n\n<div>\n  <b>x</b>\n</div>\n\nan <i>inline</i> tag\n\n<p>\nlast\n</p>\n";
        assert_snapshot!(render(text, 40), @"
        text

        «d:<div>»
        «d:  <b>x</b>»
        «d:</div>»

        an «d:<i>»inline«d:</i>» tag

        «d:<p>»
        «d:last»
        «d:</p>»
        ");
    }

    #[test]
    fn a_line_end_in_inline_html_is_a_space() {
        assert_snapshot!(render("a <span\nclass=x>b</span>\r\nc <i\r\nx>d", 40), @"a «d:<span class=x>»b«d:</span>» c «d:<i x>»d");
    }

    #[test]
    fn frontmatter_with_crlf_line_ends_shows_no_carriage_return() {
        assert_snapshot!(render("---\r\ntitle: x\r\n---\r\n# T\r\n", 20), @"
        «d:---»
        «d:title: x»
        «d:---»

        «b:T»
        «d:━━━━━━━━━━━━━━━━━━━━»
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
        let text = "```bash\necho \u{1b}[31mred\u{7f} \u{9b}x \u{202e}y\n```";
        assert_snapshot!(render(text, 60), @"
        ▐ «b:#1»  «yellow:hidden characters in code»
        ▐ «blue:echo» «yellow:^[»«magenta:[»31mred«yellow:^?» «yellow:<9b>»x «yellow:<202e>»y
        ");
    }

    #[test]
    fn control_characters_in_text_show_escaped() {
        let text = "# T\u{1b}\n\na\u{1b}]52;c;eA==\u{7}b \u{85}c \u{202e}d\n\n| \u{1b} |\n|---|";
        assert_snapshot!(render(text, 60), @"
        «b:T»«yellow,b:^[»
        «d:━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━»

        a«yellow:^[»]52;c;eA==«yellow:^G»b «yellow:<85>»c «yellow:<202e>»d

        «d:┌────┐»
        «d:│» «yellow,b:^[» «d:│»
        «d:├────┤»
        «d:└────┘»
        ");
    }

    #[test]
    fn control_characters_in_attributes_show_escaped() {
        assert_snapshot!(render("```bash [\u{1b}]\n```", 60), @"▐ «b:#1»  «yellow:unexpected '^[' where a key starts»");
    }

    #[test]
    fn code_shows_escaped_what_a_terminal_would_not_draw() {
        let text = "```bash\necho \"prod\u{200b}\" ok\u{fe0f}\n\u{301}ls 'x\u{ff9e}\u{ff9e}'\ntrue\u{a0}#; ls\necho a\u{70f}b\n\u{93e}ls\necho \u{1f1eb}\u{1f1f7} \u{fff9}x \u{2d7f}\n```";
        assert_snapshot!(render(text, 60), @r#"
        ▐ «b:#1»  «yellow:hidden characters in code»
        ▐ «blue:echo» «green:"prod»«yellow:<200b>»«green:"» ok«yellow:<fe0f>»
        ▐ «yellow:<0301>»«blue:ls» «green:'x»«yellow:<ff9e><ff9e>»«green:'»
        ▐ «blue:true»«yellow:<a0>»«i:#; ls»
        ▐ «blue:echo» a«yellow:<070f>»b
        ▐ «yellow:<093e>»«blue:ls»
        ▐ «blue:echo» «yellow:<1f1eb><1f1f7>» «yellow:<fff9>»x «yellow:<2d7f>»
        "#);
    }

    #[test]
    fn code_shows_the_glyphs_of_its_marks_escaped() {
        assert_snapshot!(render("```bash\necho a␣b⇥c\n```", 50), @"
        ▐ «b:#1»  «yellow:hidden characters in code»
        ▐ «blue:echo» a«yellow:<2423>»b«yellow:<21e5>»c
        ");
    }

    #[test]
    fn a_space_that_ends_a_row_of_code_shows_a_mark() {
        // The first row of the rm line takes 38 columns, and ends with the
        // space before "/".
        let text = format!(
            "```bash\necho would remove: \\ \nrm -rf /tmp/{} /\n```",
            "a".repeat(25)
        );
        assert_snapshot!(render(&text, 40), @r"
        ▐ «b:#1»
        ▐ «blue:echo» would remove: \«d:␣»
        ▐ «blue:rm» -rf /tmp/aaaaaaaaaaaaaaaaaaaaaaaaa«d:␣»
        ▐«d:↪»/
        ");
    }

    #[test]
    fn tabs_in_code_expand_to_8_columns() {
        assert_snapshot!(render("```\na\tb\n\tc\n```", 40), @"
        «d:▐ »a«d:⇥»      b
        «d:▐ ⇥»       c
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
            Just("\u{202e}".to_string()),
            Just("---\n".to_string()),
            Just("    ".to_string()),
            Just("![a](b)".to_string()),
        ];
        prop::collection::vec(parts, 0..20).prop_map(|parts| parts.concat())
    }

    fn code_text() -> impl Strategy<Value = String> {
        let words = prop_oneof![
            Just("ls".to_string()),
            Just("#".to_string()),
            Just("↪".to_string()),
            Just("echo".to_string()),
            Just("\"aaaaaaaa\"#;".to_string()),
            // Two spaces in a row, or a space at the end of the line.
            Just(String::new()),
            "[a-z]{1,30}",
        ];
        let line = prop::collection::vec(words, 0..12).prop_map(|w| w.join(" "));
        prop::collection::vec(line, 1..6).prop_map(|lines| lines.join("\n"))
    }

    proptest! {
        // The mark of a wrapped row is a security rule: a part of a line
        // that starts with "#" would pass for a comment, and a line that
        // starts with "↪" for the rest of the one above.
        #[test]
        fn only_the_rows_that_go_on_from_a_line_have_the_mark(
            code in code_text(),
            width in 40..100usize,
        ) {
            let doc = parse(&format!("```bash\n{code}\n```\n"));
            let page = layout(&doc, width);
            let rows = &page.lines[page.blocks[0].start + 1..page.blocks[0].end];
            // The text of spans, with the dim mark of a space at a row end
            // read as the space.
            let text_of = |spans: &[Span]| -> String {
                (spans.iter())
                    .map(|s| match s.content.as_ref() {
                        SPACE_MARK if s.style.add_modifier.contains(Modifier::DIM) => " ",
                        text => text,
                    })
                    .collect()
            };
            let mut lines: Vec<String> = Vec::new();
            for row in rows {
                // A space at the end of a row would not show.
                let last = row.spans[1..].last().map_or("", |s| s.content.as_ref());
                prop_assert!(!last.ends_with(' '), "{:?}", row);
                let text = text_of(&row.spans[1..]);
                match row.spans[0].content.as_ref() {
                    "▐ " => lines.push(text),
                    "▐" => {
                        prop_assert_eq!(&row.spans[1], &Span::styled(WRAP_MARK, DIM));
                        lines.last_mut().unwrap().push_str(&text_of(&row.spans[2..]));
                    }
                    other => prop_assert!(false, "gutter {:?}", other),
                }
            }
            let code = &doc.blocks[0].code;
            prop_assert_eq!(lines.join("\n"), code.strip_suffix('\n').unwrap_or(code));
        }

        // Without the mark, the rest of a long name would pass for a line
        // of code.
        #[test]
        fn the_rows_that_go_on_from_an_info_line_have_the_mark(
            name in prop::collection::vec("[a-z]{1,30}", 1..8).prop_map(|w| w.join("-")),
            width in 40..100usize,
        ) {
            let doc = parse(&format!("```bash [name:{name}]\nls\n```\n"));
            let page = layout(&doc, width);
            let (start, end) = (page.blocks[0].start, page.blocks[0].end);
            let text_of = |spans: &[Span]| -> String { spans.iter().map(|s| s.content.as_ref()).collect() };
            prop_assert_eq!(text_of(&page.lines[end - 1].spans), "▐ ls");
            let rows = &page.lines[start..end - 1];
            prop_assert_eq!(rows[0].spans[0].content.as_ref(), GUTTER);
            let mut text = text_of(&rows[0].spans[1..]);
            for row in &rows[1..] {
                let mark = [Span::styled("▐", BLOCK_GUTTER), Span::styled(WRAP_MARK, DIM)];
                prop_assert_eq!(&row.spans[..2], &mark);
                text.push_str(&text_of(&row.spans[2..]));
            }
            prop_assert_eq!(text, name);
        }

        // Text right under a code block would look like more code. The
        // unit test doc::tests::a_code_block_is_followed_by_a_blank_line
        // pins the tight list item, where the rule started; this property
        // lives here, with the generator of Markdown, to try any nesting.
        #[test]
        fn a_code_block_never_touches_the_next_item(text in markdown()) {
            let items = parse(&text).items;
            for pair in items.windows(2) {
                if matches!(pair[0].kind, Kind::Code { .. }) {
                    prop_assert!(matches!(pair[1].kind, Kind::Blank), "{:?}", pair[1]);
                }
            }
        }

        // The layout test below checks every path of the text; this one
        // checks the wiring of the screen: the title, the sidebar and the
        // drawing of the lines.
        #[test]
        fn no_escaped_character_reaches_the_screen(
            text in markdown(),
            title in markdown(),
            width in 20..100u16,
        ) {
            let view = View::new(parse(&text), title);
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            for cell in terminal.backend().buffer().content() {
                prop_assert!(!cell.symbol().chars().any(needs_escape), "{:?}", cell.symbol());
            }
        }
    }

    proptest! {
        #[test]
        fn no_control_character_reaches_the_view(text in markdown(), width in 1..60usize) {
            for line in layout(&parse(&text), width).lines {
                for span in &line.spans {
                    prop_assert!(!span.content.chars().any(needs_escape), "{:?}", span);
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
                let glyphs: Vec<Glyph> = cells
                    .map(|c| Glyph {
                        text: c.symbol(),
                        style: c.style(),
                        width: 1,
                        source: 0,
                    })
                    .collect();
                Line::from(join_styles(&glyphs))
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
        │           │ «cyan,b:▐ »«b:check»  «red:duplicate name»                                          │
        │           │ «cyan,b:▐ »«blue:ls»                                                             │
        │           │                                                                  │
        │           │ ▐ «b:restart»                                                        │
        │           │ ▐ «blue:systemctl» restart api                                          │
        │           │                                                                  │
        │           │ ▐ «b:#3»                                                             │
        │           │ ▐ «blue:echo»                                                           │
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
        │           │ ▐ «b:check»  «red:duplicate name»                                          │
        │           │ ▐ «blue:ls»                                                             │
        │           │                                                                  │
        │           │ «cyan,b:▐ »«b:restart»                                                        │
        │           │ «cyan,b:▐ »«blue:systemctl» restart api                                          │
        └───────────┴──────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_selection_styles_the_gutter_after_the_prefixes_and_on_wrapped_rows() {
        let view = view(&format!("> - ```bash\n>   {}\n>   ```\n", "x".repeat(70)));
        assert_snapshot!(screen(&view, 60, 8), @"
        ┌ run.md ──────────────────────────────────────────────────┐
        │ «d:│ »• «cyan,b:▐ »«b:#1»                                                 │
        │ «d:│ »  «cyan,b:▐ »«blue:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx»│
        │ «d:│ »  «cyan,b:▐»«d:↪»«blue:xxxxxxxxxxxxxxxxxxx»                                │
        │                                                          │
        │                                                          │
        └──────────────────────────────────────────────────────────┘
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
            screen(&view(markdown), 80, 8)
                .lines()
                .next()
                .unwrap()
                .to_string()
        };
        assert_snapshot!(rows("```bash [name:a]\n```\n"), @"┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐");
        assert_snapshot!(rows("```bash [name:abcdefghijklmnop]\n```\n"), @"┌ blocks ───────────┬ run.md ──────────────────────────────────────────────────┐");
        assert_snapshot!(screen(&view(&long), 80, 8), @"
        ┌ blocks ─────────────────────┬ run.md ────────────────────────────────────────┐
        │«cyan,b:▶»  «cyan,b:nnnnnnnnnnnnnnnnnnnnnnnnn…»│ «cyan,b:▐ »«b:nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn»     │
        │                             │                                                │
        │                             │                                                │
        │                             │                                                │
        │                             │                                                │
        └─────────────────────────────┴────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn the_sidebar_scrolls_to_keep_the_selection_visible() {
        let mut view = view(&"```bash\n```\n".repeat(10));
        view.selected = Some(7);
        assert_snapshot!(screen(&view, 80, 8), @"
        ┌ blocks ───┬ run.md ──────────────────────────────────────────────────────────┐
        │   #4      │ ▐ «b:#1»                                                             │
        │   #5      │                                                                  │
        │   #6      │ ▐ «b:#2»                                                             │
        │   #7      │                                                                  │
        │«cyan,b:▶»  «cyan,b:#8»      │ ▐ «b:#3»                                                             │
        └───────────┴──────────────────────────────────────────────────────────────────┘
        ");
    }

    #[test]
    fn a_screen_under_40_by_8_shows_only_a_message() {
        assert_snapshot!(screen_with_footer(&view(BLOCKS), 39, 8), @"
        Enlarge the terminal to at least 40
        columns and 8 rows.
        ");
        assert!(screen_with_footer(&view(BLOCKS), 40, 7).starts_with("Enlarge"));
        assert!(screen_with_footer(&view(BLOCKS), 40, 8).starts_with("┌"));
    }

    #[test]
    fn the_file_name_shows_control_characters_escaped() {
        let view = View::new(parse(""), "a\u{1b}]52;c;x\u{7}\nb.md".into());
        assert_snapshot!(screen(&view, 40, 8), @"
        ┌ a«yellow:^[»]52;c;x«yellow:^G^J»b.md ──────────────────┐
        │                                      │
        │                                      │
        │                                      │
        │                                      │
        │                                      │
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
            ((2, 5, 7..10, 6), 4),
            ((0, 2, 12..30, 10), 12),
            // The block fits, with only the end of its paragraph.
            ((0, 0, 4..7, 5), 2),
            ((0, 0, 4..9, 5), 4),
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
        │ «cyan,b:▐ »«b:s2»                                                     │
        │ «cyan,b:▐ »«blue:ls»                                                     │
        │ «cyan,b:▐ »«blue:ls»                                                     │
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
    fn reveal_scrolls_to_a_block_that_the_view_does_not_show_whole() {
        // At the startup, the view shows the info line of block 0, but not
        // its code.
        let mut view = steps();
        assert!(!view.reveal(SMALL));
        assert_eq!(position(&view), (Some(0), 2));
        assert!(view.reveal(SMALL));
        assert_eq!(position(&view), (Some(0), 2));
        // A view 1 line shorter cuts the last line of block 1.
        view.select_by(1, SMALL);
        assert_eq!(view.scroll, 11);
        let shorter = Size::new(60, 8);
        assert!(!view.reveal(shorter));
        assert_eq!(view.scroll, 12);
    }

    #[test]
    fn reveal_shows_nothing_without_blocks() {
        let mut view = view("Text.\n");
        assert!(!view.reveal(SMALL));
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
    fn page_keys_never_move_the_selection_against_the_scroll() {
        let step = |i| format!("Step {i}.\n\n```bash [name:s{i}]\nls\n```\n\n");
        let mut view = view(&(1..=8).map(step).collect::<String>());
        let size = Size::new(60, 12);
        // At the bottom, the first info line in view is that of block 6.
        view.select_last(size);
        view.scroll_pages(1, size);
        assert_eq!(view.selected, Some(7));
        // Block 0 is far above: the first info line in view is below it.
        view.selected = Some(0);
        view.scroll = 20;
        view.scroll_pages(-1, size);
        assert_eq!(view.selected, Some(0));
    }

    #[test]
    fn page_keys_keep_the_selection_if_no_block_is_in_view() {
        let mut view = steps();
        // Lines 15 to 20: the code of block 1, and no info line.
        view.scroll = 9;
        view.scroll_pages(1, SMALL);
        assert_eq!(position(&view), (Some(1), 15));
        // Lines 17 to 22: no block. Block 1 ends at 17, block 2 starts at 23.
        view.selected = Some(0);
        view.scroll = 11;
        view.scroll_pages(1, SMALL);
        assert_eq!(position(&view), (Some(0), 17));
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
            for (width, height) in [(40, 8), (79, 24), (120, 40)] {
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

    /// The widths of the lines of `markdown` at `width`, as the ratatui
    /// buffer measures them.
    fn line_widths(markdown: &str, width: usize) -> Vec<usize> {
        let page = layout(&parse(markdown), width);
        page.lines.iter().map(|l| text_width(&l.spans)).collect()
    }

    #[test]
    fn emoji_with_a_variation_selector_count_two_columns() {
        // "⚠️" is 1 column as a sum of chars, and 2 on screen.
        let table = "| ⚠\u{fe0f}⚠\u{fe0f}⚠\u{fe0f} | b |\n|---|---|\n";
        assert!(line_widths(table, 12).iter().all(|&w| w <= 12));
        // In prose, which keeps U+FE0F.
        let text = format!("echo {}", "⚠\u{fe0f}".repeat(8));
        assert_eq!(line_widths(&text, 8), [4, 8, 8]);
    }

    /// Styled text in several spans, with characters that join the
    /// characters before them into one grapheme.
    fn spans() -> impl Strategy<Value = Vec<Span<'static>>> {
        let joiners = prop_oneof![
            any::<char>(),
            Just('*'),
            Just(' '),
            Just('\u{fe0f}'),
            Just('\u{20e3}'),
            Just('\u{301}'),
            Just('\u{200d}'),
            Just('\u{1f468}'),
            Just('\u{1f1eb}'),
        ];
        let part = (prop::collection::vec(joiners, 0..6), any::<bool>());
        prop::collection::vec(part, 0..8).prop_map(|parts| {
            let mut spans = Vec::new();
            for (chars, bold) in parts {
                let style = if bold { BOLD } else { Style::new() };
                spans.extend(escaped(&chars.into_iter().collect::<String>(), style));
            }
            spans
        })
    }

    proptest! {
        #[test]
        fn wrapped_lines_stay_within_the_width_as_the_buffer_measures_them(
            text in spans(),
            width in 1..12usize,
        ) {
            for line in hard_wrap(&text, width).into_iter().chain(wrap(&text, width)) {
                // text_width splits each span into graphemes, as the buffer
                // does. Only a glyph wider than the width passes it, alone.
                let glyphs = glyphs(&line.spans).count();
                prop_assert!(text_width(&line.spans) <= width || glyphs == 1, "{:?}", line);
            }
        }

        #[test]
        fn hard_wrap_keeps_each_glyph(text in spans(), width in 1..12usize) {
            let lines = hard_wrap(&text, width);
            let shown: String = lines.iter().flat_map(|l| l.spans.iter().map(|s| s.content.as_ref())).collect();
            prop_assert_eq!(shown, text.iter().map(|s| s.content.as_ref()).collect::<String>());
        }
    }

    #[test]
    fn a_grapheme_split_across_two_spans_keeps_its_cells() {
        // pulldown-cmark gives an unmatched `*` its own text event, so
        // "*️⃣" comes in two spans: 1 and 0 columns, but 2 columns merged.
        let cell = "| *\u{fe0f}\u{20e3} or *\u{fe0f}\u{20e3} |\n|---|\n";
        assert!(line_widths(cell, 8).iter().all(|&w| w <= 8));
        let text = format!("{} then ~/data", "*\u{fe0f}".repeat(36));
        assert!(line_widths(&text, 20).iter().all(|&w| w <= 20));
        assert!(screen(&view(&text), 40, 9).contains("then ~/data"));
    }

    #[test]
    fn a_table_wider_than_the_view_draws_only_the_columns_in_view() {
        let row = |cell: &str| format!("|{}\n", format!("{cell}|").repeat(1_000));
        let text = format!("{}{}{}", row("a"), row("-"), row("b"));
        assert_snapshot!(render(&text, 20), @"
        «d:┌───┬───┬───┬───┬───┐»
        «d:│» «b:a» «d:│» «b:a» «d:│» «b:a» «d:│» «b:a» «d:│» «b:a» «d:│»
        «d:├───┼───┼───┼───┼───┤»
        «d:│» b «d:│» b «d:│» b «d:│» b «d:│» b «d:│»
        «d:└───┴───┴───┴───┴───┘»
        ");
    }

    #[test]
    fn inline_styles_use_terminal_styles() {
        let text = "*em* **strong** ~~gone~~ `code` [link](http://x) ***both***";
        assert_snapshot!(render(text, 60), @"«i:em» «b:strong» «s:gone» «cyan:code» «u:link» «b,i:both»");
    }
}
