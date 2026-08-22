use crate::{
    agent::{AgentEvent, ToolOutcome},
    approval::{Decision, PendingApproval},
};

pub struct App {
    pub(crate) transcript: Vec<TranscriptEntry>,
    pub(crate) assistant_draft: String,
    pub(crate) composer: String,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) status: Status,
    pub(crate) scroll: u16,
}

impl App {
    pub fn new() -> Self {
        Self {
            transcript: Vec::new(),
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
        self.transcript.push(TranscriptEntry::User(prompt.clone()));
        self.composer.clear();
        self.status = Status::Working;
        Some(prompt)
    }

    pub fn reduce(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantDelta(delta) => self.assistant_draft.push_str(&delta),
            AgentEvent::ToolStarted { id, name } => {
                self.commit_assistant_draft();
                self.transcript.push(TranscriptEntry::Tool {
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
                    *entry = TranscriptEntry::Tool {
                        id,
                        name,
                        outcome: Some(outcome),
                    };
                }
            }
            AgentEvent::TurnFinished { .. } => {
                self.commit_assistant_draft();
                self.status = Status::Ready;
            }
            AgentEvent::TurnFailed(message) => {
                self.commit_assistant_draft();
                self.transcript.push(TranscriptEntry::Error(message));
                self.status = Status::Failed;
            }
        }
    }

    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    pub fn push_input(&mut self, character: char) {
        self.composer.push(character);
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

    fn commit_assistant_draft(&mut self) {
        if !self.assistant_draft.is_empty() {
            self.transcript
                .push(TranscriptEntry::Assistant(std::mem::take(
                    &mut self.assistant_draft,
                )));
        }
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
