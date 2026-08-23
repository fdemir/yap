use std::fs;

use serde_json::json;
use tempfile::tempdir;
use yap::{
    tool::{Tool, ToolError},
    tools::ListFilesTool,
};

#[tokio::test]
async fn list_files_returns_sorted_workspace_entries() {
    let workspace = tempdir().expect("workspace should be created");
    fs::write(workspace.path().join("README.md"), "# project").expect("file should be created");
    fs::create_dir(workspace.path().join("src")).expect("directory should be created");
    fs::write(workspace.path().join("src/main.rs"), "fn main() {}")
        .expect("file should be created");
    let tool = ListFilesTool::new(workspace.path()).expect("workspace should be valid");

    let output = tool
        .execute(json!({"path": "."}))
        .await
        .expect("listing should succeed");

    assert_eq!(output.into_model_text(), "README.md\nsrc/");
}

#[tokio::test]
async fn list_files_limits_large_directories() {
    let workspace = tempdir().expect("workspace should be created");
    for index in 0..2_001 {
        fs::write(workspace.path().join(format!("{index:04}.txt")), "")
            .expect("file should be created");
    }
    let tool = ListFilesTool::new(workspace.path()).expect("workspace should be valid");

    let output = tool
        .execute(json!({"path": "."}))
        .await
        .expect("listing should succeed")
        .into_model_text();

    assert!(output.contains("[listing truncated after 2000 entries]"));
    assert_eq!(output.lines().count(), 2_001);
}

#[tokio::test]
#[cfg(unix)]
async fn list_files_rejects_a_symlink_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    let private = root.path().join("private");
    fs::create_dir(&workspace).expect("workspace should be created");
    fs::create_dir(&private).expect("private directory should be created");
    fs::write(private.join("secret.txt"), "secret").expect("secret should be created");
    symlink(&private, workspace.join("escape")).expect("symlink should be created");
    let tool = ListFilesTool::new(&workspace).expect("workspace should be valid");

    let error = tool
        .execute(json!({"path": "escape"}))
        .await
        .expect_err("symlink escape should be rejected");

    assert_eq!(error, ToolError::OutsideWorkspace);
}

#[tokio::test]
async fn list_files_rejects_parent_traversal() {
    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    let private = root.path().join("private");
    fs::create_dir(&workspace).expect("workspace should be created");
    fs::create_dir(&private).expect("private directory should be created");
    fs::write(private.join("secret.txt"), "secret").expect("secret should be created");
    let tool = ListFilesTool::new(&workspace).expect("workspace should be valid");

    let error = tool
        .execute(json!({"path": "../private"}))
        .await
        .expect_err("parent traversal should be rejected");

    assert_eq!(error, ToolError::OutsideWorkspace);
}
