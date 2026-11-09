use pulldown_cmark::HeadingLevel;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::doc::{Document, Item};

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

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
    let width = width.max(1);
    let mut lines = Vec::new();
    for item in &doc.items {
        match item {
            Item::Text(text) => lines.extend(wrap(text, width)),
            Item::Heading(level, text) => {
                lines.extend(wrap(text, width));
                let rule = match level {
                    HeadingLevel::H1 => "━",
                    HeadingLevel::H2 => "─",
                    _ => continue,
                };
                lines.push(Line::styled(rule.repeat(width), DIM));
            }
            Item::Blank => lines.push(Line::default()),
        }
    }
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
    fn inline_styles_use_terminal_styles() {
        let text = "*em* **strong** ~~gone~~ `code` [link](http://x) ***both***";
        assert_snapshot!(render(text, 60), @"«i:em» «b:strong» «s:gone» «cyan:code» «u:link» «b,i:both»");
    }
}
