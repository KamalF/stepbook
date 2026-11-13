use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

pub const INLINE_CODE: Style = Style::new().fg(Color::Cyan);
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);
pub const WARNING: Style = Style::new().fg(Color::Yellow);

/// Prefixes show only the innermost levels of quotes and list items, so
/// that the text keeps room in the view.
const MAX_DEPTH: usize = 10;

/// A fenced code block with the language `bash` or `sh` (spec 3).
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub name: Option<String>,
    pub code: String,
    /// Attribute problems. The block can still run.
    pub warnings: Vec<String>,
    /// Why the block cannot run.
    pub problem: Option<&'static str>,
}

/// One element of the rendered runbook. The view wraps it to its width.
#[derive(Debug, PartialEq)]
pub struct Item {
    /// The prefix of the first line: quote bars, list indent and marker.
    pub first: Vec<Span<'static>>,
    /// The prefix of the other lines.
    pub rest: Vec<Span<'static>>,
    pub kind: Kind,
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    /// Styled text. A span `\n` alone is a hard line break.
    Text(Vec<Span<'static>>),
    Heading(HeadingLevel, Vec<Span<'static>>),
    /// The first row is the header.
    Table(Vec<Alignment>, Vec<Vec<Cell>>),
    /// A code block, one entry per line. `block` is the index of its block
    /// in `Document::blocks`, if it is one.
    Code {
        lines: Vec<Vec<Span<'static>>>,
        block: Option<usize>,
    },
    /// Lines shown as written, such as raw HTML. They wrap at any
    /// grapheme.
    Source(Vec<Vec<Span<'static>>>),
    Rule,
    Blank,
}

pub type Cell = Vec<Span<'static>>;

#[derive(Debug)]
pub struct Document {
    pub items: Vec<Item>,
    pub blocks: Vec<Block>,
}

/// The problem of a block that a reload removed while it ran (spec 10).
pub const REMOVED: &str = "removed";

impl Document {
    /// Appends block `i` of `old`, which a reload removed while it runs:
    /// it stays at the end of the document, marked removed, until its run
    /// ends (spec 10).
    pub fn keep_removed(&mut self, old: &Document, i: usize) {
        let lines = old.items.iter().find_map(|item| match &item.kind {
            Kind::Code {
                lines,
                block: Some(block),
            } if *block == i => Some(lines.clone()),
            _ => None,
        });
        self.blocks.push(Block {
            problem: Some(REMOVED),
            ..old.blocks[i].clone()
        });
        let block = Some(self.blocks.len() - 1);
        let lines = lines.unwrap_or_default();
        for kind in [Kind::Blank, Kind::Code { lines, block }] {
            let (first, rest) = (Vec::new(), Vec::new());
            self.items.push(Item { first, rest, kind });
        }
    }

    /// The name of block `i`, or `#<n>` for the n-th block if it has no name.
    pub fn label(&self, i: usize) -> String {
        match &self.blocks[i].name {
            Some(name) => name.clone(),
            None => format!("#{}", i + 1),
        }
    }
}

/// What identifies a block across reloads (spec 3): its name, or its code
/// if it has no name.
#[derive(PartialEq, Eq, Hash)]
enum Key<'a> {
    Name(&'a str),
    Code(&'a str),
}

impl Block {
    fn key(&self) -> Key<'_> {
        match &self.name {
            Some(name) => Key::Name(name),
            None => Key::Code(&self.code),
        }
    }
}

/// The block of `old` that each block of `new` matches, by key: the n-th
/// block with a key matches the n-th block of `old` with that key (spec
/// 10). A block of `old` that no block matches was removed.
pub fn match_blocks(old: &[Block], new: &[Block]) -> Vec<Option<usize>> {
    let mut keys: HashMap<Key, VecDeque<usize>> = HashMap::new();
    for (i, block) in old.iter().enumerate() {
        keys.entry(block.key()).or_default().push_back(i);
    }
    let matched = new
        .iter()
        .map(|block| keys.get_mut(&block.key())?.pop_front());
    matched.collect()
}

/// The inverse of `matches`, the result of `match_blocks`: for each of the
/// `old` blocks, the block that matches it, if any.
pub fn moves(matches: &[Option<usize>], old: usize) -> Vec<Option<usize>> {
    let mut moves = vec![None; old];
    for (j, &i) in matches.iter().enumerate() {
        if let Some(i) = i {
            moves[i] = Some(j);
        }
    }
    moves
}

pub fn parse(text: &str) -> Document {
    parse_with(Builder::default(), text)
}

fn parse_with(mut builder: Builder, text: &str) -> Document {
    // CommonMark replaces U+0000 with U+FFFD, but pulldown-cmark keeps it.
    // bash drops it: "r<U+0000>m" would show "r^@m" and run "rm".
    let text = &text.replace('\0', "\u{fffd}");
    let (frontmatter, body) = split_frontmatter(text);
    if let Some(frontmatter) = frontmatter {
        let lines = source_lines(&format!("---\n{frontmatter}---"));
        builder.push_item(Kind::Source(lines));
        builder.blank = true;
    }
    for (event, range) in Parser::new_ext(body, options(body)).into_offset_iter() {
        builder.event(event, &body[range]);
    }
    mark_duplicates(&mut builder.blocks);
    Document {
        items: builder.items,
        blocks: builder.blocks,
    }
}

/// Turns the events of pulldown-cmark into items. It keeps its own stacks
/// and never recurses, so no nesting depth can overflow the call stack.
#[derive(Default)]
struct Builder {
    items: Vec<Item>,
    blocks: Vec<Block>,
    /// The text of the paragraph or heading being read.
    text: Vec<Span<'static>>,
    styles: Vec<Style>,
    heading: Option<HeadingLevel>,
    /// The code and info string of the code block being read.
    code: Option<(String, String)>,
    /// A blank line goes before the next element.
    blank: bool,
    containers: Vec<Container>,
    /// The number of open lists.
    lists: usize,
    /// The columns of text since the last line break, for tab stops.
    column: usize,
    table: Option<Table>,
    /// The source of the HTML block being read.
    html: Option<String>,
    highlighted: Highlighted,
}

/// The table being read.
struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
}

enum Container {
    Quote,
    List {
        /// The number of the next item, in an ordered list.
        next: Option<u64>,
        /// pulldown-cmark wraps the text of an item in a paragraph only in
        /// a loose list, which has blank lines between its items.
        loose: bool,
    },
    /// A list item. Its marker shows on the first line of its content.
    Item {
        marker: Option<String>,
        indent: usize,
        /// The number of items before the content of the item.
        start: usize,
    },
}

impl Builder {
    fn event(&mut self, event: Event, source: &str) {
        match event {
            Event::Start(tag) => self.start(tag, source),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some((code, _)) => code.push_str(&text),
                None => self.push(&text, Style::new()),
            },
            Event::Code(text) => self.push(&text, INLINE_CODE),
            Event::SoftBreak => self.push(" ", Style::new()),
            Event::HardBreak => {
                self.text.push(Span::raw("\n"));
                self.column = 0;
            }
            Event::Html(text) => {
                if let Some(html) = &mut self.html {
                    html.push_str(&text);
                }
            }
            // A line end in inline HTML is a soft break, as in the text
            // around it.
            Event::InlineHtml(text) => {
                self.push(&text.replace("\r\n", " ").replace(['\r', '\n'], " "), DIM)
            }
            Event::Rule => {
                self.start_element();
                self.push_item(Kind::Rule);
                self.blank = true;
            }
            Event::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " }, Style::new())
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag, source: &str) {
        match tag {
            Tag::Paragraph => {
                self.start_element();
                if let [.., Container::List { loose, .. }, Container::Item { .. }] =
                    &mut self.containers[..]
                {
                    *loose = true;
                }
            }
            Tag::Table(aligns) => {
                self.start_element();
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::BlockQuote(_) => {
                self.start_element();
                self.containers.push(Container::Quote);
            }
            Tag::List(first) => {
                self.start_element();
                self.containers.push(Container::List {
                    next: first,
                    loose: false,
                });
                self.lists += 1;
            }
            Tag::Item => {
                self.start_element();
                let marker = match self.containers.last_mut() {
                    Some(Container::List { next: Some(n), .. }) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ if self.lists % 2 == 1 => "• ".to_string(),
                    _ => "◦ ".to_string(),
                };
                let indent = marker.chars().count();
                self.containers.push(Container::Item {
                    marker: Some(marker),
                    indent,
                    start: self.items.len(),
                });
            }
            Tag::Heading { level, .. } => {
                self.start_element();
                self.heading = Some(level);
                self.push_style(Style::new().add_modifier(Modifier::BOLD));
            }
            Tag::CodeBlock(kind) => {
                self.start_element();
                let info = match kind {
                    CodeBlockKind::Fenced(_) => raw_info(source).to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((String::new(), info));
            }
            Tag::Emphasis => self.push_style(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self.push_style(Style::new().add_modifier(Modifier::UNDERLINED)),
            Tag::Image { .. } => self.push("[image: ", Style::new()),
            Tag::HtmlBlock => {
                self.start_element();
                self.html = Some(String::new());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_text(),
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table {
                    table.row.push(std::mem::take(&mut self.text));
                }
                self.column = 0;
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    // pulldown-cmark pads a short row with empty cells, up
                    // to the columns of the header: a few bytes of input
                    // can make thousands. The view shows a missing cell as
                    // an empty one.
                    if tag == TagEnd::TableRow {
                        while table.row.last().is_some_and(Vec::is_empty) {
                            table.row.pop();
                        }
                    }
                    let mut row = std::mem::take(&mut table.row);
                    row.shrink_to_fit();
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_item(Kind::Table(table.aligns, table.rows));
                }
                self.blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.end_text();
                self.containers.pop();
            }
            TagEnd::List(_) => {
                self.end_text();
                self.containers.pop();
                self.lists -= 1;
            }
            TagEnd::Item => {
                self.flush_text();
                // An empty item still shows its marker, on an empty line.
                if let Some(Container::Item { start, .. }) = self.containers.last()
                    && *start == self.items.len()
                {
                    self.push_item(Kind::Text(Vec::new()));
                }
                self.containers.pop();
                // Whatever ends the item, only a loose list puts a blank
                // line before the next item.
                self.blank = matches!(
                    self.containers.last(),
                    Some(Container::List { loose: true, .. })
                );
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.end_text();
            }
            TagEnd::CodeBlock => {
                if let Some((mut code, info)) = self.code.take() {
                    // A fence that the file leaves open at its end, with no
                    // final line feed: bash runs a last "\\" otherwise.
                    if !code.is_empty() && !code.ends_with('\n') {
                        code.push('\n');
                    }
                    let block = block(&info, &code).map(|block| {
                        self.blocks.push(block);
                        self.blocks.len() - 1
                    });
                    let mut hidden = false;
                    let total = block.and_then(|_| self.highlighted.with(&code));
                    let lines = match total {
                        Some(total) => {
                            self.highlighted = total;
                            highlight(&code, &mut hidden)
                        }
                        // Split on LF only: str::lines would drop a CR
                        // before an LF, which bash still gets.
                        _ => {
                            // Code that cannot run looks inert, as
                            // frontmatter does.
                            let style = match block {
                                Some(_) => Style::new(),
                                None => DIM,
                            };
                            (code.split_terminator('\n'))
                                .map(|l| code_line(&[Span::styled(l, style)], &mut hidden))
                                .collect()
                        }
                    };
                    // The warning follows what the view shows: a tab
                    // makes spaces that a mark after it can join.
                    if let Some(i) = block
                        && hidden
                    {
                        self.blocks[i]
                            .warnings
                            .push("hidden characters in code".into());
                    }
                    self.push_item(Kind::Code { lines, block });
                }
                self.blank = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::Image => self.push("]", Style::new()),
            TagEnd::HtmlBlock => {
                if let Some(html) = self.html.take() {
                    self.push_item(Kind::Source(source_lines(&html)));
                }
                self.blank = true;
            }
            _ => {}
        }
    }

    /// The style of the inline text being read.
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    /// Starts an inline style, such as emphasis. The stack holds combined
    /// styles, so that deep nesting costs nothing per text event.
    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style().patch(style));
    }

    fn push(&mut self, text: &str, style: Style) {
        let style = self.style().patch(style);
        escape_into(&mut self.text, text, style, &mut self.column, Text::Prose);
    }

    /// Ends the text before a new element, and adds the blank line that
    /// separates them.
    fn start_element(&mut self) {
        self.flush_text();
        if std::mem::take(&mut self.blank) {
            self.push_blank();
        }
    }

    /// Adds a blank line. Its prefix has as many levels as the lines
    /// around it, so that the view cuts the same outer levels.
    fn push_blank(&mut self) {
        let (_, rest) = self.prefixes(false);
        self.items.push(Item {
            first: rest.clone(),
            rest,
            kind: Kind::Blank,
        });
    }

    /// The prefixes of the first line and of the other lines of the next
    /// item. If `show_marker`, the first line takes the pending list markers.
    fn prefixes(&mut self, show_marker: bool) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let mut first = Vec::new();
        let mut rest = Vec::new();
        // From the inside out, so that deep nesting costs no more than 10
        // levels. The markers of hidden outer items stay pending.
        let mut levels: Vec<_> = (self.containers.iter_mut().rev())
            .filter(|c| !matches!(c, Container::List { .. }))
            .take(MAX_DEPTH)
            .collect();
        levels.reverse();
        for container in levels {
            match container {
                Container::Quote => {
                    first.push(Span::styled("│ ", DIM));
                    rest.push(Span::styled("│ ", DIM));
                }
                Container::Item { marker, indent, .. } => {
                    let spaces = Span::raw(" ".repeat(*indent));
                    match marker.take_if(|_| show_marker) {
                        Some(marker) => first.push(Span::raw(marker)),
                        None => first.push(spaces.clone()),
                    }
                    rest.push(spaces);
                }
                Container::List { .. } => {}
            }
        }
        (first, rest)
    }

    fn end_text(&mut self) {
        self.flush_text();
        self.blank = true;
    }

    fn flush_text(&mut self) {
        let heading = self.heading.take();
        self.column = 0;
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        self.push_item(match heading {
            Some(level) => Kind::Heading(level, text),
            None => Kind::Text(text),
        });
    }

    /// Adds an item, with the prefixes of the containers around it.
    fn push_item(&mut self, kind: Kind) {
        // A code block always ends with a blank line, or with the end of
        // the file. Text right under it would look like more code: the
        // text of a tight list item can draw a gutter with "▐".
        if matches!(
            self.items.last(),
            Some(Item {
                kind: Kind::Code { .. },
                ..
            })
        ) {
            self.push_blank();
        }
        let (first, rest) = self.prefixes(true);
        self.items.push(Item { first, rest, kind });
    }
}

/// The dimmed lines of verbatim text, such as an HTML block. `lines` also
/// ends a line at a CRLF, which pulldown-cmark leaves in the frontmatter.
fn source_lines(text: &str) -> Vec<Vec<Span<'static>>> {
    text.lines().map(|line| escaped(line, DIM)).collect()
}

/// pulldown-cmark rescans every open quote and list on each line: a file
/// that opens 100,000 quotes, then has 200,000 lazy lines, took 30 s. At
/// 1,000 levels, 1 MB of lazy lines takes 1 s. The lines of a runbook nest
/// a few levels.
pub const MAX_NESTING: usize = 1_000;

/// The deepest nesting at the start of a line of `text`: its quote and
/// list markers, and each 2 columns of the whitespace around them, with a
/// tab as 4 columns. A list item stays open through the indent of the
/// lines after it, so the indent counts too.
pub fn nesting(text: &str) -> usize {
    let lines = text.split_terminator(['\n', '\r']);
    lines.map(line_nesting).max().unwrap_or(0)
}

fn line_nesting(line: &str) -> usize {
    let mut markers = 0;
    let mut columns = 0;
    let mut rest = line;
    loop {
        let text = rest.trim_start_matches([' ', '\t']);
        let indent = &rest[..rest.len() - text.len()];
        columns += indent
            .chars()
            .map(|c| if c == '\t' { 4 } else { 1 })
            .sum::<usize>();
        rest = text;
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let after = match rest.strip_prefix('>') {
            Some(after) => after,
            None => match rest[digits..].strip_prefix(['-', '*', '+', '.', ')']) {
                // A list marker is a bullet, or 1 to 9 digits and . or ),
                // then a space or the end of the line.
                Some(after)
                    if (digits == 0) == rest[digits..].starts_with(['-', '*', '+'])
                        && digits <= 9
                        && (after.is_empty() || after.starts_with([' ', '\t'])) =>
                {
                    after
                }
                _ => return markers + columns / 2,
            },
        };
        markers += 1;
        rest = after;
    }
}

/// pulldown-cmark pads each short row of a table with empty cells, up to
/// the columns of its header, and keeps them all: 1 MB of crafted tables
/// takes gigabytes. A file that could make more cells than this shows its
/// tables as text.
const MAX_TABLE_CELLS: usize = 200_000;

fn options(text: &str) -> Options {
    let mut options = Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    // At most each line is a row, and a row has at most as many cells as
    // the most `|` on one line, plus 1. A lone CR also ends a line in
    // CommonMark; a CR LF then counts twice, which only raises the bound.
    let (lines, pipes) = (text.split_terminator(['\n', '\r']))
        .fold((0, 0), |(lines, pipes), line| {
            (lines + 1, pipes.max(line.matches('|').count()))
        });
    if lines * (pipes + 1) <= MAX_TABLE_CELLS {
        options |= Options::ENABLE_TABLES;
    }
    options
}

/// Splits off the lines between a first line `---` and the next `---`
/// line. Without that closing line, the file has no frontmatter.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return (None, text);
    }
    let mut end = first.len();
    let start = end;
    for line in lines {
        if line.trim_end() == "---" {
            return (Some(&text[start..end]), &text[end + line.len()..]);
        }
        end += line.len();
    }
    (None, text)
}

/// The info string as written in the file. pulldown-cmark decodes
/// backslash escapes and entities in it, which would change quoted values.
fn raw_info(source: &str) -> &str {
    let fence = source.lines().next().unwrap_or("").trim_start();
    let marker = fence.chars().next().unwrap_or('`');
    fence.trim_start_matches(marker).trim()
}

fn block(info: &str, code: &str) -> Option<Block> {
    let (lang, section) = match info.find('[') {
        Some(i) => (&info[..i], Some(&info[i..])),
        None => (info, None),
    };
    if !matches!(lang.trim(), "bash" | "sh") {
        return None;
    }
    let mut block = Block {
        name: None,
        code: code.to_string(),
        warnings: Vec::new(),
        problem: None,
    };
    let (pairs, error) = section.map_or_else(Default::default, parse_attributes);
    let mut named = false;
    for (key, value) in pairs {
        if key != "name" {
            block
                .warnings
                .push(format!("unknown attribute '{}'", short(&key)));
        } else if std::mem::replace(&mut named, true) {
            block.warnings.push("name is given twice".into());
        } else if is_valid_name(&value) {
            block.name = Some(value);
        } else {
            block.warnings.push(INVALID_NAME.into());
        }
    }
    block.warnings.extend(error);
    Some(block)
}

/// Whether stepbook shows `c` escaped: a control character, or a bidi
/// control, which can reorder text in terminals that support right-to-left
/// scripts (spec 6.5).
pub fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Whether stepbook shows `c` escaped in code. Code also escapes the
/// default ignorable code points of Unicode, which a terminal draws as
/// nothing, such as U+200B (zero-width space): `"prod<U+200B>"` would look
/// like `"prod"`. It escapes the spaces other than U+0020, and the
/// characters that fonts draw as a blank, U+2800 and U+1D159, which bash
/// reads as part of a word: `true<U+00A0>#; curl x | sh` looks like a
/// comment, and runs curl. It escapes the format characters that join the
/// next character, such as U+070F, U+2D7F and the regional indicators: a
/// terminal may count their columns otherwise than ratatui, and the text
/// after them would shift. It escapes the characters that a terminal
/// draws 2 columns wide by default, where ratatui counts 1, since the text
/// after them would cover the end of the line: U+3248 to U+324F, the
/// circled numbers on a black square, which glibc and tmux draw wide, and
/// the 9 that tmux forces wide, such as U+261D. It escapes
/// "␣" and "⇥", which would pass for the marks of a space and a tab. Prose
/// keeps them all, for emoji, scripts and no-break spaces.
pub fn needs_escape_in_code(c: char) -> bool {
    needs_escape(c)
        || matches!(
            c,
            '\u{a0}'
                | '\u{600}'..='\u{605}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{d4e}'
                | '\u{2d7f}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{111c2}'..='\u{111c3}'
                | '\u{1193f}'
                | '\u{11941}'
                | '\u{11a3a}'
                | '\u{11a84}'..='\u{11a89}'
                | '\u{11d46}'
                | '\u{11f02}'
                | '\u{113d1}'
                | '\u{1f1e6}'..='\u{1f1ff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'..='\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{2800}'
                | '\u{1d159}'
                | '\u{21e5}'
                | '\u{2423}'
                | '\u{3000}'
                | '\u{3248}'..='\u{324f}'
                | '\u{261d}'
                | '\u{26f9}'
                | '\u{270c}'..='\u{270d}'
                | '\u{1f3cb}'..='\u{1f3cc}'
                | '\u{1f574}'..='\u{1f575}'
                | '\u{1f590}'
                | '\u{ad}'
                | '\u{34f}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fff8}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e0fff}'
        )
}

/// Prose or code: code escapes more characters, and marks its tabs.
#[derive(Clone, Copy, PartialEq)]
pub enum Text {
    Prose,
    Code,
}

/// The dim mark of a tab in code: with `<<-`, bash strips the tabs at the
/// start of a heredoc line, not the spaces. The mark has a width of 1 in
/// every terminal, unlike "→", whose East Asian width is ambiguous.
pub const TAB_MARK: &str = "⇥";

/// Appends `text` to `out`, with each character that needs an escape
/// shown as vim shows it, in the warning color: `^[` for ESC, `^?` for
/// DEL, `<9b>` for a C1 character and `<202e>` for a bidi control. A tab
/// expands to spaces up to the next multiple of 8 columns, and in code
/// starts with `TAB_MARK`. `column` counts the columns of the line so far.
/// Returns whether it escaped a character.
pub fn escape_into(
    out: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    column: &mut usize,
    kind: Text,
) -> bool {
    let escape = match kind {
        Text::Prose => needs_escape,
        Text::Code => needs_escape_in_code,
    };
    // A run of plain text or of escapes, and the bytes of it that `column`
    // counts already.
    let mut run = String::new();
    let mut escapes = false;
    let mut escaped = false;
    let mut counted = 0;
    for c in text.chars() {
        let escape = c != '\t' && escape(c);
        if escape != escapes && !run.is_empty() {
            *column += width(&run[counted..]);
            out.push(run_span(std::mem::take(&mut run), style, escapes));
            counted = 0;
        }
        escapes = escape;
        if c == '\t' {
            *column += width(&run[counted..]);
            let mut spaces = 8 - *column % 8;
            *column += spaces;
            if kind == Text::Code {
                if !run.is_empty() {
                    out.push(run_span(std::mem::take(&mut run), style, false));
                }
                out.push(Span::styled(TAB_MARK, style.patch(DIM)));
                spaces -= 1;
            }
            run.extend(std::iter::repeat_n(' ', spaces));
            counted = run.len();
        } else if escape {
            push_escape(&mut run, c);
            escaped = true;
        } else {
            run.push(c);
        }
    }
    *column += width(&run[counted..]);
    if !run.is_empty() {
        out.push(run_span(run, style, escapes));
    }
    escaped
}

fn push_escape(out: &mut String, c: char) {
    match u32::from(c) {
        0x7f => out.push_str("^?"),
        n @ 0..0x20 => out.extend(['^', char::from_u32(n + 0x40).unwrap_or('?')]),
        n @ 0x80..0x100 => out.push_str(&format!("<{n:02x}>")),
        n => out.push_str(&format!("<{n:04x}>")),
    }
}

fn run_span(run: String, style: Style, escapes: bool) -> Span<'static> {
    Span::styled(run, if escapes { style.patch(WARNING) } else { style })
}

/// The width of text with no control character on screen, as the
/// ratatui buffer measures it: grapheme by grapheme.
pub fn width(text: &str) -> usize {
    let span = Span::raw(text);
    let graphemes = span.styled_graphemes(Style::new());
    graphemes.map(|g| usize::from(g.symbol.cell_width())).sum()
}

/// Text for a message outside the screen, such as an error on stderr,
/// with its control characters escaped.
pub fn escaped_text(text: &str) -> String {
    escaped(text, Style::new())
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// One line of text, with its control characters escaped.
pub fn escaped(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    escape_into(&mut out, text, style, &mut 0, Text::Prose);
    out
}

/// Whether code shows a grapheme escaped, after its first character. The
/// ratatui buffer drops a grapheme of width 0, such as a combining mark at
/// the start of a line. A grapheme of several code points is a risk too:
/// a terminal may count its columns otherwise than ratatui, by code point
/// or with other tables, and then shift the rest of the line over a space.
/// "k" and 8 × U+302E is 1 column for ratatui, and 17 in tmux.
fn hides(grapheme: &str) -> bool {
    grapheme.chars().nth(1).is_some() || width(grapheme) == 0
}

/// Whether `text` after a space makes one grapheme with it.
fn joins_a_space(text: &str) -> bool {
    let span = Span::raw(format!(" {text}"));
    span.styled_graphemes(Style::new()).count() == 1
}

fn push_styled(out: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match out.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => out.push(Span::styled(text.to_string(), style)),
    }
}

/// A line of code as the view shows it. Its characters are escaped as
/// code needs, and each grapheme sits in one span, with the style of its
/// first character: syntect ends a token where its grammar says, and a
/// combining mark after a closing quote would start a span of its own. The
/// graphemes that the buffer would not draw are escaped too (see `hides`).
/// Sets `hidden` if the line shows an escape, other than a tab.
pub fn code_line(spans: &[Span], hidden: &mut bool) -> Vec<Span<'static>> {
    let mut escaped = Vec::new();
    let mut column = 0;
    for span in spans {
        let (text, style) = (&span.content, span.style);
        *hidden |= escape_into(&mut escaped, text, style, &mut column, Text::Code);
    }
    let text: String = escaped.iter().map(|s| s.content.as_ref()).collect();
    let mut starts = Vec::new();
    let mut offset = 0;
    for span in &escaped {
        starts.push((offset, span.style));
        offset += span.content.len();
    }
    let mut out: Vec<Span> = Vec::new();
    let mut offset = 0;
    // The escaped text has no control character for styled_graphemes to
    // drop.
    for grapheme in Span::raw(text.as_str()).styled_graphemes(Style::new()) {
        let mut style = starts[starts.partition_point(|(start, _)| *start <= offset) - 1].1;
        // The view draws the gutter "▐ " before each line: a mark at the
        // start of a line would join its space into one grapheme.
        let at_start = offset == 0;
        offset += grapheme.symbol.len();
        let mut chars = grapheme.symbol.chars().peekable();
        if hides(grapheme.symbol) || (at_start && joins_a_space(grapheme.symbol)) {
            *hidden = true;
            // The grapheme keeps its first character, if the terminal
            // draws it alone, and shows the others escaped.
            let drawn_alone = |c: &char| {
                let c = c.encode_utf8(&mut [0; 4]).to_string();
                !hides(&c) && !(at_start && joins_a_space(&c))
            };
            if let Some(base) = chars.next_if(drawn_alone) {
                push_styled(&mut out, &base.to_string(), style);
            }
            // Not the style of the first character: it can be the dim
            // mark of a tab.
            style = WARNING;
            let mut shown = String::new();
            chars.for_each(|c| push_escape(&mut shown, c));
            push_styled(&mut out, &shown, style);
        } else {
            push_styled(&mut out, grapheme.symbol, style);
        }
    }
    out
}

/// syntect is slow on some bash. Its time grows with the bytes, and with
/// the square of the length of each line: 32 KB of heredoc takes about
/// 1 s, 32 lines of 1,000 spaces 10 s, and a line of 80 KB 10 s. So a
/// block is highlighted only if its lines are at most `HIGHLIGHT_LINE`
/// bytes long, and if the highlighted code of the file stays within
/// `HIGHLIGHT_BYTES` and `HIGHLIGHT_SQUARES`, the sum of the squares of
/// its line lengths. On 2026-11-09, the runbooks of ~/dev/claude/runbooks
/// had at most 16 KB of bash, lines of at most 386 bytes, and a sum of
/// squares of at most 2,000,000.
const HIGHLIGHT_LINE: usize = 1_000;
const HIGHLIGHT_BYTES: usize = 32 * 1024;
const HIGHLIGHT_SQUARES: usize = 4_000_000;

/// The code highlighted so far.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
struct Highlighted {
    bytes: usize,
    squares: usize,
}

impl Highlighted {
    /// The total with `code`, if `code` can be highlighted.
    fn with(self, code: &str) -> Option<Highlighted> {
        let mut squares = self.squares;
        for line in code.lines() {
            if line.len() > HIGHLIGHT_LINE {
                return None;
            }
            squares += line.len() * line.len();
        }
        let bytes = self.bytes + code.len();
        (bytes <= HIGHLIGHT_BYTES && squares <= HIGHLIGHT_SQUARES)
            .then_some(Highlighted { bytes, squares })
    }
}

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// The style of each kind of bash token, in the 16 terminal colors.
static SCOPE_STYLES: LazyLock<Vec<(Scope, Style)>> = LazyLock::new(|| {
    [
        // Not dim: the grammar works line by line, and marks as a comment
        // code that bash runs, such as "#; curl x | sh" after a line that
        // ends with "\". Dim code would look inert.
        ("comment", Style::new().add_modifier(Modifier::ITALIC)),
        ("string.quoted", Style::new().fg(Color::Green)),
        ("keyword.control", Style::new().fg(Color::Magenta)),
        ("keyword.operator.logical", Style::new().fg(Color::Magenta)),
        ("storage.modifier", Style::new().fg(Color::Magenta)),
        ("variable.other.readwrite", Style::new().fg(Color::Cyan)),
        (
            "punctuation.definition.variable",
            Style::new().fg(Color::Cyan),
        ),
        ("variable.function", Style::new().fg(Color::Blue)),
        ("support.function", Style::new().fg(Color::Blue)),
        ("entity.name.function", Style::new().fg(Color::Blue)),
        ("constant.numeric", Style::new().fg(Color::Yellow)),
    ]
    .into_iter()
    .map(|(scope, style)| (Scope::new(scope).expect("a valid scope"), style))
    .collect()
});

/// Highlights bash code, one entry per line. The innermost scope of each
/// token that has a style gives the token's style.
fn highlight(code: &str, hidden: &mut bool) -> Vec<Vec<Span<'static>>> {
    let syntax = SYNTAXES
        .find_syntax_by_token("bash")
        .expect("syntect has a bash syntax");
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ops = state.parse_line(line, &SYNTAXES).unwrap_or_default();
        let mut spans = Vec::new();
        for (region, op) in ScopeRegionIterator::new(&ops, line) {
            // An invalid operation only loses the colors of the rest.
            let _ = stack.apply(op);
            // The newline is not part of the text of the line.
            let text = region.trim_end_matches('\n').to_string();
            spans.push(Span::styled(text, scope_style(&stack)));
        }
        lines.push(code_line(&spans, hidden));
    }
    lines
}

fn scope_style(stack: &ScopeStack) -> Style {
    for scope in stack.as_slice().iter().rev() {
        if let Some((_, style)) = SCOPE_STYLES.iter().find(|(s, _)| s.is_prefix_of(*scope)) {
            return *style;
        }
    }
    Style::new()
}

/// Warnings never quote free text of the info string: the info line wraps,
/// and a quoted value with spaces could make a wrapped line that passes
/// for code. Keys hold no space, and show at most 20 characters.
const INVALID_NAME: &str = "invalid name: use [A-Za-z0-9_.-]";

fn short(key: &str) -> String {
    match key.get(..20) {
        Some(start) if key.len() > 20 => format!("{start}…"),
        _ => key.to_string(),
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_unquoted(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '/' | '.' | '~' | '-')
}

fn is_separator(c: char) -> bool {
    matches!(c, ' ' | '\t' | ',')
}

/// Parses `[key:value, key:"value"]`. Returns the pairs read before the
/// first syntax error, and that error.
fn parse_attributes(section: &str) -> (Vec<(String, String)>, Option<String>) {
    let mut pairs = Vec::new();
    let mut rest = &section[1..];
    loop {
        rest = rest.trim_start_matches(is_separator);
        if let Some(after) = rest.strip_prefix(']') {
            let after = after.trim();
            let error = (!after.is_empty()).then(|| "unexpected text after ']'".to_string());
            return (pairs, error);
        }
        let Some(first) = rest.chars().next() else {
            return (pairs, Some("missing ']'".into()));
        };
        let key_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(rest.len());
        if key_len == 0 {
            return (
                pairs,
                Some(format!("unexpected '{first}' where a key starts")),
            );
        }
        let key = &rest[..key_len];
        let Some(after) = rest[key_len..].strip_prefix(':') else {
            return (pairs, Some(format!("missing ':' after '{}'", short(key))));
        };
        let value;
        (value, rest) = match after.strip_prefix('"') {
            Some(quoted) => match unquote(quoted) {
                Some(found) => found,
                None => {
                    let error = format!("missing '\"' in the value of '{}'", short(key));
                    return (pairs, Some(error));
                }
            },
            None => {
                let len = after.find(|c| !is_unquoted(c)).unwrap_or(after.len());
                if len == 0 {
                    return (
                        pairs,
                        Some(format!("missing value after '{}:'", short(key))),
                    );
                }
                (after[..len].to_string(), &after[len..])
            }
        };
        if let Some(c) = rest.chars().next()
            && !(is_separator(c) || c == ']')
        {
            return (
                pairs,
                Some(format!(
                    "unexpected '{c}' after the value of '{}'",
                    short(key)
                )),
            );
        }
        pairs.push((key.to_string(), value));
    }
}

/// Reads a quoted value up to its closing quote, and returns it with the
/// text after the quote. Only `\"` and `\\` are escapes.
fn unquote(text: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((value, &text[i + 1..])),
            '\\' if matches!(chars.clone().next(), Some((_, '"' | '\\'))) => {
                value.extend(chars.next().map(|(_, c)| c));
            }
            c => value.push(c),
        }
    }
    None
}

fn mark_duplicates(blocks: &mut [Block]) {
    let mut counts = HashMap::new();
    for name in blocks.iter().filter_map(|b| b.name.as_deref()) {
        *counts.entry(name.to_string()).or_insert(0) += 1;
    }
    for block in blocks {
        if block.name.as_ref().is_some_and(|n| counts[n] > 1) {
            block.problem = Some("duplicate name");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The blocks of the new `markdown` matched against those of `old`.
    fn matches(old: &str, new: &str) -> Vec<Option<usize>> {
        match_blocks(&parse(old).blocks, &parse(new).blocks)
    }

    fn fence(info: &str, code: &str) -> String {
        format!("```{info}\n{code}\n```\n\n")
    }

    #[test]
    fn blocks_match_by_name_or_by_code() {
        let old = [
            fence("bash [name:a]", "ls"),
            fence("bash", "echo one"),
            fence("bash [name:b]", "pwd"),
            fence("bash [name:gone]", "true"),
        ]
        .concat();
        // b moved up and changed its code; the unnamed block keeps its
        // code; a block is new; gone is gone.
        let new = [
            fence("bash [name:b]", "pwd -P"),
            fence("bash [name:a]", "ls"),
            fence("bash", "echo one"),
            fence("bash", "echo two"),
        ]
        .concat();
        assert_eq!(matches(&old, &new), [Some(2), Some(0), Some(1), None]);
    }

    #[test]
    fn blocks_with_the_same_key_match_in_order() {
        let old = [fence("bash", "ls"), fence("bash", "ls")].concat();
        let new = [
            fence("bash", "ls"),
            fence("bash", "pwd"),
            fence("bash", "ls"),
        ]
        .concat();
        assert_eq!(matches(&old, &new), [Some(0), None, Some(1)]);
        let three = [
            fence("bash", "ls"),
            fence("bash", "ls"),
            fence("bash", "ls"),
        ]
        .concat();
        assert_eq!(matches(&old, &three), [Some(0), Some(1), None]);
        // A name and code of the same text are two keys.
        let named = fence("bash [name:ls]", "x");
        assert_eq!(matches(&fence("bash", "ls"), &named), [None]);
    }

    fn info(info: &str) -> Option<(Option<String>, Vec<String>)> {
        block(info, "").map(|b| (b.name, b.warnings))
    }

    fn named(name: &str, warnings: &[&str]) -> Option<(Option<String>, Vec<String>)> {
        let name = (!name.is_empty()).then(|| name.to_string());
        Some((name, warnings.iter().map(|w| w.to_string()).collect()))
    }

    #[test]
    fn only_bash_and_sh_make_blocks() {
        assert_eq!(info("bash"), named("", &[]));
        assert_eq!(info("sh [name:a]"), named("a", &[]));
        assert_eq!(info("bash[name:a]"), named("a", &[]));
        for other in ["", "python", "Bash", "bash extra", "shell [name:a]"] {
            assert_eq!(info(other), None, "{other:?}");
        }
    }

    #[test]
    fn attributes_give_the_name_or_warnings() {
        let cases = [
            ("bash [name:a.b_c-1]", named("a.b_c-1", &[])),
            ("bash [name:\"x\"]", named("x", &[])),
            ("bash [ , name:a ,, ]", named("a", &[])),
            ("bash [name:a]  ", named("a", &[])),
            ("bash [name:a b:c]", named("a", &["unknown attribute 'b'"])),
            (
                "bash [name:a, name:b]",
                named("a", &["name is given twice"]),
            ),
            (
                "bash [name:a/b, name:c]",
                named("", &[INVALID_NAME, "name is given twice"]),
            ),
            ("bash [name:a/b]", named("", &[INVALID_NAME])),
            ("bash [name:\"a b\"]", named("", &[INVALID_NAME])),
            ("bash [name:\"\"]", named("", &[INVALID_NAME])),
            ("bash [name:a", named("a", &["missing ']'"])),
            ("bash [", named("", &["missing ']'"])),
            (
                "bash [name:a] x   cd /",
                named("a", &["unexpected text after ']'"]),
            ),
            (
                "bash [name:a abcdefghijklmnopqrstuvwxyz:1]",
                named("a", &["unknown attribute 'abcdefghijklmnopqrst…'"]),
            ),
            (
                "bash [name:a abcdefghijklmnopqrst:1]",
                named("a", &["unknown attribute 'abcdefghijklmnopqrst'"]),
            ),
            (
                "bash [:a]",
                named("", &["unexpected ':' where a key starts"]),
            ),
            ("bash [name]", named("", &["missing ':' after 'name'"])),
            ("bash [name:]", named("", &["missing value after 'name:'"])),
            (
                "bash [name:a!]",
                named("", &["unexpected '!' after the value of 'name'"]),
            ),
            (
                "bash [name:\"a\"b]",
                named("", &["unexpected 'b' after the value of 'name'"]),
            ),
            (
                "bash [name:\"a]",
                named("", &["missing '\"' in the value of 'name'"]),
            ),
            ("bash [name:a x]", named("a", &["missing ':' after 'x'"])),
        ];
        for (text, expected) in cases {
            assert_eq!(info(text), expected, "{text:?}");
        }
    }

    #[test]
    fn quoted_values_decode_only_quote_and_backslash_escapes() {
        let (pairs, error) = parse_attributes(r#"[a:"q\"b\\s\n"]"#);
        assert_eq!(pairs, [("a".into(), r#"q"b\s\n"#.into())]);
        assert_eq!(error, None);
    }

    fn quote(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    proptest! {
        #[test]
        fn quoted_values_round_trip(
            pairs in prop::collection::vec(("[A-Za-z0-9_-]{1,8}", any::<String>()), 0..5),
            separator in "[ ,\t]{1,3}",
        ) {
            let section = pairs
                .iter()
                .map(|(k, v)| format!("{k}:{}", quote(v)))
                .collect::<Vec<_>>()
                .join(&separator);
            let (parsed, error) = parse_attributes(&format!("[{section}]"));
            prop_assert_eq!(error, None);
            prop_assert_eq!(parsed, pairs);
        }

        #[test]
        fn unquoted_values_round_trip(
            pairs in prop::collection::vec(("[A-Za-z0-9_-]{1,8}", "[A-Za-z0-9_+#/.~-]{1,8}"), 0..5),
        ) {
            let section = pairs.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",");
            let (parsed, error) = parse_attributes(&format!("[{section}]"));
            prop_assert_eq!(error, None);
            prop_assert_eq!(parsed, pairs);
        }

        #[test]
        fn escaped_text_has_no_control_character_and_tracks_its_width(text in any::<String>()) {
            let mut column = 0;
            let mut out = Vec::new();
            escape_into(&mut out, &text, Style::new(), &mut column, Text::Prose);
            prop_assert!(out.iter().all(|s| !s.content.chars().any(needs_escape)));
            prop_assert_eq!(column, out.iter().map(|s| width(&s.content)).sum::<usize>());
        }

        #[test]
        fn any_info_string_parses_without_panic(text in any::<String>()) {
            block(&format!("bash [{text}"), "");
        }
    }

    fn names(text: &str) -> Vec<Option<String>> {
        parse(text).blocks.into_iter().map(|b| b.name).collect()
    }

    #[test]
    fn blocks_keep_their_code_and_order() {
        let doc = parse("```bash [name:a]\necho 1\n```\n\n```sh\necho 2\n```\n");
        let code: Vec<_> = doc.blocks.iter().map(|b| b.code.as_str()).collect();
        assert_eq!(code, ["echo 1\n", "echo 2\n"]);
        assert_eq!(
            names("```bash [name:a]\n```\n```sh\n```\n"),
            [Some("a".into()), None]
        );
    }

    #[test]
    fn the_code_of_a_block_open_at_the_end_of_the_file_ends_with_a_line_feed() {
        // bash runs "x=1 \\" with no final line feed as the command "\\".
        assert_eq!(parse("```bash\nx=1 \\").blocks[0].code, "x=1 \\\n");
        assert_eq!(parse("```bash\n").blocks[0].code, "");
    }

    #[test]
    fn a_nul_becomes_u_fffd_in_code_and_text() {
        let doc = parse("a\0b\n\n```bash\nr\0m -rf x\n```\n");
        assert_eq!(doc.blocks[0].code, "r\u{fffd}m -rf x\n");
        let Kind::Text(spans) = &doc.items[0].kind else {
            panic!("{:?}", doc.items[0])
        };
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a\u{fffd}b");
    }

    #[test]
    fn indented_and_other_code_blocks_are_not_blocks() {
        assert_eq!(names("    bash\n\n```python\nx\n```\n\n```\nls\n```\n"), []);
    }

    #[test]
    fn blocks_inside_quotes_and_lists_are_blocks() {
        let text = "> ```bash [name:a]\n> ls\n> ```\n\n- ~~~sh [name:b]\n  ls\n  ~~~\n";
        assert_eq!(names(text), [Some("a".into()), Some("b".into())]);
        assert_eq!(parse(text).blocks[1].code, "ls\n");
    }

    #[test]
    fn the_info_string_is_read_as_written() {
        // pulldown-cmark would decode \" to ", and end the value at the
        // second quote.
        let doc = parse("```bash [name:\"a\\\"b\"]\n```\n");
        assert_eq!(doc.blocks[0].warnings, [INVALID_NAME]);
    }

    #[test]
    fn all_blocks_with_a_duplicate_name_cannot_run() {
        let doc = parse("```bash [name:a]\n```\n```bash [name:b]\n```\n```bash [name:a]\n```\n");
        let problems: Vec<_> = doc.blocks.iter().map(|b| b.problem).collect();
        assert_eq!(
            problems,
            [Some("duplicate name"), None, Some("duplicate name")]
        );
    }

    fn shown(text: &str) -> String {
        escaped(text, Style::new())
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn control_characters_show_as_vim_shows_them() {
        let cases = [
            ("a\u{1b}[2K\rb", "a^[[2K^Mb"),
            ("\0\u{1f}\n", "^@^_^J"),
            ("\u{7f}", "^?"),
            ("\u{80}\u{9b}\u{9f}", "<80><9b><9f>"),
            ("a\u{202e}b\u{2066}", "a<202e>b<2066>"),
            (
                "\u{61c}\u{200e}\u{200f}\u{202a}\u{2069}",
                "<061c><200e><200f><202a><2069>",
            ),
            ("é\u{a0}日", "é\u{a0}日"),
        ];
        for (text, expected) in cases {
            assert_eq!(shown(text), expected, "{text:?}");
        }
    }

    #[test]
    fn escapes_use_the_warning_color() {
        let spans = escaped("a\u{1b}", Style::new().add_modifier(Modifier::BOLD));
        assert_eq!(
            spans[0],
            Span::styled("a", Style::new().add_modifier(Modifier::BOLD))
        );
        assert_eq!(
            spans[1].style,
            Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow)
        );
    }

    #[test]
    fn escapes_in_a_row_make_one_span() {
        let spans = escaped("a\u{1b}\u{7}b", Style::new());
        let texts: Vec<_> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["a", "^[^G", "b"]);
    }

    #[test]
    fn tabs_expand_to_the_next_multiple_of_8_columns() {
        assert_eq!(shown("\ta\tbc"), "        a       bc");
        assert_eq!(shown("日本語\tx"), "日本語  x");
        assert_eq!(shown("⚠\u{fe0f}\tx"), "⚠\u{fe0f}      x");
        assert_eq!(shown("^\u{1b}\tx"), "^^[     x");
        assert_eq!(shown("abc\t\tx"), format!("abc{}x", " ".repeat(13)));
    }

    /// The warnings of a bash block of `code`.
    fn warnings(code: &str) -> Vec<String> {
        parse(&format!("```bash\n{code}```\n"))
            .blocks
            .remove(0)
            .warnings
    }

    #[test]
    fn code_with_control_characters_gets_a_warning() {
        let warning = warnings;
        assert_eq!(warning("a\u{1b}b\n"), ["hidden characters in code"]);
        assert_eq!(warning("a\u{85}\n"), ["hidden characters in code"]);
        assert_eq!(warning("a\u{202e}\n"), ["hidden characters in code"]);
        assert!(warning("a\tb\n").is_empty());
    }

    #[test]
    fn a_file_that_could_make_too_many_table_cells_shows_tables_as_text() {
        let header = format!("{}|\n{}|\n", "|a".repeat(399), "|-".repeat(399));
        let is_table = |text: &str| matches!(parse(text).items[0].kind, Kind::Table(..));
        // The header has 400 `|`: 498 lines can make 199,698 cells, and
        // 499 lines 200,099.
        assert!(is_table(&format!("{header}{}", "b\n".repeat(496))));
        assert!(!is_table(&format!("{header}{}", "b\n".repeat(497))));
        // A lone CR ends a line too.
        let header = header.replace('\n', "\r");
        assert!(!is_table(&format!("{header}{}", "b\r".repeat(497))));
    }

    #[test]
    fn body_rows_lose_their_trailing_empty_cells() {
        let doc = parse("| a | b | c |\n|---|---|---|\n| x |\n| | y |\n");
        let Kind::Table(_, rows) = &doc.items[0].kind else {
            panic!("not a table: {:?}", doc.items[0]);
        };
        let lengths: Vec<_> = rows.iter().map(Vec::len).collect();
        assert_eq!(lengths, [3, 1, 2]);
    }

    #[test]
    fn highlighting_stops_at_long_lines_and_at_the_budgets() {
        let line = |n| format!("{}\n", "a".repeat(n));
        let none = Highlighted::default();
        let total = |bytes, squares| Some(Highlighted { bytes, squares });
        assert_eq!(none.with(&line(1_000)), total(1_001, 1_000_000));
        assert_eq!(none.with(&line(1_001)), None);
        let bytes = Highlighted {
            bytes: HIGHLIGHT_BYTES - 3,
            squares: 0,
        };
        assert_eq!(bytes.with("ls\n"), total(HIGHLIGHT_BYTES, 4));
        let bytes = Highlighted {
            bytes: HIGHLIGHT_BYTES - 2,
            squares: 0,
        };
        assert_eq!(bytes.with("ls\n"), None);
        let squares = Highlighted {
            bytes: 0,
            squares: HIGHLIGHT_SQUARES - 4,
        };
        assert_eq!(squares.with("ls\n"), total(3, HIGHLIGHT_SQUARES));
        assert_eq!(squares.with("lsx\n"), None);
    }

    #[test]
    fn a_block_of_2_kb_is_highlighted() {
        let doc = parse(&format!("```bash\n{}```\n", "ls\n".repeat(700)));
        let Kind::Code { lines, .. } = &doc.items[0].kind else {
            panic!("not code: {:?}", doc.items[0]);
        };
        assert_ne!(lines[0][0].style, Style::new());
    }

    #[test]
    fn the_highlight_budget_adds_up_over_the_blocks_of_a_file() {
        let mut builder = Builder::default();
        builder.highlighted.bytes = HIGHLIGHT_BYTES - 6;
        let doc = parse_with(
            builder,
            "```bash\nls\n```\n\n```bash\nls\n```\n\n```bash\nls\n```\n",
        );
        let highlighted: Vec<bool> = (doc.items.iter())
            .filter_map(|item| match &item.kind {
                Kind::Code { lines, .. } => Some(lines[0][0].style != Style::new()),
                _ => None,
            })
            .collect();
        assert_eq!(highlighted, [true, true, false]);
    }

    #[test]
    fn code_escapes_the_characters_that_a_terminal_draws_as_nothing() {
        let shown = |code: &str| -> String {
            code_line(&[Span::raw(code)], &mut false)
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(shown("\"prod\u{200b}\""), "\"prod<200b>\"");
        assert_eq!(shown("a\u{ad}\u{feff}\u{e0041}"), "a<ad><feff><e0041>");
        assert_eq!(shown("\u{301}x"), "<0301>x");
        assert_eq!(shown("'\u{ff9e}\u{ff9e}'"), "'<ff9e><ff9e>'");
        // ratatui counts a halfwidth sound mark twice: 3 columns here.
        assert_eq!(shown("\u{ff76}\u{ff9e}"), "\u{ff76}<ff9e>");
        assert_eq!(shown("\u{301}\u{ff9e}\u{ff9e}"), "<0301><ff9e><ff9e>");
        assert_eq!(shown("e\u{301}"), "e<0301>");
        assert_eq!(shown("k\u{302e}\u{302e}"), "k<302e><302e>");
        assert_eq!(shown("\u{e9}\u{1f600}"), "\u{e9}\u{1f600}");
        assert_eq!(escaped_text("\u{200b}\u{200d}"), "\u{200b}\u{200d}");
        for code in ["a\u{200b}\n", "\u{301}a\n", "'\u{ff9e}\u{ff9e}'\n"] {
            assert_eq!(warnings(code), ["hidden characters in code"], "{code:?}");
        }
        assert!(warnings("\u{e9}\t\u{1f600}\n").is_empty());
    }

    #[test]
    fn the_warning_follows_the_escapes_after_a_tab() {
        // The last space of a tab and U+FF9E make one grapheme of 3 columns.
        assert_eq!(warnings("echo\t\u{ff9e}\n"), ["hidden characters in code"]);
        // U+0301 joins the last space of the tab into one grapheme.
        assert_eq!(warnings("echo\t\u{301}x\n"), ["hidden characters in code"]);
    }

    #[test]
    fn code_escapes_the_format_characters_that_join_the_next_one() {
        let joiners = [
            '\u{600}',
            '\u{605}',
            '\u{6dd}',
            '\u{70f}',
            '\u{890}',
            '\u{891}',
            '\u{8e2}',
            '\u{2d7f}',
            '\u{110bd}',
            '\u{110cd}',
            '\u{d4e}',
            '\u{111c2}',
            '\u{111c3}',
            '\u{1193f}',
            '\u{11941}',
            '\u{11a3a}',
            '\u{11a84}',
            '\u{11a89}',
            '\u{11d46}',
            '\u{11f02}',
            '\u{113d1}',
            '\u{fff9}',
            '\u{fffb}',
            '\u{13430}',
            '\u{1343f}',
        ];
        for c in joiners {
            assert!(needs_escape_in_code(c) && !needs_escape(c), "{c:?}");
        }
        // U+070F joins the "b" after it into one grapheme of width 1.
        assert_eq!(warnings("a\u{70f}b\n"), ["hidden characters in code"]);
    }

    #[test]
    fn code_escapes_a_mark_that_would_join_the_space_of_the_gutter() {
        let shown = |code: &str| -> String {
            code_line(&[Span::raw(code)], &mut false)
                .iter()
                .map(|s| s.content.as_ref())
                .collect()
        };
        assert_eq!(shown("\u{93e}rm -rf /tmp/x"), "<093e>rm -rf /tmp/x");
        assert_eq!(shown("\u{1f3fb}x"), "<1f3fb>x");
        assert_eq!(shown("x\u{93e}"), "x<093e>");
        assert_eq!(warnings("\u{93e}rm\n"), ["hidden characters in code"]);
    }

    #[test]
    fn a_tab_in_code_starts_with_a_dim_mark() {
        let spans = code_line(&[Span::raw("a\tb")], &mut false);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, format!("a{TAB_MARK}      b"));
        assert!(
            spans
                .iter()
                .any(|s| s.content == TAB_MARK && s.style == DIM)
        );
        assert_eq!(shown("a\tb"), "a       b");
    }

    #[test]
    fn code_escapes_regional_indicators() {
        // ratatui counts one as 1 column; some terminals count 2.
        assert!(needs_escape_in_code('\u{1f1e6}') && needs_escape_in_code('\u{1f1ff}'));
        assert!(!needs_escape('\u{1f1eb}'));
        assert_eq!(
            warnings("echo \u{1f1eb}\u{1f1f7}\n"),
            ["hidden characters in code"]
        );
    }

    #[test]
    fn code_escapes_the_spaces_other_than_u_0020_and_the_blanks() {
        let spaces = [
            '\u{a0}',
            '\u{1680}',
            '\u{2000}',
            '\u{200a}',
            '\u{2028}',
            '\u{2029}',
            '\u{202f}',
            '\u{205f}',
            '\u{2800}',
            '\u{3000}',
            '\u{1d159}',
        ];
        for c in spaces {
            assert!(needs_escape_in_code(c) && !needs_escape(c), "{c:?}");
        }
        assert!(!needs_escape_in_code(' '));
        assert_eq!(warnings("true\u{a0}#; ls\n"), ["hidden characters in code"]);
    }

    #[test]
    fn code_escapes_the_characters_that_terminals_draw_wider_than_ratatui() {
        // ratatui counts 1 column. glibc's wcwidth and tmux give 2 to the
        // circled numbers in every locale, and tmux 3.8 forces the others
        // wide.
        let wide = [
            '\u{3248}',
            '\u{324f}',
            '\u{261d}',
            '\u{26f9}',
            '\u{270c}',
            '\u{270d}',
            '\u{1f3cb}',
            '\u{1f3cc}',
            '\u{1f574}',
            '\u{1f575}',
            '\u{1f590}',
        ];
        for c in wide {
            assert!(needs_escape_in_code(c) && !needs_escape(c), "{c:?}");
        }
        // Their neighbours are wide for ratatui too.
        assert!(!needs_escape_in_code('\u{3247}') && !needs_escape_in_code('\u{3250}'));
        assert_eq!(
            warnings("echo ok \u{3248};reboot x\n"),
            ["hidden characters in code"]
        );
    }

    #[test]
    fn code_escapes_the_characters_of_its_marks() {
        for c in ['␣', '⇥'] {
            assert!(needs_escape_in_code(c) && !needs_escape(c), "{c:?}");
        }
        assert_eq!(warnings("rm -rf /tmp/a␣/\n"), ["hidden characters in code"]);
    }

    #[test]
    fn code_escapes_each_range_of_default_ignorable_code_points() {
        let ends = [
            '\u{ad}',
            '\u{34f}',
            '\u{115f}',
            '\u{1160}',
            '\u{17b4}',
            '\u{17b5}',
            '\u{180b}',
            '\u{180f}',
            '\u{200b}',
            '\u{200d}',
            '\u{2060}',
            '\u{206f}',
            '\u{3164}',
            '\u{fe00}',
            '\u{fe0f}',
            '\u{feff}',
            '\u{ffa0}',
            '\u{fff0}',
            '\u{fff8}',
            '\u{1bca0}',
            '\u{1bca3}',
            '\u{1d173}',
            '\u{1d17a}',
            '\u{e0000}',
            '\u{e0fff}',
        ];
        for c in ends {
            assert!(needs_escape_in_code(c) && !needs_escape(c), "{c:?}");
        }
        let outside = ['\u{ac}', '\u{2010}', '\u{fdff}', '\u{fffc}', '\u{e1000}'];
        assert!(!outside.into_iter().any(needs_escape_in_code));
    }

    fn code_text() -> impl Strategy<Value = String> {
        let chars = prop_oneof![
            any::<char>(),
            Just('\n'),
            Just('\r'),
            Just('\t'),
            Just('"'),
            Just('\u{301}'),
            Just('\u{fe0f}'),
            Just('\u{200b}'),
            Just('\u{1b}'),
            Just('$'),
            Just('\u{ff9e}'),
            Just('\u{93e}'),
            Just('\u{1f468}'),
            Just('\u{1f3fb}'),
            Just('\\'),
            Just('⇥'),
            Just('␣'),
            // A comment, which must never show dim.
            Just('#'),
            Just(' '),
        ];
        prop::collection::vec(chars, 0..60).prop_map(|c| c.into_iter().collect())
    }

    proptest! {
        // What runs is what the view shows: each line of a block shows its
        // code, escaped, whether highlighted or not, and every grapheme
        // takes room on screen.
        #[test]
        fn the_shown_code_is_the_code_that_runs(code in code_text()) {
            let doc = parse(&format!("```bash\n{code}\n```\n"));
            for item in &doc.items {
                let Kind::Code { lines, block: Some(i) } = &item.kind else { continue };
                // Without the spaces after a tab mark: syntect can split a
                // grapheme in two spans, and each part then takes its own
                // width, so the tab stops can differ.
                let text = |spans: &[Span]| -> String {
                    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
                    let mut parts = text.split(TAB_MARK);
                    let first = parts.next().unwrap_or_default();
                    let rest = parts.map(|p| p.trim_start_matches(' '));
                    std::iter::once(first).chain(rest).collect::<Vec<_>>().join(TAB_MARK)
                };
                let shown: Vec<String> = lines.iter().map(|l| text(l)).collect();
                let code = doc.blocks[*i].code.split_terminator('\n');
                let expected: Vec<String> =
                    code.map(|l| text(&code_line(&[Span::raw(l)], &mut false))).collect();
                prop_assert_eq!(&shown, &expected);
                for span in lines.iter().flatten() {
                    // Only the mark of a tab is dim, and the glyph of a
                    // mark is always a mark.
                    let dim = span.style.add_modifier.contains(Modifier::DIM);
                    prop_assert!(!dim || span.content.replace(TAB_MARK, "").is_empty(), "{:?}", span);
                    prop_assert!(dim || !span.content.contains(['⇥', '␣']), "{:?}", span);
                    for g in span.styled_graphemes(Style::new()) {
                        prop_assert!(!hides(g.symbol), "{:?} in {:?}", g.symbol, span);
                    }
                }
            }
        }
    }

    #[test]
    fn a_highlighted_grapheme_keeps_its_combining_mark() {
        // syntect ends the string at the quote, before the U+0301.
        let lines = highlight("cd \"/tmp/x\"\u{301}\n", &mut false);
        let spans: Vec<_> = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(!spans.iter().any(|s| s.starts_with('\u{301}')), "{spans:?}");
    }

    #[test]
    fn nesting_counts_markers_and_indent_at_the_start_of_a_line() {
        let cases = [
            ("text", 0),
            ("> a", 1),
            (">>> a", 3),
            // 4 markers and 4 columns of spaces.
            ("> - > 1. a", 6),
            ("  - * + 123456789) a", 7),
            ("        code", 4),
            ("\t\t\tcode", 6),
            ("-", 1),
            ("--- a", 0),
            ("-a", 0),
            ("1234567890. a", 0),
            ("1- a", 0),
            ("a\r>>\n> > >", 4),
        ];
        for (text, expected) in cases {
            assert_eq!(nesting(text), expected, "{text:?}");
        }
    }

    #[test]
    fn a_code_block_is_followed_by_a_blank_line() {
        let text =
            "- Dry run first:\n  ```bash\n  ls \\\n  ```\n  ▐ --dry-run\n- ```\n  x\n  ```\n- next";
        let kinds: Vec<_> = (parse(text).items.iter())
            .map(|item| match item.kind {
                Kind::Code { .. } => "code",
                Kind::Blank => "blank",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            ["other", "code", "blank", "other", "code", "blank", "other"]
        );
    }

    #[test]
    fn frontmatter_runs_from_the_first_line_to_the_next_dash_line() {
        let text = "---\ntitle: x\n---  \n# T";
        assert_eq!(split_frontmatter(text), (Some("title: x\n"), "# T"));
        assert_eq!(split_frontmatter("---\n---"), (Some(""), ""));
    }

    #[test]
    fn without_a_closing_line_there_is_no_frontmatter() {
        assert_eq!(split_frontmatter("---\na\n"), (None, "---\na\n"));
        assert_eq!(
            split_frontmatter(" ---\na\n---\n"),
            (None, " ---\na\n---\n")
        );
    }

    #[test]
    fn dashes_after_the_first_line_never_hide_blocks() {
        let blocks = ["\n---\na: b\n---\n", "# T\n\n---\nx\n---\n", "a\n---\n"];
        for start in blocks {
            let text = format!("{start}\n```bash [name:a]\n```\n");
            let doc = parse(&text);
            assert_eq!(split_frontmatter(&text).0, None, "{start:?}");
            assert_eq!(doc.blocks.len(), 1, "{start:?}");
        }
    }
}
