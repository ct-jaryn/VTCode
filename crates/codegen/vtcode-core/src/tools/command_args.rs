//! Shared helpers for command-style tool arguments.

use std::path::Path;

use serde_json::Value;

use crate::tools::tool_intent::{command_session_action, command_session_action_in, command_session_action_is};

const INDEXED_COMMAND_TYPE_ERROR: &str = "command array must contain only strings";
const COMMAND_VALUE_TYPE_ERROR: &str = "command must be a string or array of strings";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteStdinDispatch {
    Write,
    Poll,
    Wait,
}

impl WriteStdinDispatch {
    #[must_use]
    pub(crate) const fn command_session_action(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Poll => "poll",
            Self::Wait => "wait",
        }
    }
}

pub(crate) fn write_stdin_dispatch(args: &Value) -> Result<WriteStdinDispatch, &'static str> {
    let payload = args.as_object().ok_or("write_stdin requires a JSON object")?;
    if payload
        .get("action")
        .and_then(Value::as_str)
        .is_some_and(|action| action.eq_ignore_ascii_case("wait"))
    {
        return Ok(WriteStdinDispatch::Wait);
    }
    let chars = payload
        .get("chars")
        .and_then(Value::as_str)
        .ok_or("write_stdin requires string chars")?;

    if chars.is_empty() {
        Ok(WriteStdinDispatch::Poll)
    } else {
        Ok(WriteStdinDispatch::Write)
    }
}

fn collect_indexed_command_parts(
    payload: &serde_json::Map<String, Value>,
    start_index: usize,
) -> Result<Vec<String>, &'static str> {
    let mut parts = Vec::new();
    let mut index = start_index;
    while let Some(value) = payload.get(&format!("command.{index}")) {
        let Some(part) = value.as_str() else {
            return Err(INDEXED_COMMAND_TYPE_ERROR);
        };
        parts.push(part.to_string());
        index += 1;
    }
    Ok(parts)
}

pub fn has_indexed_command_parts(args: &Value) -> bool {
    let Some(payload) = args.as_object() else {
        return false;
    };

    payload.contains_key("command.0") || payload.contains_key("command.1")
}

pub fn parse_indexed_command_parts(
    payload: &serde_json::Map<String, Value>,
) -> Result<Option<Vec<String>>, &'static str> {
    let zero_based = collect_indexed_command_parts(payload, 0)?;
    if !zero_based.is_empty() {
        return Ok(Some(zero_based));
    }

    let one_based = collect_indexed_command_parts(payload, 1)?;
    if one_based.is_empty() {
        Ok(None)
    } else {
        Ok(Some(one_based))
    }
}

pub fn normalize_indexed_command_args(args: &Value) -> Result<Option<Value>, &'static str> {
    let Some(payload) = args.as_object() else {
        return Ok(None);
    };
    if payload.get("command").is_some() {
        return Ok(None);
    }

    let Some(parts) = parse_indexed_command_parts(payload)? else {
        return Ok(None);
    };

    let mut normalized = payload.clone();
    normalized.insert("command".to_string(), Value::String(shell_words::join(parts.iter().map(String::as_str))));
    Ok(Some(Value::Object(normalized)))
}

pub fn normalized_command_value(args: &Value) -> Result<Option<Value>, &'static str> {
    if let Some(command) = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .or_else(|| args.get("raw_command"))
    {
        return Ok(Some(command.clone()));
    }

    Ok(normalize_indexed_command_args(args)?.and_then(|normalized| normalized.get("command").cloned()))
}

pub fn command_words(args: &Value) -> Result<Option<Vec<String>>, &'static str> {
    let Some(command) = normalized_command_value(args)? else {
        return Ok(None);
    };

    let mut parts = match command {
        Value::String(command) => shell_words::split(&command).map_err(|_e| COMMAND_VALUE_TYPE_ERROR)?,
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(ToOwned::to_owned).ok_or(COMMAND_VALUE_TYPE_ERROR))
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(COMMAND_VALUE_TYPE_ERROR),
    };

    if let Some(extra_args) = args.get("args").and_then(Value::as_array) {
        for value in extra_args {
            let Some(part) = value.as_str() else {
                return Err(COMMAND_VALUE_TYPE_ERROR);
            };
            parts.push(part.to_string());
        }
    }

    if parts.is_empty() { Ok(None) } else { Ok(Some(parts)) }
}

pub fn command_text(args: &Value) -> Result<Option<String>, &'static str> {
    let Some(parts) = command_words(args)? else {
        return Ok(None);
    };
    Ok(Some(shell_words::join(parts.iter().map(String::as_str))))
}

/// Ordered argument keys that may carry a shell command string for display.
///
/// Canonical display precedence shared by the binary's tool summaries so
/// headline and detail extraction can never drift. Safety classification must
/// keep using [`raw_command_text`]/[`command_words`], which re-parse via
/// `shell_words` and intentionally exclude the legacy display-only
/// `bash_command` key that core handlers never execute.
pub const DISPLAY_COMMAND_KEYS: &[&str] = &["command", "raw_command", "bash_command", "cmd"];

/// Display-only extraction of a shell command string plus the key it came from.
///
/// Unlike [`raw_command_text`], array commands are joined with plain spaces
/// (no quoting) because the result is rendered, never executed or re-parsed:
/// `["bash", "-lc", "ls -R"]` displays as `bash -lc ls -R`. The `command` key
/// trims before the emptiness check while other keys use raw emptiness,
/// preserving historical per-key behavior. Indexed `command.N` parts are
/// handled by [`command_words`]'s primary path, not here.
///
/// Returns `None` when no key carries a non-empty command.
pub fn extract_command_text_with_key(args: &Value) -> Option<(String, &'static str)> {
    if let Some(array) = args.get("command").and_then(Value::as_array) {
        let joined: String = array
            .iter()
            .filter_map(|value| value.as_str())
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some((joined, "command"));
        }
    }
    for &key in DISPLAY_COMMAND_KEYS {
        let Some(value) = args.get(key).and_then(Value::as_str) else {
            continue;
        };
        // The `command` key trims before the emptiness check; the others do not,
        // matching historical per-key behavior.
        let (text, ok) = if key == "command" {
            let trimmed = value.trim();
            (trimmed.to_string(), !trimmed.is_empty())
        } else {
            (value.to_string(), !value.is_empty())
        };
        if ok {
            return Some((text, key));
        }
    }
    None
}

fn has_nonempty_string_field(args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
}

pub fn interactive_input_text(args: &Value) -> Option<&str> {
    args.get("input")
        .and_then(Value::as_str)
        .or_else(|| args.get("chars").and_then(Value::as_str))
        .or_else(|| args.get("text").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

pub fn session_id_text_from_payload(payload: &serde_json::Map<String, Value>) -> Option<&str> {
    payload
        .get("session_id")
        .or_else(|| payload.get("s"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn session_id_text(args: &Value) -> Option<&str> {
    args.as_object().and_then(session_id_text_from_payload)
}

pub fn command_session_missing_required_args(args: &Value) -> Vec<&'static str> {
    if command_session_action(args).is_none() {
        return Vec::new();
    }

    let mut missing = Vec::new();
    if command_session_action_is(args, "run") {
        if command_text(args).ok().flatten().is_none() {
            missing.push("command");
        }
    } else if command_session_action_is(args, "write") {
        if session_id_text(args).is_none() {
            missing.push("session_id");
        }
        if interactive_input_text(args).is_none() {
            missing.push("input or chars or text");
        }
    } else if command_session_action_in(args, &["poll", "wait", "continue", "close"]) {
        if session_id_text(args).is_none() {
            missing.push("session_id");
        }
    } else if command_session_action_is(args, "inspect") {
        let has_session_id = session_id_text(args).is_some();
        let has_spool_path = has_nonempty_string_field(args, "spool_path");
        if !has_session_id && !has_spool_path {
            missing.push("session_id or spool_path");
        }
    } else if command_session_action_is(args, "code") {
        let has_code = has_nonempty_string_field(args, "code") || has_nonempty_string_field(args, "command");
        if !has_code {
            missing.push("code or command");
        }
    }

    missing
}

pub fn command_session_requires_command_safety(args: &Value) -> bool {
    command_session_action_is(args, "run")
}

pub fn working_dir_text_from_payload(payload: &serde_json::Map<String, Value>) -> Option<&str> {
    payload
        .get("working_dir")
        .or_else(|| payload.get("cwd"))
        .or_else(|| payload.get("workdir"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn working_dir_text(args: &Value) -> Option<&str> {
    args.as_object().and_then(working_dir_text_from_payload)
}

/// Extract the raw command string from command_session-style arguments without
/// splitting it into words. This is useful for checking shell syntax that
/// would be lost after `shell_words::split`, such as redirections and pipes.
pub fn raw_command_text(args: &Value) -> Option<String> {
    let payload = args.as_object()?;

    if let Some(command) = payload
        .get("command")
        .or_else(|| payload.get("cmd"))
        .or_else(|| payload.get("raw_command"))
    {
        match command {
            Value::String(text) => return Some(text.clone()),
            Value::Array(values) => {
                let parts: Option<Vec<&str>> = values.iter().map(|v| v.as_str()).collect();
                return parts.map(shell_words::join);
            }
            _ => return None,
        }
    }

    let indexed = parse_indexed_command_parts(payload).ok().flatten()?;
    Some(shell_words::join(indexed.iter().map(String::as_str)))
}

/// Returns whether a shell command contains syntax whose meaning depends on
/// shell expansion rather than the literal argument text.
///
/// Read-only classification and approval-family learning must only operate on
/// static command shapes. Parameter expansion, command substitution, brace
/// expansion, globbing, and unquoted backslash escapes can otherwise turn a
/// harmless-looking token into a different executable argument at runtime.
pub fn contains_dynamic_shell_syntax(command: &str) -> bool {
    crate::command_safety::shell_parser::contains_dynamic_shell_syntax(command)
}

/// Skip an optional `env` prefix and leading shell environment assignments.
///
/// The returned slice starts at the executable that will actually run. Keep
/// this helper shared by intent and activity classification so both paths
/// inspect the same command shape.
pub fn command_words_after_environment_prefix(words: &[String]) -> &[String] {
    let mut start = 0;
    loop {
        if words.get(start).is_some_and(|word| word == "env") {
            start += 1;
            loop {
                let Some(word) = words.get(start) else {
                    break;
                };
                if word == "--" {
                    start += 1;
                    break;
                }
                let is_split_string = word == "-S"
                    || word == "--split-string"
                    || word.starts_with("-S")
                    || word.starts_with("--split-string=");
                let consumes_next = matches!(word.as_str(), "-u" | "--unset" | "-C" | "--chdir");
                start += 1;
                if is_split_string {
                    return &words[words.len()..];
                }
                if consumes_next {
                    if words.get(start).is_some() {
                        start += 1;
                    }
                    continue;
                }
                if word.starts_with('-') || word.contains('=') {
                    continue;
                }
                start -= 1;
                break;
            }
            continue;
        }
        if words
            .get(start)
            .is_some_and(|word| !word.starts_with('-') && word.contains('='))
        {
            start += 1;
            continue;
        }
        break;
    }
    &words[start..]
}

/// Return whether a parsed command uses an option that can turn an otherwise
/// read-only inspection program into a writer or command launcher.
///
/// This is deliberately option-focused; the read-only executable/subcommand
/// allow-list remains in `tool_intent::readonly`. Keeping the unsafe option
/// rules here lets raw argument validation and activity classification share
/// the same token-aware guard without searching quoted text.
pub(crate) fn has_unsafe_readonly_options(words: &[String]) -> bool {
    let command_words = command_words_after_environment_prefix(words);
    let Some(program) = command_words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let program = program.to_ascii_lowercase();

    match program.as_str() {
        "git" => command_words.iter().skip(1).any(|word| {
            crate::command_safety::git_global_option_requires_prompt(word)
                || word == "--ext-diff"
                || word == "--textconv"
                || word == "-o"
                || word.starts_with("-o")
                || word == "--output"
                || word.starts_with("--output=")
        }),
        "sed" => has_unsafe_sed_options(&command_words[1..]),
        "find" => command_words.iter().skip(1).any(|word| {
            matches!(
                word.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls" | "-fprint" | "-fprint0" | "-fprintf"
            )
        }),
        "sort" => command_words.iter().skip(1).any(|word| {
            word == "-o"
                || word.starts_with("-o")
                || word == "--output"
                || word.starts_with("--output=")
                || word == "--compress-program"
                || word.starts_with("--compress-program=")
        }),
        "date" => command_words
            .iter()
            .skip(1)
            .any(|word| word == "-s" || word.starts_with("-s") || word == "--set" || word.starts_with("--set=")),
        "rg" => command_words.iter().skip(1).any(|word| {
            matches!(word.as_str(), "--pre" | "--hostname-bin" | "--search-zip" | "-z")
                || word.starts_with("--pre=")
                || word.starts_with("--hostname-bin=")
        }),
        "ast-grep" | "sg" => command_words
            .iter()
            .skip(1)
            .any(|word| word == "-r" || word == "--rewrite" || word.starts_with("--rewrite=")),
        "fd" => command_words
            .iter()
            .skip(1)
            .any(|word| word == "-x" || word == "--exec" || word == "--exec-batch" || word.starts_with("--exec=")),
        "tree" => command_words
            .iter()
            .skip(1)
            .any(|word| word == "-o" || word.starts_with("-o") || word == "--output" || word.starts_with("--output=")),
        // `awk` stays on the read-only allow-list, but its program text can
        // write (`print > file`), pipe into commands (`print | "cmd"`),
        // execute them (`system()`), call them indirectly (`@func()`), or
        // load code (`@include`, `@load`), and its options can edit in place
        // (`-i inplace`), load external code (`-l`), or write profiles
        // (`-p`). `has_unsafe_awk_options` fails closed on all of those; only
        // data-only options (`-v`, `-F`) and a write-free program pass.
        "awk" => has_unsafe_awk_options(&command_words[1..]),
        _ => false,
    }
}

fn has_unsafe_sed_options(arguments: &[String]) -> bool {
    let mut expects_expression = false;
    let mut expression_seen = false;

    for word in arguments {
        if expects_expression {
            if sed_script_may_write(word) {
                return true;
            }
            expects_expression = false;
            expression_seen = true;
            continue;
        }

        if word == "-e" || word == "--expression" {
            expects_expression = true;
            continue;
        }
        if let Some(expression) = word.strip_prefix("--expression=") {
            if sed_script_may_write(expression) {
                return true;
            }
            expression_seen = true;
            continue;
        }
        if let Some(expression) = word.strip_prefix("-e")
            && !expression.is_empty()
        {
            if sed_script_may_write(expression) {
                return true;
            }
            expression_seen = true;
            continue;
        }

        if word == "-f"
            || word.starts_with("-f")
            || word.starts_with("--file")
            || word == "-i"
            || word.starts_with("-i")
            || word == "-I"
            || word.starts_with("-I")
            || word.starts_with("--in-place")
        {
            return true;
        }

        if word == "--" {
            expects_expression = true;
            continue;
        }
        if !word.starts_with('-') && !expression_seen {
            if sed_script_may_write(word) {
                return true;
            }
            expression_seen = true;
        }
    }

    // A missing argument makes the command invalid. Treat it as unsafe here
    // so malformed input never receives a read-only classification.
    expects_expression || !expression_seen
}

fn sed_script_may_write(script: &str) -> bool {
    let chars = script.to_ascii_lowercase().chars().collect::<Vec<_>>();
    let mut index = 0;

    while index < chars.len() {
        while chars
            .get(index)
            .is_some_and(|character| character.is_whitespace() || *character == ';')
        {
            index += 1;
        }

        if let Some(after_address) = consume_sed_address(&chars, index) {
            index = after_address;
            if chars.get(index) == Some(&',') {
                index += 1;
                let Some(after_second_address) = consume_sed_address(&chars, index) else {
                    return true;
                };
                index = after_second_address;
            }
            if chars.get(index) == Some(&'!') {
                index += 1;
            }
            while chars.get(index).is_some_and(|character| character.is_whitespace()) {
                index += 1;
            }
        }

        let Some(&command) = chars.get(index) else {
            break;
        };
        match command {
            'e' | 'r' | 'w' => return true,
            's' => {
                let Some(after_replacement) = consume_sed_substitution(&chars, index) else {
                    return true;
                };
                if sed_substitution_may_write(&chars, after_replacement) {
                    return true;
                }
                index = after_replacement;
                while chars.get(index).is_some_and(|character| !matches!(*character, ';' | '\n')) {
                    index += 1;
                }
            }
            _ => {
                while chars.get(index).is_some_and(|character| !matches!(*character, ';' | '\n')) {
                    index += 1;
                }
            }
        }
    }

    false
}

fn consume_sed_address(chars: &[char], start: usize) -> Option<usize> {
    match chars.get(start)? {
        character if character.is_ascii_digit() => {
            let mut index = start + 1;
            while chars.get(index).is_some_and(|character| character.is_ascii_digit()) {
                index += 1;
            }
            Some(index)
        }
        '$' => Some(start + 1),
        '/' => find_unescaped_sed_delimiter(chars, start + 1, '/').map(|index| index + 1),
        _ => None,
    }
}

fn find_unescaped_sed_delimiter(chars: &[char], start: usize, delimiter: char) -> Option<usize> {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '\\' {
            index = index.saturating_add(2);
            continue;
        }
        if chars[index] == delimiter {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn consume_sed_substitution(chars: &[char], start: usize) -> Option<usize> {
    let delimiter = *chars.get(start + 1)?;
    if delimiter.is_whitespace() {
        return None;
    }
    let pattern_end = find_unescaped_sed_delimiter(chars, start + 2, delimiter)?;
    let replacement_end = find_unescaped_sed_delimiter(chars, pattern_end + 1, delimiter)?;
    Some(replacement_end + 1)
}

fn sed_substitution_may_write(chars: &[char], start: usize) -> bool {
    let mut index = start;
    while chars.get(index).is_some_and(|character| character.is_whitespace()) {
        index += 1;
    }
    matches!(chars.get(index), Some('e' | 'w'))
        && chars
            .get(index + 1)
            .is_none_or(|character| character.is_whitespace() || *character == ';')
}

/// Return whether an `awk` invocation uses an option or program shape that can
/// write workspace state or execute commands.
///
/// Only data-only options pass: `-v`/`--assign` (variable binding) and
/// `-F`/`--field-separator`, each either attached or consuming the next word.
/// Every other option fails closed — `-i`/`--include` edits in place,
/// `-f`/`--file`/`--source` load program text this scanner cannot see,
/// `-l`/`--load` loads native extensions, `-p`/`--profile`,
/// `-W`/`--dump-variables`, and `--pretty-print` write files, and the
/// remaining option surface is too wide to allow-list safely. `--` ends option
/// parsing. The first bare word is the program and must
/// be write-free per [`awk_program_may_write`]; later words are filenames. A
/// missing program is malformed, so it fails closed like the `sed` guard.
fn has_unsafe_awk_options(arguments: &[String]) -> bool {
    let mut program_seen = false;
    let mut options_ended = false;
    let mut index = 0;

    while index < arguments.len() {
        let word = arguments[index].as_str();
        if !options_ended && word == "--" {
            options_ended = true;
            index += 1;
            continue;
        }
        if !options_ended && word.starts_with('-') && word.len() > 1 {
            if word == "-v" || word == "--assign" || word == "-F" || word == "--field-separator" {
                // Value-taking option with a separate argument.
                index += 2;
                continue;
            }
            if word.starts_with("-v")
                || word.starts_with("--assign=")
                || word.starts_with("-F")
                || word.starts_with("--field-separator=")
            {
                // Attached value (`-F:`, `-vlimit=10`).
                index += 1;
                continue;
            }
            return true;
        }
        if !program_seen {
            program_seen = true;
            if awk_program_may_write(word) {
                return true;
            }
        }
        index += 1;
    }

    !program_seen
}

/// Return whether an `awk` program can write files, pipe into commands,
/// execute them, or load external code. `>` (unless the `>=` comparison)
/// and bare `|` (unless the `||` operator) are output redirection and
/// command pipes; `system()` runs shell commands; `@` invokes gawk indirect
/// calls (`@func()`) and directives (`@include`, `@load`), which can execute
/// or load arbitrary code — including a `system` name smuggled via `-v`.
/// String and regex literals are not distinguished from code: a literal
/// containing `>`, `|`, or `@` fails closed as a possible write instead
/// of risking a missed redirection. Bare `>` comparisons (`$3>100`) and `|`
/// alternations (`/a|b/`) therefore stay mutating by design.
fn awk_program_may_write(program: &str) -> bool {
    let chars = program.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '@' {
            return true;
        }
        if character == '>' {
            if chars.get(index + 1) == Some(&'=') {
                index += 2;
                continue;
            }
            return true;
        }
        if character == '|' {
            if chars.get(index + 1) == Some(&'|') {
                index += 2;
                continue;
            }
            return true;
        }
        if (character == 's' || character == 'S') && awk_calls_system(&chars, index) {
            return true;
        }
        index += 1;
    }
    false
}

/// Return whether `chars[start..]` invokes awk's `system()` builtin: the
/// identifier `system` (case-insensitive) on an identifier boundary followed
/// by optional whitespace and `(`.
fn awk_calls_system(chars: &[char], start: usize) -> bool {
    const NAME: &[char] = &['s', 'y', 's', 't', 'e', 'm'];
    if start > 0 && is_awk_ident_char(chars[start - 1]) {
        return false;
    }
    let candidate = chars.get(start..start + NAME.len());
    if candidate.is_none_or(|slice| {
        slice
            .iter()
            .zip(NAME.iter())
            .any(|(actual, expected)| !actual.eq_ignore_ascii_case(expected))
    }) {
        return false;
    }
    let mut index = start + NAME.len();
    while chars.get(index).is_some_and(|character| character.is_whitespace()) {
        index += 1;
    }
    chars.get(index) == Some(&'(')
}

fn is_awk_ident_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn has_unsafe_readonly_options_in_command(command: &str) -> bool {
    let Ok(commands) = crate::command_safety::shell_parser::parse_shell_commands_tree_sitter(command) else {
        return true;
    };
    if commands.is_empty() {
        return true;
    }

    commands.into_iter().any(|command| {
        let mut words = Vec::new();
        for word in command {
            let Ok(tokens) = shell_words::split(&word) else {
                return true;
            };
            words.extend(tokens);
        }
        has_unsafe_readonly_options(&words)
    })
}

/// Returns true if the raw command string appears to be a safe read-only
/// inspection command. It checks for shell write operators, process
/// substitutions, and common destructive subcommands/flags.
///
/// This is intentionally conservative: a command that does anything suspicious
/// is treated as mutating so it does not get silently cached or parallelized.
pub fn is_readonly_command_string(args: &Value) -> bool {
    let Some(command) = raw_command_text(args) else {
        return false;
    };
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }

    if contains_dynamic_shell_syntax(trimmed) {
        return false;
    }

    // Deny shell operators that produce side effects or hide them.
    // Matches: >, >>, >|, <(, >(, ;, ||, $() backticks handled below.
    // `&&` is allowed when every segment passes the same safety checks
    // (no redirections, substitutions, or destructive commands) — see
    // `is_readonly_chained_segments` in `readonly.rs` for the per-segment
    // allow-list verification. This lets plan-mode run harmless exploration
    // like `ls -la && echo '---' && ls -la crates/` (checkpoint turn_726:
    // the `&&` rejection blocked all chained read-only inspection in plan
    // mode, forcing the model to fall back to `request_user_input` which was
    // also denied, dead-ending the turn).
    // Pipelines are allowed here but validated segment-by-segment by the caller.
    // `2>&1` merely merges stderr into stdout (no file is written), so it is
    // stripped before scanning for write redirections — plan mode must not
    // reject `cargo check 2>&1 | head -c 4000` style inspection.
    let without_stderr_merge = trimmed.replace("2>&1", "");
    if without_stderr_merge.contains('>')
        || without_stderr_merge.contains("<(")
        || without_stderr_merge.contains(';')
        || without_stderr_merge.contains("||")
    {
        return false;
    }

    // Deny command substitution and process redirection that can run arbitrary code.
    // Check each `&&`-separated segment individually so a substitution hidden in
    // one segment is still caught.
    let chain_segments: Vec<&str> = trimmed.split("&&").map(str::trim).collect();
    for segment in &chain_segments {
        if segment.contains("$(") || segment.contains('`') || segment.contains("$((") {
            return false;
        }
    }

    // Deny common destructive commands outright, regardless of flags.
    // Check the full command (catches destructive commands in any segment) AND
    // each segment individually (catches segment-starting destructive commands).
    let lower = trimmed.to_ascii_lowercase();
    if has_unsafe_readonly_options_in_command(trimmed) {
        return false;
    }
    for destructive in [
        " rm ",
        "rm ",
        " shred ",
        "shred ",
        " truncate ",
        "truncate ",
        " tee ",
        "tee ",
        " mv ",
        "mv ",
        " cp ",
        "cp ",
        " install ",
        "install ",
        " chmod ",
        "chmod ",
        " chown ",
        "chown ",
        " chattr ",
        "chattr ",
        " mkfs ",
        "mkfs",
        " dd ",
        "dd ",
        " wipe ",
        "wipe ",
        " srm ",
        "srm ",
        " rm\t",
        "shred\t",
        "truncate\t",
        "tee\t",
        "mv\t",
        "cp\t",
    ] {
        if lower.contains(destructive) {
            return false;
        }
    }
    if lower.starts_with("rm ")
        || lower.starts_with("shred ")
        || lower.starts_with("truncate ")
        || lower.starts_with("tee ")
        || lower.starts_with("mv ")
        || lower.starts_with("cp ")
        || lower.starts_with("install ")
        || lower.starts_with("chmod ")
        || lower.starts_with("chown ")
        || lower.starts_with("chattr ")
        || lower.starts_with("mkfs")
        || lower.starts_with("dd ")
        || lower.starts_with("wipe ")
        || lower.starts_with("srm ")
    {
        return false;
    }

    // Also check each `&&`-separated segment for destructive commands that
    // start at the beginning of a segment (e.g. `ls -la && rm foo.txt`).
    for segment in &chain_segments {
        let seg_lower = segment.to_ascii_lowercase();
        if seg_lower.starts_with("rm ")
            || seg_lower.starts_with("shred ")
            || seg_lower.starts_with("truncate ")
            || seg_lower.starts_with("tee ")
            || seg_lower.starts_with("mv ")
            || seg_lower.starts_with("cp ")
            || seg_lower.starts_with("install ")
            || seg_lower.starts_with("chmod ")
            || seg_lower.starts_with("chown ")
            || seg_lower.starts_with("chattr ")
            || seg_lower.starts_with("mkfs")
            || seg_lower.starts_with("dd ")
            || seg_lower.starts_with("wipe ")
            || seg_lower.starts_with("srm ")
        {
            return false;
        }
    }

    // Deny in-place editing commands (sed -i/--in-place, perl -i, ruby -i) which modify files
    // despite being in the read-only allow-list.
    if lower.contains("perl ") && lower.contains(" -i") {
        return false;
    }
    if lower.contains("ruby ") && lower.contains(" -i") {
        return false;
    }

    // Deny destructive `find` flags before we allow `find` as read-only.
    if lower.contains("find ") {
        for destructive_flag in [
            " -delete",
            "-delete ",
            "\t-delete",
            " -exec rm",
            "-exec rm",
            " -exec shred",
            "-exec shred",
            " -exec chmod",
            "-exec chmod",
            " -exec chown",
            "-exec chown",
            " -exec truncate",
            "-exec truncate",
            " -exec tee",
            "-exec tee",
            " -exec mv",
            "-exec mv",
            " -exec cp",
            "-exec cp",
            " -exec install",
            "-exec install",
            " -execdd",
            " -exec bash",
            "-exec bash",
        ] {
            if lower.contains(destructive_flag) {
                return false;
            }
        }
    }

    true
}

pub fn normalize_shell_args(args: &Value) -> Result<Value, &'static str> {
    let mut normalized = match normalize_indexed_command_args(args)? {
        Some(value) => value,
        None => args.clone(),
    };

    let Some(payload) = normalized.as_object_mut() else {
        return Ok(normalized);
    };

    if payload.get("command").is_none() {
        if let Some(command) = payload.get("cmd").cloned() {
            payload.insert("command".to_string(), command);
        } else if let Some(command) = payload.get("raw_command").cloned() {
            payload.insert("command".to_string(), command);
        }
    }

    if payload.get("input").is_none() {
        if let Some(input) = payload.get("chars").cloned() {
            payload.insert("input".to_string(), input);
        } else if let Some(input) = payload.get("text").cloned() {
            payload.insert("input".to_string(), input);
        }
    }

    if payload.get("session_id").is_none()
        && let Some(session_id) = payload.get("s").cloned()
    {
        payload.insert("session_id".to_string(), session_id);
    }

    if payload.get("max_tokens").is_none()
        && let Some(max_output_tokens) = payload.get("max_output_tokens").cloned()
    {
        payload.insert("max_tokens".to_string(), max_output_tokens);
    }

    if payload.get("max_output_tokens").is_none()
        && let Some(max_tokens) = payload.get("max_tokens").cloned()
    {
        payload.insert("max_output_tokens".to_string(), max_tokens);
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::{
        WriteStdinDispatch, command_session_missing_required_args, command_session_requires_command_safety,
        command_text, command_words, contains_dynamic_shell_syntax, extract_command_text_with_key,
        has_indexed_command_parts, interactive_input_text, is_readonly_command_string, normalize_indexed_command_args,
        normalize_shell_args, normalized_command_value, parse_indexed_command_parts, raw_command_text, session_id_text,
        session_id_text_from_payload, working_dir_text, working_dir_text_from_payload, write_stdin_dispatch,
    };
    use serde_json::{Value, json};

    #[test]
    fn detects_indexed_command_keys() {
        assert!(has_indexed_command_parts(&json!({"command.0": "ls"})));
        assert!(has_indexed_command_parts(&json!({"command.1": "ls"})));
        assert!(!has_indexed_command_parts(&json!({"command.2": "ls"})));
    }

    #[test]
    fn parses_zero_based_indexed_command_parts() {
        let parts = parse_indexed_command_parts(
            json!({
                "command.0": "ls",
                "command.1": "-a"
            })
            .as_object()
            .expect("object"),
        )
        .expect("valid indexed args");

        assert_eq!(parts, Some(vec!["ls".to_string(), "-a".to_string()]));
    }

    #[test]
    fn parses_one_based_indexed_command_parts() {
        let parts = parse_indexed_command_parts(
            json!({
                "command.1": "ls",
                "command.2": "-a"
            })
            .as_object()
            .expect("object"),
        )
        .expect("valid indexed args");

        assert_eq!(parts, Some(vec!["ls".to_string(), "-a".to_string()]));
    }

    #[test]
    fn rejects_non_string_indexed_command_parts() {
        let error = parse_indexed_command_parts(
            json!({
                "command.0": 42
            })
            .as_object()
            .expect("object"),
        )
        .expect_err("non-string segment should fail");

        assert_eq!(error, "command array must contain only strings");
    }

    #[test]
    fn normalizes_indexed_command_args_into_command_string() {
        let normalized = normalize_indexed_command_args(&json!({
            "command.1": "ls",
            "command.2": "-a",
            "working_dir": "."
        }))
        .expect("valid indexed args")
        .expect("normalized payload");

        assert_eq!(normalized.get("command").and_then(Value::as_str), Some("ls -a"));
        assert_eq!(normalized.get("working_dir").and_then(Value::as_str), Some("."));
    }

    #[test]
    fn normalized_command_value_prefers_cmd_aliases() {
        let normalized = normalized_command_value(&json!({"cmd": "ls -a"}))
            .expect("valid command alias")
            .expect("command value");

        assert_eq!(normalized.as_str(), Some("ls -a"));
    }

    #[test]
    fn command_text_joins_command_arrays() {
        let command = command_text(&json!({"command": ["git", "status", "--short"]}))
            .expect("valid command")
            .expect("command text");

        assert_eq!(command, "git status --short");
    }

    #[test]
    fn command_words_append_extra_args() {
        let words = command_words(&json!({
            "command": "cargo test",
            "args": ["-p", "vtcode-core"]
        }))
        .expect("valid command")
        .expect("command words");

        assert_eq!(words, vec!["cargo", "test", "-p", "vtcode-core"]);
    }

    #[test]
    fn interactive_input_text_preserves_whitespace() {
        assert_eq!(interactive_input_text(&json!({"chars": "  echo hi\n"})), Some("  echo hi\n"));
    }

    #[test]
    fn write_stdin_dispatch_distinguishes_write_from_poll() {
        assert_eq!(write_stdin_dispatch(&json!({"chars": ""})), Ok(WriteStdinDispatch::Poll));
        assert_eq!(write_stdin_dispatch(&json!({"chars": "  status\n"})), Ok(WriteStdinDispatch::Write));
    }

    #[test]
    fn write_stdin_dispatch_accepts_explicit_wait_without_chars() {
        assert_eq!(
            write_stdin_dispatch(&json!({"action": "wait", "session_id": "run-1"})),
            Ok(WriteStdinDispatch::Wait)
        );
    }

    #[test]
    fn write_stdin_dispatch_requires_public_chars() {
        assert_eq!(write_stdin_dispatch(&json!({"input": "status\n"})), Err("write_stdin requires string chars"));
        assert_eq!(write_stdin_dispatch(&json!({"chars": 1})), Err("write_stdin requires string chars"));
    }

    #[test]
    fn session_id_text_trims_whitespace() {
        assert_eq!(session_id_text(&json!({"session_id": " run-1 "})), Some("run-1"));
    }

    #[test]
    fn session_id_text_accepts_compact_alias() {
        assert_eq!(session_id_text(&json!({"s": " run-1 "})), Some("run-1"));
    }

    #[test]
    fn session_id_text_from_payload_accepts_aliases() {
        let value = json!({"s": " run-1 "});
        let payload = value.as_object().expect("object");
        assert_eq!(session_id_text_from_payload(payload), Some("run-1"));
    }

    #[test]
    fn working_dir_text_accepts_aliases() {
        assert_eq!(working_dir_text(&json!({"workdir": " src "})), Some("src"));
        assert_eq!(working_dir_text(&json!({"cwd": "."})), Some("."));
    }

    #[test]
    fn working_dir_text_from_payload_accepts_aliases() {
        let value = json!({"workdir": " src "});
        let payload = value.as_object().expect("object");
        assert_eq!(working_dir_text_from_payload(payload), Some("src"));
    }

    #[test]
    fn normalize_shell_args_maps_codex_fields() {
        let normalized = normalize_shell_args(&json!({
            "cmd": "echo hi",
            "chars": "status\n"
        }))
        .expect("valid shell args");

        assert_eq!(normalized.get("command").and_then(Value::as_str), Some("echo hi"));
        assert_eq!(normalized.get("input").and_then(Value::as_str), Some("status\n"));
    }

    #[test]
    fn normalize_shell_args_maps_compact_session_id() {
        let normalized = normalize_shell_args(&json!({
            "s": "run-1"
        }))
        .expect("valid shell args");

        assert_eq!(normalized.get("session_id").and_then(Value::as_str), Some("run-1"));
    }

    #[test]
    fn normalize_shell_args_copies_max_output_tokens_to_max_tokens() {
        let normalized = normalize_shell_args(&json!({
            "command": "echo hi",
            "max_output_tokens": 42
        }))
        .expect("valid shell args");

        assert_eq!(normalized.get("max_output_tokens").and_then(Value::as_u64), Some(42));
        assert_eq!(normalized.get("max_tokens").and_then(Value::as_u64), Some(42));
    }

    #[test]
    fn normalize_shell_args_copies_max_tokens_to_max_output_tokens() {
        let normalized = normalize_shell_args(&json!({
            "command": "echo hi",
            "max_tokens": 42
        }))
        .expect("valid shell args");

        assert_eq!(normalized.get("max_tokens").and_then(Value::as_u64), Some(42));
        assert_eq!(normalized.get("max_output_tokens").and_then(Value::as_u64), Some(42));
    }

    #[test]
    fn command_session_missing_required_args_is_action_aware() {
        assert_eq!(command_session_missing_required_args(&json!({"action": "run"})), vec!["command"]);
        assert_eq!(
            command_session_missing_required_args(&json!({"action": "write", "session_id": "run-1"})),
            vec!["input or chars or text"]
        );
        assert_eq!(
            command_session_missing_required_args(&json!({"action": "inspect"})),
            vec!["session_id or spool_path"]
        );
        assert!(command_session_missing_required_args(&json!({"action": "list"})).is_empty());
    }

    #[test]
    fn command_session_requires_command_safety_only_for_run() {
        assert!(command_session_requires_command_safety(&json!({
            "action": "run",
            "command": "cargo check"
        })));
        assert!(!command_session_requires_command_safety(&json!({
            "action": "poll",
            "session_id": "run-1"
        })));
    }

    #[test]
    fn raw_command_text_extracts_command_string() {
        assert_eq!(raw_command_text(&json!({"command": "rg foo"})), Some("rg foo".to_string()));
        assert_eq!(raw_command_text(&json!({"cmd": "ls -la"})), Some("ls -la".to_string()));
        assert_eq!(
            raw_command_text(&json!({"command.0": "cat", "command.1": "file.txt"})),
            Some("cat file.txt".to_string())
        );
        assert_eq!(raw_command_text(&json!({"command": ["wc", "-l"]})), Some("wc -l".to_string()));
        assert!(raw_command_text(&json!({})).is_none());
    }

    #[test]
    fn display_extraction_reports_key_and_preserves_precedence() {
        assert_eq!(
            extract_command_text_with_key(&json!({"command": ["bash", "-lc", "ls -R"]})),
            Some(("bash -lc ls -R".to_string(), "command"))
        );
        assert_eq!(
            extract_command_text_with_key(&json!({"bash_command": "pwd"})),
            Some(("pwd".to_string(), "bash_command"))
        );
        assert_eq!(
            extract_command_text_with_key(&json!({"raw_command": "cargo test"})),
            Some(("cargo test".to_string(), "raw_command"))
        );
        // Missing `command` must not block later keys (the copilot
        // early-return bug this canonical helper fixes).
        assert_eq!(extract_command_text_with_key(&json!({"cmd": "ls -la"})), Some(("ls -la".to_string(), "cmd")));
        // `command` trims; other keys keep raw emptiness semantics.
        assert_eq!(extract_command_text_with_key(&json!({"command": "  "})), None);
        // First non-empty key in canonical order wins.
        assert_eq!(
            extract_command_text_with_key(
                &json!({"raw_command": "cargo check -p vtcode", "bash_command": "cargo test"})
            ),
            Some(("cargo check -p vtcode".to_string(), "raw_command"))
        );
        assert!(extract_command_text_with_key(&json!({})).is_none());
    }

    #[test]
    fn is_readonly_command_string_allows_inspection_commands() {
        for cmd in [
            "diff a.rs b.rs",
            "find . -type f -name '*.rs'",
            "wc -l src/main.rs",
            "grep -rn 'todo' src",
            "head -50 src/lib.rs",
            "sort src/words.txt | uniq",
        ] {
            assert!(is_readonly_command_string(&json!({"command": cmd})), "expected '{cmd}' to be read-only");
        }
    }

    #[test]
    fn is_readonly_command_string_allows_pipelines() {
        // Pipelines are allowed here; the caller is responsible for checking each
        // segment against an allow-list of safe commands.
        assert!(is_readonly_command_string(&json!({"command": "diff a b | wc -l"})));
        assert!(is_readonly_command_string(&json!({"command": "grep x src | sort | uniq"})));
    }

    #[test]
    fn is_readonly_command_string_allows_stderr_merge() {
        // `2>&1` merges streams without writing a file; real redirections stay rejected.
        assert!(is_readonly_command_string(&json!({"command": "cargo check 2>&1 | head -c 4000"})));
        assert!(!is_readonly_command_string(&json!({"command": "cargo check 2>&1 > out.txt"})));
        assert!(!is_readonly_command_string(&json!({"command": "ls 2> errors.txt"})));
    }

    #[test]
    fn is_readonly_command_string_allows_and_chains() {
        // `&&`-chained read-only commands are allowed when every segment is
        // safe (no redirections, substitutions, or destructive commands).
        // The per-segment allow-list check is handled by the caller via
        // `is_readonly_chained_segments` (checkpoint turn_726).
        assert!(is_readonly_command_string(&json!({"command": "ls -la && echo '---' && ls -la crates/"})));
        assert!(is_readonly_command_string(&json!({"command": "pwd && ls src/"})));
        assert!(is_readonly_command_string(&json!({"command": "true && cat foo.txt"})));
    }

    #[test]
    fn is_readonly_command_string_rejects_destructive_and_chains() {
        // Destructive commands in `&&` chains are still rejected.
        assert!(!is_readonly_command_string(&json!({"command": "ls -la && rm foo.txt"})));
        assert!(!is_readonly_command_string(&json!({"command": "true && mv a b"})));
        assert!(!is_readonly_command_string(&json!({"command": "cat x && cp a b"})));
        // Substitutions in any `&&` segment are still rejected.
        assert!(!is_readonly_command_string(&json!({"command": "ls && echo $(date)"})));
    }

    #[test]
    fn is_readonly_command_string_rejects_redirections_and_substitutions() {
        for cmd in [
            "cat a.txt > b.txt",
            "grep x src >> out.txt",
            "git diff --output=out.txt",
            "git diff '--output=out.txt'",
            "git diff -o out.txt",
            "git diff -oout.txt",
            "git log --output=out.txt",
            "git -c diff.external=sh diff",
            "git -C /external/repo=alt status",
            "git --git-dir=.evil-git diff",
            "git --exec-path=.evil-git diff",
            "find . -fprint output.txt",
            "find . -fprintf output.txt '%p'",
            "rg --hostname-bin sh pattern",
            "rg --search-zip pattern",
            "rg -z pattern",
            "sed --in-place= README.md",
            "sort -o out.txt README.md",
            "sort --compress-program=sh README.md",
            "date -s now",
            "awk -i inplace '{print}' README.md",
            "awk -v f=system 'BEGIN{@f(\"id\")}' README.md",
            "awk '@include \"x.awk\"' README.md",
            "sed -n 's/a/b/e' README.md",
            "fd --exec sh -c 'touch out'",
            "tree -o out.txt",
            "ast-grep -r 'README.md'",
            "sed -n -fmalicious.sed -e '1p' src/main.rs",
            "sed -I '' 's/a/b/' src/main.rs",
            "sed -n '1p\nw leaked.txt' src/main.rs",
            "echo $(date)",
            "echo `date`",
            "cat <(echo hi)",
            "cat >(echo hi)",
            "true && rm a.txt",
        ] {
            assert!(!is_readonly_command_string(&json!({"command": cmd})), "expected '{cmd}' to be rejected");
        }

        assert!(is_readonly_command_string(&json!({
            "command": "echo 'git diff --output=out.txt'"
        })));
    }

    #[test]
    fn is_readonly_command_string_rejects_destructive_commands() {
        for cmd in [
            "rm a.txt",
            "find . -type f -delete",
            "find . -name '*.tmp' -exec rm {} \\;",
            "shred a.txt",
            "mv a.txt b.txt",
        ] {
            assert!(!is_readonly_command_string(&json!({"command": cmd})), "expected '{cmd}' to be rejected");
        }
    }

    #[test]
    fn is_readonly_command_string_rejects_spliced_find_exec() {
        assert!(!is_readonly_command_string(&json!({
            "command": "find src -maxdepth 0 -exe$''c touch /tmp/VT_BYPASS_POC {} +"
        })));
    }

    #[test]
    fn dynamic_shell_syntax_allows_quoted_globs_only() {
        assert!(!contains_dynamic_shell_syntax("find src -name '*.rs'"));
        assert!(!contains_dynamic_shell_syntax("find src -name \"*.rs\""));
        assert!(contains_dynamic_shell_syntax("find src -exe$''c touch {} +"));
        assert!(contains_dynamic_shell_syntax("find src -ex{e,}c touch {} +"));
    }
}
