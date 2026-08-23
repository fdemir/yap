use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use crate::{
    agent::ToolOutcome,
    app::{App, Status, TranscriptEntry},
    markdown::{render_markdown, render_tool_output},
    security::terminal_safe_text,
};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const USER_BG: Color = Color::Indexed(236);
const MAX_EDITOR_CONTENT_LINES: usize = 6;

pub fn render(frame: &mut Frame<'_>, app: &mut App, model: &str, workspace: &str) {
    let editor_height = if app.pending_approval.is_some() {
        8
    } else {
        app.composer_line_count().clamp(1, MAX_EDITOR_CONTENT_LINES) as u16 + 2
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(editor_height),
            Constraint::Length(2),
        ])
        .split(frame.area());

    render_transcript(frame, areas[0], app);
    if app.pending_approval.is_some() {
        render_approval(frame, areas[1], app);
    } else {
        render_editor(frame, areas[1], app);
    }
    render_footer(frame, areas[2], app, model, workspace);
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let mut lines = Vec::new();
    for entry in &app.transcript {
        push_message_gap(&mut lines);
        push_entry_lines(&mut lines, entry, area.width as usize);
    }
    if !app.assistant_draft.is_empty() {
        push_message_gap(&mut lines);
        lines.extend(render_markdown(&app.assistant_draft));
    }
    if let Some(status) = active_status(app.status) {
        if app.assistant_draft.is_empty() {
            push_message_gap(&mut lines);
        }
        lines.push(Line::styled(
            format!("⠋ {status}"),
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
    }

    let rows = wrap_transcript_lines(lines, area.width);
    let visible_height = usize::from(area.height);
    let top = app.update_transcript_viewport(rows.len(), visible_height);
    let visible_rows = rows
        .into_iter()
        .skip(top)
        .take(visible_height)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(visible_rows)), area);
}

fn wrap_transcript_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width);
    if width == 0 {
        return Vec::new();
    }

    let mut rows = Vec::new();
    for line in &lines {
        let mut spans = Vec::new();
        let mut row_width = 0usize;
        let mut has_graphemes = false;
        for grapheme in line.styled_graphemes(Style::default()) {
            has_graphemes = true;
            let grapheme_width = UnicodeWidthStr::width(grapheme.symbol);
            if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
                rows.push(Line::from(std::mem::take(&mut spans)));
                row_width = 0;
            }
            push_styled_grapheme(&mut spans, grapheme.symbol, grapheme.style);
            row_width = row_width.saturating_add(grapheme_width);
        }
        if has_graphemes {
            rows.push(Line::from(spans));
        } else {
            rows.push(Line::raw(""));
        }
    }
    rows
}

fn push_styled_grapheme(spans: &mut Vec<Span<'static>>, symbol: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(symbol);
    } else {
        spans.push(Span::styled(symbol.to_owned(), style));
    }
}

fn push_message_gap(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
}

fn active_status(status: Status) -> Option<&'static str> {
    match status {
        Status::Working => Some("Thinking..."),
        Status::AwaitingApproval => Some("Waiting for approval..."),
        Status::Ready | Status::Failed => None,
    }
}

fn push_entry_lines(lines: &mut Vec<Line<'static>>, entry: &TranscriptEntry, width: usize) {
    match entry {
        TranscriptEntry::User(message) => {
            let style = Style::default().bg(USER_BG);
            lines.push(Line::styled(fit_background_line("", width), style));
            let content_width = width.saturating_sub(2).min(usize::from(u16::MAX)) as u16;
            for line in message.lines() {
                let safe_line = terminal_safe_text(line).into_owned();
                for row in wrap_transcript_lines(vec![Line::raw(safe_line)], content_width) {
                    let text = row
                        .spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>();
                    lines.push(Line::styled(fit_background_line(&text, width), style));
                }
            }
            lines.push(Line::styled(fit_background_line("", width), style));
        }
        TranscriptEntry::Assistant(message) => lines.extend(render_markdown(message)),
        TranscriptEntry::Tool {
            name,
            outcome,
            output,
            ..
        } => {
            let (marker, foreground) = match outcome {
                None => ("●", Color::Yellow),
                Some(ToolOutcome::Completed) => ("✓", Color::Green),
                Some(ToolOutcome::Denied | ToolOutcome::Failed) => ("×", Color::Red),
                Some(ToolOutcome::Cancelled) => ("×", Color::Yellow),
            };
            lines.push(Line::styled(
                format!("{marker} {}", terminal_safe_text(name)),
                Style::default().fg(foreground).add_modifier(Modifier::BOLD),
            ));
            if let Some(output) = output
                && !output.is_empty()
            {
                lines.extend(render_tool_output(
                    output,
                    *outcome == Some(ToolOutcome::Failed),
                ));
            }
        }
        TranscriptEntry::Error(message) => {
            lines.push(Line::styled(
                format!("Error: {}", terminal_safe_text(message)),
                Style::default().fg(Color::Red),
            ));
        }
    }
}

fn render_editor(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let composer = terminal_safe_text(app.composer_text());
    let cursor_prefix = terminal_safe_text(&app.composer_text()[..app.composer_cursor()]);
    let viewport = editor_viewport(
        &cursor_prefix,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let border = if app.status == Status::Working {
        ACCENT
    } else {
        MUTED
    };
    frame.render_widget(
        Paragraph::new(composer.as_ref())
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_style(Style::default().fg(border))
                    .padding(Padding::horizontal(1)),
            )
            .scroll((viewport.vertical_scroll, viewport.horizontal_scroll)),
        area,
    );

    let cursor_x = area
        .x
        .saturating_add(1)
        .saturating_add(viewport.cursor_x)
        .min(area.right().saturating_sub(1));
    let cursor_y = area
        .y
        .saturating_add(1)
        .saturating_add(viewport.cursor_y)
        .min(area.bottom().saturating_sub(2));
    frame.set_cursor_position((cursor_x, cursor_y));
}

struct EditorViewport {
    vertical_scroll: u16,
    horizontal_scroll: u16,
    cursor_x: u16,
    cursor_y: u16,
}

fn editor_viewport(cursor_prefix: &str, width: u16, height: u16) -> EditorViewport {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    let cursor_line = cursor_prefix.bytes().filter(|byte| *byte == b'\n').count();
    let cursor_column = UnicodeWidthStr::width(
        cursor_prefix
            .rsplit_once('\n')
            .map_or(cursor_prefix, |(_, line)| line),
    );
    let vertical_scroll = cursor_line.saturating_sub(height.saturating_sub(1));
    let horizontal_scroll = cursor_column.saturating_sub(width.saturating_sub(1));

    EditorViewport {
        vertical_scroll: vertical_scroll.min(usize::from(u16::MAX)) as u16,
        horizontal_scroll: horizontal_scroll.min(usize::from(u16::MAX)) as u16,
        cursor_x: cursor_column
            .saturating_sub(horizontal_scroll)
            .min(usize::from(u16::MAX)) as u16,
        cursor_y: cursor_line
            .saturating_sub(vertical_scroll)
            .min(usize::from(u16::MAX)) as u16,
    }
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
    let body = terminal_safe_text(&body);
    frame.render_widget(
        Paragraph::new(format!("{body}\n\nenter allow once · d/esc deny"))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(format!(
                        " {} approval ",
                        terminal_safe_text(&approval.request.tool_name)
                    ))
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
        Paragraph::new(terminal_safe_text(&shorten_home(workspace)).into_owned())
            .style(Style::default().fg(MUTED)),
        rows[0],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    let status = app.transcript_scroll_percentage().map_or_else(
        || app.status.label().to_owned(),
        |percentage| format!("{} · scroll {percentage}%", app.status.label()),
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(MUTED)),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(terminal_safe_text(model).into_owned())
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
    let used = UnicodeWidthStr::width(text);
    format!(" {text}{} ", " ".repeat(content_width.saturating_sub(used)))
}

fn shorten_home(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy();
    path.strip_prefix(home.as_ref())
        .map_or_else(|| path.to_owned(), |suffix| format!("~{suffix}"))
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn tool_entries_include_a_bounded_output_preview() {
        let output = (0..30)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut lines = Vec::new();

        push_entry_lines(
            &mut lines,
            &TranscriptEntry::Tool {
                id: "call_1".into(),
                name: "run_command".into(),
                outcome: Some(ToolOutcome::Completed),
                output: Some(output),
            },
            80,
        );

        assert_eq!(lines.len(), 15);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("lines omitted"))
        }));
    }

    #[test]
    fn transcript_lines_are_wrapped_into_visual_rows() {
        let style = Style::default().fg(Color::Green);
        let rows = wrap_transcript_lines(vec![Line::styled("abcdef", style)], 3);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content, "abc");
        assert_eq!(rows[1].spans[0].content, "def");
        assert_eq!(rows[0].spans[0].style, style);
    }

    #[test]
    fn long_user_lines_are_wrapped_without_truncation() {
        let mut lines = Vec::new();
        push_entry_lines(
            &mut lines,
            &TranscriptEntry::User("abcdefghij".to_owned()),
            6,
        );
        let rows = wrap_transcript_lines(lines, 6);
        let content = rows
            .iter()
            .skip(1)
            .take(3)
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.trim())
            .collect::<String>();

        assert_eq!(content, "abcdefghij");
    }

    #[test]
    fn transcript_wrapping_preserves_blank_lines() {
        let rows = wrap_transcript_lines(vec![Line::raw("")], 20);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].width(), 0);
    }

    #[test]
    fn multiline_editor_grows_and_places_the_cursor_on_its_line() {
        let mut app = App::new();
        app.insert_text("first\nsecond");
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();

        terminal
            .draw(|frame| render(frame, &mut app, "model", "/workspace"))
            .unwrap();

        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (7, 8));
    }

    #[test]
    fn editor_viewport_keeps_a_multiline_cursor_visible() {
        let viewport = editor_viewport("one\ntwo\nthree\nfour", 20, 2);

        assert_eq!(viewport.vertical_scroll, 2);
        assert_eq!(viewport.cursor_y, 1);
        assert_eq!(viewport.horizontal_scroll, 0);
        assert_eq!(viewport.cursor_x, 4);
    }

    #[test]
    fn editor_viewport_scrolls_long_lines_horizontally() {
        let viewport = editor_viewport("0123456789", 5, 1);

        assert_eq!(viewport.horizontal_scroll, 6);
        assert_eq!(viewport.cursor_x, 4);
    }
}
