use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub fn draw(frame: &mut Frame) {
    let [_, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let line = Line::from(vec![
        Span::styled(" NORMAL ", Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw("  q quit"),
    ]);
    frame.render_widget(line, footer);
}
