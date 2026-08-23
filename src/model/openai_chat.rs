use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::security::bounded_redacted;

use super::{
    FinishReason, MAX_PROVIDER_ERROR_BYTES, MAX_STREAM_FIELD_BYTES, Model, ModelError, ModelEvent,
    ModelInput, ModelRequest, ModelStream, OpenAiModelOptions, ParsedSseEvent, ToolSpec,
    take_sse_event,
};

pub struct OpenAiChatModel {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<SecretString>,
    options: OpenAiModelOptions,
    stream_idle_timeout: Option<Duration>,
}

impl OpenAiChatModel {
    pub fn new(endpoint: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::configured(
            endpoint.into(),
            Some(api_key.into()),
            HeaderMap::new(),
            OpenAiModelOptions::default(),
            None,
            None,
        )
        .expect("default OpenAI-compatible client configuration should be valid")
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

#[derive(Default)]
struct ChatStreamState {
    tools: BTreeMap<u64, ChatToolState>,
    call_ids: HashSet<String>,
}

#[derive(Default)]
struct ChatToolState {
    id: Option<String>,
    name: Option<String>,
    started: bool,
}

enum NormalizedChatEvent {
    Ignore,
    Emit(Vec<ModelEvent>),
    Complete,
}

impl ChatStreamState {
    fn normalize(&mut self, data: &str) -> Result<NormalizedChatEvent, ModelError> {
        if data.trim() == "[DONE]" {
            if let Some((index, _)) = self.tools.iter().find(|(_, state)| !state.started) {
                return Err(ModelError::Protocol(format!(
                    "chat stream completed with incomplete tool call {index}"
                )));
            }
            return Ok(NormalizedChatEvent::Complete);
        }

        let payload: Value =
            serde_json::from_str(data).map_err(|error| ModelError::Protocol(error.to_string()))?;
        if let Some(message) = payload.pointer("/error/message").and_then(Value::as_str) {
            return Err(ModelError::Provider(bounded_redacted(
                message,
                MAX_PROVIDER_ERROR_BYTES,
                "provider error",
            )));
        }
        let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
            return Ok(NormalizedChatEvent::Ignore);
        };
        let mut events = Vec::new();
        for choice in choices {
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(content) = delta.get("content")
                && !content.is_null()
            {
                let content = content.as_str().ok_or_else(|| {
                    ModelError::Protocol("chat text delta content is not a string".into())
                })?;
                if content.len() > MAX_STREAM_FIELD_BYTES {
                    return Err(ModelError::Protocol(format!(
                        "chat text delta exceeds {MAX_STREAM_FIELD_BYTES} bytes"
                    )));
                }
                if !content.is_empty() {
                    events.push(ModelEvent::TextDelta(content.to_owned()));
                }
            }
            let Some(tool_calls) = delta.get("tool_calls") else {
                continue;
            };
            let tool_calls = tool_calls.as_array().ok_or_else(|| {
                ModelError::Protocol("chat tool_calls delta is not an array".into())
            })?;
            for tool_call in tool_calls {
                self.normalize_tool_delta(tool_call, &mut events)?;
            }
        }
        Ok(if events.is_empty() {
            NormalizedChatEvent::Ignore
        } else {
            NormalizedChatEvent::Emit(events)
        })
    }

    fn normalize_tool_delta(
        &mut self,
        tool_call: &Value,
        events: &mut Vec<ModelEvent>,
    ) -> Result<(), ModelError> {
        let index = tool_call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| ModelError::Protocol("chat tool call delta is missing index".into()))?;
        let state = self.tools.entry(index).or_default();
        if let Some(id_value) = tool_call.get("id")
            && !id_value.is_null()
        {
            let id = id_value
                .as_str()
                .ok_or_else(|| ModelError::Protocol("chat tool call id is not a string".into()))?;
            if id.len() > MAX_STREAM_FIELD_BYTES {
                return Err(ModelError::Protocol(format!(
                    "chat tool call id exceeds {MAX_STREAM_FIELD_BYTES} bytes"
                )));
            }
            if state.id.as_deref().is_some_and(|current| current != id) {
                return Err(ModelError::Protocol(format!(
                    "chat tool call {index} changed id"
                )));
            }
            state.id = Some(id.to_owned());
        }
        let function = tool_call.get("function");
        if let Some(name_value) = function.and_then(|value| value.get("name"))
            && !name_value.is_null()
        {
            let name = name_value.as_str().ok_or_else(|| {
                ModelError::Protocol("chat tool call name is not a string".into())
            })?;
            if name.len() > MAX_STREAM_FIELD_BYTES {
                return Err(ModelError::Protocol(format!(
                    "chat tool call name exceeds {MAX_STREAM_FIELD_BYTES} bytes"
                )));
            }
            if state.name.as_deref().is_some_and(|current| current != name) {
                return Err(ModelError::Protocol(format!(
                    "chat tool call {index} changed name"
                )));
            }
            state.name = Some(name.to_owned());
        }
        if !state.started
            && let (Some(id), Some(name)) = (&state.id, &state.name)
        {
            if !self.call_ids.insert(id.clone()) {
                return Err(ModelError::Protocol(format!(
                    "duplicate chat tool call id {id}"
                )));
            }
            state.started = true;
            events.push(ModelEvent::ToolCallStarted {
                id: id.clone(),
                name: name.clone(),
            });
        }
        if let Some(arguments_value) = function.and_then(|value| value.get("arguments"))
            && !arguments_value.is_null()
        {
            let arguments = arguments_value.as_str().ok_or_else(|| {
                ModelError::Protocol("chat tool arguments delta is not a string".into())
            })?;
            if arguments.len() > MAX_STREAM_FIELD_BYTES {
                return Err(ModelError::Protocol(format!(
                    "chat tool arguments delta exceeds {MAX_STREAM_FIELD_BYTES} bytes"
                )));
            }
            if !arguments.is_empty() {
                if !state.started {
                    return Err(ModelError::Protocol(format!(
                        "chat tool arguments arrived before tool call {index} had an id and name"
                    )));
                }
                events.push(ModelEvent::ToolArgumentsDelta {
                    id: state.id.clone().expect("started tool call has an id"),
                    delta: arguments.to_owned(),
                });
            }
        }
        Ok(())
    }
}

impl Model for OpenAiChatModel {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_> {
        Box::pin(try_stream! {
            let cancellation = request.cancellation;
            let mut messages = openai_chat_messages(request.input);
            if let Some(system_prompt) = request.system_prompt {
                messages.insert(0, json!({ "role": "system", "content": system_prompt }));
            }
            let tools = request
                .tools
                .iter()
                .map(ToolSpec::to_openai_chat_json)
                .collect::<Vec<_>>();
            let mut body = json!({
                "model": request.model,
                "messages": messages,
                "stream": true,
            });
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools);
            }
            if let Some(temperature) = self.options.temperature {
                body["temperature"] = json!(temperature);
            }
            if let Some(max_output_tokens) = self.options.max_output_tokens {
                body["max_completion_tokens"] = json!(max_output_tokens);
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
            let mut state = ChatStreamState::default();
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
                            NormalizedChatEvent::Ignore => {}
                            NormalizedChatEvent::Emit(events) => {
                                for event in events {
                                    yield event;
                                }
                            }
                            NormalizedChatEvent::Complete => {
                                completed = true;
                                yield ModelEvent::Finished(FinishReason::Completed);
                                break 'stream;
                            }
                        }
                    }
                }
            }

            if !completed {
                Err(ModelError::Protocol("stream ended before chat completion [DONE]".into()))?;
            }
        })
    }
}

fn openai_chat_messages(input: Vec<ModelInput>) -> Vec<Value> {
    let mut messages = Vec::new();
    for item in input {
        match item {
            ModelInput::UserMessage(content) => {
                messages.push(json!({ "role": "user", "content": content }));
            }
            ModelInput::AssistantMessage(content) => {
                messages.push(json!({ "role": "assistant", "content": content }));
            }
            ModelInput::FunctionCall {
                id,
                name,
                arguments,
            } => {
                let tool_call = json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments.to_string(),
                    }
                });
                let append_to_last = messages.last().is_some_and(|message: &Value| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                });
                if append_to_last {
                    let message = messages.last_mut().expect("last message exists");
                    if message.get("tool_calls").is_none() {
                        message["tool_calls"] = json!([]);
                    }
                    message["tool_calls"]
                        .as_array_mut()
                        .expect("tool_calls was initialized as an array")
                        .push(tool_call);
                } else {
                    messages.push(json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [tool_call],
                    }));
                }
            }
            ModelInput::FunctionCallOutput { id, output } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": output,
                }));
            }
        }
    }
    messages
}
