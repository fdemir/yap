use std::{
    collections::VecDeque,
    fs,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use tempfile::tempdir;
use yap::{
    agent::Agent,
    approval::{ApprovalBroker, ApprovalRequest, Decision},
    model::{FinishReason, Model, ModelError, ModelEvent, ModelInput, ModelRequest, ModelStream},
    tools::{ApplyPatchTool, ReadFileTool, RunCommandTool},
};

struct FakeProvider {
    responses: Mutex<VecDeque<Vec<Result<ModelEvent, ModelError>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Model for FakeProvider {
    fn stream(&self, request: ModelRequest) -> ModelStream<'_> {
        self.requests.lock().unwrap().push(request);
        stream::iter(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("fake provider should have another response"),
        )
        .boxed()
    }
}

struct AllowingApproval {
    requests: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl ApprovalBroker for AllowingApproval {
    async fn decide(&self, request: ApprovalRequest) -> Decision {
        self.requests.lock().unwrap().push(request);
        Decision::Allow
    }
}

#[tokio::test]
async fn fake_provider_drives_read_edit_command_and_final_response() {
    let workspace = tempdir().expect("workspace should be created");
    let file = workspace.path().join("note.txt");
    fs::write(&file, "old\n").expect("fixture file should be created");

    let model_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = FakeProvider {
        responses: Mutex::new(VecDeque::from([
            tool_call("read_1", "read_file", r#"{"path":"note.txt"}"#),
            tool_call(
                "edit_1",
                "apply_patch",
                r#"{"path":"note.txt","old_text":"old","new_text":"new"}"#,
            ),
            tool_call("command_1", "run_command", r#"{"command":"echo verified"}"#),
            vec![
                Ok(ModelEvent::TextDelta("Done.".into())),
                Ok(ModelEvent::Finished(FinishReason::Completed)),
            ],
        ])),
        requests: model_requests.clone(),
    };
    let approval_requests = Arc::new(Mutex::new(Vec::new()));
    let mut agent = Agent::new(provider, "gpt-5.3-codex");
    agent.set_approval_broker(AllowingApproval {
        requests: approval_requests.clone(),
    });
    agent.register_tool(ReadFileTool::new(workspace.path()).unwrap());
    agent.register_tool(ApplyPatchTool::new(workspace.path()).unwrap());
    agent.register_tool(RunCommandTool::new(workspace.path()).unwrap());

    let outcome = agent
        .run_turn("Update the note and verify it")
        .await
        .expect("end-to-end turn should complete");

    assert_eq!(outcome.assistant_text, "Done.");
    assert_eq!(fs::read_to_string(file).unwrap(), "new\n");
    assert!(approval_requests.lock().unwrap().is_empty());

    let requests = model_requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_tool_output(&requests[1], "read_1", |output| output == "old\n");
    assert_tool_output(&requests[2], "edit_1", |output| {
        output == "updated note.txt"
    });
    assert_tool_output(&requests[3], "command_1", |output| {
        output.starts_with("exit code: 0") && output.contains("verified")
    });
}

fn tool_call(id: &str, name: &str, arguments: &str) -> Vec<Result<ModelEvent, ModelError>> {
    vec![
        Ok(ModelEvent::ToolCallStarted {
            id: id.into(),
            name: name.into(),
        }),
        Ok(ModelEvent::ToolArgumentsDelta {
            id: id.into(),
            delta: arguments.into(),
        }),
        Ok(ModelEvent::Finished(FinishReason::Completed)),
    ]
}

fn assert_tool_output(
    request: &ModelRequest,
    expected_id: &str,
    predicate: impl FnOnce(&str) -> bool,
) {
    let output = request
        .input()
        .iter()
        .rev()
        .find_map(|input| match input {
            ModelInput::FunctionCallOutput { id, output } if id == expected_id => Some(output),
            _ => None,
        })
        .expect("request should contain the expected tool output");
    assert!(predicate(output), "unexpected tool output: {output}");
}
