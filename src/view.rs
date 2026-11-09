use pulldown_cmark::{Alignment, HeadingLevel};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::doc::{Cell, DIM, Document, Item, Kind};

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const WARNING: Style = Style::new().fg(Color::Yellow);
const ERROR: Style = Style::new().fg(Color::Red);
const BLOCK_GUTTER: Style = Style::new();
const GUTTER: &str = "▌ ";

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
            Kind::Table(aligns, rows) => table(aligns, rows, width),
            Kind::Code { lines, block } => code(doc, lines, *block, width),
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

/// Draws a code block with a gutter. A block also gets an info line: its
/// label, its problem and its warnings. Long lines wrap at the width, so
/// that the view shows all the code that runs.
fn code(
    doc: &Document,
    lines: &[Vec<Span<'static>>],
    block: Option<usize>,
    width: usize,
) -> Vec<Line<'static>> {
    let gutter = Span::styled(GUTTER, if block.is_some() { BLOCK_GUTTER } else { DIM });
    let width = width.saturating_sub(GUTTER.chars().count()).max(1);
    let mut out = Vec::new();
    if let Some(i) = block {
        let mut info = vec![Span::styled(doc.label(i), BOLD)];
        if let Some(problem) = doc.blocks[i].problem {
            info.extend([Span::raw("  "), Span::styled(problem, ERROR)]);
        }
        let warnings = &doc.blocks[i].warnings;
        if !warnings.is_empty() {
            info.extend([Span::raw("  "), Span::styled(warnings.join("; "), WARNING)]);
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
    fn inline_styles_use_terminal_styles() {
        let text = "*em* **strong** ~~gone~~ `code` [link](http://x) ***both***";
        assert_snapshot!(render(text, 60), @"«i:em» «b:strong» «s:gone» «cyan:code» «u:link» «b,i:both»");
    }
}
