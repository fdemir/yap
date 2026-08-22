use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    ReadOnly,
    WorkspaceWrite,
    Mutating,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub risk: Risk,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    async fn decide(&self, request: ApprovalRequest) -> Decision;
}

pub struct PendingApproval {
    pub request: ApprovalRequest,
    pub respond_to: oneshot::Sender<Decision>,
}

pub struct ChannelApprovalBroker {
    sender: mpsc::Sender<PendingApproval>,
}

impl ChannelApprovalBroker {
    pub fn new(sender: mpsc::Sender<PendingApproval>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl ApprovalBroker for ChannelApprovalBroker {
    async fn decide(&self, request: ApprovalRequest) -> Decision {
        let (respond_to, response) = oneshot::channel();
        if self
            .sender
            .send(PendingApproval {
                request,
                respond_to,
            })
            .await
            .is_err()
        {
            return Decision::Deny;
        }
        response.await.unwrap_or(Decision::Deny)
    }
}

pub(crate) struct DenyAll;

#[async_trait]
impl ApprovalBroker for DenyAll {
    async fn decide(&self, _request: ApprovalRequest) -> Decision {
        Decision::Deny
    }
}
