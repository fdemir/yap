use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use yap::{
    agent::{Agent, AgentEvent},
    model::{FinishReason, Model, ModelError, ModelEvent, ModelRequest, ModelStream},
};

struct PendingModel;

impl Model for PendingModel {
    fn stream(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::pending().boxed()
    }
}

struct ScriptedModel {
    events: Vec<Result<ModelEvent, ModelError>>,
}

impl ScriptedModel {
    fn new(events: Vec<Result<ModelEvent, ModelError>>) -> Self {
        Self { events }
    }
}

impl Model for ScriptedModel {
    fn stream(&self, _request: ModelRequest) -> ModelStream<'_> {
        stream::iter(self.events.clone()).boxed()
    }
}

#[tokio::test]
async fn agent_turn_returns_the_completed_assistant_text() {
    let model = ScriptedModel::new(vec![
        Ok(ModelEvent::TextDelta("Hello ".into())),
        Ok(ModelEvent::TextDelta("world".into())),
        Ok(ModelEvent::Finished(FinishReason::Completed)),
    ]);
    let mut agent = Agent::new(model, "gpt-5.3-codex");

    let outcome = agent
        .run_turn("Greet me")
        .await
        .expect("turn should complete");

    assert_eq!(outcome.assistant_text, "Hello world");
}

#[tokio::test]
async fn agent_turn_rejects_oversized_prompts_and_responses() {
    let mut prompt_agent = Agent::new(
        ScriptedModel::new(vec![Ok(ModelEvent::Finished(FinishReason::Completed))]),
        "gpt-5.3-codex",
    );
    let prompt_error = prompt_agent
        .run_turn("x".repeat(64 * 1024 + 1))
        .await
        .expect_err("oversized prompt should be rejected");
    assert!(matches!(
        prompt_error,
        yap::agent::AgentError::LimitExceeded("user prompt")
    ));

    let mut response_agent = Agent::new(
        ScriptedModel::new(vec![
            Ok(ModelEvent::TextDelta("x".repeat(1024 * 1024 + 1))),
            Ok(ModelEvent::Finished(FinishReason::Completed)),
        ]),
        "gpt-5.3-codex",
    );
    let response_error = response_agent
        .run_turn("respond")
        .await
        .expect_err("oversized response should be rejected");
    assert!(matches!(
        response_error,
        yap::agent::AgentError::LimitExceeded("assistant response")
    ));
}

#[tokio::test]
async fn agent_turn_rejects_oversized_tool_arguments() {
    let model = ScriptedModel::new(vec![
        Ok(ModelEvent::ToolCallStarted {
            id: "call_1".into(),
            name: "read_file".into(),
        }),
        Ok(ModelEvent::ToolArgumentsDelta {
            id: "call_1".into(),
            delta: "x".repeat(256 * 1024 + 1),
        }),
        Ok(ModelEvent::Finished(FinishReason::Completed)),
    ]);
    let mut agent = Agent::new(model, "gpt-5.3-codex");

    let error = agent
        .run_turn("read")
        .await
        .expect_err("oversized tool arguments should be rejected");

    assert!(matches!(
        error,
        yap::agent::AgentError::LimitExceeded("tool arguments")
    ));
}

#[tokio::test]
async fn agent_turn_stops_when_cancelled() {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        let mut agent = Agent::new(PendingModel, "gpt-5.3-codex");
        agent
            .run_turn_with_cancellation("Wait forever", task_cancellation)
            .await
    });

    tokio::task::yield_now().await;
    cancellation.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("cancelled turn should stop promptly")
        .expect("agent task should not panic");
    assert!(matches!(result, Err(yap::agent::AgentError::Cancelled)));
}

#[tokio::test]
async fn agent_turn_publishes_streaming_events_without_rendering() {
    let model = ScriptedModel::new(vec![
        Ok(ModelEvent::TextDelta("Hello ".into())),
        Ok(ModelEvent::TextDelta("world".into())),
        Ok(ModelEvent::Finished(FinishReason::Completed)),
    ]);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
    let mut agent = Agent::new(model, "gpt-5.3-codex");
    agent.set_event_sender(event_tx);

    agent
        .run_turn("Greet me")
        .await
        .expect("turn should complete");

    assert_eq!(
        event_rx.recv().await,
        Some(AgentEvent::AssistantDelta("Hello ".into()))
    );
    assert_eq!(
        event_rx.recv().await,
        Some(AgentEvent::AssistantDelta("world".into()))
    );
    assert_eq!(
        event_rx.recv().await,
        Some(AgentEvent::TurnFinished {
            assistant_text: "Hello world".into(),
        })
    );
}
