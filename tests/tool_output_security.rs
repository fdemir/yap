use yap::tool::ToolOutput;

#[test]
fn tool_output_redacts_common_secret_shapes() {
    let output = ToolOutput::new(
        "OPENAI_API_KEY=sk-abcdefghijklmnop\nAuthorization: Bearer ghp_abcdefghijklmnop\nhttps://user:password@example.com/path?token=topsecret\nok",
    )
    .into_model_text();

    assert!(!output.contains("sk-abcdefghijklmnop"));
    assert!(!output.contains("ghp_abcdefghijklmnop"));
    assert!(output.contains("OPENAI_API_KEY=[REDACTED]"));
    assert!(output.contains("Authorization: Bearer [REDACTED]"));
    assert!(!output.contains("password@example.com"));
    assert!(!output.contains("token=topsecret"));
    assert!(output.ends_with("ok"));
}

#[test]
fn tool_output_redacts_exact_secret_keys_and_expanded_token_formats() {
    let output = ToolOutput::new(
        "TOKEN=plain-secret DATABASE_URL=postgres://user:pass@host/db xoxb-1234567890123456",
    )
    .into_model_text();

    assert!(!output.contains("plain-secret"));
    assert!(!output.contains("postgres://user:pass"));
    assert!(!output.contains("xoxb-1234567890123456"));
}

#[test]
fn tool_output_redacts_structured_text_headers_and_signed_urls() {
    let output = ToolOutput::new(
        r#"{"token":"plain-token"}
Cookie: session=plain-cookie
https://user:plain-password@example.com/path?X-Amz-%43redential=plain-credential"#,
    )
    .into_model_text();

    for secret in [
        "plain-token",
        "plain-cookie",
        "user:plain-password",
        "plain-credential",
    ] {
        assert!(!output.contains(secret), "secret leaked: {secret}");
    }
}

#[test]
fn tool_output_does_not_treat_equality_checks_as_secret_assignments() {
    let output = ToolOutput::new("if password == candidate {").into_model_text();

    assert_eq!(output, "if password == candidate {");
}

#[test]
fn tool_output_is_bounded() {
    let output = ToolOutput::new("x".repeat(70 * 1024)).into_model_text();

    assert!(output.len() <= 64 * 1024);
    assert!(output.contains("tool output truncated"));
}
