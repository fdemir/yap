use crate::{
    agent::{AgentEvent, ToolOutcome},
    approval::{Decision, PendingApproval},
};

pub struct App {
    pub(crate) transcript: Vec<String>,
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
        self.transcript.push(format!("You\n{prompt}"));
        self.composer.clear();
        self.status = Status::Working;
        Some(prompt)
    }

    pub fn reduce(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantDelta(delta) => self.assistant_draft.push_str(&delta),
            AgentEvent::ToolStarted { name, .. } => {
                self.commit_assistant_draft();
                self.transcript.push(format!("● {name}"));
            }
            AgentEvent::ToolFinished { name, outcome, .. } => {
                let marker = match outcome {
                    ToolOutcome::Completed => "✓",
                    ToolOutcome::Denied => "×",
                };
                self.transcript.push(format!("{marker} {name}"));
            }
            AgentEvent::TurnFinished { .. } => {
                self.commit_assistant_draft();
                self.status = Status::Ready;
            }
            AgentEvent::TurnFailed(message) => {
                self.commit_assistant_draft();
                self.transcript.push(format!("Error\n{message}"));
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
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
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

    pub(crate) fn transcript_text(&self) -> String {
        let mut sections = self.transcript.clone();
        if !self.assistant_draft.is_empty() {
            sections.push(format!("Assistant\n{}", self.assistant_draft));
        }
        sections.join("\n\n")
    }

    fn commit_assistant_draft(&mut self) {
        if !self.assistant_draft.is_empty() {
            self.transcript
                .push(format!("Assistant\n{}", self.assistant_draft));
            self.assistant_draft.clear();
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
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
