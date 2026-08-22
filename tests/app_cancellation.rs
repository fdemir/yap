use serde_json::json;
use tokio::sync::oneshot;
use yap::{
    agent::AgentEvent,
    app::{App, Status},
    approval::{ApprovalRequest, Decision, PendingApproval, Risk},
};

#[test]
fn cancelled_turn_returns_the_app_to_ready() {
    let mut app = App::new();
    app.push_input('x');
    app.submit().expect("prompt should submit");
    assert_eq!(app.status(), Status::Working);

    app.reduce(AgentEvent::TurnCancelled);

    assert_eq!(app.status(), Status::Ready);
}

#[tokio::test]
async fn cancelling_a_pending_approval_denies_it() {
    let mut app = App::new();
    let (respond_to, response) = oneshot::channel();
    app.receive_approval(PendingApproval {
        request: ApprovalRequest {
            call_id: "command_1".into(),
            tool_name: "run_command".into(),
            arguments: json!({"command": "sleep 10"}),
            risk: Risk::Mutating,
            preview: None,
        },
        respond_to,
    });

    app.cancel_active_turn();

    assert_eq!(response.await, Ok(Decision::Deny));
}
