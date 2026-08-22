use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::tempdir;
use yap::{
    approval::Risk,
    tool::{Tool, ToolError},
    tools::RunCommandTool,
};

#[tokio::test]
#[cfg(unix)]
async fn run_command_executes_in_the_workspace_and_captures_output() {
    let workspace = tempdir().expect("workspace should be created");
    let tool = RunCommandTool::new(workspace.path()).expect("workspace should be valid");
    let arguments = json!({
        "command": "printf 'hello'; printf 'warn' >&2"
    });

    assert_eq!(tool.risk(&arguments), Risk::Mutating);
    let output = tool
        .execute(arguments)
        .await
        .expect("command should succeed");

    assert_eq!(
        output.into_model_text(),
        "exit code: 0\nstdout:\nhello\nstderr:\nwarn"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn run_command_truncates_output_at_its_byte_limit() {
    let workspace = tempdir().expect("workspace should be created");
    let tool = RunCommandTool::with_limits(workspace.path(), Duration::from_secs(1), 8)
        .expect("workspace should be valid");

    let output = tool
        .execute(json!({"command": "printf '0123456789'"}))
        .await
        .expect("command should complete");

    assert_eq!(
        output.into_model_text(),
        "exit code: 0\nstdout:\n01234567\n[stdout truncated after 8 bytes]\nstderr:\n"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn run_command_stops_at_its_timeout() {
    let workspace = tempdir().expect("workspace should be created");
    let tool = RunCommandTool::with_timeout(workspace.path(), Duration::from_millis(20))
        .expect("workspace should be valid");
    let started = Instant::now();

    let error = tool
        .execute(json!({"command": "sleep 5"}))
        .await
        .expect_err("slow command should time out");

    assert_eq!(error, ToolError::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
}
