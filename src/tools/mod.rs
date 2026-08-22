use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::tool::ToolError;

mod apply_patch;
mod list_files;
mod read_file;
mod run_command;

pub use apply_patch::ApplyPatchTool;
pub use list_files::ListFilesTool;
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;

fn resolve_existing(root: &Path, relative: &Path) -> Result<PathBuf, ToolError> {
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ToolError::OutsideWorkspace);
    }

    let target = fs::canonicalize(root.join(relative))
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    if !target.starts_with(root) {
        return Err(ToolError::OutsideWorkspace);
    }
    Ok(target)
}
