use std::{borrow::Cow, collections::VecDeque};

use crate::{
    agent::{AgentEvent, ToolOutcome},
    approval::{Decision, PendingApproval},
    security::{bounded_redacted, checked_append, truncate_text},
};

const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_ASSISTANT_DRAFT_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 1_000;

pub struct App {
    pub(crate) transcript: VecDeque<TranscriptEntry>,
    transcript_bytes: usize,
    pub(crate) assistant_draft: String,
    pub(crate) composer: String,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) status: Status,
    pub(crate) scroll: u16,
}

impl App {
    pub fn new() -> Self {
        Self {
            transcript: VecDeque::new(),
            transcript_bytes: 0,
            assistant_draft: String::new(),
            composer: String::new(),
            pending_approval: None,
            status: Status::Ready,
            scroll: 0,
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let prompt = self.composer.trim().to_owned();
        if prompt.is_empty() {
            return None;
        }
        self.push_transcript(TranscriptEntry::User(prompt.clone()));
        self.composer.clear();
        self.status = Status::Working;
        Some(prompt)
    }

    pub fn reduce(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantDelta(delta) => {
                if !checked_append(
                    &mut self.assistant_draft,
                    &delta,
                    MAX_ASSISTANT_DRAFT_BYTES,
                ) {
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
                });
            }
            AgentEvent::ToolFinished {
                id,
                name,
                outcome,
            } => {
                if let Some(entry) = self.transcript.iter_mut().rev().find(|entry| {
                    matches!(entry, TranscriptEntry::Tool { id: entry_id, .. } if entry_id == &id)
                }) {
                    let old_bytes = entry.retained_bytes();
                    *entry = TranscriptEntry::Tool {
                        id,
                        name,
                        outcome: Some(outcome),
                    };
                    self.transcript_bytes = self
                        .transcript_bytes
                        .saturating_sub(old_bytes)
                        .saturating_add(entry.retained_bytes());
                }
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
        if self.composer.len().saturating_add(character.len_utf8()) <= MAX_COMPOSER_BYTES {
            self.composer.push(character);
        }
    }

    pub fn pop_input(&mut self) {
        self.composer.pop();
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
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
        let entry_bytes = entry.retained_bytes();
        while self.transcript.len() >= MAX_TRANSCRIPT_ENTRIES
            || self.transcript_bytes.saturating_add(entry_bytes) > MAX_TRANSCRIPT_BYTES
        {
            let Some(removed) = self.transcript.pop_front() else {
                break;
            };
            self.transcript_bytes = self
                .transcript_bytes
                .saturating_sub(removed.retained_bytes());
        }
        self.transcript_bytes = self.transcript_bytes.saturating_add(entry_bytes);
        self.transcript.push_back(entry);
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
    },
    Error(String),
}

impl TranscriptEntry {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::User(message) | Self::Assistant(message) | Self::Error(message) => message.len(),
            Self::Tool { id, name, .. } => id.len().saturating_add(name.len()),
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
    fn assistant_draft_is_bounded() {
        let mut app = App::new();
        app.reduce(AgentEvent::AssistantDelta(
            "x".repeat(MAX_ASSISTANT_DRAFT_BYTES + 100),
        ));

        assert!(app.assistant_draft.len() <= MAX_ASSISTANT_DRAFT_BYTES);
        assert!(app.assistant_draft.contains("assistant draft truncated"));
    }
}
