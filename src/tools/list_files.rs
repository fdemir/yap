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

pub struct ListFilesTool {
    root: PathBuf,
}

impl ListFilesTool {
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root.into())?,
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
        let target = resolve_existing(&self.root, &arguments.path)?;
        let entries =
            fs::read_dir(target).map_err(|error| ToolError::Execution(error.to_string()))?;
        let mut names = entries
            .map(|entry| {
                let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry
                    .file_type()
                    .map_err(|error| ToolError::Execution(error.to_string()))?
                    .is_dir()
                {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        names.sort();

        Ok(ToolOutput::new(names.join("\n")))
    }
}
