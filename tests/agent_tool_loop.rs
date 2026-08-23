use std::{
    collections::VecDeque,
    fs,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tempfile::tempdir;
use yap::{
    agent::{Agent, AgentEvent, ToolOutcome},
    approval::{ApprovalBroker, ApprovalRequest, Decision, Risk},
    model::{
        FinishReason, Model, ModelError, ModelEvent, ModelInput, ModelRequest, ModelStream,
        ToolSpec,
    },
    system_prompt::SYSTEM_PROMPT,
    tool::{Tool, ToolError, ToolOutput},
    tools::{ApplyPatchTool, RunCommandTool},
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

    fn approval_preview(&self, _arguments: &Value) -> Result<Option<String>, ToolError> {
        Ok(Some("x".repeat(70 * 1024)))
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
    let requests = requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.system_prompt() == Some(SYSTEM_PROMPT))
    );
    assert_eq!(
        requests[0].tools(),
        &[ToolSpec::new(
            "read_file",
            "Read a text file",
            json!({"type": "object"}),
        )]
    );
    assert_eq!(
        requests[1].input(),
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
async fn agent_requests_approval_on_the_third_identical_tool_call() {
    let model_requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            read_call("call_1"),
            read_call("call_2"),
            read_call("call_3"),
            vec![
                Ok(ModelEvent::TextDelta("Stopped repeating.".into())),
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
    agent.register_tool(RecordingTool {
        calls: tool_calls.clone(),
    });

    let outcome = agent
        .run_turn("Keep inspecting main")
        .await
        .expect("denied repetition should return to the model");

    assert_eq!(outcome.assistant_text, "Stopped repeating.");
    assert_eq!(tool_calls.lock().unwrap().len(), 2);
    let approvals = approval_requests.lock().unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].risk, Risk::RepeatedCall);
    assert_eq!(
        model_requests.lock().unwrap()[3].input().last(),
        Some(&ModelInput::FunctionCallOutput {
            id: "call_3".into(),
            output: "denied by user".into(),
        })
    );
}

fn read_call(id: &str) -> Vec<Result<ModelEvent, ModelError>> {
    vec![
        Ok(ModelEvent::ToolCallStarted {
            id: id.into(),
            name: "read_file".into(),
        }),
        Ok(ModelEvent::ToolArgumentsDelta {
            id: id.into(),
            delta: "{\"path\":\"src/main.rs\"}".into(),
        }),
        Ok(ModelEvent::Finished(FinishReason::Completed)),
    ]
}

#[tokio::test]
async fn agent_requests_approval_before_a_command_accesses_an_external_path() {
    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should be created");
    let model_requests = Arc::new(Mutex::new(Vec::new()));
    let model = ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            vec![
                Ok(ModelEvent::ToolCallStarted {
                    id: "command_1".into(),
                    name: "run_command".into(),
                }),
                Ok(ModelEvent::ToolArgumentsDelta {
                    id: "command_1".into(),
                    delta: "{\"command\":\"OPENAI_API_KEY=sk-abcdefghijklmnop touch ../outside\"}"
                        .into(),
                }),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
            vec![
                Ok(ModelEvent::TextDelta("External command denied.".into())),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
        ])),
        requests: model_requests.clone(),
    };
    let approval_requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(model, "gpt-5.3-codex");
    agent.set_approval_broker(DenyingApproval {
        requests: approval_requests.clone(),
    });
    agent.register_tool(RunCommandTool::new(&workspace).expect("workspace should be valid"));

    let outcome = agent
        .run_turn("Touch a file outside the workspace")
        .await
        .expect("denial should return to the model");

    assert_eq!(outcome.assistant_text, "External command denied.");
    assert!(!root.path().join("outside").exists());
    let approvals = approval_requests.lock().unwrap();
    assert_eq!(approvals[0].risk, Risk::ExternalAccess);
    assert!(
        !approvals[0]
            .preview
            .as_deref()
            .unwrap_or_default()
            .contains("sk-abcdefghijklmnop")
    );
    assert!(
        !approvals[0]
            .arguments
            .to_string()
            .contains("sk-abcdefghijklmnop")
    );
    drop(approvals);
    assert_eq!(
        model_requests.lock().unwrap()[1].input().last(),
        Some(&ModelInput::FunctionCallOutput {
            id: "command_1".into(),
            output: "denied by user".into(),
        })
    );
}

#[tokio::test]
async fn agent_applies_workspace_edits_without_requesting_approval() {
    let workspace = tempdir().expect("workspace should be created");
    let path = workspace.path().join("main.rs");
    fs::write(&path, "old\n").expect("file should be created");
    let model = ScriptedModel {
        responses: Mutex::new(VecDeque::from([
            vec![
                Ok(ModelEvent::ToolCallStarted {
                    id: "call_edit".into(),
                    name: "apply_patch".into(),
                }),
                Ok(ModelEvent::ToolArgumentsDelta {
                    id: "call_edit".into(),
                    delta: "{\"path\":\"main.rs\",\"old_text\":\"old\",\"new_text\":\"new\"}"
                        .into(),
                }),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
            vec![
                Ok(ModelEvent::TextDelta("Updated.".into())),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
        ])),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let approval_requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(model, "gpt-5.3-codex");
    agent.set_approval_broker(DenyingApproval {
        requests: approval_requests.clone(),
    });
    let patch = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");
    let patch_arguments = json!({
        "path": "main.rs",
        "old_text": "old",
        "new_text": "new"
    });
    assert_eq!(patch.risk(&patch_arguments), Risk::WorkspaceWrite);
    agent.register_tool(patch);

    agent
        .run_turn("Update main")
        .await
        .expect("turn should complete");

    assert_eq!(
        fs::read_to_string(path).expect("file should remain readable"),
        "new\n"
    );
    assert!(approval_requests.lock().unwrap().is_empty());
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
    let approvals = approval_requests.lock().unwrap();
    assert_eq!(approvals[0].call_id, "call_2");
    let preview = approvals[0]
        .preview
        .as_deref()
        .expect("approval should include a preview");
    assert!(preview.len() <= 64 * 1024);
    assert!(preview.contains("approval preview truncated"));
    drop(approvals);
    assert_eq!(
        model_requests.lock().unwrap()[1].input().last(),
        Some(&ModelInput::FunctionCallOutput {
            id: "call_2".into(),
            output: "denied by user".into(),
        })
    );
}
