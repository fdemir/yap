use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::{approval::Risk, model::ToolSpec};

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn risk(&self, arguments: &Value) -> Risk;

    fn approval_preview(&self, _arguments: &Value) -> Result<Option<String>, ToolError> {
        Ok(None)
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    model_text: String,
}

impl ToolOutput {
    pub fn new(model_text: impl Into<String>) -> Self {
        Self {
            model_text: model_text.into(),
        }
    }

    pub fn into_model_text(self) -> String {
        self.model_text
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ToolError {
    #[error("tool path is outside workspace")]
    OutsideWorkspace,
    #[error("patch text must match exactly once")]
    PatchMismatch,
    #[error("tool execution timed out")]
    TimedOut,
    #[error("tool execution failed: {0}")]
    Execution(String),
}
