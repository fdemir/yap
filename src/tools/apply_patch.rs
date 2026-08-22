use std::{fs, io, path::PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    approval::Risk,
    model::ToolSpec,
    tool::{Tool, ToolError, ToolOutput},
    tools::resolve_existing,
};

pub struct ApplyPatchTool {
    root: PathBuf,
}

impl ApplyPatchTool {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root.into())?,
        })
    }
}

#[derive(Deserialize)]
struct ApplyPatchArguments {
    path: PathBuf,
    old_text: String,
    new_text: String,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "apply_patch",
            "Replace one exact text match in a workspace file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        )
    }

    fn risk(&self, _arguments: &Value) -> Risk {
        Risk::WorkspaceWrite
    }

    fn approval_preview(&self, arguments: &Value) -> Result<Option<String>, ToolError> {
        let arguments: ApplyPatchArguments = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut lines = vec![
            format!("--- {}", arguments.path.display()),
            format!("+++ {}", arguments.path.display()),
            "@@ proposed replacement @@".into(),
        ];
        lines.extend(arguments.old_text.lines().map(|line| format!("-{line}")));
        lines.extend(arguments.new_text.lines().map(|line| format!("+{line}")));
        Ok(Some(lines.join("\n")))
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let arguments: ApplyPatchArguments = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let target = resolve_existing(&self.root, &arguments.path)?;
        let contents =
            fs::read_to_string(&target).map_err(|error| ToolError::Execution(error.to_string()))?;
        let match_count = contents.match_indices(&arguments.old_text).take(2).count();
        if arguments.old_text.is_empty() || match_count != 1 {
            return Err(ToolError::PatchMismatch);
        }
        let updated = contents.replacen(&arguments.old_text, &arguments.new_text, 1);
        fs::write(target, updated).map_err(|error| ToolError::Execution(error.to_string()))?;

        Ok(ToolOutput::new(format!(
            "updated {}",
            arguments.path.display()
        )))
    }
}
