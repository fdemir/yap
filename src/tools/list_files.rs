use std::{io, path::PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    approval::Risk,
    model::ToolSpec,
    tool::{Tool, ToolError, ToolOutput},
    workspace::Workspace,
};

const DEFAULT_MAX_ENTRIES: usize = 2_000;

pub struct ListFilesTool {
    workspace: Workspace,
}

impl ListFilesTool {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            workspace: Workspace::open(root)?,
        })
    }
}

#[derive(Deserialize)]
struct ListFilesArguments {
    path: PathBuf,
}

#[async_trait]
impl Tool for ListFilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_files",
            "List entries in a workspace directory",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn risk(&self, _arguments: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let arguments: ListFilesArguments = serde_json::from_value(arguments)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let (entries, truncated) = self
            .workspace
            .read_dir_bounded(&arguments.path, DEFAULT_MAX_ENTRIES)?;
        let mut names = entries
            .into_iter()
            .map(|entry| {
                if entry.is_dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name
                }
            })
            .collect::<Vec<_>>();
        names.sort();
        if truncated {
            names.push(format!(
                "[listing truncated after {DEFAULT_MAX_ENTRIES} entries]"
            ));
        }

        Ok(ToolOutput::new(names.join("\n")))
    }
}
