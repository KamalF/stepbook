use std::collections::HashMap;
use std::sync::LazyLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::buffer::CellWidth;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

pub const INLINE_CODE: Style = Style::new().fg(Color::Cyan);
pub const DIM: Style = Style::new().add_modifier(Modifier::DIM);
pub const WARNING: Style = Style::new().fg(Color::Yellow);

/// Prefixes show only the innermost levels of quotes and list items, so
/// that the text keeps room in the view.
const MAX_DEPTH: usize = 10;

/// A fenced code block with the language `bash` or `sh` (spec 3).
#[derive(Debug, PartialEq)]
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

impl Document {
    /// The name of block `i`, or `#<n>` for the n-th block if it has no name.
    pub fn label(&self, i: usize) -> String {
        match &self.blocks[i].name {
            Some(name) => name.clone(),
            None => format!("#{}", i + 1),
        }
    }
}

pub fn parse(text: &str) -> Document {
    let (frontmatter, body) = split_frontmatter(text);
    let mut builder = Builder::default();
    if let Some(frontmatter) = frontmatter {
        let lines = source_lines(&format!("---\n{frontmatter}---"));
        builder.push_item(Kind::Source(lines));
        builder.blank = true;
    }
    for (event, range) in Parser::new_ext(body, options()).into_offset_iter() {
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
                    table.rows.push(std::mem::take(&mut table.row));
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
                if let Some((code, info)) = self.code.take() {
                    let block = block(&info, &code).map(|block| {
                        self.blocks.push(block);
                        self.blocks.len() - 1
                    });
                    let lines = match block {
                        Some(_) => highlight(&code),
                        None => code.lines().map(|l| escaped(l, Style::new())).collect(),
                    };
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
        escape_into(&mut self.text, text, style, &mut self.column);
    }

    /// Ends the text before a new element, and adds the blank line that
    /// separates them.
    fn start_element(&mut self) {
        self.flush_text();
        if std::mem::take(&mut self.blank) {
            // The prefix of a blank line keeps its quote bars, with no
            // trailing spaces.
            let (_, mut rest) = self.prefixes(false);
            while rest.last().is_some_and(|s| s.content.trim().is_empty()) {
                rest.pop();
            }
            if let Some(last) = rest.last_mut() {
                *last = Span::styled(last.content.trim_end().to_string(), last.style);
            }
            self.items.push(Item {
                first: rest.clone(),
                rest,
                kind: Kind::Blank,
            });
        }
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
                Container::Item { marker, indent } => {
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
        let (first, rest) = self.prefixes(true);
        self.items.push(Item { first, rest, kind });
    }
}

/// The dimmed lines of verbatim text, such as an HTML block. `lines` also
/// ends a line at a CRLF, which pulldown-cmark leaves in the frontmatter.
fn source_lines(text: &str) -> Vec<Vec<Span<'static>>> {
    text.lines().map(|line| escaped(line, DIM)).collect()
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH
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
    for (key, value) in pairs {
        if key != "name" {
            block.warnings.push(format!("unknown attribute '{key}'"));
        } else if block.name.is_some() {
            block.warnings.push("name is given twice".into());
        } else if is_valid_name(&value) {
            block.name = Some(value);
        } else {
            block.warnings.push(format!("invalid name '{value}'"));
        }
    }
    block.warnings.extend(error);
    if code
        .chars()
        .any(|c| needs_escape(c) && !matches!(c, '\t' | '\n'))
    {
        block.warnings.push("control characters in code".into());
    }
    Some(block)
}

/// Whether stepbook shows `c` escaped: a control character, or a bidi
/// control, which can reorder text in terminals that support right-to-left
/// scripts (spec 6.5).
pub fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Appends `text` to `out`, with each character that needs an escape
/// shown as vim shows it, in the warning color: `^[` for ESC, `^?` for
/// DEL, `<9b>` for a C1 character and `<202e>` for a bidi control. A tab
/// expands to spaces up to the next multiple of 8 columns. `column` counts
/// the columns of the line so far.
pub fn escape_into(out: &mut Vec<Span<'static>>, text: &str, style: Style, column: &mut usize) {
    // A run of plain text or of escapes, and the bytes of it that `column`
    // counts already.
    let mut run = String::new();
    let mut escapes = false;
    let mut counted = 0;
    for c in text.chars() {
        let escape = c != '\t' && needs_escape(c);
        if escape != escapes && !run.is_empty() {
            *column += width(&run[counted..]);
            out.push(run_span(std::mem::take(&mut run), style, escapes));
            counted = 0;
        }
        escapes = escape;
        if c == '\t' {
            *column += width(&run[counted..]);
            let spaces = 8 - *column % 8;
            run.extend(std::iter::repeat_n(' ', spaces));
            *column += spaces;
            counted = run.len();
        } else if escape {
            match u32::from(c) {
                0x7f => run.push_str("^?"),
                n @ 0..0x20 => run.extend(['^', char::from_u32(n + 0x40).unwrap_or('?')]),
                n @ 0x80..0x100 => run.push_str(&format!("<{n:02x}>")),
                n => run.push_str(&format!("<{n:04x}>")),
            }
        } else {
            run.push(c);
        }
    }
    *column += width(&run[counted..]);
    if !run.is_empty() {
        out.push(run_span(run, style, escapes));
    }
}

fn run_span(run: String, style: Style, escapes: bool) -> Span<'static> {
    Span::styled(run, if escapes { style.patch(WARNING) } else { style })
}

/// The width of text on screen, as the ratatui buffer measures it.
pub fn width(text: &str) -> usize {
    usize::from(text.cell_width())
}

/// One line of text, with its control characters escaped.
pub fn escaped(text: &str, style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    escape_into(&mut out, text, style, &mut 0);
    out
}

fn escape_line(spans: &[Span]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut column = 0;
    for span in spans {
        escape_into(&mut out, &span.content, span.style, &mut column);
    }
    out
}

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// The style of each kind of bash token, in the 16 terminal colors.
static SCOPE_STYLES: LazyLock<Vec<(Scope, Style)>> = LazyLock::new(|| {
    [
        ("comment", DIM),
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
fn highlight(code: &str) -> Vec<Vec<Span<'static>>> {
    let syntax = SYNTAXES
        .find_syntax_by_token("bash")
        .expect("syntect has a bash syntax");
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ops = state.parse_line(line, &SYNTAXES).unwrap_or_default();
        // The newline is not part of the text of the line.
        let text = line.strip_suffix('\n').unwrap_or(line);
        let mut spans = Vec::new();
        let mut start = 0;
        for (end, op) in ops {
            let end = end.min(text.len());
            spans.push(Span::styled(
                text[start..end].to_string(),
                scope_style(&stack),
            ));
            // An invalid operation only loses the colors of the rest.
            let _ = stack.apply(&op);
            start = end;
        }
        spans.push(Span::styled(text[start..].to_string(), scope_style(&stack)));
        lines.push(escape_line(&spans));
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
            let error = (!after.is_empty()).then(|| format!("unexpected text after ']': {after}"));
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
            return (pairs, Some(format!("missing ':' after '{key}'")));
        };
        let value;
        (value, rest) = match after.strip_prefix('"') {
            Some(quoted) => match unquote(quoted) {
                Some(found) => found,
                None => return (pairs, Some(format!("missing '\"' in the value of '{key}'"))),
            },
            None => {
                let len = after.find(|c| !is_unquoted(c)).unwrap_or(after.len());
                if len == 0 {
                    return (pairs, Some(format!("missing value after '{key}:'")));
                }
                (after[..len].to_string(), &after[len..])
            }
        };
        if let Some(c) = rest.chars().next()
            && !(is_separator(c) || c == ']')
        {
            return (
                pairs,
                Some(format!("unexpected '{c}' after the value of '{key}'")),
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
            ("bash [name:a/b]", named("", &["invalid name 'a/b'"])),
            ("bash [name:\"a b\"]", named("", &["invalid name 'a b'"])),
            ("bash [name:\"\"]", named("", &["invalid name ''"])),
            ("bash [name:a", named("a", &["missing ']'"])),
            ("bash [", named("", &["missing ']'"])),
            (
                "bash [name:a] x",
                named("a", &["unexpected text after ']': x"]),
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
            escape_into(&mut out, &text, Style::new(), &mut column);
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
        assert_eq!(doc.blocks[0].warnings, ["invalid name 'a\"b'"]);
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

    #[test]
    fn code_with_control_characters_gets_a_warning() {
        let warning = |code| block("bash", code).unwrap().warnings;
        assert_eq!(warning("a\u{1b}b\n"), ["control characters in code"]);
        assert_eq!(warning("a\u{85}\n"), ["control characters in code"]);
        assert_eq!(warning("a\u{202e}\n"), ["control characters in code"]);
        assert!(warning("a\tb\n").is_empty());
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
