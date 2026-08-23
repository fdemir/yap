use std::{
    io,
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::{
    approval::Risk,
    model::ToolSpec,
    process_tree::CommandSupervisor,
    tool::{Tool, ToolError, ToolOutput},
    tools::command_policy,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_TOTAL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

pub struct RunCommandTool {
    root: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
    max_total_output_bytes: usize,
}

impl RunCommandTool {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_timeout(root, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(root: impl Into<PathBuf>, timeout: Duration) -> io::Result<Self> {
        Self::with_limits(root, timeout, DEFAULT_MAX_OUTPUT_BYTES)
    }

    pub fn with_limits(
        root: impl Into<PathBuf>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> io::Result<Self> {
        Self::with_output_limits(
            root,
            timeout,
            max_output_bytes,
            DEFAULT_MAX_TOTAL_OUTPUT_BYTES,
        )
    }

    pub fn with_output_limits(
        root: impl Into<PathBuf>,
        timeout: Duration,
        max_output_bytes: usize,
        max_total_output_bytes: usize,
    ) -> io::Result<Self> {
        Ok(Self {
            root: std::fs::canonicalize(root.into())?,
            timeout,
            max_output_bytes,
            max_total_output_bytes,
        })
    }
}

#[derive(Deserialize)]
struct RunCommandArguments {
    command: String,
}

#[async_trait]
impl Tool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "run_command",
            "Run a shell command in the workspace",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
        )
    }

    fn risk(&self, arguments: &Value) -> Risk {
        let external = arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command_policy::accesses_outside(&self.root, command));
        if external {
            Risk::ExternalAccess
        } else {
            Risk::WorkspaceWrite
        }
    }

    fn approval_preview(&self, arguments: &Value) -> Result<Option<String>, ToolError> {
        let arguments: RunCommandArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        Ok(Some(format!(
            "command: {}\ncwd: {}\ntimeout: {}s",
            arguments.command,
            self.root.display(),
            self.timeout.as_secs()
        )))
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let arguments: RunCommandArguments = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let supervisor = CommandSupervisor::start().await;
        let mut command = shell_command(&arguments.command);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_tree(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut process_tree = supervisor.track(child.id());
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("command stdout was not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("command stderr was not captured".into()))?;
        let total_output = AtomicUsize::new(0);
        let run = async {
            tokio::try_join!(
                wait_for_child(&mut child, &mut process_tree),
                read_capped(
                    stdout,
                    self.max_output_bytes,
                    self.max_total_output_bytes,
                    &total_output,
                ),
                read_capped(
                    stderr,
                    self.max_output_bytes,
                    self.max_total_output_bytes,
                    &total_output,
                ),
            )
        };
        let (status, stdout, stderr) = tokio::time::timeout(self.timeout, run)
            .await
            .map_err(|_| ToolError::TimedOut)??;
        process_tree.disarm();
        let exit_code = status
            .code()
            .map_or_else(|| "signal".into(), |code| code.to_string());

        Ok(ToolOutput::new(format!(
            "exit code: {exit_code}\nstdout:\n{}\nstderr:\n{}",
            stdout.render("stdout", self.max_output_bytes),
            stderr.render("stderr", self.max_output_bytes)
        )))
    }
}

struct CappedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CappedOutput {
    fn render(&self, stream: &str, limit: usize) -> String {
        let mut rendered = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            rendered.push_str(&format!("\n[{stream} truncated after {limit} bytes]"));
        }
        rendered
    }
}

async fn wait_for_child(
    child: &mut tokio::process::Child,
    process_tree: &mut crate::process_tree::ProcessTreeGuard,
) -> Result<std::process::ExitStatus, ToolError> {
    let mut refresh = tokio::time::interval(Duration::from_millis(2));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status.map_err(|error| ToolError::Execution(error.to_string()))?;
                process_tree.leader_finished();
                return Ok(status);
            }
            _ = refresh.tick() => process_tree.refresh(),
        }
    }
}

async fn read_capped(
    mut reader: impl AsyncRead + Unpin,
    retained_limit: usize,
    total_limit: usize,
    aggregate_total: &AtomicUsize,
) -> Result<CappedOutput, ToolError> {
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        let aggregate = aggregate_total
            .fetch_add(read, Ordering::Relaxed)
            .saturating_add(read);
        if aggregate > total_limit {
            return Err(ToolError::OutputLimit { limit: total_limit });
        }
        let remaining = retained_limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(CappedOutput {
        bytes,
        truncated: total > retained_limit,
    })
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_tree(_command: &mut Command) {}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.arg("-lc").arg(command);
    shell
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.arg("/C").arg(command);
    shell
}
