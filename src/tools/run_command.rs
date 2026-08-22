use std::{io, path::PathBuf, process::Stdio, time::Duration};

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
    tool::{Tool, ToolError, ToolOutput},
    tools::command_policy,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct RunCommandTool {
    root: PathBuf,
    timeout: Duration,
    max_output_bytes: usize,
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
        Ok(Self {
            root: std::fs::canonicalize(root.into())?,
            timeout,
            max_output_bytes,
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
        let mut command = shell_command(&arguments.command);
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("command stdout was not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("command stderr was not captured".into()))?;
        let run = async {
            tokio::try_join!(
                child.wait(),
                read_capped(stdout, self.max_output_bytes),
                read_capped(stderr, self.max_output_bytes),
            )
        };
        let (status, stdout, stderr) = tokio::time::timeout(self.timeout, run)
            .await
            .map_err(|_| ToolError::TimedOut)?
            .map_err(|error| ToolError::Execution(error.to_string()))?;
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

async fn read_capped(mut reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<CappedOutput> {
    let mut bytes = Vec::new();
    let mut total = 0usize;
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read);
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(CappedOutput {
        bytes,
        truncated: total > limit,
    })
}

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
