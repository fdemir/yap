use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub(super) fn accesses_outside(workspace: &Path, command: &str) -> bool {
    split_commands(command).into_iter().any(|command| {
        let Ok(tokens) = shell_words::split(&command) else {
            return true;
        };
        let Some(command_index) = tokens.iter().position(|token| !is_assignment(token)) else {
            return false;
        };
        if !inspects_paths(&tokens[command_index]) {
            return false;
        }
        tokens[command_index + 1..]
            .iter()
            .flat_map(|token| path_candidates(token))
            .any(|candidate| is_outside(workspace, candidate))
    })
}

fn is_assignment(token: &str) -> bool {
    token
        .split_once('=')
        .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'))
}

fn inspects_paths(command: &str) -> bool {
    matches!(
        command.to_ascii_lowercase().as_str(),
        "cd" | "chdir"
            | "popd"
            | "pushd"
            | "push-location"
            | "set-location"
            | "rm"
            | "cp"
            | "mv"
            | "mkdir"
            | "touch"
            | "chmod"
            | "chown"
            | "cat"
            | "get-content"
            | "set-content"
            | "add-content"
            | "copy-item"
            | "move-item"
            | "remove-item"
            | "new-item"
            | "rename-item"
    )
}

fn split_commands(command: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                current.push(character);
                if character == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                current.push(character);
                if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quote = None;
                }
            }
            _ => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    current.push(character);
                }
                '\\' => {
                    escaped = true;
                    current.push(character);
                }
                ';' | '\n' | '|' | '&' | '(' | ')' | '`' => {
                    if !current.trim().is_empty() {
                        commands.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(character),
            },
        }
    }
    if !current.trim().is_empty() {
        commands.push(current);
    }
    commands
}

fn path_candidates(token: &str) -> impl Iterator<Item = &str> {
    token
        .split(['<', '>'])
        .flat_map(|part| {
            part.rsplit_once('=')
                .map_or([part, ""], |(_, value)| [part, value])
        })
        .map(|part| part.trim_matches(['(', ')', '{', '}', ',', '\'', '"']))
        .filter(|part| !part.is_empty() && !part.starts_with('-'))
}

fn is_outside(workspace: &Path, candidate: &str) -> bool {
    if candidate.contains("://") {
        return false;
    }

    let expanded = if candidate == "~" {
        std::env::var_os("HOME").map(PathBuf::from)
    } else if let Some(rest) = candidate.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
    } else if let Some(rest) = candidate
        .strip_prefix("$HOME/")
        .or_else(|| candidate.strip_prefix("${HOME}/"))
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
    } else if let Some(rest) = candidate
        .strip_prefix("$PWD/")
        .or_else(|| candidate.strip_prefix("${PWD}/"))
    {
        Some(workspace.join(rest))
    } else if candidate.starts_with('~') {
        return true;
    } else if candidate.contains(['$', '`']) {
        return false;
    } else {
        let path = Path::new(candidate_prefix(candidate));
        Some(if path.is_absolute() {
            path.to_owned()
        } else {
            workspace.join(path)
        })
    };

    expanded.is_some_and(|path| !canonical_or_lexical(&path).starts_with(workspace))
}

fn candidate_prefix(candidate: &str) -> &str {
    candidate
        .find(['*', '?', '['])
        .map_or(candidate, |index| &candidate[..index])
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    let normalized = normalize_lexically(path);
    let mut ancestor = normalized.as_path();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return normalized;
        };
        ancestor = parent;
    }
    let Ok(canonical) = fs::canonicalize(ancestor) else {
        return normalized;
    };
    let suffix = normalized.strip_prefix(ancestor).unwrap_or(Path::new(""));
    normalize_lexically(&canonical.join(suffix))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}
