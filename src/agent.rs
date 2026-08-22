use std::collections::HashMap;

use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    approval::{ApprovalBroker, ApprovalRequest, Decision, DenyAll, Risk},
    model::{Model, ModelError, ModelEvent, ModelInput, ModelRequest},
    system_prompt::SYSTEM_PROMPT,
    tool::{Tool, ToolError, ToolOutput},
};

const MAX_STEPS: usize = 12;

pub struct Agent<M> {
    model: M,
    model_name: String,
    tools: HashMap<String, Box<dyn Tool>>,
    approval_broker: Box<dyn ApprovalBroker>,
    event_sender: Option<mpsc::Sender<AgentEvent>>,
}

impl<M> Agent<M>
where
    M: Model,
{
    pub fn new(model: M, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            tools: HashMap::new(),
            approval_broker: Box::new(DenyAll),
            event_sender: None,
        }
    }

    pub fn set_event_sender(&mut self, event_sender: mpsc::Sender<AgentEvent>) {
        self.event_sender = Some(event_sender);
    }

    pub fn set_approval_broker(&mut self, approval_broker: impl ApprovalBroker + 'static) {
        self.approval_broker = Box::new(approval_broker);
    }

    pub fn register_tool(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.spec().name, Box::new(tool));
    }

    pub async fn run_turn(&mut self, prompt: impl Into<String>) -> Result<TurnOutcome, AgentError> {
        self.run_turn_with_cancellation(prompt, CancellationToken::new())
            .await
    }

    pub async fn run_turn_with_cancellation(
        &mut self,
        prompt: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentError> {
        let mut input = vec![ModelInput::UserMessage(prompt.into())];

        for _ in 0..MAX_STEPS {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let mut tool_specs = self
                .tools
                .values()
                .map(|tool| tool.spec())
                .collect::<Vec<_>>();
            tool_specs.sort_by(|left, right| left.name.cmp(&right.name));
            let request = ModelRequest::from_input(self.model_name.clone(), input.clone())
                .with_tools(tool_specs)
                .with_system_prompt(SYSTEM_PROMPT)
                .with_cancellation(cancellation.clone());
            let mut stream = self.model.stream(request);
            let mut assistant_text = String::new();
            let mut tool_calls: Vec<PendingToolCall> = Vec::new();

            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                    event = stream.next() => event,
                };
                let Some(event) = event else { break };
                let event = match event {
                    Err(ModelError::Cancelled) if cancellation.is_cancelled() => {
                        return Err(AgentError::Cancelled);
                    }
                    event => event?,
                };
                match event {
                    ModelEvent::TextDelta(delta) => {
                        assistant_text.push_str(&delta);
                        self.emit(AgentEvent::AssistantDelta(delta)).await;
                    }
                    ModelEvent::ToolCallStarted { id, name } => {
                        self.emit(AgentEvent::ToolStarted {
                            id: id.clone(),
                            name: name.clone(),
                        })
                        .await;
                        tool_calls.push(PendingToolCall {
                            id,
                            name,
                            arguments: String::new(),
                        });
                    }
                    ModelEvent::ToolArgumentsDelta { id, delta } => {
                        let call = tool_calls
                            .iter_mut()
                            .find(|call| call.id == id)
                            .ok_or_else(|| AgentError::UnknownToolCall(id.clone()))?;
                        call.arguments.push_str(&delta);
                    }
                    ModelEvent::Finished(_) => break,
                }
            }
            drop(stream);

            if tool_calls.is_empty() {
                self.emit(AgentEvent::TurnFinished {
                    assistant_text: assistant_text.clone(),
                })
                .await;
                return Ok(TurnOutcome { assistant_text });
            }
            if !assistant_text.is_empty() {
                input.push(ModelInput::AssistantMessage(assistant_text));
            }

            for call in tool_calls {
                let arguments: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                    AgentError::InvalidToolArguments {
                        name: call.name.clone(),
                        message: error.to_string(),
                    }
                })?;
                input.push(ModelInput::FunctionCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: arguments.clone(),
                });

                let tool = self
                    .tools
                    .get(&call.name)
                    .ok_or_else(|| AgentError::UnknownTool(call.name.clone()))?;
                let risk = tool.risk(&arguments);
                let decision = match risk {
                    Risk::ReadOnly | Risk::WorkspaceWrite => Decision::Allow,
                    Risk::Mutating => {
                        let preview = tool.approval_preview(&arguments)?;
                        tokio::select! {
                            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                            decision = self.approval_broker.decide(ApprovalRequest {
                                call_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                arguments: arguments.clone(),
                                risk,
                                preview,
                            }) => decision,
                        }
                    }
                };
                let (output, outcome) = match decision {
                    Decision::Allow => {
                        let output = tokio::select! {
                            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                            output = tool.execute(arguments) => output?,
                        };
                        (output, ToolOutcome::Completed)
                    }
                    Decision::Deny => (ToolOutput::new("denied by user"), ToolOutcome::Denied),
                };
                self.emit(AgentEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name,
                    outcome,
                })
                .await;
                input.push(ModelInput::FunctionCallOutput {
                    id: call.id,
                    output: output.into_model_text(),
                });
            }
        }

        Err(AgentError::StepLimit)
    }

    async fn emit(&self, event: AgentEvent) {
        if let Some(sender) = &self.event_sender {
            let _ = sender.send(event).await;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    AssistantDelta(String),
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        outcome: ToolOutcome,
    },
    TurnFinished {
        assistant_text: String,
    },
    TurnCancelled,
    TurnFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Completed,
    Denied,
    Cancelled,
}

struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub assistant_text: String,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent turn was cancelled")]
    Cancelled,
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("tool arguments arrived for unknown call {0}")]
    UnknownToolCall(String),
    #[error("model requested unknown tool {0}")]
    UnknownTool(String),
    #[error("invalid arguments for tool {name}: {message}")]
    InvalidToolArguments { name: String, message: String },
    #[error("agent reached its step limit")]
    StepLimit,
}
