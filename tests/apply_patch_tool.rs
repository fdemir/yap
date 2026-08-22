use std::fs;

use serde_json::json;
use tempfile::tempdir;
use yap::{
    approval::Risk,
    tool::{Tool, ToolError},
    tools::ApplyPatchTool,
};

#[tokio::test]
async fn apply_patch_replaces_one_exact_match_inside_the_workspace() {
    let workspace = tempdir().expect("workspace should be created");
    let path = workspace.path().join("src/main.rs");
    fs::create_dir(path.parent().unwrap()).expect("directory should be created");
    fs::write(&path, "fn main() {\n    old();\n}\n").expect("file should be created");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");
    let arguments = json!({
        "path": "src/main.rs",
        "old_text": "    old();",
        "new_text": "    new();"
    });

    assert_eq!(tool.risk(&arguments), Risk::Mutating);
    let output = tool
        .execute(arguments)
        .await
        .expect("exact replacement should succeed");

    assert_eq!(output.into_model_text(), "updated src/main.rs");
    assert_eq!(
        fs::read_to_string(path).expect("file should remain readable"),
        "fn main() {\n    new();\n}\n"
    );
}

#[test]
fn apply_patch_provides_a_diff_for_approval() {
    let workspace = tempdir().expect("workspace should be created");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");
    let arguments = json!({
        "path": "src/main.rs",
        "old_text": "    old();\n    old_two();",
        "new_text": "    new();\n    new_two();"
    });

    let preview = tool
        .approval_preview(&arguments)
        .expect("preview should be generated")
        .expect("patch should have a preview");

    assert_eq!(
        preview,
        "--- src/main.rs\n+++ src/main.rs\n@@ proposed replacement @@\n-    old();\n-    old_two();\n+    new();\n+    new_two();"
    );
}

#[tokio::test]
async fn apply_patch_rejects_an_ambiguous_match_without_writing() {
    let workspace = tempdir().expect("workspace should be created");
    let path = workspace.path().join("main.rs");
    let original = "old();\nold();\n";
    fs::write(&path, original).expect("file should be created");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "path": "main.rs",
            "old_text": "old();",
            "new_text": "new();"
        }))
        .await
        .expect_err("ambiguous match should be rejected");

    assert_eq!(error, ToolError::PatchMismatch);
    assert_eq!(
        fs::read_to_string(path).expect("file should remain readable"),
        original
    );
}

#[tokio::test]
async fn apply_patch_rejects_a_missing_exact_match_without_writing() {
    let workspace = tempdir().expect("workspace should be created");
    let path = workspace.path().join("main.rs");
    fs::write(&path, "fn main() {}\n").expect("file should be created");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "path": "main.rs",
            "old_text": "missing();",
            "new_text": "replacement();"
        }))
        .await
        .expect_err("missing match should be rejected");

    assert_eq!(error, ToolError::PatchMismatch);
    assert_eq!(
        fs::read_to_string(path).expect("file should remain readable"),
        "fn main() {}\n"
    );
}
