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
use yap::model::{
    FinishReason, Model, ModelEvent, ModelInput, ModelRequest, OpenAiChatModel, ToolSpec,
};

#[tokio::test]
async fn chat_model_normalizes_fragmented_text_streams() {
    let endpoint = serve_fragmented_sse(&[
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel",
        "lo\"}}]}\n\ndata: [DONE]\n\n",
    ])
    .await
    .unwrap();
    let model = OpenAiChatModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("model", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![
            Ok(ModelEvent::TextDelta("Hello".into())),
            Ok(ModelEvent::Finished(FinishReason::Completed)),
        ]
    );
}

#[tokio::test]
async fn chat_model_assembles_streamed_tool_calls() {
    let endpoint = serve_fragmented_sse(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/main.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    ])
    .await
    .unwrap();
    let model = OpenAiChatModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("model", "Read main"))
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
async fn chat_model_serializes_messages_tools_and_system_prompt() {
    let (endpoint, captured) = serve_sse_and_capture_request().await;
    let model = OpenAiChatModel::new(endpoint, "test-key");
    let request = ModelRequest::from_input(
        "model",
        vec![
            ModelInput::UserMessage("Read main".into()),
            ModelInput::AssistantMessage("I'll read it.".into()),
            ModelInput::FunctionCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
            },
            ModelInput::FunctionCallOutput {
                id: "call_1".into(),
                output: "fn main() {}".into(),
            },
        ],
    )
    .with_system_prompt("You are Yap.")
    .with_tools(vec![ToolSpec::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )]);

    let events = model.stream(request).collect::<Vec<_>>().await;

    assert!(events.iter().all(Result::is_ok));
    let request = captured.lock().unwrap().clone().unwrap();
    let body: serde_json::Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1)
        .expect("request body should be JSON");
    assert_eq!(
        body["messages"][0],
        serde_json::json!({
            "role": "system",
            "content": "You are Yap."
        })
    );
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
}

#[tokio::test]
async fn chat_model_rejects_tool_arguments_before_identity() {
    let endpoint = serve_fragmented_sse(&[
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
    ])
    .await
    .unwrap();
    let model = OpenAiChatModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("model", "Read main"))
        .collect::<Vec<_>>()
        .await;

    assert!(matches!(
        events.last(),
        Some(Err(yap::model::ModelError::Protocol(message)))
            if message.contains("before tool call 0 had an id and name")
    ));
}

#[tokio::test]
async fn chat_model_rejects_an_oversized_sse_event() {
    let oversized = format!("data: {}\n\n", "x".repeat(256 * 1024 + 1));
    let endpoint = serve_fragmented_sse(&[&oversized]).await.unwrap();
    let model = OpenAiChatModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("model", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events,
        vec![Err(yap::model::ModelError::Protocol(
            "SSE event exceeds 262144 bytes".into(),
        ))]
    );
}

#[tokio::test]
async fn chat_model_reports_cancellation() {
    let endpoint = serve_stalled_sse().await.unwrap();
    let cancellation = CancellationToken::new();
    let model = OpenAiChatModel::new(endpoint, "test-key");
    let mut stream = model
        .stream(ModelRequest::new("model", "Say hello").with_cancellation(cancellation.clone()));

    cancellation.cancel();

    assert_eq!(
        stream.next().await,
        Some(Err(yap::model::ModelError::Cancelled))
    );
}

#[tokio::test]
async fn chat_model_rejects_a_stream_without_done() {
    let endpoint =
        serve_fragmented_sse(&["data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"])
            .await
            .unwrap();
    let model = OpenAiChatModel::new(endpoint, "test-key");

    let events = model
        .stream(ModelRequest::new("model", "Say hello"))
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        events.last(),
        Some(&Err(yap::model::ModelError::Protocol(
            "stream ended before chat completion [DONE]".into(),
        )))
    );
}

async fn serve_sse_and_capture_request() -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let server_capture = captured.clone();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut socket).await;
        *server_capture.lock().unwrap() = Some(request);
        let event = "data: [DONE]\n\n";
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                    event.len(), event
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });

    (format!("http://{address}/v1/chat/completions"), captured)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let read = socket.read(&mut buffer).await.unwrap();
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
            return String::from_utf8(request).unwrap();
        }
    }
}

async fn serve_stalled_sse() -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    Ok(format!("http://{address}/v1/chat/completions"))
}

async fn serve_fragmented_sse(chunks: &[&str]) -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let chunks = chunks.iter().map(ToString::to_string).collect::<Vec<_>>();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        for chunk in chunks {
            socket
                .write_all(format!("{:X}\r\n{}\r\n", chunk.len(), chunk).as_bytes())
                .await
                .unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
    });

    Ok(format!("http://{address}/v1/chat/completions"))
}
