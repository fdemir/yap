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
    let descendant_pid_file = workspace.path().join("descendant.pid");
    let provider = CommandProvider {
        responses: Mutex::new(VecDeque::from([vec![
            Ok(ModelEvent::ToolCallStarted {
                id: "command_1".into(),
                name: "run_command".into(),
            }),
            Ok(ModelEvent::ToolArgumentsDelta {
                id: "command_1".into(),
                delta: "{\"command\":\"sh -c 'sleep 10 & echo $! > descendant.pid; wait'\"}".into(),
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
    wait_for_file(&descendant_pid_file).await;
    let descendant_pid = std::fs::read_to_string(&descendant_pid_file)
        .expect("descendant pid should be readable")
        .trim()
        .parse::<i32>()
        .expect("descendant pid should be numeric");
    assert!(process_exists(descendant_pid));

    let cancelled_at = Instant::now();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("command cancellation should be prompt")
        .expect("agent task should not panic");

    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    wait_for_process_exit(descendant_pid).await;
}

async fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while !path.exists() {
        assert!(Instant::now() < deadline, "command did not start in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_process_exit(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_exists(pid) {
        assert!(
            Instant::now() < deadline,
            "descendant process {pid} survived cancellation"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn process_exists(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}
