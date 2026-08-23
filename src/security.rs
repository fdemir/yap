use std::{borrow::Cow, sync::LazyLock};

use regex::Regex;
use serde_json::Value;

pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_APPROVAL_PREVIEW_BYTES: usize = 64 * 1024;

static ASSIGNMENT_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)(\b(?:[A-Z][A-Z0-9_]*_)?(?:API_KEY|APIKEY|ACCESS_KEY|SECRET_KEY|TOKEN|SECRET|PASSWORD|PASSWD|PWD|PRIVATE_KEY|DATABASE_URL)\b\s*[:=]\s*)(?:["'][^"'\r\n]*["']|[^=\s,;][^\s,;]*)"#,
    )
    .expect("secret assignment regex should compile")
});
static AUTHORIZATION_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)(authorization\s*:\s*(?:bearer|basic)\s+)[^\s]+")
        .expect("authorization regex should compile")
});
static TOKEN_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:sk-[a-z0-9_-]{12,}|sk_(?:live|test)_[a-z0-9_-]{8,}|xox[bp]-[a-z0-9-]{12,}|gh[pousr]_[a-z0-9]{12,}|github_pat_[a-z0-9_]{12,}|(?:AKIA|ASIA|AIDA|AGPA|AROA|ANPA)[0-9A-Z]{16})\b",
    )
    .expect("token regex should compile")
});
static CREDENTIAL_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://)[^/\s@]+@").expect("credential URL regex should compile")
});
static COOKIE_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)((?:set-)?cookie\s*:\s*)[^\r\n]+")
        .expect("cookie header regex should compile")
});
static JSON_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)(["'](?:[A-Z][A-Z0-9_]*_)?(?:API_KEY|APIKEY|ACCESS_KEY|SECRET_KEY|TOKEN|SECRET|PASSWORD|PASSWD|PWD|PRIVATE_KEY|DATABASE_URL)["']\s*:\s*)(?:["'][^"'\r\n]*["']|[^,\s}\]]+)"#,
    )
    .expect("JSON secret regex should compile")
});
static SENSITIVE_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([?&](?:api[_-]?key|token|access_token|key|secret|password|passwd|authorization|cookie|sig|signature|credential|access[_-]?key|private[_-]?key|x-amz-(?:credential|%43redential|signature|security-token))=)[^&\s]+")
        .expect("sensitive query regex should compile")
});
static PRIVATE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
        .expect("private key regex should compile")
});

pub(crate) fn redact_secrets(input: &str) -> String {
    let redacted = ASSIGNMENT_SECRET.replace_all(input, "${1}[REDACTED]");
    let redacted = AUTHORIZATION_SECRET.replace_all(&redacted, "${1}[REDACTED]");
    let redacted = TOKEN_SECRET.replace_all(&redacted, "[REDACTED]");
    let redacted = CREDENTIAL_URL.replace_all(&redacted, "${1}[REDACTED]@");
    let redacted = COOKIE_SECRET.replace_all(&redacted, "${1}[REDACTED]");
    let redacted = JSON_SECRET.replace_all(&redacted, "${1}[REDACTED]");
    let redacted = SENSITIVE_QUERY.replace_all(&redacted, "${1}[REDACTED]");
    PRIVATE_KEY
        .replace_all(&redacted, "[REDACTED PRIVATE KEY]")
        .into_owned()
}

pub(crate) fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(value) => Value::String(redact_secrets(value)),
        Value::Array(values) => Value::Array(values.iter().map(redact_json).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        Value::String("[REDACTED]".into())
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

pub(crate) fn bounded_redacted(input: &str, max_bytes: usize, label: &str) -> String {
    let redacted = redact_secrets(input);
    truncate_text(Cow::Owned(redacted), max_bytes, label)
}

pub(crate) fn truncate_text(input: Cow<'_, str>, max_bytes: usize, label: &str) -> String {
    if input.len() <= max_bytes {
        return input.into_owned();
    }
    let marker = format!("\n[{label} truncated at {max_bytes} bytes]");
    if marker.len() >= max_bytes {
        return truncate_at_boundary(&marker, max_bytes).to_owned();
    }
    let keep = max_bytes - marker.len();
    let prefix = truncate_at_boundary(&input, keep);
    format!("{prefix}{marker}")
}

pub(crate) fn checked_append(target: &mut String, fragment: &str, max_bytes: usize) -> bool {
    if target.len().saturating_add(fragment.len()) > max_bytes {
        return false;
    }
    target.push_str(fragment);
    true
}

pub(crate) fn terminal_safe_text(input: &str) -> Cow<'_, str> {
    if !input.chars().any(is_unsafe_terminal_character) {
        return Cow::Borrowed(input);
    }

    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        if is_unsafe_terminal_character(character) {
            let codepoint = u32::from(character);
            if codepoint <= 0xff {
                output.push_str(&format!("\\x{codepoint:02x}"));
            } else {
                output.push_str(&format!("\\u{{{codepoint:04x}}}"));
            }
        } else {
            output.push(character);
        }
    }
    Cow::Owned(output)
}

fn is_unsafe_terminal_character(character: char) -> bool {
    let codepoint = u32::from(character);
    (codepoint < 0x20 && character != '\n')
        || codepoint == 0x7f
        || (0x80..=0x9f).contains(&codepoint)
        || (0x200b..=0x200f).contains(&codepoint)
        || (0x2028..=0x202e).contains(&codepoint)
        || (0x2060..=0x206f).contains(&codepoint)
        || codepoint == 0xfeff
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "API_KEY",
        "ACCESS_KEY",
        "SECRET_KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PWD",
        "PRIVATE_KEY",
        "DATABASE_URL",
    ]
    .iter()
    .any(|suffix| key == *suffix || key.ends_with(&format!("_{suffix}")))
}

fn truncate_at_boundary(input: &str, max_bytes: usize) -> &str {
    let mut end = input.len().min(max_bytes);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn terminal_text_escapes_control_and_direction_override_characters() {
        assert_eq!(
            terminal_safe_text("ok\u{1b}[31m\u{202e}txt"),
            "ok\\x1b[31m\\u{202e}txt"
        );
        assert_eq!(
            terminal_safe_text("line one\nline two"),
            "line one\nline two"
        );
    }

    #[test]
    fn structured_secret_fields_are_redacted_without_redacting_unrelated_keys() {
        let redacted = redact_json(&json!({
            "password": "plain-secret",
            "nested": {"api_token": "plain-token"},
            "monkey": "banana"
        }));

        assert_eq!(redacted["password"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_token"], "[REDACTED]");
        assert_eq!(redacted["monkey"], "banana");
    }
}
