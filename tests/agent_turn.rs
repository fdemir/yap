use futures_util::{StreamExt, stream};
use yap::{
    agent::{Agent, AgentEvent},
    model::{FinishReason, Model, ModelError, ModelEvent, ModelRequest, ModelStream},
};

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
