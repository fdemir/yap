use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::App;

pub fn render(frame: &mut Frame<'_>, app: &App, model: &str, workspace: &str) {
    let approval_height = if app.pending_approval.is_some() { 9 } else { 0 };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(approval_height),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, areas[0], model, workspace);
    frame.render_widget(
        Paragraph::new(app.transcript_text())
            .block(Block::default().borders(Borders::ALL).title(" Transcript "))
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0)),
        areas[1],
    );
    if app.pending_approval.is_some() {
        render_approval(frame, areas[2], app);
    }
    frame.render_widget(
        Paragraph::new(app.composer.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Ask yap ")),
        areas[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(app.status.label(), Style::default().fg(Color::Cyan)),
            Span::raw(" · Enter submit · ↑↓ scroll · Ctrl+C quit"),
        ])),
        areas[4],
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, model: &str, workspace: &str) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " yap ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {model} · {workspace}")),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_approval(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let approval = app
        .pending_approval
        .as_ref()
        .expect("approval area requires a pending request");
    let body = approval.request.preview.clone().unwrap_or_else(|| {
        serde_json::to_string_pretty(&approval.request.arguments)
            .unwrap_or_else(|_| "unable to display arguments".into())
    });
    frame.render_widget(
        Paragraph::new(format!("{}\n\nEnter: allow once    d/Esc: deny", body))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(format!(" Approval: {} ", approval.request.tool_name)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}
