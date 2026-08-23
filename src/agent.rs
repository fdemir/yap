use std::collections::{HashMap, VecDeque};

use futures_util::StreamExt;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    approval::{ApprovalBroker, ApprovalRequest, Decision, DenyAll, Risk},
    model::{Model, ModelError, ModelEvent, ModelInput, ModelRequest},
    security::{
        MAX_APPROVAL_PREVIEW_BYTES, MAX_TOOL_OUTPUT_BYTES, bounded_redacted, checked_append,
        redact_json,
    },
    system_prompt::SYSTEM_PROMPT,
    tool::{Tool, ToolError, ToolOutput},
};

const MAX_STEPS: usize = 12;
const DOOM_LOOP_THRESHOLD: usize = 3;
const MAX_USER_PROMPT_BYTES: usize = 64 * 1024;
const MAX_ASSISTANT_TEXT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_TOOL_CALLS_PER_STEP: usize = 32;
const MAX_TOOL_IDENTIFIER_BYTES: usize = 1024;
const MAX_MODEL_INPUT_BYTES: usize = 2 * 1024 * 1024;

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
        let prompt = prompt.into();
        if prompt.len() > MAX_USER_PROMPT_BYTES {
            return Err(AgentError::LimitExceeded("user prompt"));
        }
        let mut input = vec![ModelInput::UserMessage(prompt)];
        let mut recent_tool_calls = VecDeque::with_capacity(DOOM_LOOP_THRESHOLD - 1);

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
            if model_input_bytes(&input) > MAX_MODEL_INPUT_BYTES {
                return Err(AgentError::LimitExceeded("model input"));
            }
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
                        if !checked_append(&mut assistant_text, &delta, MAX_ASSISTANT_TEXT_BYTES) {
                            return Err(AgentError::LimitExceeded("assistant response"));
                        }
                        self.emit(AgentEvent::AssistantDelta(delta)).await;
                    }
                    ModelEvent::ToolCallStarted { id, name } => {
                        if id.len() > MAX_TOOL_IDENTIFIER_BYTES
                            || name.len() > MAX_TOOL_IDENTIFIER_BYTES
                        {
                            return Err(AgentError::LimitExceeded("tool identifier"));
                        }
                        if tool_calls.len() == MAX_TOOL_CALLS_PER_STEP {
                            return Err(AgentError::LimitExceeded("tool calls per step"));
                        }
                        if tool_calls.iter().any(|call| call.id == id) {
                            return Err(AgentError::DuplicateToolCall(id));
                        }
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
                        if !checked_append(&mut call.arguments, &delta, MAX_TOOL_ARGUMENT_BYTES) {
                            return Err(AgentError::LimitExceeded("tool arguments"));
                        }
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
                let repeated = recent_tool_calls.len() == DOOM_LOOP_THRESHOLD - 1
                    && recent_tool_calls
                        .iter()
                        .all(|(name, previous)| name == &call.name && previous == &arguments);
                if recent_tool_calls.len() == DOOM_LOOP_THRESHOLD - 1 {
                    recent_tool_calls.pop_front();
                }
                recent_tool_calls.push_back((call.name.clone(), arguments.clone()));

                let risk = if repeated {
                    Risk::RepeatedCall
                } else {
                    tool.risk(&arguments)
                };
                let decision = if risk.requires_approval() {
                    let mut preview = tool.approval_preview(&arguments)?;
                    if risk == Risk::RepeatedCall {
                        let details = preview.take().unwrap_or_else(|| {
                            serde_json::to_string_pretty(&arguments)
                                .unwrap_or_else(|_| "unable to display arguments".into())
                        });
                        preview = Some(format!(
                            "same tool call repeated {DOOM_LOOP_THRESHOLD} times\n\n{details}"
                        ));
                    }
                    preview = preview.map(|preview| {
                        bounded_redacted(&preview, MAX_APPROVAL_PREVIEW_BYTES, "approval preview")
                    });
                    let approval_arguments = redact_json(&arguments);
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                        decision = self.approval_broker.decide(ApprovalRequest {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments: approval_arguments,
                            risk,
                            preview,
                        }) => decision,
                    }
                } else {
                    Decision::Allow
                };
                let (output, outcome) = match decision {
                    Decision::Allow => {
                        let result = tokio::select! {
                            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                            output = tool.execute(arguments) => output,
                        };
                        match result {
                            Ok(output) => (output, ToolOutcome::Completed),
                            Err(error) => {
                                self.emit(AgentEvent::ToolFinished {
                                    id: call.id,
                                    name: call.name,
                                    outcome: ToolOutcome::Failed,
                                    output: bounded_redacted(
                                        &error.to_string(),
                                        MAX_TOOL_OUTPUT_BYTES,
                                        "tool error",
                                    ),
                                })
                                .await;
                                return Err(error.into());
                            }
                        }
                    }
                    Decision::Deny => (ToolOutput::new("denied by user"), ToolOutcome::Denied),
                };
                let display_output = output.model_text().to_owned();
                self.emit(AgentEvent::ToolFinished {
                    id: call.id.clone(),
                    name: call.name,
                    outcome,
                    output: display_output,
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
        output: String,
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
    Failed,
}

fn model_input_bytes(input: &[ModelInput]) -> usize {
    input.iter().fold(0usize, |total, item| {
        let item_bytes = match item {
            ModelInput::UserMessage(message) | ModelInput::AssistantMessage(message) => {
                message.len()
            }
            ModelInput::FunctionCall {
                id,
                name,
                arguments,
            } => id
                .len()
                .saturating_add(name.len())
                .saturating_add(arguments.to_string().len()),
            ModelInput::FunctionCallOutput { id, output } => id.len().saturating_add(output.len()),
        };
        total.saturating_add(item_bytes)
    })
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
    #[error("model emitted duplicate tool call {0}")]
    DuplicateToolCall(String),
    #[error("model requested unknown tool {0}")]
    UnknownTool(String),
    #[error("invalid arguments for tool {name}: {message}")]
    InvalidToolArguments { name: String, message: String },
    #[error("agent reached its step limit")]
    StepLimit,
    #[error("agent exceeded the {0} limit")]
    LimitExceeded(&'static str),
}
