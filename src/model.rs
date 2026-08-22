use std::collections::HashMap;

use async_stream::try_stream;
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub type ModelStream<'a> = BoxStream<'a, Result<ModelEvent, ModelError>>;

pub trait Model: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_>;
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    model: String,
    input: Vec<ModelInput>,
    tools: Vec<ToolSpec>,
    system_prompt: Option<String>,
    cancellation: CancellationToken,
}

impl ModelRequest {
    pub fn new(model: impl Into<String>, input: impl Into<String>) -> Self {
        Self::from_input(model, vec![ModelInput::UserMessage(input.into())])
    }

    pub fn from_input(model: impl Into<String>, input: Vec<ModelInput>) -> Self {
        Self {
            model: model.into(),
            input,
            tools: Vec::new(),
            system_prompt: None,
            cancellation: CancellationToken::new(),
        }
    }

    pub fn input(&self) -> &[ModelInput] {
        &self.input
    }

    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(system_prompt.into());
        self
    }

    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }

    fn to_openai_json(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.input_schema,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelInput {
    UserMessage(String),
    AssistantMessage(String),
    FunctionCall {
        id: String,
        name: String,
        arguments: Value,
    },
    FunctionCallOutput {
        id: String,
        output: String,
    },
}

impl ModelInput {
    fn to_openai_json(&self) -> Value {
        match self {
            Self::UserMessage(content) => json!({
                "type": "message",
                "role": "user",
                "content": content,
            }),
            Self::AssistantMessage(content) => json!({
                "type": "message",
                "role": "assistant",
                "content": content,
            }),
            Self::FunctionCall {
                id,
                name,
                arguments,
            } => json!({
                "type": "function_call",
                "call_id": id,
                "name": name,
                "arguments": arguments.to_string(),
            }),
            Self::FunctionCallOutput { id, output } => json!({
                "type": "function_call_output",
                "call_id": id,
                "output": output,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    TextDelta(String),
    ToolCallStarted { id: String, name: String },
    ToolArgumentsDelta { id: String, delta: String },
    Finished(FinishReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Completed,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("model transport failed: {0}")]
    Transport(String),
    #[error("model provider failed: {0}")]
    Provider(String),
    #[error("model request was cancelled")]
    Cancelled,
    #[error("model protocol failed: {0}")]
    Protocol(String),
}

pub struct OpenAiModel {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl OpenAiModel {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
            api_key: api_key.into(),
        }
    }
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    event_name: &str,
) -> Result<&'a str, ModelError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ModelError::Protocol(format!("{event_name} is missing {field}")))
}

impl Model for OpenAiModel {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_> {
        Box::pin(try_stream! {
            let cancellation = request.cancellation;
            let input = request
                .input
                .iter()
                .map(ModelInput::to_openai_json)
                .collect::<Vec<_>>();
            let tools = request
                .tools
                .iter()
                .map(ToolSpec::to_openai_json)
                .collect::<Vec<_>>();
            let mut body = json!({
                "model": request.model,
                "input": input,
                "tools": tools,
                "stream": true,
            });
            if let Some(system_prompt) = request.system_prompt {
                body["instructions"] = Value::String(system_prompt);
            }
            let send = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send();
            let response = tokio::select! {
                _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                response = send => response
                    .map_err(|error| ModelError::Transport(error.to_string()))
                    .and_then(|response| response.error_for_status()
                        .map_err(|error| ModelError::Transport(error.to_string()))),
            }?;

            let mut source = response.bytes_stream().eventsource();
            let mut tool_call_ids = HashMap::new();
            let mut completed = false;
            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                    event = source.next() => Ok(event),
                }?;
                let Some(event) = event else { break };
                let event = event.map_err(|error| ModelError::Protocol(error.to_string()))?;
                let payload: Value = serde_json::from_str(&event.data)
                    .map_err(|error| ModelError::Protocol(error.to_string()))?;

                let event_type = required_string(&payload, "type", "event")?;
                match event_type {
                    "response.output_text.delta" => {
                        let delta = required_string(&payload, "delta", "text delta")?;
                        yield ModelEvent::TextDelta(delta.to_owned());
                    }
                    "response.output_item.added"
                        if payload.pointer("/item/type").and_then(Value::as_str)
                            == Some("function_call") =>
                    {
                        let item = payload
                            .get("item")
                            .ok_or_else(|| ModelError::Protocol("function call is missing item".into()))?;
                        let item_id = required_string(item, "id", "function call")?;
                        let call_id = required_string(item, "call_id", "function call")?;
                        let name = required_string(item, "name", "function call")?;
                        tool_call_ids.insert(item_id.to_owned(), call_id.to_owned());
                        yield ModelEvent::ToolCallStarted {
                            id: call_id.to_owned(),
                            name: name.to_owned(),
                        };
                    }
                    "response.function_call_arguments.delta" => {
                        let item_id = required_string(&payload, "item_id", "tool arguments delta")?;
                        let call_id = tool_call_ids.get(item_id).ok_or_else(|| {
                            ModelError::Protocol(format!("tool arguments reference unknown item {item_id}"))
                        })?;
                        let delta = required_string(&payload, "delta", "tool arguments delta")?;
                        yield ModelEvent::ToolArgumentsDelta {
                            id: call_id.clone(),
                            delta: delta.to_owned(),
                        };
                    }
                    "response.completed" => {
                        completed = true;
                        yield ModelEvent::Finished(FinishReason::Completed);
                    }
                    "response.failed" => {
                        let message = payload
                            .pointer("/response/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("provider response failed");
                        Err(ModelError::Provider(message.to_owned()))?;
                    }
                    _ => {}
                }
            }

            if !completed {
                Err(ModelError::Protocol("stream ended before response.completed".into()))?;
            }
        })
    }
}
