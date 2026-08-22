use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use yap::model::{FinishReason, Model, ModelEvent, ModelRequest, OpenAiModel, ToolSpec};

#[tokio::test]
async fn model_stream_normalizes_fragmented_openai_sse_events() {
    let endpoint = serve_fragmented_sse(&[
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.del",
        "ta\",\"delta\":\"Hel\"}\n\nevent: response.output_text.delta\ndata: {\"type\":",
        "\"response.output_text.delta\",\"delta\":\"lo\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
    ])
    .await
    .expect("fixture server should start");
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            Ok(ModelEvent::TextDelta("Hel".into())),
            Ok(ModelEvent::TextDelta("lo".into())),
            Ok(ModelEvent::Finished(FinishReason::Completed)),
        ]
    );
}

#[tokio::test]
async fn model_stream_sends_system_prompt_as_openai_instructions() {
    let (endpoint, captured_request) = serve_sse_and_capture_request().await;
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Say hello").with_system_prompt("You are Yap."))
        .collect::<Vec<_>>()
        .await;

    assert!(events.iter().all(Result::is_ok));
    let request = captured_request
        .lock()
        .unwrap()
        .clone()
        .expect("request should be captured");
    let body: serde_json::Value = serde_json::from_str(
        request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1,
    )
    .expect("request body should be JSON");
    assert_eq!(body["instructions"], "You are Yap.");
}

#[tokio::test]
async fn model_stream_sends_function_tool_definitions() {
    let (endpoint, captured_request) = serve_sse_and_capture_request().await;
    let model = OpenAiModel::new(endpoint, "test-key");
    let request = ModelRequest::new("gpt-5.3-codex", "Read main").with_tools(vec![ToolSpec::new(
        "read_file",
        "Read a text file inside the workspace",
        serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
    )]);

    let events = model.stream(request).collect::<Vec<_>>().await;

    assert!(events.iter().all(Result::is_ok));
    let request = captured_request
        .lock()
        .unwrap()
        .clone()
        .expect("request should be captured");
    let body: serde_json::Value = serde_json::from_str(
        request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1,
    )
    .expect("request body should be JSON");
    assert_eq!(
        body["tools"],
        serde_json::json!([{
            "type": "function",
            "name": "read_file",
            "description": "Read a text file inside the workspace",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }])
    );
}

#[tokio::test]
async fn model_stream_normalizes_function_call_events() {
    let endpoint = serve_fragmented_sse(&[
        "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":\"{\\\"path\\\":\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":\"\\\"src/main.rs\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\"}\n\n",
    ])
    .await
    .expect("fixture server should start");
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Read main"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            Ok(ModelEvent::ToolCallStarted {
                id: "call_1".into(),
                name: "read_file".into(),
            }),
            Ok(ModelEvent::ToolArgumentsDelta {
                id: "call_1".into(),
                delta: "{\"path\":".into(),
            }),
            Ok(ModelEvent::ToolArgumentsDelta {
                id: "call_1".into(),
                delta: "\"src/main.rs\"}".into(),
            }),
            Ok(ModelEvent::Finished(FinishReason::Completed)),
        ]
    );
}

#[tokio::test]
async fn model_stream_surfaces_a_provider_failure() {
    let endpoint = serve_fragmented_sse(&[
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"quota exceeded\"}}}\n\n",
    ])
    .await
    .expect("fixture server should start");
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![Err(yap::model::ModelError::Provider(
            "quota exceeded".into(),
        ))]
    );
}

#[tokio::test]
async fn model_stream_rejects_an_event_without_a_type() {
    let endpoint = serve_fragmented_sse(&["data: {}\n\n"])
        .await
        .expect("fixture server should start");
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![Err(yap::model::ModelError::Protocol(
            "event is missing type".into(),
        ))]
    );
}

#[tokio::test]
async fn model_stream_reports_cancellation() {
    let endpoint = serve_stalled_sse()
        .await
        .expect("fixture server should start");
    let cancellation = CancellationToken::new();
    let model = OpenAiModel::new(endpoint, "test-key");
    let mut stream = model.stream(
        ModelRequest::new("gpt-5.3-codex", "Say hello").with_cancellation(cancellation.clone()),
    );

    cancellation.cancel();

    assert_eq!(
        stream.next().await,
        Some(Err(yap::model::ModelError::Cancelled))
    );
}

#[tokio::test]
async fn model_stream_rejects_an_incomplete_response() {
    let endpoint = serve_fragmented_sse(&[
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
    ])
    .await
    .expect("fixture server should start");
    let model = OpenAiModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("gpt-5.3-codex", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            Ok(ModelEvent::TextDelta("partial".into())),
            Err(yap::model::ModelError::Protocol(
                "stream ended before response.completed".into(),
            )),
        ]
    );
}

async fn serve_sse_and_capture_request() -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture server should bind");
    let address = listener.local_addr().expect("fixture address should exist");
    let captured = Arc::new(Mutex::new(None));
    let server_capture = captured.clone();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("client should connect");
        let request = read_http_request(&mut socket).await;
        *server_capture.lock().unwrap() = Some(request);
        let event = "data: {\"type\":\"response.completed\"}\n\n";
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                    event.len(),
                    event
                )
                .as_bytes(),
            )
            .await
            .expect("response should be writable");
    });

    (format!("http://{address}/v1/responses"), captured)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let read = socket
            .read(&mut buffer)
            .await
            .expect("request should be readable");
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + content_length {
            return String::from_utf8(request).expect("request should be UTF-8");
        }
    }
}

async fn serve_stalled_sse() -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("client should connect");
        let mut request = vec![0; 4096];
        let _ = socket
            .read(&mut request)
            .await
            .expect("request should be readable");
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
            .await
            .expect("headers should be writable");
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    Ok(format!("http://{address}/v1/responses"))
}

async fn serve_fragmented_sse(chunks: &[&str]) -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let chunks = chunks.iter().map(ToString::to_string).collect::<Vec<_>>();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("client should connect");
        let mut request = vec![0; 4096];
        let _ = socket
            .read(&mut request)
            .await
            .expect("request should be readable");

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .expect("headers should be writable");

        for chunk in chunks {
            socket
                .write_all(format!("{:X}\r\n{}\r\n", chunk.len(), chunk).as_bytes())
                .await
                .expect("chunk should be writable");
        }
        socket
            .write_all(b"0\r\n\r\n")
            .await
            .expect("stream should finish");
    });

    Ok(format!("http://{address}/v1/responses"))
}
