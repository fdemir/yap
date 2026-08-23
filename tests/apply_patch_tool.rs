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

    assert_eq!(tool.risk(&arguments), Risk::WorkspaceWrite);
    let output = tool
        .execute(arguments)
        .await
        .expect("exact replacement should succeed");

    assert_eq!(output.into_model_text(), "updated src/main.rs");
    assert_eq!(
        fs::read_to_string(path).expect("file should remain readable"),
        "fn main() {\n    new();\n}\n"
    );
    assert!(
        fs::read_dir(workspace.path().join("src"))
            .expect("directory should remain readable")
            .all(|entry| !entry
                .expect("entry should be readable")
                .file_name()
                .to_string_lossy()
                .contains(".yap-")),
        "atomic patch temp files should be cleaned up"
    );
}

#[tokio::test]
#[cfg(unix)]
async fn apply_patch_preserves_file_permissions() {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let workspace = tempdir().expect("workspace should be created");
    let path = workspace.path().join("script.sh");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o750)
        .open(&path)
        .expect("script should be created");
    fs::write(&path, "old\n").expect("script should be writable");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");

    tool.execute(json!({
        "path": "script.sh",
        "old_text": "old",
        "new_text": "new"
    }))
    .await
    .expect("patch should succeed");

    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o750
    );
}

#[tokio::test]
#[cfg(unix)]
async fn apply_patch_rejects_a_symlink_to_an_external_file() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root should be created");
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should be created");
    let external = root.path().join("external.txt");
    fs::write(&external, "old\n").expect("external file should be created");
    symlink(&external, workspace.join("link.txt")).expect("symlink should be created");
    let tool = ApplyPatchTool::new(&workspace).expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "path": "link.txt",
            "old_text": "old",
            "new_text": "new"
        }))
        .await
        .expect_err("external symlink should be rejected");

    assert_eq!(error, ToolError::OutsideWorkspace);
    assert_eq!(fs::read_to_string(external).unwrap(), "old\n");
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
async fn apply_patch_rejects_oversized_patch_text() {
    let workspace = tempdir().expect("workspace should be created");
    fs::write(workspace.path().join("main.rs"), "old").expect("file should be created");
    let tool = ApplyPatchTool::new(workspace.path()).expect("workspace should be valid");

    let error = tool
        .execute(json!({
            "path": "main.rs",
            "old_text": "x".repeat(256 * 1024 + 1),
            "new_text": "new"
        }))
        .await
        .expect_err("oversized patch should be rejected");

    assert_eq!(
        error,
        ToolError::InputTooLarge {
            field: "old_text",
            limit: 256 * 1024,
        }
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
