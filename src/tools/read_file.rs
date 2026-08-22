use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    approval::Risk,
    model::ToolSpec,
    tool::{Tool, ToolError, ToolOutput},
    tools::resolve_existing,
};

const DEFAULT_MAX_BYTES: usize = 64 * 1024;

pub struct ReadFileTool {
    root: PathBuf,
    max_bytes: usize,
}

impl ReadFileTool {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_max_bytes(root, DEFAULT_MAX_BYTES)
    }

    pub fn with_max_bytes(root: impl Into<PathBuf>, max_bytes: usize) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root.into())?,
            max_bytes,
        })
    }
}

#[derive(Deserialize)]
struct ReadFileArguments {
    path: PathBuf,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Read a text file inside the workspace",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn risk(&self, arguments: &Value) -> Risk {
        let sensitive = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(Path::new)
            .is_some_and(|path| {
                is_sensitive_env_path(path) && resolve_existing(&self.root, path).is_ok()
            });
        if sensitive {
            Risk::SensitiveRead
        } else {
            Risk::ReadOnly
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let arguments: ReadFileArguments = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let target = resolve_existing(&self.root, &arguments.path)?;
        let mut file =
            File::open(target).map_err(|error| ToolError::Execution(error.to_string()))?;
        let read_limit = u64::try_from(self.max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.by_ref()
            .take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let truncated = bytes.len() > self.max_bytes;
        bytes.truncate(self.max_bytes);
        let mut contents = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            contents.push_str(&format!("\n[truncated after {} bytes]", self.max_bytes));
        }
        Ok(ToolOutput::new(contents))
    }
}

fn is_sensitive_env_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name != ".env.example" && (name.ends_with(".env") || name.contains(".env."))
}
