use std::collections::HashMap;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

pub const INLINE_CODE: Style = Style::new().fg(Color::Cyan);

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
pub enum Item {
    /// Styled text. A `\n` is a hard line break.
    Text(Vec<Span<'static>>),
    Heading(HeadingLevel, Vec<Span<'static>>),
    Blank,
}

#[derive(Debug)]
pub struct Document {
    #[cfg_attr(not(test), expect(dead_code, reason = "a later commit renders it"))]
    pub frontmatter: Option<String>,
    pub items: Vec<Item>,
    #[cfg_attr(not(test), expect(dead_code, reason = "a later commit renders them"))]
    pub blocks: Vec<Block>,
}

pub fn parse(text: &str) -> Document {
    let (frontmatter, body) = split_frontmatter(text);
    let mut builder = Builder::default();
    for (event, range) in Parser::new_ext(body, options()).into_offset_iter() {
        builder.event(event, &body[range]);
    }
    mark_duplicates(&mut builder.blocks);
    Document {
        frontmatter: frontmatter.map(str::to_string),
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
            Event::HardBreak => self.push("\n", Style::new()),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag, source: &str) {
        match tag {
            Tag::Paragraph => self.start_element(),
            Tag::Heading { level, .. } => {
                self.start_element();
                self.heading = Some(level);
                self.styles.push(Style::new().add_modifier(Modifier::BOLD));
            }
            Tag::CodeBlock(kind) => {
                self.start_element();
                let info = match kind {
                    CodeBlockKind::Fenced(_) => raw_info(source).to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((String::new(), info));
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { .. } => self
                .styles
                .push(Style::new().add_modifier(Modifier::UNDERLINED)),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_text(),
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.end_text();
            }
            TagEnd::CodeBlock => {
                if let Some((code, info)) = self.code.take() {
                    self.blocks.extend(block(&info, code));
                }
                self.blank = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            _ => {}
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        let style = self
            .styles
            .iter()
            .fold(Style::new(), |a, s| a.patch(*s))
            .patch(style);
        self.text.push(Span::styled(text.to_string(), style));
    }

    /// Ends the text before a new element, and adds the blank line that
    /// separates them.
    fn start_element(&mut self) {
        self.flush_text();
        if std::mem::take(&mut self.blank) {
            self.items.push(Item::Blank);
        }
    }

    fn end_text(&mut self) {
        self.flush_text();
        self.blank = true;
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        self.items.push(match self.heading.take() {
            Some(level) => Item::Heading(level, text),
            None => Item::Text(text),
        });
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH
}

/// Splits off the lines between a first line `---` and the next `---`
/// line. Without that closing line, the file has no frontmatter.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return (None, text);
    }
    let start = text.find('\n').map_or(text.len(), |i| i + 1);
    let mut end = start;
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

fn block(info: &str, code: String) -> Option<Block> {
    let (lang, section) = match info.find('[') {
        Some(i) => (&info[..i], Some(&info[i..])),
        None => (info, None),
    };
    if !matches!(lang.trim(), "bash" | "sh") {
        return None;
    }
    let mut block = Block {
        name: None,
        code,
        warnings: Vec::new(),
        problem: None,
    };
    let Some(section) = section else {
        return Some(block);
    };
    let (pairs, error) = parse_attributes(section);
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
    Some(block)
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
        block(info, String::new()).map(|b| (b.name, b.warnings))
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
        fn any_info_string_parses_without_panic(text in any::<String>()) {
            block(&format!("bash [{text}"), String::new());
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

    #[test]
    fn frontmatter_runs_from_the_first_line_to_the_next_dash_line() {
        let doc = parse("---\ntitle: x\n---  \n```bash [name:a]\n```\n");
        assert_eq!(doc.frontmatter.as_deref(), Some("title: x\n"));
        assert_eq!(doc.blocks.len(), 1);
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
            assert_eq!(doc.frontmatter, None, "{start:?}");
            assert_eq!(doc.blocks.len(), 1, "{start:?}");
        }
    }
}
