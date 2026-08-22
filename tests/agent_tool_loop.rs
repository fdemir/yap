use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use yap::{
    agent::{Agent, AgentEvent, ToolOutcome},
    approval::{ApprovalBroker, ApprovalRequest, Decision, Risk},
    model::{
        FinishReason, Model, ModelError, ModelEvent, ModelInput, ModelRequest, ModelStream,
        ToolSpec,
    },
    tool::{Tool, ToolError, ToolOutput},
};

struct ScriptedModel {
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Model for ScriptedModel {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_> {
        self.requests.lock().unwrap().push(request);
        let events = self.responses.lock().unwrap().pop_front().unwrap();
        stream::iter(events).boxed()
    }
}

struct RecordingTool {
    calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("read_file", "Read a text file", json!({"type": "object"}))
    }

    fn risk(&self, _arguments: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        self.calls.lock().unwrap().push(arguments);
        Ok(ToolOutput::new("fn main() {}"))
    }
}

struct DenyingApproval {
    requests: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl ApprovalBroker for DenyingApproval {
    async fn decide(&self, request: ApprovalRequest) -> Decision {
        self.requests.lock().unwrap().push(request);
        Decision::Deny
    }
}

struct MutatingTool {
    calls: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for MutatingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("run_command", "Run a command", json!({"type": "object"}))
    }

    fn risk(&self, _arguments: &Value) -> Risk {
        Risk::Mutating
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        self.calls.lock().unwrap().push(arguments);
        Ok(ToolOutput::new("command ran"))
    }
}

#[tokio::test]
async fn agent_executes_a_tool_and_continues_with_its_result() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            vec![
                Ok(ModelEvent::ToolCallStarted {
                    id: "call_1".into(),
                    name: "read_file".into(),
                }),
                Ok(ModelEvent::ToolArgumentsDelta {
                    id: "call_1".into(),
                    delta: "{\"path\":\"src/main.rs\"}".into(),
                }),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
            vec![
                Ok(ModelEvent::TextDelta("The file is valid.".into())),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
        ])),
        requests: requests.clone(),
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
    let mut agent = Agent::new(model, "gpt-5.3-codex");
    agent.set_event_sender(event_tx);
    agent.register_tool(RecordingTool {
        calls: calls.clone(),
    });

    let outcome = agent
        .run_turn("Inspect main")
        .await
        .expect("turn should complete");

    assert_eq!(outcome.assistant_text, "The file is valid.");
    assert_eq!(*calls.lock().unwrap(), vec![json!({"path": "src/main.rs"})]);
    assert_eq!(
        event_rx.recv().await,
        Some(AgentEvent::ToolStarted {
            id: "call_1".into(),
            name: "read_file".into(),
        })
    );
    assert_eq!(
        event_rx.recv().await,
        Some(AgentEvent::ToolFinished {
            id: "call_1".into(),
            name: "read_file".into(),
            outcome: ToolOutcome::Completed,
        })
    );
    assert_eq!(
        requests.lock().unwrap()[0].tools(),
        &[ToolSpec::new(
            "read_file",
            "Read a text file",
            json!({"type": "object"}),
        )]
    );
    assert_eq!(
        requests.lock().unwrap()[1].input(),
        &[
            ModelInput::UserMessage("Inspect main".into()),
            ModelInput::FunctionCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "src/main.rs"}),
            },
            ModelInput::FunctionCallOutput {
                id: "call_1".into(),
                output: "fn main() {}".into(),
            },
        ]
    );
}

#[tokio::test]
async fn agent_returns_a_denied_mutation_to_the_model_without_executing_it() {
    let model_requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            vec![
                Ok(ModelEvent::ToolCallStarted {
                    id: "call_2".into(),
                    name: "run_command".into(),
                }),
                Ok(ModelEvent::ToolArgumentsDelta {
                    id: "call_2".into(),
                    delta: "{\"command\":\"cargo test\"}".into(),
                }),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
            vec![
                Ok(ModelEvent::TextDelta("Command denied.".into())),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
        ])),
        requests: model_requests.clone(),
    };
    let approval_requests = Arc::new(Mutex::new(Vec::new()));
    let tool_calls = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(model, "gpt-5.3-codex");
    agent.set_approval_broker(DenyingApproval {
        requests: approval_requests.clone(),
    });
    agent.register_tool(MutatingTool {
        calls: tool_calls.clone(),
    });

    let outcome = agent
        .run_turn("Run tests")
        .await
        .expect("denial should be returned to the model");

    assert_eq!(outcome.assistant_text, "Command denied.");
    assert!(tool_calls.lock().unwrap().is_empty());
    assert_eq!(approval_requests.lock().unwrap()[0].call_id, "call_2");
    assert_eq!(
        model_requests.lock().unwrap()[1].input().last(),
        Some(&ModelInput::FunctionCallOutput {
            id: "call_2".into(),
            output: "denied by user".into(),
        })
    );
}
