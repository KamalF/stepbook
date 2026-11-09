use pulldown_cmark::HeadingLevel;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::doc::{DIM, Document, Item, Kind};

pub fn draw(frame: &mut Frame, doc: &Document) {
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let lines = layout(doc, usize::from(body.width));
    for (row, line) in (body.y..body.bottom()).zip(&lines) {
        frame.buffer_mut().set_line(body.x, row, line, body.width);
    }
    let line = Line::from(vec![
        Span::styled(" NORMAL ", Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw("  q quit"),
    ]);
    frame.render_widget(line, footer);
}

/// The lines of the document view at `width` columns.
pub fn layout(doc: &Document, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &doc.items {
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
            Kind::Blank => vec![Line::default()],
        };
        lines.extend(prefixed(item, body));
    }
    lines
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
        if let Some(fg) = style.fg {
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
        markup(&layout(&parse(markdown), width))
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
        let quotes = layout(&parse(&format!("{} x\n", ">".repeat(100_000))), 80);
        assert_eq!(quotes.len(), 1);
        let list: String = (0..1_000)
            .map(|i| format!("{}- a\n", "  ".repeat(i)))
            .collect();
        assert_eq!(layout(&parse(&list), 80).len(), 1_000);
    }

    #[test]
    fn inline_styles_use_terminal_styles() {
        let text = "*em* **strong** ~~gone~~ `code` [link](http://x) ***both***";
        assert_snapshot!(render(text, 60), @"«i:em» «b:strong» «s:gone» «cyan:code» «u:link» «b,i:both»");
    }
}
