// Re-export from shared utils to break the tool_policy <-> tools cycle.
pub use crate::utils::tool_name_parsing::canonical_tool_name;

/// Returns true when a bare shell token names the `apply_patch` tool
/// (including the `applypatch` alias), with or without a path prefix.
fn is_apply_patch_tool_name(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let base = base.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    matches!(base, "apply_patch" | "applypatch")
}

fn first_shell_token_preserving_backslashes(command: &str) -> &str {
    let command = command.trim_start();
    let mut active_quote = None;
    let mut escaped = false;

    for (index, character) in command.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && active_quote != Some('\'') {
            escaped = true;
            continue;
        }

        if let Some(quote) = active_quote {
            if character == quote {
                active_quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' | '`' => active_quote = Some(character),
            character if character.is_whitespace() => return &command[..index],
            _ => {}
        }
    }

    command
}

/// Returns true when a full shell command line tries to run the
/// `apply_patch` tool as a shell binary (e.g. `apply_patch`, or
/// `/usr/bin/applypatch --help`). Only the first token is considered, so
/// `sudo apply_patch` is not a collision.
pub fn is_apply_patch_shell_collision_command(command: &str) -> bool {
    // Preserve Windows backslashes before the POSIX-oriented shell tokenizer
    // can interpret them as escape characters.
    let raw_first_token = first_shell_token_preserving_backslashes(command).trim();
    if is_apply_patch_tool_name(raw_first_token) {
        return true;
    }

    if let Ok(parts) = shell_words::split(command)
        && let Some(first) = parts.into_iter().next()
        && !first.trim().is_empty()
    {
        return is_apply_patch_tool_name(&first);
    }
    // Fallback for unbalanced quoting, where `shell_words` fails: whitespace
    // split with surrounding-quote trimming still identifies the tool name.
    let first_token = first_shell_token_preserving_backslashes(command).trim();
    is_apply_patch_tool_name(first_token)
}

#[test]
fn test_canonical_tool_name_passes_through() {
    // With registration-based aliases, this function now just passes through
    // Alias resolution happens earlier in the inventory layer
    assert_eq!(canonical_tool_name("list_files"), "list_files");

    assert_eq!(canonical_tool_name("unknown_tool"), "unknown_tool");

    assert_eq!(canonical_tool_name("container.exec"), "container.exec");
}

#[test]
fn test_is_apply_patch_shell_collision_command() {
    assert!(is_apply_patch_shell_collision_command("apply_patch"));
    assert!(is_apply_patch_shell_collision_command("applypatch"));
    assert!(is_apply_patch_shell_collision_command("apply_patch --help"));
    assert!(is_apply_patch_shell_collision_command("/usr/bin/apply_patch"));
    assert!(is_apply_patch_shell_collision_command("./applypatch foo"));
    assert!(is_apply_patch_shell_collision_command(r"C:\tools\apply_patch --help"));
    assert!(is_apply_patch_shell_collision_command(r#""C:\Program Files\apply_patch" --help"#));
    assert!(is_apply_patch_shell_collision_command(r#""C:\Program Files"\apply_patch --help"#));
    assert!(is_apply_patch_shell_collision_command("'apply_patch'"));
    assert!(is_apply_patch_shell_collision_command("apply_patch \"unclosed"));
    assert!(!is_apply_patch_shell_collision_command("'apply_patch'foo"));
    assert!(!is_apply_patch_shell_collision_command("sudo apply_patch"));
    assert!(!is_apply_patch_shell_collision_command("pip install foo"));
    assert!(!is_apply_patch_shell_collision_command(""));
    assert!(!is_apply_patch_shell_collision_command("APPLY_PATCH"));
}
