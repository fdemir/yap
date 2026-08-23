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

    assert_eq!(tool.risk(&arguments), Risk::WorkspaceWrite);
    let output = tool
        .execute(arguments)
        .await
        .expect("command should succeed");

    assert_eq!(
        output.into_model_text(),
        "exit code: 0\nstdout:\nhello\nstderr:\nwarn"
    );
}

#[test]
fn run_command_requests_approval_for_external_paths() {
    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace should be created");
    let tool = RunCommandTool::new(&workspace).expect("workspace should be valid");

    let external = root.path().join("secret.txt");
    std::fs::write(&external, "secret").expect("external file should be created");

    assert_eq!(
        tool.risk(&json!({"command": "cat ../secret.txt"})),
        Risk::ExternalAccess
    );
    assert_eq!(
        tool.risk(&json!({"command": "echo $(cat ../secret.txt)"})),
        Risk::ExternalAccess
    );
    assert_eq!(
        tool.risk(&json!({"command": "cat ${HOME}/.config/example"})),
        Risk::ExternalAccess
    );
    assert_eq!(
        tool.risk(&json!({"command": "cat $PWD/src/main.rs"})),
        Risk::WorkspaceWrite
    );
    assert_eq!(
        tool.risk(&json!({"command": format!("cat '{}'", external.display())})),
        Risk::ExternalAccess
    );
    assert_eq!(
        tool.risk(&json!({"command": "cargo test && rm target/stale"})),
        Risk::WorkspaceWrite
    );
    assert_eq!(
        tool.risk(&json!({"command": "curl https://example.com"})),
        Risk::WorkspaceWrite
    );
    assert_eq!(
        tool.risk(&json!({"command": format!("echo '{}'", external.display())})),
        Risk::WorkspaceWrite
    );
}

#[test]
#[cfg(unix)]
fn run_command_requests_approval_for_a_workspace_symlink_to_an_external_file() {
    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace should be created");
    let external = root.path().join("secret.txt");
    std::fs::write(&external, "secret").expect("external file should be created");
    std::os::unix::fs::symlink(&external, workspace.join("secret-link"))
        .expect("symlink should be created");
    let tool = RunCommandTool::new(&workspace).expect("workspace should be valid");

    assert_eq!(
        tool.risk(&json!({"command": "cat secret-link"})),
        Risk::ExternalAccess
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
async fn run_command_stops_output_floods() {
    let workspace = tempdir().expect("workspace should be created");
    let tool =
        RunCommandTool::with_output_limits(workspace.path(), Duration::from_secs(1), 64, 1024)
            .expect("workspace should be valid");

    let error = tool
        .execute(json!({"command": "yes output"}))
        .await
        .expect_err("output flood should be stopped");

    assert_eq!(error, ToolError::OutputLimit { limit: 1024 });
}

#[tokio::test]
#[cfg(unix)]
async fn run_command_applies_its_total_limit_across_both_streams() {
    let workspace = tempdir().expect("workspace should be created");
    let tool =
        RunCommandTool::with_output_limits(workspace.path(), Duration::from_secs(1), 1024, 1024)
            .expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "command": "head -c 600 /dev/zero; head -c 600 /dev/zero >&2"
        }))
        .await
        .expect_err("aggregate output flood should be stopped");

    assert_eq!(error, ToolError::OutputLimit { limit: 1024 });
}

#[tokio::test]
#[cfg(unix)]
async fn natural_completion_kills_descendants_that_escape_the_process_group() {
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        return;
    }

    let workspace = tempdir().expect("workspace should be created");
    let pid_file = workspace.path().join("escaped.pid");
    let pid_path = serde_json::to_string(&pid_file.to_string_lossy()).unwrap();
    let command = format!(
        r#"python3 -c 'import os,time
path={pid_path}
pid=os.fork()
if pid == 0:
 os.setsid()
 null=os.open("/dev/null",os.O_RDWR)
 os.dup2(null,0); os.dup2(null,1); os.dup2(null,2)
 open(path,"w").write(str(os.getpid()))
 time.sleep(30)
else:
 while not os.path.exists(path): time.sleep(0.001)
 os._exit(0)'"#
    );
    let tool = RunCommandTool::with_timeout(workspace.path(), Duration::from_secs(2))
        .expect("workspace should be valid");

    tool.execute(json!({"command": command}))
        .await
        .expect("foreground command should complete");

    let pid = std::fs::read_to_string(pid_file)
        .expect("escaped descendant pid should be written")
        .trim()
        .parse::<i32>()
        .expect("escaped descendant pid should be numeric");
    wait_for_process_exit(pid).await;
}

#[tokio::test]
#[cfg(unix)]
async fn run_command_timeout_kills_descendant_processes() {
    let workspace = tempdir().expect("workspace should be created");
    let tool = RunCommandTool::with_timeout(workspace.path(), Duration::from_millis(100))
        .expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "command": "sh -c 'sleep 10 & echo $! > descendant.pid; wait'"
        }))
        .await
        .expect_err("slow process tree should time out");

    assert_eq!(error, ToolError::TimedOut);
    let pid = std::fs::read_to_string(workspace.path().join("descendant.pid"))
        .expect("descendant pid should be written")
        .trim()
        .parse::<i32>()
        .expect("descendant pid should be numeric");
    wait_for_process_exit(pid).await;
}

#[tokio::test]
#[cfg(unix)]
async fn timeout_gracefully_terminates_then_kills_an_escaped_descendant() {
    if std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        return;
    }

    let workspace = tempdir().expect("workspace should be created");
    let pid_file = workspace.path().join("escaped-timeout.pid");
    let term_file = workspace.path().join("escaped-timeout.term");
    let pid_path = serde_json::to_string(&pid_file.to_string_lossy()).unwrap();
    let term_path = serde_json::to_string(&term_file.to_string_lossy()).unwrap();
    let command = format!(
        r#"python3 -c 'import os,signal,time
pid_path={pid_path}
term_path={term_path}
pid=os.fork()
if pid == 0:
 os.setsid()
 open(pid_path,"w").write(str(os.getpid()))
 def stop(signum,frame):
  open(term_path,"w").write("TERM")
  time.sleep(3)
 signal.signal(signal.SIGTERM,stop)
 while True: time.sleep(1)
else:
 while True: time.sleep(1)'"#
    );
    let tool = RunCommandTool::with_timeout(workspace.path(), Duration::from_millis(200))
        .expect("workspace should be valid");

    let error = tool
        .execute(json!({"command": command}))
        .await
        .expect_err("escaped process should time out");

    assert_eq!(error, ToolError::TimedOut);
    assert_eq!(
        std::fs::read_to_string(term_file).expect("TERM handler should run"),
        "TERM"
    );
    let pid = std::fs::read_to_string(pid_file)
        .expect("escaped descendant pid should be written")
        .trim()
        .parse::<i32>()
        .expect("escaped descendant pid should be numeric");
    wait_for_process_exit(pid).await;
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

#[cfg(unix)]
async fn wait_for_process_exit(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_exists(pid) {
        assert!(
            Instant::now() < deadline,
            "descendant process {pid} survived timeout"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}
