use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    agent::ToolOutcome,
    app::{App, Status, TranscriptEntry},
};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const USER_BG: Color = Color::Indexed(236);
const TOOL_PENDING_BG: Color = Color::Indexed(236);
const TOOL_SUCCESS_BG: Color = Color::Indexed(22);
const TOOL_DENIED_BG: Color = Color::Indexed(52);

pub fn render(frame: &mut Frame<'_>, app: &App, model: &str, workspace: &str) {
    let editor_height = if app.pending_approval.is_some() { 8 } else { 3 };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(editor_height),
            Constraint::Length(2),
        ])
        .split(frame.area());

    render_transcript(frame, areas[0], app);
    render_working_indicator(frame, areas[1], app.status);
    if app.pending_approval.is_some() {
        render_approval(frame, areas[2], app);
    } else {
        render_editor(frame, areas[2], app);
    }
    render_footer(frame, areas[3], app, model, workspace);
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for entry in &app.transcript {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        push_entry_lines(&mut lines, entry, area.width as usize);
    }
    if !app.assistant_draft.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.extend(
            app.assistant_draft
                .lines()
                .map(|line| Line::raw(line.to_owned())),
        );
    }

    let visible_height = area.height as usize;
    let bottom = lines.len().saturating_sub(visible_height);
    let scroll = bottom.saturating_sub(app.scroll as usize) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn push_entry_lines(lines: &mut Vec<Line<'static>>, entry: &TranscriptEntry, width: usize) {
    match entry {
        TranscriptEntry::User(message) => {
            for line in message.lines() {
                lines.push(Line::styled(
                    fit_background_line(line, width),
                    Style::default().bg(USER_BG),
                ));
            }
        }
        TranscriptEntry::Assistant(message) => {
            lines.extend(message.lines().map(|line| Line::raw(line.to_owned())));
        }
        TranscriptEntry::Tool { name, outcome, .. } => {
            let (marker, background, foreground) = match outcome {
                None => ("●", TOOL_PENDING_BG, Color::Yellow),
                Some(ToolOutcome::Completed) => ("✓", TOOL_SUCCESS_BG, Color::Green),
                Some(ToolOutcome::Denied) => ("×", TOOL_DENIED_BG, Color::Red),
                Some(ToolOutcome::Cancelled) => ("×", TOOL_PENDING_BG, Color::Yellow),
            };
            let content = fit_background_line(&format!("{marker} {name}"), width);
            lines.push(Line::styled(
                content,
                Style::default()
                    .fg(foreground)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        TranscriptEntry::Error(message) => {
            lines.push(Line::styled(
                fit_background_line(&format!("Error: {message}"), width),
                Style::default().fg(Color::Red).bg(TOOL_DENIED_BG),
            ));
        }
    }
}

fn render_working_indicator(frame: &mut Frame<'_>, area: Rect, status: Status) {
    let text = match status {
        Status::Working => "Thinking...",
        Status::AwaitingApproval => "Waiting for approval...",
        Status::Ready | Status::Failed => "",
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
        area,
    );
}

fn render_editor(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let border = if app.status == Status::Working {
        ACCENT
    } else {
        MUTED
    };
    frame.render_widget(
        Paragraph::new(app.composer.as_str()).block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(border))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );

    let cursor_offset = UnicodeWidthStr::width(app.composer.as_str()) as u16;
    let cursor_x = area
        .x
        .saturating_add(1)
        .saturating_add(cursor_offset)
        .min(area.right().saturating_sub(2));
    frame.set_cursor_position((cursor_x, area.y.saturating_add(1)));
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
        Paragraph::new(format!("{body}\n\nenter allow once · d/esc deny"))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(format!(" {} approval ", approval.request.tool_name))
                    .padding(Padding::horizontal(1)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, model: &str, workspace: &str) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(shorten_home(workspace)).style(Style::default().fg(MUTED)),
        rows[0],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    frame.render_widget(
        Paragraph::new(app.status.label()).style(Style::default().fg(MUTED)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(format!("(openai) {model}"))
            .alignment(Alignment::Right)
            .style(Style::default().fg(MUTED)),
        columns[1],
    );
}

fn fit_background_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(2);
    let mut fitted = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        fitted.push(character);
        used += character_width;
    }
    format!(
        " {fitted}{} ",
        " ".repeat(content_width.saturating_sub(used))
    )
}

fn shorten_home(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy();
    path.strip_prefix(home.as_ref())
        .map_or_else(|| path.to_owned(), |suffix| format!("~{suffix}"))
}
