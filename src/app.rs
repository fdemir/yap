use std::{borrow::Cow, collections::VecDeque};

use crate::{
    agent::{AgentEvent, ToolOutcome},
    approval::{Decision, PendingApproval},
    composer::Composer,
    security::{bounded_redacted, checked_append, truncate_text},
};

const MAX_ASSISTANT_DRAFT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 1_000;

pub struct App {
    pub(crate) transcript: VecDeque<TranscriptEntry>,
    transcript_bytes: usize,
    pub(crate) assistant_draft: String,
    composer: Composer,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) status: Status,
    transcript_navigation: TranscriptNavigation,
}

impl App {
    pub fn new() -> Self {
        Self {
            transcript: VecDeque::new(),
            transcript_bytes: 0,
            assistant_draft: String::new(),
            composer: Composer::new(),
            pending_approval: None,
            status: Status::Ready,
            transcript_navigation: TranscriptNavigation::new(),
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let prompt = self.composer.submit()?;
        self.push_transcript(TranscriptEntry::User(prompt.clone()));
        self.status = Status::Working;
        self.transcript_navigation.follow_tail();
        Some(prompt)
    }

    pub fn reduce(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantDelta(delta) => {
                if !checked_append(&mut self.assistant_draft, &delta, MAX_ASSISTANT_DRAFT_BYTES) {
                    let combined = format!("{}{delta}", self.assistant_draft);
                    self.assistant_draft = truncate_text(
                        Cow::Owned(combined),
                        MAX_ASSISTANT_DRAFT_BYTES,
                        "assistant draft",
                    );
                }
            }
            AgentEvent::ToolStarted { id, name } => {
                self.commit_assistant_draft();
                self.push_transcript(TranscriptEntry::Tool {
                    id,
                    name,
                    outcome: None,
                    output: None,
                });
            }
            AgentEvent::ToolFinished {
                id,
                name,
                outcome,
                output,
            } => {
                if let Some(entry) = self.transcript.iter_mut().rev().find(|entry| {
                    matches!(entry, TranscriptEntry::Tool { id: entry_id, .. } if entry_id == &id)
                }) {
                    let old_bytes = entry.retained_bytes();
                    *entry = TranscriptEntry::Tool {
                        id,
                        name,
                        outcome: Some(outcome),
                        output: Some(output),
                    };
                    self.transcript_bytes = self
                        .transcript_bytes
                        .saturating_sub(old_bytes)
                        .saturating_add(entry.retained_bytes());
                }
                self.trim_transcript();
            }
            AgentEvent::TurnFinished { .. } => {
                self.commit_assistant_draft();
                self.status = Status::Ready;
            }
            AgentEvent::TurnCancelled => {
                self.cancel_pending_approval();
                self.commit_assistant_draft();
                for entry in &mut self.transcript {
                    if let TranscriptEntry::Tool { outcome, .. } = entry
                        && outcome.is_none()
                    {
                        *outcome = Some(ToolOutcome::Cancelled);
                    }
                }
                self.status = Status::Ready;
            }
            AgentEvent::TurnFailed(message) => {
                self.commit_assistant_draft();
                self.push_transcript(TranscriptEntry::Error(bounded_redacted(
                    &message,
                    MAX_ERROR_BYTES,
                    "error",
                )));
                self.status = Status::Failed;
            }
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    pub fn push_input(&mut self, character: char) {
        self.composer.insert_character(character);
    }

    pub fn insert_text(&mut self, text: &str) {
        self.composer.insert_text(text);
    }

    pub fn insert_newline(&mut self) {
        self.composer.insert_character('\n');
    }

    pub fn pop_input(&mut self) {
        self.composer.backspace();
    }

    pub fn delete_input(&mut self) {
        self.composer.delete();
    }

    pub fn move_cursor_left(&mut self) {
        self.composer.move_left();
    }

    pub fn move_cursor_right(&mut self) {
        self.composer.move_right();
    }

    pub fn move_cursor_up(&mut self) {
        self.composer.move_vertical(-1);
    }

    pub fn move_cursor_down(&mut self) {
        self.composer.move_vertical(1);
    }

    pub fn move_cursor_home(&mut self) {
        self.composer.move_home();
    }

    pub fn move_cursor_end(&mut self) {
        self.composer.move_end();
    }

    pub fn previous_prompt(&mut self) {
        self.composer.previous_history();
    }

    pub fn next_prompt(&mut self) {
        self.composer.next_history();
    }

    pub(crate) fn composer_text(&self) -> &str {
        self.composer.text()
    }

    pub(crate) fn composer_cursor(&self) -> usize {
        self.composer.cursor()
    }

    pub(crate) fn composer_line_count(&self) -> usize {
        self.composer.line_count()
    }

    pub fn scroll_page_up(&mut self) {
        self.transcript_navigation.page_up();
    }

    pub fn scroll_page_down(&mut self) {
        self.transcript_navigation.page_down();
    }

    pub fn scroll_to_top(&mut self) {
        self.transcript_navigation.jump_to_top();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.transcript_navigation.follow_tail();
    }

    pub(crate) fn update_transcript_viewport(
        &mut self,
        total_rows: usize,
        visible_rows: usize,
    ) -> usize {
        self.transcript_navigation.update(total_rows, visible_rows)
    }

    pub(crate) fn transcript_scroll_percentage(&self) -> Option<usize> {
        self.transcript_navigation.percentage()
    }

    pub fn receive_approval(&mut self, approval: PendingApproval) {
        self.pending_approval = Some(approval);
        self.status = Status::AwaitingApproval;
    }

    pub fn decide(&mut self, decision: Decision) {
        if let Some(approval) = self.pending_approval.take() {
            let _ = approval.respond_to.send(decision);
            self.status = Status::Working;
        }
    }

    pub fn cancel_active_turn(&mut self) {
        self.cancel_pending_approval();
        self.status = Status::Working;
    }

    fn cancel_pending_approval(&mut self) {
        if let Some(approval) = self.pending_approval.take() {
            let _ = approval.respond_to.send(Decision::Deny);
        }
    }

    fn commit_assistant_draft(&mut self) {
        if !self.assistant_draft.is_empty() {
            let message = std::mem::take(&mut self.assistant_draft);
            self.push_transcript(TranscriptEntry::Assistant(message));
        }
    }

    fn push_transcript(&mut self, entry: TranscriptEntry) {
        self.transcript_bytes = self.transcript_bytes.saturating_add(entry.retained_bytes());
        self.transcript.push_back(entry);
        self.trim_transcript();
    }

    fn trim_transcript(&mut self) {
        while self.transcript.len() > MAX_TRANSCRIPT_ENTRIES
            || self.transcript_bytes > MAX_TRANSCRIPT_BYTES
        {
            let Some(removed) = self.transcript.pop_front() else {
                break;
            };
            self.transcript_bytes = self
                .transcript_bytes
                .saturating_sub(removed.retained_bytes());
        }
    }
}

struct TranscriptNavigation {
    mode: TranscriptNavigationMode,
    visible_top: usize,
    max_top: usize,
    page_rows: usize,
}

enum TranscriptNavigationMode {
    FollowTail,
    Manual { top: usize },
}

impl TranscriptNavigation {
    fn new() -> Self {
        Self {
            mode: TranscriptNavigationMode::FollowTail,
            visible_top: 0,
            max_top: 0,
            page_rows: 10,
        }
    }

    fn update(&mut self, total_rows: usize, visible_rows: usize) -> usize {
        self.max_top = total_rows.saturating_sub(visible_rows);
        self.page_rows = visible_rows.saturating_sub(1).max(1);
        if self.max_top == 0 {
            self.follow_tail();
            return 0;
        }
        self.visible_top = match &mut self.mode {
            TranscriptNavigationMode::FollowTail => self.max_top,
            TranscriptNavigationMode::Manual { top } => {
                *top = (*top).min(self.max_top);
                *top
            }
        };
        self.visible_top
    }

    fn page_up(&mut self) {
        if self.max_top == 0 {
            return;
        }
        let top = self.visible_top.saturating_sub(self.page_rows);
        self.visible_top = top;
        self.mode = TranscriptNavigationMode::Manual { top };
    }

    fn page_down(&mut self) {
        if matches!(self.mode, TranscriptNavigationMode::FollowTail) {
            return;
        }
        let top = self.visible_top.saturating_add(self.page_rows);
        if top >= self.max_top {
            self.follow_tail();
        } else {
            self.visible_top = top;
            self.mode = TranscriptNavigationMode::Manual { top };
        }
    }

    fn jump_to_top(&mut self) {
        if self.max_top == 0 {
            self.follow_tail();
            return;
        }
        self.visible_top = 0;
        self.mode = TranscriptNavigationMode::Manual { top: 0 };
    }

    fn follow_tail(&mut self) {
        self.mode = TranscriptNavigationMode::FollowTail;
        self.visible_top = self.max_top;
    }

    fn percentage(&self) -> Option<usize> {
        if matches!(self.mode, TranscriptNavigationMode::FollowTail) || self.max_top == 0 {
            return None;
        }
        Some(self.visible_top.saturating_mul(100) / self.max_top)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) enum TranscriptEntry {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        name: String,
        outcome: Option<ToolOutcome>,
        output: Option<String>,
    },
    Error(String),
}

impl TranscriptEntry {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::User(message) | Self::Assistant(message) | Self::Error(message) => message.len(),
            Self::Tool {
                id, name, output, ..
            } => id
                .len()
                .saturating_add(name.len())
                .saturating_add(output.as_ref().map_or(0, String::len)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    Working,
    AwaitingApproval,
    Failed,
}

impl Status {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Working => "working",
            Self::AwaitingApproval => "approval required",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::composer::MAX_COMPOSER_BYTES;

    use super::*;

    #[test]
    fn composer_and_transcript_retention_are_bounded() {
        let mut app = App::new();
        for _ in 0..MAX_COMPOSER_BYTES + 100 {
            app.push_input('x');
        }
        let prompt = app.submit().expect("bounded prompt should submit");
        assert_eq!(prompt.len(), MAX_COMPOSER_BYTES);
        app.reduce(AgentEvent::TurnFinished {
            assistant_text: String::new(),
        });

        for _ in 0..MAX_TRANSCRIPT_ENTRIES + 100 {
            app.push_input('x');
            app.submit().expect("prompt should submit");
            app.reduce(AgentEvent::TurnFinished {
                assistant_text: String::new(),
            });
        }

        assert!(app.transcript.len() <= MAX_TRANSCRIPT_ENTRIES);
        assert!(app.transcript_bytes <= MAX_TRANSCRIPT_BYTES);
    }

    #[test]
    fn composer_edits_at_unicode_character_boundaries() {
        let mut app = App::new();
        app.insert_text("aéb");
        app.move_cursor_left();
        app.push_input('X');
        assert_eq!(app.composer_text(), "aéXb");

        app.pop_input();
        app.delete_input();
        assert_eq!(app.composer_text(), "aé");
        assert_eq!(app.composer_cursor(), "aé".len());
    }

    #[test]
    fn composer_moves_between_lines_with_a_stable_preferred_column() {
        let mut app = App::new();
        app.insert_text("abc\nx\n12345");

        app.move_cursor_up();
        assert_eq!(app.composer_cursor(), "abc\nx".len());
        app.move_cursor_up();
        assert_eq!(app.composer_cursor(), "abc".len());
        app.move_cursor_down();
        assert_eq!(app.composer_cursor(), "abc\nx".len());
        app.move_cursor_down();
        assert_eq!(app.composer_cursor(), "abc\nx\n12345".len());

        app.move_cursor_home();
        assert_eq!(app.composer_cursor(), "abc\nx\n".len());
        app.move_cursor_end();
        assert_eq!(app.composer_cursor(), app.composer_text().len());
    }

    #[test]
    fn composer_normalizes_multiline_paste_line_endings() {
        let mut app = App::new();
        app.insert_text("one\r\ntwo\rthree");

        assert_eq!(app.composer_text(), "one\ntwo\nthree");
        assert_eq!(app.composer_line_count(), 3);
    }

    #[test]
    fn prompt_history_restores_the_draft_after_navigation() {
        let mut app = App::new();
        for prompt in ["first", "second"] {
            app.insert_text(prompt);
            app.submit().expect("prompt should submit");
            app.reduce(AgentEvent::TurnFinished {
                assistant_text: String::new(),
            });
        }
        app.insert_text("draft");

        app.previous_prompt();
        assert_eq!(app.composer_text(), "second");
        app.previous_prompt();
        assert_eq!(app.composer_text(), "first");
        app.next_prompt();
        assert_eq!(app.composer_text(), "second");
        app.next_prompt();
        assert_eq!(app.composer_text(), "draft");
        assert_eq!(app.composer_cursor(), "draft".len());
    }

    #[test]
    fn manual_transcript_navigation_preserves_its_top_when_content_grows() {
        let mut navigation = TranscriptNavigation::new();
        assert_eq!(navigation.update(100, 20), 80);

        navigation.page_up();
        assert_eq!(navigation.update(120, 20), 61);
        assert_eq!(navigation.percentage(), Some(61));
    }

    #[test]
    fn transcript_navigation_returns_to_following_at_the_bottom() {
        let mut navigation = TranscriptNavigation::new();
        navigation.update(100, 20);
        navigation.jump_to_top();

        navigation.page_down();
        assert_eq!(navigation.update(120, 20), 19);
        for _ in 0..5 {
            navigation.page_down();
        }

        assert_eq!(navigation.update(130, 20), 110);
        assert_eq!(navigation.percentage(), None);
    }

    #[test]
    fn transcript_navigation_can_jump_between_top_and_tail() {
        let mut navigation = TranscriptNavigation::new();
        navigation.update(100, 20);

        navigation.jump_to_top();
        assert_eq!(navigation.update(100, 20), 0);
        navigation.follow_tail();
        assert_eq!(navigation.update(120, 20), 100);
    }

    #[test]
    fn assistant_draft_is_bounded() {
        let mut app = App::new();
        app.reduce(AgentEvent::AssistantDelta(
            "x".repeat(MAX_ASSISTANT_DRAFT_BYTES + 100),
        ));

        assert!(app.assistant_draft.len() <= MAX_ASSISTANT_DRAFT_BYTES);
        assert!(app.assistant_draft.contains("assistant draft truncated"));
    }
}
