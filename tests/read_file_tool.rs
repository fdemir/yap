use std::fs;

use serde_json::json;
use tempfile::tempdir;
use yap::{
    approval::Risk,
    tool::{Tool, ToolError},
    tools::ReadFileTool,
};

#[tokio::test]
async fn read_file_returns_workspace_file_contents() {
    let workspace = tempdir().expect("workspace should be created");
    fs::create_dir(workspace.path().join("src")).expect("directory should be created");
    fs::write(
        workspace.path().join("src/main.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .expect("file should be created");
    let tool = ReadFileTool::new(workspace.path()).expect("workspace should be valid");

    let output = tool
        .execute(json!({"path": "src/main.rs"}))
        .await
        .expect("read should succeed");

    assert_eq!(
        output.into_model_text(),
        "fn main() {\n    println!(\"hello\");\n}\n"
    );
}

#[test]
fn read_file_requests_approval_for_dotenv_files() {
    let workspace = tempdir().expect("workspace should be created");
    fs::create_dir(workspace.path().join("config")).expect("directory should be created");
    fs::write(workspace.path().join(".env"), "SECRET=value").expect("file should be created");
    fs::write(workspace.path().join("config/.env.local"), "SECRET=value")
        .expect("file should be created");
    let tool = ReadFileTool::new(workspace.path()).expect("workspace should be valid");

    assert_eq!(tool.risk(&json!({"path": ".env"})), Risk::SensitiveRead);
    assert_eq!(
        tool.risk(&json!({"path": "config/.env.local"})),
        Risk::SensitiveRead
    );
    assert_eq!(tool.risk(&json!({"path": ".env.example"})), Risk::ReadOnly);
}

#[tokio::test]
async fn read_file_truncates_content_at_its_byte_limit() {
    let workspace = tempdir().expect("workspace should be created");
    fs::write(workspace.path().join("large.txt"), "0123456789").expect("file should be created");
    let tool =
        ReadFileTool::with_max_bytes(workspace.path(), 8).expect("workspace should be valid");

    let output = tool
        .execute(json!({"path": "large.txt"}))
        .await
        .expect("read should succeed");

    assert_eq!(
        output.into_model_text(),
        "01234567\n[truncated after 8 bytes]"
    );
}

#[tokio::test]
async fn read_file_rejects_parent_traversal() {
    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should be created");
    fs::write(root.path().join("secret.txt"), "secret").expect("secret should be created");
    let tool = ReadFileTool::new(&workspace).expect("workspace should be valid");

    let error = tool
        .execute(json!({"path": "../secret.txt"}))
        .await
        .expect_err("parent traversal should be rejected");

    assert_eq!(error, ToolError::OutsideWorkspace);
}
