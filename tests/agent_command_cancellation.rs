#![cfg(unix)]

use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use yap::{
    agent::{Agent, AgentError, AgentEvent},
    approval::{ApprovalBroker, ApprovalRequest, Decision},
    model::{FinishReason, Model, ModelError, ModelEvent, ModelRequest, ModelStream},
    tools::RunCommandTool,
};

struct CommandProvider {
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
}

impl Model for CommandProvider {
    fn stream(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(self.responses.lock().unwrap().pop_front().unwrap()).boxed()
    }
}

struct AllowCommands;

#[async_trait]
impl ApprovalBroker for AllowCommands {
    async fn decide(&self, _request: ApprovalRequest) -> Decision {
        Decision::Allow
    }
}

#[tokio::test]
async fn cancelling_a_turn_stops_a_running_command_promptly() {
    let workspace = tempdir().expect("workspace should be created");
    let marker = workspace.path().join("started");
    let provider = CommandProvider {
        responses: Mutex::new(VecDeque::from([vec![
            Ok(ModelEvent::ToolCallStarted {
                id: "command_1".into(),
                name: "run_command".into(),
            }),
            Ok(ModelEvent::ToolArgumentsDelta {
                id: "command_1".into(),
                delta: "{\"command\":\"touch started && sleep 10\"}".into(),
            }),
            Ok(ModelEvent::Finished(FinishReason::Completed)),
        ]])),
    };
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
    let mut agent = Agent::new(provider, "gpt-5.3-codex");
    agent.set_event_sender(event_tx);
    agent.set_approval_broker(AllowCommands);
    agent.register_tool(RunCommandTool::new(workspace.path()).unwrap());

    let task = tokio::spawn(async move {
        agent
            .run_turn_with_cancellation("Run a slow command", task_cancellation)
            .await
    });
    while !matches!(event_rx.recv().await, Some(AgentEvent::ToolStarted { .. })) {}
    wait_for_file(&marker).await;

    let cancelled_at = Instant::now();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("command cancellation should be prompt")
        .expect("agent task should not panic");

    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
}

async fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !path.exists() {
        assert!(Instant::now() < deadline, "command did not start in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
