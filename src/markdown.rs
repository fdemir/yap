use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::security::terminal_safe_text;

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const CODE_BG: Color = Color::Indexed(235);
const CODE_FG: Color = Color::Indexed(252);
const MAX_TOOL_PREVIEW_LINES: usize = 14;

pub(crate) fn render_markdown(markdown: &str) -> Vec<Line<'static>> {
    let safe = terminal_safe_text(markdown);
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut renderer = MarkdownRenderer::default();
    for event in Parser::new_ext(safe.as_ref(), options) {
        renderer.handle(event);
    }
    renderer.finish()
}

pub(crate) fn render_tool_output(output: &str, failed: bool) -> Vec<Line<'static>> {
    let safe = terminal_safe_text(output);
    let lines = safe.lines().collect::<Vec<_>>();
    let selected = preview_line_indices(lines.len());
    if looks_like_diff(&lines) {
        let preview = selected
            .into_iter()
            .map(|item| match item {
                PreviewLine::Line(index) => lines[index].to_owned(),
                PreviewLine::Omitted(count) => format!("… {count} lines omitted"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return render_code_block("diff", &preview);
    }

    let mut rendered = Vec::new();
    for item in selected {
        match item {
            PreviewLine::Line(index) => {
                let content = lines[index];
                let style = if failed {
                    Style::default().fg(Color::Red)
                } else if matches!(content, "stdout:" | "stderr:")
                    || content.starts_with("exit code:")
                {
                    Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                };
                rendered.push(Line::from(vec![
                    Span::styled("  │ ", Style::default().fg(MUTED)),
                    Span::styled(content.to_owned(), style),
                ]));
            }
            PreviewLine::Omitted(count) => rendered.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("… {count} lines omitted"),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                ),
            ])),
        }
    }
    rendered
}

#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    item_depth: usize,
    quote_depth: usize,
    code: Option<CodeBlock>,
    links: Vec<String>,
    table_cell: usize,
}

struct CodeBlock {
    language: String,
    contents: String,
}

enum ListState {
    Bullet,
    Ordered(u64),
}

impl MarkdownRenderer {
    fn handle(&mut self, event: Event<'_>) {
        if self.code.is_some() {
            match event {
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    if let Some(code) = &mut self.code {
                        code.contents.push_str(&text);
                    }
                }
                Event::SoftBreak | Event::HardBreak => {
                    if let Some(code) = &mut self.code {
                        code.contents.push('\n');
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().expect("code block is active");
                    self.lines
                        .extend(render_code_block(&code.language, &code.contents));
                }
                _ => {}
            }
            return;
        }

        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.append_text(&text, self.current_style()),
            Event::Code(code) => self.append_text(
                &format!(" {code} "),
                Style::default().fg(ACCENT).bg(CODE_BG),
            ),
            Event::InlineMath(math) => self.append_text(
                &format!("${math}$"),
                self.current_style().add_modifier(Modifier::ITALIC),
            ),
            Event::DisplayMath(math) => {
                self.ensure_gap();
                self.append_text(&format!("$${math}$$"), Style::default().fg(Color::Magenta));
                self.flush_line();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.append_text(&html, Style::default().fg(MUTED));
            }
            Event::FootnoteReference(label) => self.append_text(
                &format!("[{label}]"),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
            Event::SoftBreak => self.append_text(" ", self.current_style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.ensure_gap();
                self.lines.push(Line::styled(
                    "────────────────────────",
                    Style::default().fg(MUTED),
                ));
            }
            Event::TaskListMarker(done) => self.append_text(
                if done { "☑ " } else { "☐ " },
                Style::default().fg(if done { Color::Green } else { MUTED }),
            ),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.item_depth == 0 {
                    self.ensure_gap();
                }
                self.start_line();
            }
            Tag::Heading { level, .. } => {
                self.ensure_gap();
                let level = level as u8;
                self.append_text(
                    &format!("{} ", "#".repeat(usize::from(level))),
                    Style::default().fg(MUTED),
                );
                let style = match level {
                    1 | 2 => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    3 => Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                    _ => Style::default().fg(ACCENT),
                };
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.ensure_gap();
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.styles.push(Style::default().fg(MUTED));
            }
            Tag::CodeBlock(kind) => {
                self.ensure_gap();
                let language = match kind {
                    CodeBlockKind::Indented => String::new(),
                    CodeBlockKind::Fenced(language) => language.into_string(),
                };
                self.code = Some(CodeBlock {
                    language,
                    contents: String::new(),
                });
            }
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.ensure_gap();
                } else {
                    self.flush_line();
                }
                self.lists
                    .push(first.map_or(ListState::Bullet, ListState::Ordered));
            }
            Tag::Item => {
                self.flush_line();
                self.item_depth = self.item_depth.saturating_add(1);
                self.start_line();
                let marker = match self.lists.last_mut() {
                    Some(ListState::Ordered(next)) => {
                        let marker = format!("{next}. ");
                        *next = next.saturating_add(1);
                        marker
                    }
                    Some(ListState::Bullet) | None => "• ".into(),
                };
                self.append_text(&marker, Style::default().fg(ACCENT));
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                self.styles.push(
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => {
                self.append_text("[image: ", Style::default().fg(MUTED));
                self.styles
                    .push(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Table(_) => self.ensure_gap(),
            Tag::TableHead => self
                .styles
                .push(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Tag::TableRow => {
                self.flush_line();
                self.table_cell = 0;
            }
            Tag::TableCell => {
                if self.table_cell > 0 {
                    self.append_text(" │ ", Style::default().fg(MUTED));
                }
                self.table_cell = self.table_cell.saturating_add(1);
            }
            Tag::FootnoteDefinition(label) => {
                self.ensure_gap();
                self.append_text(&format!("[{label}]: "), Style::default().fg(MUTED));
            }
            Tag::DefinitionList | Tag::DefinitionListDefinition | Tag::DefinitionListTitle => {}
            Tag::Superscript | Tag::Subscript | Tag::MetadataBlock(_) | Tag::HtmlBlock => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::TableRow => self.flush_line(),
            TagEnd::Heading(_) => {
                self.flush_line();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.styles.pop();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_line();
                self.item_depth = self.item_depth.saturating_sub(1);
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::TableHead => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                self.links.pop();
            }
            TagEnd::Image => {
                self.styles.pop();
                self.append_text("]", Style::default().fg(MUTED));
            }
            TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableCell
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListDefinition
            | TagEnd::DefinitionListTitle
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_)
            | TagEnd::HtmlBlock => {}
            TagEnd::CodeBlock => unreachable!("active code blocks are handled before block events"),
        }
    }

    fn append_text(&mut self, text: &str, style: Style) {
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.flush_line();
            }
            if !part.is_empty() {
                self.start_line();
                if let Some(last) = self.current.last_mut()
                    && last.style == style
                {
                    last.content.to_mut().push_str(part);
                } else {
                    self.current.push(Span::styled(part.to_owned(), style));
                }
            }
        }
    }

    fn start_line(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.quote_depth {
            self.current
                .push(Span::styled("│ ", Style::default().fg(MUTED)));
        }
        if self.item_depth > 0 {
            let indent = self.lists.len().saturating_sub(1).saturating_mul(2);
            if indent > 0 {
                self.current.push(Span::raw(" ".repeat(indent)));
            }
        }
    }

    fn current_style(&self) -> Style {
        self.styles
            .iter()
            .copied()
            .fold(Style::default(), Style::patch)
    }

    fn flush_line(&mut self) {
        if !self.current.is_empty() {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
    }

    fn ensure_gap(&mut self) {
        self.flush_line();
        if self.lines.last().is_some_and(|line| line.width() > 0) {
            self.lines.push(Line::raw(""));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        self.lines
    }
}

fn render_code_block(language: &str, contents: &str) -> Vec<Line<'static>> {
    let language = language.trim();
    let mut lines = Vec::new();
    let title = if language.is_empty() {
        "┌─".to_owned()
    } else {
        format!("┌─ {language}")
    };
    lines.push(Line::styled(title, Style::default().fg(MUTED)));

    let contents = contents.strip_suffix('\n').unwrap_or(contents);
    let source_lines = if contents.is_empty() {
        vec![""]
    } else {
        contents.split('\n').collect::<Vec<_>>()
    };
    let diff = is_diff_language(language) || looks_like_diff(&source_lines);
    for source in source_lines {
        let style = if diff {
            diff_style(source)
        } else {
            Style::default().fg(CODE_FG).bg(CODE_BG)
        };
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(MUTED)),
            Span::styled(source.to_owned(), style),
        ]));
    }
    lines.push(Line::styled("└─", Style::default().fg(MUTED)));
    lines
}

fn is_diff_language(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "diff" | "patch" | "udiff"
    )
}

fn looks_like_diff(lines: &[&str]) -> bool {
    let has_hunk = lines.iter().any(|line| line.starts_with("@@"));
    let has_old = lines
        .iter()
        .any(|line| line.starts_with("--- ") || line.starts_with("diff --git "));
    let has_new = lines.iter().any(|line| line.starts_with("+++ "));
    has_hunk || has_old && has_new
}

fn diff_style(line: &str) -> Style {
    if line.starts_with("diff --git ") || line.starts_with("index ") {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(ACCENT)
    } else if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(CODE_FG)
    }
}

enum PreviewLine {
    Line(usize),
    Omitted(usize),
}

fn preview_line_indices(total: usize) -> Vec<PreviewLine> {
    if total <= MAX_TOOL_PREVIEW_LINES {
        return (0..total).map(PreviewLine::Line).collect();
    }
    let head = MAX_TOOL_PREVIEW_LINES.saturating_sub(5);
    let tail = 4;
    let mut selected = (0..head).map(PreviewLine::Line).collect::<Vec<_>>();
    selected.push(PreviewLine::Omitted(total.saturating_sub(head + tail)));
    selected.extend((total - tail..total).map(PreviewLine::Line));
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn markdown_renders_headings_lists_and_inline_styles() {
        let lines = render_markdown("## Plan\n\n- use **Rust** and `ratatui`");

        assert_eq!(content(&lines[0]), "## Plan");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.fg == Some(ACCENT))
        );
        assert_eq!(content(lines.last().unwrap()), "• use Rust and  ratatui ");
        assert!(
            lines
                .last()
                .unwrap()
                .spans
                .iter()
                .any(|span| { span.style.add_modifier.contains(Modifier::BOLD) })
        );
        assert!(
            lines
                .last()
                .unwrap()
                .spans
                .iter()
                .any(|span| { span.content.contains("use") && span.style.fg.is_none() })
        );
    }

    #[test]
    fn fenced_diff_blocks_receive_line_styles() {
        let lines = render_markdown("```diff\n-old\n+new\n```");

        assert_eq!(content(&lines[0]), "┌─ diff");
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Green));
        assert_eq!(content(lines.last().unwrap()), "└─");
    }

    #[test]
    fn long_tool_output_keeps_its_head_and_tail() {
        let output = (0..30)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = render_tool_output(&output, false);

        assert_eq!(lines.len(), MAX_TOOL_PREVIEW_LINES);
        assert!(
            lines
                .iter()
                .any(|line| content(line).contains("17 lines omitted"))
        );
        assert!(content(lines.last().unwrap()).contains("line 29"));
    }

    #[test]
    fn long_diff_tool_output_is_also_bounded() {
        let mut source = vec![
            "--- old".to_owned(),
            "+++ new".to_owned(),
            "@@ -1 +1 @@".to_owned(),
        ];
        source.extend((0..30).map(|index| format!("+line {index}")));

        let lines = render_tool_output(&source.join("\n"), false);

        assert_eq!(lines.len(), MAX_TOOL_PREVIEW_LINES + 2);
        assert!(
            lines
                .iter()
                .any(|line| content(line).contains("lines omitted"))
        );
    }

    #[test]
    fn markdown_escapes_terminal_control_sequences() {
        let lines = render_markdown("hello\u{1b}[31m");

        assert_eq!(content(&lines[0]), "hello\\x1b[31m");
    }
}
