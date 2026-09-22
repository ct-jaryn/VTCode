#![no_main]

use libfuzzer_sys::fuzz_target;
use vtcode_core::command_safety::dangerous_commands::{command_might_be_dangerous, command_requires_approval};
use vtcode_core::command_safety::shell_parser::parse_shell_commands;

const MAX_TOKENS: usize = 8;
const MAX_INPUT_BYTES: usize = 128;

fn bounded_tokens(data: &[u8]) -> Vec<String> {
    let slice = if data.len() > MAX_INPUT_BYTES {
        &data[..MAX_INPUT_BYTES]
    } else {
        data
    };
    let text = String::from_utf8_lossy(slice);
    text.split_whitespace()
        .take(MAX_TOKENS)
        .map(|s| {
            let t: String = s.chars().take(32).collect();
            t
        })
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let command = bounded_tokens(data);
    let dangerous = command_might_be_dangerous(&command);
    let needs_approval = command_requires_approval(&command);
    std::hint::black_box(dangerous);
    std::hint::black_box(needs_approval);

    // Oracle: classification must be deterministic (assert, not debug_assert, so
    // the check fires in every fuzz build configuration).
    assert_eq!(dangerous, command_might_be_dangerous(&command), "non-deterministic danger verdict");
    assert_eq!(needs_approval, command_requires_approval(&command), "non-deterministic approval verdict");

    // Oracle: empty argv is never dangerous and never needs approval.
    if command.is_empty() {
        assert!(!dangerous, "empty argv must not be dangerous");
        assert!(!needs_approval, "empty argv must not need approval");
        return;
    }

    // Oracle: encoded PowerShell hides its payload from argv-level parsing, so
    // it must be both hard-blocked and approval-gated.
    if is_encoded_powershell_shape(&command) {
        assert!(dangerous, "encoded PowerShell must be dangerous: {command:?}");
        assert!(needs_approval, "encoded PowerShell must require approval: {command:?}");
    }

    // Oracle: `bash -c/-lc/-ilc script` nesting must be consistent with the
    // parsed sub-commands. A dangerous sub-command must taint the wrapper, and
    // an unparseable script must fail closed to dangerous.
    if let Some(script) = bash_inline_script(&command) {
        match parse_shell_commands(script) {
            Ok(sub_commands) => {
                let nested_dangerous = sub_commands.iter().any(|sub| command_might_be_dangerous(sub));
                if nested_dangerous {
                    assert!(dangerous, "wrapper hid a dangerous sub-command: {command:?} -> {sub_commands:?}");
                }
            }
            Err(_) => assert!(dangerous, "unparseable bash inline script must fail closed: {command:?}"),
        }
    }
});

fn base_name(executable: &str) -> &str {
    std::path::Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable)
}

fn is_encoded_powershell_shape(command: &[String]) -> bool {
    let Some(executable) = command.first() else {
        return false;
    };
    if !matches!(
        base_name(executable).to_ascii_lowercase().as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) {
        return false;
    }
    command.iter().skip(1).any(|argument| {
        matches!(argument.to_ascii_lowercase().as_str(), "-encodedcommand" | "-encoded" | "-enc" | "-e")
    })
}

fn bash_inline_script(command: &[String]) -> Option<&String> {
    if command.len() < 3 {
        return None;
    }
    if !matches!(base_name(&command[0]), "bash" | "sh" | "zsh") {
        return None;
    }
    // Mirrors the `-c | -lc | -ilc` set used by `command_might_be_dangerous`
    // and `command_requires_approval`, not the wider `-il | -ic` set accepted
    // by `parse_bash_lc_commands`.
    if !matches!(command[1].as_str(), "-c" | "-lc" | "-ilc") {
        return None;
    }
    command.get(2)
}
