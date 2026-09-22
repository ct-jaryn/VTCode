#![no_main]

use libfuzzer_sys::fuzz_target;
use vtcode_core::command_safety::shell_parser::{
    parse_bash_lc_commands, parse_shell_commands, parse_shell_commands_tree_sitter,
};

const MAX_INPUT_BYTES: usize = 2048;
const MAX_TOKENS: usize = 64;

fn bounded_input(data: &[u8]) -> String {
    let slice = if data.len() > MAX_INPUT_BYTES {
        &data[..MAX_INPUT_BYTES]
    } else {
        data
    };
    String::from_utf8_lossy(slice).into_owned()
}

fn touch_commands(commands: &[Vec<String>]) {
    for command in commands {
        for token in command {
            std::hint::black_box(token);
        }
    }
}

fn tokenized_invocation(script: &str) -> Vec<String> {
    script.split_whitespace().take(MAX_TOKENS).map(ToString::to_string).collect()
}

fuzz_target!(|data: &[u8]| {
    let script = bounded_input(data);

    let lenient = parse_shell_commands(&script);
    let strict = parse_shell_commands_tree_sitter(&script);

    if let Ok(ref commands) = lenient {
        touch_commands(commands);
    }
    if let Ok(ref commands) = strict {
        touch_commands(commands);
    }

    // Oracle (matklad `regex` vs `regex_lite`): the lenient facade returns the
    // tree-sitter result verbatim whenever it is non-empty, so a successful
    // strict parse must agree exactly with the lenient result. An empty strict
    // result falls through to basic tokenization, which is excluded here.
    if let Ok(strict_commands) = &strict {
        if !strict_commands.is_empty() {
            match &lenient {
                Ok(lenient_commands) => assert_eq!(
                    lenient_commands, strict_commands,
                    "lenient/strict shell parser divergence for script: {script:?}"
                ),
                Err(_) => panic!("lenient parser failed while strict succeeded for script: {script:?}"),
            }
        }
    }

    let bash_lc = vec!["bash".to_string(), "-lc".to_string(), script.clone()];
    let bash_lc_result = parse_bash_lc_commands(&bash_lc);
    // Oracle: `parse_bash_lc_commands(["bash", "-lc", script])` delegates to
    // `parse_shell_commands(script)` and maps `Ok -> Some`, `Err -> None`.
    match &lenient {
        Ok(commands) => match &bash_lc_result {
            Some(wrapped) => {
                assert_eq!(wrapped, commands, "bash -lc wrapper diverged from shell parser for script: {script:?}")
            }
            None => panic!("bash -lc wrapper returned None while shell parser succeeded: {script:?}"),
        },
        Err(_) => assert!(bash_lc_result.is_none(), "bash -lc wrapper succeeded while shell parser failed: {script:?}"),
    }
    if let Some(ref commands) = bash_lc_result {
        touch_commands(commands);
    }

    let raw_tokens = tokenized_invocation(&script);
    if let Some(commands) = parse_bash_lc_commands(&raw_tokens) {
        touch_commands(&commands);
    }
});
