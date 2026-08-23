use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use async_stream::try_stream;
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::header::HeaderMap;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::security::bounded_redacted;

mod openai_chat;

pub use openai_chat::OpenAiChatModel;

pub type ModelStream<'a> = BoxStream<'a, Result<ModelEvent, ModelError>>;

const MAX_SSE_EVENT_BYTES: usize = 256 * 1024;
const MAX_STREAM_FIELD_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 64 * 1024;

struct ToolStreamState {
    call_id: String,
    name: String,
    arguments_done: bool,
    closed: bool,
}

#[derive(Default)]
struct ResponseStreamState {
    tool_calls: HashMap<String, ToolStreamState>,
    call_ids: HashSet<String>,
    last_sequence_number: Option<u64>,
}

enum NormalizedResponseEvent {
    Ignore,
    Emit(ModelEvent),
    Complete,
}

pub trait Model: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_>;
}

impl<T> Model for Box<T>
where
    T: Model + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ModelStream<'_> {
        (**self).stream(request)
    }
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

    fn to_openai_chat_json(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.input_schema,
            }
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
    api_key: Option<SecretString>,
    options: OpenAiModelOptions,
    stream_idle_timeout: Option<Duration>,
}

#[derive(Clone, Default)]
pub(crate) struct OpenAiModelOptions {
    pub(crate) reasoning_effort: Option<&'static str>,
    pub(crate) text_verbosity: Option<&'static str>,
    pub(crate) temperature: Option<f64>,
    pub(crate) max_output_tokens: Option<u32>,
}

impl OpenAiModel {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::configured(
            endpoint.into(),
            Some(api_key.into()),
            HeaderMap::new(),
            OpenAiModelOptions::default(),
            None,
            None,
        )
        .expect("default OpenAI client configuration should be valid")
    }

    pub(crate) fn configured(
        endpoint: String,
        api_key: Option<String>,
        headers: HeaderMap,
        options: OpenAiModelOptions,
        timeout: Option<Duration>,
        stream_idle_timeout: Option<Duration>,
    ) -> Result<Self, reqwest::Error> {
        let mut client = reqwest::Client::builder().default_headers(headers);
        if let Some(timeout) = timeout {
            client = client.timeout(timeout);
        }
        Ok(Self {
            client: client.build()?,
            endpoint,
            api_key: api_key.map(SecretString::from),
            options,
            stream_idle_timeout,
        })
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

fn bounded_string<'a>(
    value: &'a Value,
    field: &str,
    event_name: &str,
) -> Result<&'a str, ModelError> {
    let value = required_string(value, field, event_name)?;
    if value.len() > MAX_STREAM_FIELD_BYTES {
        return Err(ModelError::Protocol(format!(
            "{event_name} {field} exceeds {MAX_STREAM_FIELD_BYTES} bytes"
        )));
    }
    Ok(value)
}

enum ParsedSseEvent {
    Data(String),
    Ignored,
}

fn take_sse_event(buffer: &mut Vec<u8>) -> Result<Option<ParsedSseEvent>, ModelError> {
    let Some((boundary, delimiter_len)) = sse_boundary(buffer) else {
        if buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err(ModelError::Protocol(format!(
                "SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes"
            )));
        }
        return Ok(None);
    };
    if boundary > MAX_SSE_EVENT_BYTES {
        return Err(ModelError::Protocol(format!(
            "SSE event exceeds {MAX_SSE_EVENT_BYTES} bytes"
        )));
    }
    let raw = buffer.drain(..boundary).collect::<Vec<_>>();
    buffer.drain(..delimiter_len);
    let raw = std::str::from_utf8(&raw)
        .map_err(|error| ModelError::Protocol(format!("SSE event is not UTF-8: {error}")))?;
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut data = Vec::new();
    for line in normalized.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    Ok(Some(if data.is_empty() {
        ParsedSseEvent::Ignored
    } else {
        ParsedSseEvent::Data(data.join("\n"))
    }))
}

fn sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        let Some(first) = line_ending_len(buffer, index) else {
            continue;
        };
        let second_index = index + first;
        if let Some(second) = line_ending_len(buffer, second_index) {
            return Some((index, first + second));
        }
    }
    None
}

fn line_ending_len(buffer: &[u8], index: usize) -> Option<usize> {
    match buffer.get(index) {
        Some(b'\n') => Some(1),
        Some(b'\r') if buffer.get(index + 1) == Some(&b'\n') => Some(2),
        Some(b'\r') => Some(1),
        _ => None,
    }
}

impl ResponseStreamState {
    fn normalize(&mut self, data: &str) -> Result<NormalizedResponseEvent, ModelError> {
        let payload: Value =
            serde_json::from_str(data).map_err(|error| ModelError::Protocol(error.to_string()))?;
        let event_type = required_string(&payload, "type", "event")?;
        self.accept_sequence_number(&payload)?;

        match event_type {
            "response.output_text.delta" => {
                let delta = bounded_string(&payload, "delta", "text delta")?;
                Ok(NormalizedResponseEvent::Emit(ModelEvent::TextDelta(
                    delta.to_owned(),
                )))
            }
            "response.output_item.added"
                if payload.pointer("/item/type").and_then(Value::as_str)
                    == Some("function_call") =>
            {
                self.start_tool_call(&payload)
            }
            "response.function_call_arguments.delta" => self.tool_arguments_delta(&payload),
            "response.function_call_arguments.done" => {
                self.tool_arguments_done(&payload)?;
                Ok(NormalizedResponseEvent::Ignore)
            }
            "response.output_item.done"
                if payload.pointer("/item/type").and_then(Value::as_str)
                    == Some("function_call") =>
            {
                self.finish_tool_call(&payload)?;
                Ok(NormalizedResponseEvent::Ignore)
            }
            "response.completed" => {
                if let Some((item_id, _)) = self.tool_calls.iter().find(|(_, state)| !state.closed)
                {
                    return Err(ModelError::Protocol(format!(
                        "response completed with open function call {item_id}"
                    )));
                }
                Ok(NormalizedResponseEvent::Complete)
            }
            "response.failed" | "response.incomplete" => {
                let message = payload
                    .pointer("/response/error/message")
                    .or_else(|| payload.pointer("/response/incomplete_details/reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("provider response failed");
                Err(ModelError::Provider(bounded_redacted(
                    message,
                    MAX_PROVIDER_ERROR_BYTES,
                    "provider error",
                )))
            }
            _ => Ok(NormalizedResponseEvent::Ignore),
        }
    }

    fn accept_sequence_number(&mut self, payload: &Value) -> Result<(), ModelError> {
        let Some(sequence) = payload.get("sequence_number").and_then(Value::as_u64) else {
            return Ok(());
        };
        if self
            .last_sequence_number
            .is_some_and(|last| sequence <= last)
        {
            return Err(ModelError::Protocol(format!(
                "non-monotonic SSE sequence number {sequence}"
            )));
        }
        self.last_sequence_number = Some(sequence);
        Ok(())
    }

    fn start_tool_call(&mut self, payload: &Value) -> Result<NormalizedResponseEvent, ModelError> {
        let item = payload
            .get("item")
            .ok_or_else(|| ModelError::Protocol("function call is missing item".into()))?;
        let item_id = bounded_string(item, "id", "function call")?;
        let call_id = bounded_string(item, "call_id", "function call")?;
        let name = bounded_string(item, "name", "function call")?;
        if self.tool_calls.contains_key(item_id) {
            return Err(ModelError::Protocol(format!(
                "duplicate function call item {item_id}"
            )));
        }
        if !self.call_ids.insert(call_id.to_owned()) {
            return Err(ModelError::Protocol(format!(
                "duplicate function call id {call_id}"
            )));
        }
        self.tool_calls.insert(
            item_id.to_owned(),
            ToolStreamState {
                call_id: call_id.to_owned(),
                name: name.to_owned(),
                arguments_done: false,
                closed: false,
            },
        );
        Ok(NormalizedResponseEvent::Emit(ModelEvent::ToolCallStarted {
            id: call_id.to_owned(),
            name: name.to_owned(),
        }))
    }

    fn tool_arguments_delta(&self, payload: &Value) -> Result<NormalizedResponseEvent, ModelError> {
        let item_id = bounded_string(payload, "item_id", "tool arguments delta")?;
        let state = self.tool_calls.get(item_id).ok_or_else(|| {
            ModelError::Protocol(format!("tool arguments reference unknown item {item_id}"))
        })?;
        if state.closed {
            return Err(ModelError::Protocol(format!(
                "tool arguments arrived after item {item_id} completed"
            )));
        }
        if state.arguments_done {
            return Err(ModelError::Protocol(format!(
                "tool arguments arrived after item {item_id} arguments completed"
            )));
        }
        let delta = bounded_string(payload, "delta", "tool arguments delta")?;
        Ok(NormalizedResponseEvent::Emit(
            ModelEvent::ToolArgumentsDelta {
                id: state.call_id.clone(),
                delta: delta.to_owned(),
            },
        ))
    }

    fn tool_arguments_done(&mut self, payload: &Value) -> Result<(), ModelError> {
        let item_id = bounded_string(payload, "item_id", "tool arguments done")?;
        let state = self.tool_calls.get_mut(item_id).ok_or_else(|| {
            ModelError::Protocol(format!("tool arguments reference unknown item {item_id}"))
        })?;
        if state.closed {
            return Err(ModelError::Protocol(format!(
                "tool arguments completed after item {item_id} completed"
            )));
        }
        if state.arguments_done {
            return Err(ModelError::Protocol(format!(
                "duplicate tool arguments completion {item_id}"
            )));
        }
        state.arguments_done = true;
        Ok(())
    }

    fn finish_tool_call(&mut self, payload: &Value) -> Result<(), ModelError> {
        let item = payload.get("item").ok_or_else(|| {
            ModelError::Protocol("function call completion is missing item".into())
        })?;
        let item_id = bounded_string(item, "id", "function call completion")?;
        let state = self.tool_calls.get_mut(item_id).ok_or_else(|| {
            ModelError::Protocol(format!(
                "function call completion references unknown item {item_id}"
            ))
        })?;
        if state.closed {
            return Err(ModelError::Protocol(format!(
                "duplicate function call completion {item_id}"
            )));
        }
        if !state.arguments_done {
            return Err(ModelError::Protocol(format!(
                "function call {item_id} completed before its arguments"
            )));
        }
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str)
            && call_id != state.call_id
        {
            return Err(ModelError::Protocol(format!(
                "function call {item_id} changed call id"
            )));
        }
        if let Some(name) = item.get("name").and_then(Value::as_str)
            && name != state.name
        {
            return Err(ModelError::Protocol(format!(
                "function call {item_id} changed name"
            )));
        }
        state.closed = true;
        Ok(())
    }
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
            if let Some(effort) = self.options.reasoning_effort {
                body["reasoning"] = json!({ "effort": effort });
            }
            if let Some(verbosity) = self.options.text_verbosity {
                body["text"] = json!({ "verbosity": verbosity });
            }
            if let Some(temperature) = self.options.temperature {
                body["temperature"] = json!(temperature);
            }
            if let Some(max_output_tokens) = self.options.max_output_tokens {
                body["max_output_tokens"] = json!(max_output_tokens);
            }
            let mut request_builder = self.client.post(&self.endpoint);
            if let Some(api_key) = &self.api_key {
                request_builder = request_builder.bearer_auth(api_key.expose_secret());
            }
            let send = request_builder.json(&body).send();
            let response = tokio::select! {
                _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                response = send => response
                    .map_err(|error| ModelError::Transport(error.to_string()))
                    .and_then(|response| response.error_for_status()
                        .map_err(|error| ModelError::Transport(error.to_string()))),
            }?;

            let mut source = response.bytes_stream();
            let mut sse_buffer = Vec::new();
            let mut state = ResponseStreamState::default();
            let mut completed = false;
            'stream: loop {
                let next_chunk = async {
                    match self.stream_idle_timeout {
                        Some(timeout) => tokio::time::timeout(timeout, source.next())
                            .await
                            .map_err(|_| ModelError::Transport("model stream idle timeout".into())),
                        None => Ok(source.next().await),
                    }
                };
                let chunk = tokio::select! {
                    _ = cancellation.cancelled() => Err(ModelError::Cancelled),
                    chunk = next_chunk => chunk,
                }?;
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|error| ModelError::Transport(error.to_string()))?;
                for piece in chunk.chunks(8 * 1024) {
                    sse_buffer.extend_from_slice(piece);
                    while let Some(event) = take_sse_event(&mut sse_buffer)? {
                        let ParsedSseEvent::Data(data) = event else {
                            continue;
                        };
                        match state.normalize(&data)? {
                            NormalizedResponseEvent::Ignore => {}
                            NormalizedResponseEvent::Emit(event) => yield event,
                            NormalizedResponseEvent::Complete => {
                                completed = true;
                                yield ModelEvent::Finished(FinishReason::Completed);
                                break 'stream;
                            }
                        }
                    }
                }
            }

            if !completed {
                Err(ModelError::Protocol("stream ended before response.completed".into()))?;
            }
        })
    }
}
