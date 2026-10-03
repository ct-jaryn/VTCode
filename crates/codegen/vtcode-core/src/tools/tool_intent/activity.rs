//! Shell activity classification for progress accounting and output previews.
//!
//! Mutation safety remains owned by [`super::classify_tool_intent`]. This
//! module adds the narrower distinction between repository inspection and
//! verification without duplicating that safety decision in binary consumers.

use std::path::Path;

use serde_json::Value;
use vtcode_commons::validation::NonEmptySlice;

use super::readonly::{
    command_words_are_readonly, static_shell_command_words, static_shell_command_words_with_output_plumbing,
};

/// Progress semantics for a command invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellActivity {
    /// Read-only repository or environment inspection.
    Inspection,
    /// A build, test, lint, or compile command that verifies work.
    Verification,
    /// A command that may mutate state and is not primarily verification.
    Mutation,
}

fn make_like_verification_targets(words: &[String]) -> bool {
    let mut saw_target = false;
    for word in words.iter().skip(1) {
        let lower = word.to_ascii_lowercase();
        if lower.starts_with('-') || lower.contains('=') {
            continue;
        }
        if !matches!(lower.as_str(), "test" | "tests" | "check" | "checks" | "lint" | "verify" | "validate") {
            return false;
        }
        saw_target = true;
    }
    saw_target
}

fn is_verification_invocation(words: &[String]) -> bool {
    let command_words = crate::tools::command_args::command_words_after_environment_prefix(words);
    let first = command_words.first().map(String::as_str).unwrap_or_default();
    let second = command_words.get(1).map(|word| word.to_ascii_lowercase());
    let third = command_words.get(2).map(|word| word.to_ascii_lowercase());
    let program = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase();

    match program.as_str() {
        "markdownlint-cli2" => command_words.get(1..).is_some_and(markdownlint_arguments_verify),
        "npx" => {
            let package_index = command_words
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, word)| !matches!(word.as_str(), "--yes" | "-y" | "--no-install"))
                .map(|(index, _)| index);
            package_index.is_some_and(|index| {
                let Some(package) = command_words.get(index) else {
                    return false;
                };
                let is_markdownlint = package == "markdownlint-cli2"
                    || package.strip_prefix("markdownlint-cli2@").is_some_and(|version| {
                        version.split('.').count() == 3
                            && version
                                .split('.')
                                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                    });
                is_markdownlint && command_words.get(index + 1..).is_some_and(markdownlint_arguments_verify)
            })
        }
        "cargo" => {
            if second.as_deref() == Some("fmt") {
                // `cargo fmt` without `--check` reformats the worktree (mutation).
                // With `--check` it is a read-only lint verification.
                return words.iter().any(|word| word == "--check");
            }
            matches!(second.as_deref(), Some("check" | "build" | "clippy" | "test"))
                || (second.as_deref() == Some("nextest") && third.as_deref() == Some("run"))
        }
        "go" => matches!(second.as_deref(), Some("test" | "build")),
        "npm" | "pnpm" | "yarn" => {
            matches!(second.as_deref(), Some("test" | "build" | "lint" | "check"))
                || (second.as_deref() == Some("run")
                    && matches!(third.as_deref(), Some("test" | "build" | "lint" | "check")))
        }
        "bun" | "bunx" => {
            matches!(second.as_deref(), Some("test"))
                || (second.as_deref() == Some("run")
                    && matches!(third.as_deref(), Some("test" | "build" | "lint" | "check")))
        }
        "deno" => matches!(second.as_deref(), Some("test" | "lint" | "check")),
        "make" | "gmake" | "just" => make_like_verification_targets(command_words),
        "uv" => {
            command_words.iter().any(|word| word == "pytest")
                || (command_words.iter().any(|word| word == "ruff") && command_words.iter().any(|word| word == "check"))
        }
        "ruff" => {
            if second.as_deref() == Some("format") {
                return words.iter().any(|word| word == "--check");
            }
            matches!(second.as_deref(), Some("check" | "lint"))
        }
        "tsc" => {
            // Bare `tsc` emits output (mutation). Only `--noEmit` type-checks.
            words.iter().any(|word| word == "--noEmit")
        }
        "eslint" => {
            // `eslint --fix` rewrites the worktree (mutation).
            !words.iter().any(|word| word == "--fix" || word == "--fix-dry-run")
        }
        "rustc" | "pytest" | "xcodebuild" | "gradle" | "gradlew" => true,
        "python" | "python3" => {
            (command_words.len() == 2
                && command_words
                    .get(1)
                    .is_some_and(|script| Path::new(script).ends_with("scripts/check_markdown.py")))
                || (command_words.iter().any(|word| word == "-m") && command_words.iter().any(|word| word == "pytest"))
        }
        _ if first.ends_with("/scripts/check.sh") || first.ends_with("/scripts/check-dev.sh") => true,
        _ => false,
    }
}

fn markdownlint_arguments_verify(arguments: &[String]) -> bool {
    !arguments.is_empty()
        && !arguments.iter().any(|argument| {
            matches!(argument.as_str(), "--fix" | "-f" | "--help" | "-h" | "--version" | "-v")
                || argument.starts_with("--fix=")
        })
}

fn contains_verification_invocation(command: &str) -> bool {
    static_shell_command_words(command)
        .is_some_and(|commands| commands.iter().any(|words| is_verification_invocation(words)))
}

/// Whether a command inspects search results that normally produce output.
///
/// Used only for progress accounting: an empty read-only search pipeline can
/// exit successfully when its final filter masks the search's no-match status.
/// Quiet/existence probes intentionally return no text and are excluded. This
/// predicate grants no execution permissions and never verifies mutations.
pub fn shell_command_is_output_search(args: &Value) -> bool {
    let Some(command) = crate::tools::command_args::raw_command_text(args) else {
        return false;
    };
    let Some(segments) = static_shell_command_words(&command) else {
        return false;
    };
    let mut saw_search = false;
    for words in &segments {
        if !command_words_are_readonly(words) || is_verification_invocation(words) {
            return false;
        }
        let words = crate::tools::command_args::command_words_after_environment_prefix(words);
        if command_words_are_grep_search(words) {
            if grep_words_request_quiet_output(words) {
                return false;
            }
            saw_search = true;
        }
    }
    saw_search
}

fn grep_words_request_quiet_output(words: &[String]) -> bool {
    let is_rg = words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .is_some_and(|name| name == "rg");
    let mut arguments = words.iter().skip(1);
    let mut quiet = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--" => break,
            "--quiet" | "--silent" => quiet = true,
            "--no-quiet" if is_rg => quiet = false,
            _ if argument.starts_with("--") => {
                // Values can look like options, including patterns such as `-query`.
                let takes_value = matches!(
                    argument.as_str(),
                    "--regexp"
                        | "--file"
                        | "--after-context"
                        | "--before-context"
                        | "--context"
                        | "--max-count"
                        | "--include"
                        | "--exclude"
                        | "--exclude-from"
                        | "--exclude-dir"
                        | "--label"
                        | "--directories"
                        | "--devices"
                ) || (is_rg
                    && matches!(
                        argument.as_str(),
                        "--glob"
                            | "--iglob"
                            | "--ignore-file"
                            | "--pre-glob"
                            | "--type"
                            | "--type-not"
                            | "--type-add"
                            | "--type-clear"
                            | "--replace"
                            | "--encoding"
                            | "--engine"
                            | "--color"
                            | "--colors"
                            | "--threads"
                            | "--max-depth"
                            | "--max-columns"
                            | "--max-filesize"
                            | "--dfa-size-limit"
                            | "--regex-size-limit"
                            | "--context-separator"
                            | "--field-context-separator"
                            | "--field-match-separator"
                            | "--hyperlink-format"
                            | "--path-separator"
                            | "--sort"
                            | "--sortr"
                            | "--generate"
                    ));
                if takes_value {
                    arguments.next();
                }
            }
            _ if argument.starts_with('-') => {
                let mut flags = argument.chars().skip(1).peekable();
                while let Some(flag) = flags.next() {
                    if flag == 'q' {
                        quiet = true;
                    }
                    let takes_value = matches!(flag, 'e' | 'f' | 'A' | 'B' | 'C' | 'm' | 'd')
                        || (is_rg && matches!(flag, 'g' | 't' | 'T' | 'r' | 'E' | 'j' | 'M'))
                        || (!is_rg && flag == 'D');
                    if takes_value {
                        if flags.peek().is_none() {
                            arguments.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    quiet
}

/// Whether exit code 1 belongs to a standalone grep-style search.
/// Compound commands, shell wrappers and redirected diagnostics cannot prove
/// the search's no-match status, even when their visible output is empty.
pub fn shell_command_is_standalone_grep_search(args: &Value) -> bool {
    let Some(command) = crate::tools::command_args::raw_command_text(args) else {
        return false;
    };
    let Some(segments) = static_shell_command_words(&command) else {
        return false;
    };
    let [words] = segments.as_slice() else {
        return false;
    };
    shell_words::split(&command).is_ok_and(|raw_words| raw_words == *words)
        && command_words_are_readonly(words)
        && command_words_are_grep_search(words)
}

fn command_words_are_grep_search(words: &[String]) -> bool {
    let words = crate::tools::command_args::command_words_after_environment_prefix(words);
    let program = words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|name| name.to_str());
    matches!(program, Some("rg" | "grep" | "egrep" | "fgrep"))
}

/// Detect a project-appropriate default verifier for autonomous recovery.
///
/// Inspects well-known project markers under `workspace_root` and returns a
/// concrete standalone verification command the agent can run via
/// `exec_command` (no pipes, no `;`/`||` joins). Priority follows the
/// existing `is_verification_invocation` coverage: Rust → Go → Deno/Bun →
/// Node → Python → Make/Just. Returns `None` when no marker is found so
/// callers can fall back to the generic verifier examples.
///
/// This is intentionally synchronous and allocation-light (existence checks
/// plus one small `package.json` read): it runs on the turn hot path when
/// the anti-blind-editing gate needs an actionable recovery directive.
/// Codex-style harness-managed verification (Stop-hook test gates,
/// auto-review) shows that naming the exact command — rather than listing
/// examples — is what unblocks long-running autonomous work.
pub fn default_verifier_for_workspace(workspace_root: &Path) -> Option<String> {
    if workspace_root.join("Cargo.toml").is_file() {
        return Some("cargo check --locked".to_string());
    }
    if workspace_root.join("go.mod").is_file() {
        return Some("go test ./...".to_string());
    }
    if workspace_root.join("deno.json").is_file() || workspace_root.join("deno.jsonc").is_file() {
        return Some("deno test".to_string());
    }
    if workspace_root.join("bun.lockb").is_file() || workspace_root.join("bun.lock").is_file() {
        return Some("bun test".to_string());
    }
    let package_json = workspace_root.join("package.json");
    if package_json.is_file() {
        if let Ok(content) = std::fs::read_to_string(&package_json)
            && let Ok(parsed) = serde_json::from_str::<Value>(&content)
            && let Some(scripts) = parsed.get("scripts").and_then(Value::as_object)
        {
            if scripts.contains_key("test") {
                return Some("npm test".to_string());
            }
            if scripts.contains_key("check") {
                return Some("npm run check".to_string());
            }
            if scripts.contains_key("lint") {
                return Some("npm run lint".to_string());
            }
            if scripts.contains_key("build") {
                return Some("npm run build".to_string());
            }
        }
        return Some("npm test".to_string());
    }
    for marker in ["pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini"] {
        if workspace_root.join(marker).is_file() {
            return Some("pytest -q".to_string());
        }
    }
    if workspace_root.join("Makefile").is_file() || workspace_root.join("makefile").is_file() {
        return Some("make test".to_string());
    }
    if workspace_root.join("justfile").is_file() || workspace_root.join("Justfile").is_file() {
        return Some("just test".to_string());
    }
    // No recognised project marker; callers fall back to generic examples.
    None
}

/// Which shell forms of a verifier clear the anti-blind-editing gate. Shared
/// by the recovery directive and the blocked-mutation `next_action` so the
/// two surfaces cannot drift.
pub const VERIFIER_SHELL_FORM_NOTE: &str = "Cap output with `max_output_tokens`. A verifier piped only into `head` or `tail` runs without \
the truncator and counts as standalone; filtering pipes (`| grep`), `;`, and `||` make the exit status another command's, \
so they do not clear the gate.";

/// Stand-in for the verifier when no project command was detected or
/// configured. It lists examples across ecosystems instead of naming one
/// command, because a single concrete fallback (such as a Cargo command in a
/// Go or Node workspace) would direct the model to a verifier that does not
/// exist.
pub const GENERIC_VERIFIER_DESCRIPTION: &str =
    "your project's build/test/lint command (e.g. `cargo check --locked`, `go test ./...`, `npm test`, or `pytest -q`)";

/// How harness text names the verifier to run: the resolved command in
/// backticks, or [`GENERIC_VERIFIER_DESCRIPTION`] when none was resolved.
/// `default_verifier` should come from [`default_verifier_for_workspace`] or
/// the harness override resolution built on it.
pub fn verifier_reference(default_verifier: Option<&str>) -> String {
    match default_verifier.map(str::trim).filter(|command| !command.is_empty()) {
        Some(command) => format!("`{command}`"),
        None => GENERIC_VERIFIER_DESCRIPTION.to_string(),
    }
}

/// Build the actionable verification-recovery directive with a concrete
/// command. `default_verifier` should come from
/// [`default_verifier_for_workspace`]; when `None`, the generic examples are
/// kept so the directive never names a command that does not exist.
pub fn verification_recovery_directive(default_verifier: Option<&str>, attempt: u8, max_attempts: u8) -> String {
    let verifier = verifier_reference(default_verifier);
    format!(
        "Verification recovery ({attempt}/{max_attempts}): pending edits have not been verified, so further mutations are blocked \
        and the turn ends blocked unless a verifier exits 0. Run {verifier} with `exec_command`, standalone or as a pure `&&` chain of verifiers. \
        {VERIFIER_SHELL_FORM_NOTE} A failed verifier grants a bounded number of fix-up edits before the next verification is required."
    )
}

/// Return whether a shell tool call is an admitted truncation-only verification
/// attempt while the anti-blind-editing gate is pending.
///
/// Piped verifiers (e.g. `cargo check 2>&1 | head -c 4000`) must be allowed to
/// run so the model can see the failure; otherwise the generic "cap output
/// with `| head`" guidance deadlocks on `Mutation blocked until verification`.
/// Only a standalone successful verifier clears the gate; this helper only
/// decides admission, never clearance.
///
/// Fail-closed smuggling guard: every parsed shell segment must be a
/// verification invocation or an allow-listed readonly command. A chained
/// mutation such as `cargo check && rm -rf target` therefore stays blocked
/// instead of riding through on the verifier prefix. Unparseable (dynamic)
/// shell syntax also stays blocked.
pub fn shell_command_is_admitted_verification_attempt(args: &Value) -> bool {
    let Some(command) = crate::tools::command_args::raw_command_text(args) else {
        return false;
    };
    if crate::tools::command_args::contains_dynamic_shell_syntax(&command) {
        return false;
    }
    let segments =
        static_shell_command_words(&command).or_else(|| static_shell_command_words_with_output_plumbing(&command));
    let Some(segments) = segments else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    let mut saw_verification = false;
    for words in &segments {
        if is_verification_invocation(words) {
            saw_verification = true;
        } else if !command_words_are_readonly(words) {
            return false;
        }
    }
    saw_verification
}

fn has_logical_sequencing(words: &[String]) -> bool {
    words.iter().any(|word| matches!(word.as_str(), "&&" | "||" | ";"))
}

/// Split a command on top-level `|` operators, tracking single/double quotes
/// and backslash escapes with the same discipline as
/// [`shell_uses_only_and_chaining`]. Returns the pipe-separated stages in
/// order, or `None` when no top-level pipe exists. `||` is not special-cased:
/// it yields an empty stage, which stage validation rejects — fail-closed
/// without a second operator table to keep in sync.
fn split_top_level_pipes(command: &str) -> Option<Vec<&str>> {
    if !command.contains('|') {
        return None;
    }
    let mut stages = Vec::new();
    let mut start = 0;
    let mut saw_pipe = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = command.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        // Outside single quotes a backslash escapes the next character for the
        // shell, so `\|` is a literal pipe, not a stage separator. Consume the
        // pair to stay aligned with the shell (mirrors the `&&`-chain scanner).
        if character == '\\' && !in_single_quote {
            chars.next();
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if in_single_quote || in_double_quote {
            continue;
        }
        if character == '|' {
            saw_pipe = true;
            stages.push(&command[start..index]);
            start = index + character.len_utf8();
        }
    }
    if !saw_pipe {
        return None;
    }
    stages.push(&command[start..]);
    Some(stages)
}

/// Whether a pipeline tail stage is a pure output truncator: a bare `head` or
/// `tail` invocation (any flags) with no shell operators of its own. Anything
/// else (`grep`, `wc`, `sort`, redirects, backgrounding, chaining) keeps its
/// pipeline semantics and must not be elided — dropping it would discard work
/// the caller asked for or change what the exit status means.
fn is_pure_truncation_stage(stage: &str) -> bool {
    let trimmed = stage.trim();
    if trimmed.is_empty() {
        return false;
    }
    // A truncator stage is one simple command: reject any operator that would
    // indicate chaining, backgrounding, redirection, or nesting. The scan is
    // quote-aware so quoted flag values (e.g. `--sep=';'`) do not false-reject;
    // an unaware scan could only over-reject, which stays fail-closed.
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\\' && !in_single_quote {
            chars.next();
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if in_single_quote || in_double_quote {
            continue;
        }
        if matches!(character, ';' | '&' | '<' | '>' | '\n' | '|') {
            return false;
        }
    }
    let words = match shell_words::split(trimmed) {
        Ok(words) => words,
        Err(_) => return false,
    };
    let Some(parsed) = NonEmptySlice::from_slice(&words) else {
        return false;
    };
    let program = Path::new(parsed.first())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(parsed.first().as_str())
        .to_ascii_lowercase();
    matches!(program.as_str(), "head" | "tail")
}

/// Rewrite a truncation-only piped verifier (`cargo check 2>&1 | head -c 4000`)
/// to its standalone verifier prefix so the observed exit status is the
/// verifier's, not the truncator's.
///
/// A pipeline's status belongs to its tail: running `cargo check | tail`
/// reports `tail`'s success even when the build fails, and the model reads
/// that exit 0 as "verified" while the anti-blind gate (correctly) stays
/// pending — a deadlock manufactured by shell exit-status semantics, observed
/// in real sessions. Eliding the truncator (output stays capped via
/// `max_output_tokens`) makes the status truthful in one round trip.
///
/// Fail-closed gates, all required:
/// - the full command is an admitted verification attempt (no dynamic
///   syntax; every segment independently verification-or-readonly), so shapes
///   like `cargo check | tail; rm …` can never reach the rewrite;
/// - the head classifies as [`ShellActivity::Verification`] on its own, so
///   `;`/`||`/background joins and smuggled mutations behind a verifier
///   prefix are rejected (only standalone verifiers and pure `&&` verifier
///   chains pass, whose `&&` exit status is truthful; output redirects such
///   as `> build.log` are preserved verbatim in the head and do not mask the
///   status, so they rewrite safely);
/// - every tail stage is a pure `is_pure_truncation_stage` truncator, so
///   `grep`/`wc`/`sort` tails (whose filtering is real work) are preserved.
///
/// Returns the standalone verifier text, or `None` when the command must run
/// (or be rejected) exactly as typed. Only commands whose raw text carries a
/// top-level `|` are candidates; array and indexed spellings re-quote
/// operators when joined, so they never present a top-level pipe and keep
/// today's behavior.
pub fn rewrite_truncation_only_verifier(args: &Value) -> Option<String> {
    use crate::config::constants::tools as tool_names;

    let command = crate::tools::command_args::raw_command_text(args)?;
    if !shell_command_is_admitted_verification_attempt(args) {
        return None;
    }
    let stages = split_top_level_pipes(&command)?;
    let (head, tails) = stages.split_first()?;
    let head = head.trim();
    if head.is_empty() || tails.iter().any(|stage| !is_pure_truncation_stage(stage)) {
        return None;
    }
    let head_args = serde_json::json!({"cmd": head});
    if !matches!(classify_shell_activity(tool_names::EXEC_COMMAND, &head_args), ShellActivity::Verification) {
        return None;
    }
    Some(head.to_string())
}

/// Shell-call arguments as the execution kernel runs them.
///
/// The kernel applies [`rewrite_truncation_only_verifier`] to every
/// command-run call before execution, so the process that runs (and whose
/// exit status the outcome reports) is the standalone verifier, not the typed
/// pipeline. Gate bookkeeping must classify that same command; classifying the
/// typed pipeline would call a truthful `cargo check 2>&1 | tail -5` success a
/// mutation and leave the gate pending. Arguments the kernel runs unchanged
/// (already-normalized arguments included, since the rewrite is idempotent)
/// are borrowed as-is.
pub fn shell_args_as_executed<'a>(tool_name: &str, args: &'a Value) -> std::borrow::Cow<'a, Value> {
    if !super::is_command_run_tool_call(tool_name, args) {
        return std::borrow::Cow::Borrowed(args);
    }
    let Some(rewritten) = rewrite_truncation_only_verifier(args) else {
        return std::borrow::Cow::Borrowed(args);
    };
    let mut executed = args.clone();
    match executed.as_object_mut() {
        Some(payload) => {
            payload.insert("command".to_string(), Value::String(rewritten));
            std::borrow::Cow::Owned(executed)
        }
        None => std::borrow::Cow::Borrowed(args),
    }
}

fn is_known_inspection(words: &[String]) -> bool {
    if words
        .iter()
        .any(|word| matches!(word.as_str(), ">" | ">>" | "|" | "&&" | ";" | "||"))
    {
        return false;
    }
    command_words_are_readonly(words)
}

fn classify_provable_shell_sequence(command: &str) -> Option<ShellActivity> {
    let (segments, has_output_plumbing) = if let Some(segments) = static_shell_command_words(command) {
        (segments, false)
    } else {
        (static_shell_command_words_with_output_plumbing(command)?, true)
    };
    if has_output_plumbing && segments.len() != 1 {
        return None;
    }
    let has_multiple_segments = segments.len() > 1;
    let mut saw_verification = false;

    for words in segments {
        if is_verification_invocation(&words) {
            saw_verification = true;
        } else if !command_words_are_readonly(&words) {
            return None;
        }
    }

    if has_output_plumbing && !saw_verification {
        return None;
    }

    // Shell execution does not guarantee that a pipeline or logical chain's
    // final status reflects every verification stage. Do not let a successful
    // downstream command clear the anti-blind checkpoint after an earlier
    // verifier failed.
    //
    // Exception: a pure `&&` chain of verification (or readonly) segments
    // short-circuits on first failure, so its exit status does represent every
    // stage. `cargo fmt --check && cargo check --locked && cargo nextest run`
    // must clear the gate; `;`, `||`, `|`, and background `&` still mask
    // failures and stay `Mutation`.
    if saw_verification && has_multiple_segments {
        if shell_uses_only_and_chaining(command) {
            return Some(ShellActivity::Verification);
        }
        return Some(ShellActivity::Mutation);
    }

    Some(if saw_verification {
        ShellActivity::Verification
    } else {
        ShellActivity::Inspection
    })
}

fn has_shell_sequence(command: &str) -> bool {
    static_shell_command_words(command).is_none_or(|segments| segments.len() > 1)
}

/// Quote-aware check that a shell command chains segments only with `&&`.
///
/// Returns `false` when an unquoted `;`, newline, `||`, single `&`
/// (background), or `|` (pipeline) is present, since those operators let a
/// downstream success mask an earlier verifier failure. `&&` short-circuits,
/// so a pure `&&` chain of verifiers has a faithful aggregate exit status and
/// may clear the anti-blind-editing gate. Backslash escapes outside single
/// quotes are honored, so a shell-literal `\"` cannot open a phantom quote
/// state that hides a later live operator.
fn shell_uses_only_and_chaining(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while index < chars.len() {
        let character = chars[index];
        // Outside single quotes a backslash escapes the next character for the
        // shell: `\"` is a literal quote (no quote-state change) and `\;` is an
        // inert character, not an operator. Skip the pair so the scanner stays
        // aligned with the shell and fails closed.
        if character == '\\' && !in_single_quote && index + 1 < chars.len() {
            index += 2;
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            index += 1;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            index += 1;
            continue;
        }
        if in_single_quote || in_double_quote {
            index += 1;
            continue;
        }
        match character {
            ';' | '\n' | '|' => return false,
            '&' => {
                let next_is_and = chars.get(index + 1) == Some(&'&');
                if !next_is_and {
                    return false;
                }
                // A `||`-style `|` was already rejected above; `&&` consumes
                // both characters. `|||`, `&&&`, and similar malformed
                // sequences fail closed.
                let third = chars.get(index + 2);
                if third == Some(&'&') || third == Some(&'|') {
                    return false;
                }
                index += 2;
                continue;
            }
            _ => {}
        }
        index += 1;
    }

    true
}

/// Classify a shell call without weakening the authoritative mutation guard.
///
/// Standalone output plumbing such as `2>&1` or `> build.log` does not turn a
/// primary verification command into a mutation for progress accounting.
/// Pipelines and `;`/`||` chains remain mutations because their final
/// status does not reliably represent every verification stage. Pure `&&`
/// chains of verification-or-readonly segments are `Verification` since `&&`
/// short-circuits on first failure.
#[must_use]
pub fn classify_shell_activity(tool_name: &str, args: &Value) -> ShellActivity {
    let command = crate::tools::command_args::raw_command_text(args);
    let words = crate::tools::command_args::command_words(args).ok().flatten();
    let has_unclassified_shell_sequence = command.as_deref().is_some_and(has_shell_sequence);

    if let Some(activity) = command.as_deref().and_then(classify_provable_shell_sequence) {
        return activity;
    }

    let intent = super::classify_tool_intent(tool_name, args);

    if !has_unclassified_shell_sequence && words.as_deref().is_some_and(is_known_inspection) {
        return ShellActivity::Inspection;
    }

    let starts_with_verification = words.as_deref().is_some_and(is_verification_invocation);
    let contains_verification =
        starts_with_verification || command.as_deref().is_some_and(contains_verification_invocation);
    if !intent.mutating {
        return if contains_verification {
            ShellActivity::Verification
        } else {
            ShellActivity::Inspection
        };
    }

    if starts_with_verification
        && !has_unclassified_shell_sequence
        && !words.as_deref().is_some_and(has_logical_sequencing)
    {
        ShellActivity::Verification
    } else {
        ShellActivity::Mutation
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::constants::tools;
    use crate::tools::tool_intent::is_readonly_command_session_command;

    fn exec_command(command: &str) -> Value {
        json!({"cmd": command})
    }

    #[test]
    fn markdownlint_verification_preserves_command_boundaries() {
        for command in [
            "markdownlint-cli2 README.md",
            "npx --yes markdownlint-cli2 README.md",
            "npx --no-install markdownlint-cli2@0.23.3 README.md",
            "npx -y markdownlint-cli2 README.md && cargo check --locked",
            "python3 scripts/check_markdown.py",
            "python ./scripts/check_markdown.py",
        ] {
            assert!(shell_command_is_admitted_verification_attempt(&exec_command(command)), "{command}");
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Verification,
                "{command}"
            );
        }
        for command in [
            "markdownlint-cli2 --fix README.md",
            "npx --yes markdownlint-cli2 -f README.md",
            "npx --yes markdownlint-cli2 --fix=true README.md",
            "markdownlint-cli2 --version",
            "npx --package evil markdownlint-cli2 README.md",
            "npx markdownlint-cli2@evil README.md",
            "npx markdownlint-cli2 README.md && rm README.md",
            "python3 scripts/check_markdown.py --fix",
            "python3 scripts/check_markdown.py --list",
            "python3 scripts/check_markdown.py --help",
            "python3 other/check_markdown.py",
        ] {
            assert!(!shell_command_is_admitted_verification_attempt(&exec_command(command)), "{command}");
        }
        assert_ne!(
            classify_shell_activity(tools::EXEC_COMMAND, &exec_command("npx markdownlint-cli2 README.md | grep error")),
            ShellActivity::Verification
        );
    }

    #[test]
    fn output_search_recognizes_static_search_pipelines() {
        for command in [
            "rg --files --hidden -g '.markdownlint*'",
            "rg --files --hidden | rg -i 'markdownlint' | sed -n '1,15p'",
            "git ls-files | grep -i markdownlint | head -10",
            "LANG=C /usr/bin/grep -n missing README.md",
            "rg -- '-q' README.md",
            "rg -e -query src | head -10",
            "grep -ne-query README.md",
            "rg --regexp --quiet src",
            "rg -g '-q*' missing src",
            "rg '-g-q*' missing src",
            "rg -q --no-quiet missing src",
        ] {
            assert!(shell_command_is_output_search(&exec_command(command)), "{command}");
        }
        assert!(shell_command_is_output_search(&json!({"command": ["rg", "missing", "src"]})));
    }

    #[test]
    fn output_search_excludes_quiet_probes_and_unproven_commands() {
        for command in [
            "rg -q pattern src",
            "grep -nq pattern README.md",
            "rg --quiet pattern src | head -10",
            "grep --silent pattern README.md",
            "rg -e -query -q src",
            "grep -nqe-query README.md",
            "rg --regexp --quiet --quiet src",
            "rg -q --no-quiet -q missing src",
            "echo 'rg missing'",
            "cat empty.txt",
            "cargo check | rg error",
            "rg missing src; touch result.txt",
            "rg missing $(pwd)",
            "rg missing src > result.txt",
            "rg --pre 'touch result.txt' missing src",
        ] {
            assert!(!shell_command_is_output_search(&exec_command(command)), "{command}");
        }
        assert!(!shell_command_is_output_search(&json!({})));
    }

    #[test]
    fn output_search_distinguishes_separator_values_from_quiet_flags() {
        for option in [
            "--context-separator",
            "--field-context-separator",
            "--field-match-separator",
        ] {
            let command = format!("rg {option} -q missing src | head -10");
            assert!(shell_command_is_output_search(&exec_command(&command)), "{command}");
            let quiet_command = format!("rg {option} -q --quiet missing src | head -10");
            assert!(!shell_command_is_output_search(&exec_command(&quiet_command)), "{quiet_command}");
        }
    }

    #[test]
    fn standalone_grep_search_preserves_exit_status_ownership() {
        for command in [
            "rg missing src",
            "grep -q missing README.md",
            "LANG=C /usr/bin/grep -n missing README.md",
            "rg 'a|b' src",
            "fgrep missing README.md",
        ] {
            assert!(shell_command_is_standalone_grep_search(&exec_command(command)), "{command}");
        }
        assert!(shell_command_is_standalone_grep_search(&json!({"command": ["rg", "missing", "src"]})));
        for command in [
            "rg missing src; false",
            "rg missing src && false",
            "rg missing src | grep other",
            "! rg missing src",
            "(rg missing src)",
            "echo '/rg missing'; false",
            "rg missing src 2>/dev/null",
            "rg missing src 2>&1",
            "rg missing $(pwd)",
            "rg --pre 'false' missing src",
        ] {
            assert!(!shell_command_is_standalone_grep_search(&exec_command(command)), "{command}");
        }
    }

    #[test]
    fn admitted_verification_attempt_allows_truncation_but_blocks_smuggled_mutations() {
        for command in [
            "cargo check --locked 2>&1 | head -c 4000",
            "cargo check --locked",
            "cargo nextest run 2>&1 | head -c 4000",
        ] {
            assert!(
                shell_command_is_admitted_verification_attempt(&exec_command(command)),
                "expected admission: {command}"
            );
        }
        for command in [
            "cargo check && rm -rf target",
            "cargo check; rm foo.txt",
            "cargo check || rm foo.txt",
            "cargo check && cargo test && rm foo.txt",
            "sed -i '' 's/old/new/' README.md",
            "echo $(date)",
            "cargo check > build.log && rm foo.txt",
        ] {
            assert!(
                !shell_command_is_admitted_verification_attempt(&exec_command(command)),
                "expected block: {command}"
            );
        }
        assert!(!shell_command_is_admitted_verification_attempt(&json!({})));
    }

    #[test]
    fn logged_compound_inspection_commands_are_not_mutations() {
        for command in [
            "cat README.md && printf '\\n--- git status ---\\n' && git status --short",
            "wc -l README.md; rg -n '^#' README.md",
            "git diff --stat; find docs -maxdepth 2 -type f | sort | head -40",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Inspection,
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn captured_read_commands_with_output_suppression_are_inspection() {
        for command in [
            r#"sed -n '1,180p' README.md; sed -n '280,350p' README.md; sed -n '389,411p' README.md; printf '\n--- repo metadata ---\n'; git log -1 --format='%h %s'; sed -n '1,100p' Cargo.toml; rg -n '^version\s*=|rust-version|workspace\.package' Cargo.toml crates -g Cargo.toml | head -40"#,
            r#"sed -n '1,120p' crates/codegen/vtcode-core/src/tools/tool_intent/activity.rs; printf '\n--- readonly policy ---\n'; rg -n 'READONLY_UNIFIED_EXEC_COMMANDS|command_words_are_readonly' crates/codegen/vtcode-core/src/tools/tool_intent/readonly.rs; printf '\n--- recent commits ---\n'; git log -5 --oneline; printf '\n--- command arguments ---\n'; sed -n '1,180p' crates/codegen/vtcode-core/src/tools/command_args.rs"#,
            r###"git diff --stat; find docs -maxdepth 2 -type f | sort | head -40; rg -n "vtcode init|vtcode models|full-auto|run-debug|cargo install" docs/user-guide docs/installation docs/development 2>/dev/null | head -50"###,
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Inspection,
                "{command}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn printf_output_safety_guards_remain_mutations() {
        for command in [
            "printf 'captured output\\n' > output.txt",
            "printf '%s\\n' \"$(git status --short)\"",
            "printf '%s\\n' `git status --short`",
            "printf '\\n--- inspection ---\\n' && rm output.txt",
        ] {
            let args = exec_command(command);
            assert_eq!(classify_shell_activity(tools::EXEC_COMMAND, &args), ShellActivity::Mutation, "{command}");
            assert!(!is_readonly_command_session_command(&args), "unexpected readonly command: {command}");
        }
    }

    #[test]
    fn git_diff_check_chain_remains_inspection() {
        assert_eq!(
            classify_shell_activity(
                tools::EXEC_COMMAND,
                &exec_command("git diff --check && git status --short && git diff --stat"),
            ),
            ShellActivity::Inspection
        );
    }

    #[test]
    fn verification_detection_skips_environment_prefixes() {
        for command in [
            "env RUSTFLAGS=-Dwarnings cargo check",
            "RUSTFLAGS=-Dwarnings env cargo check",
            "env -u PATH cargo check",
            "env -C /tmp cargo check",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Verification,
                "{command}"
            );
        }
    }

    #[test]
    fn ambiguous_or_mutating_compounds_remain_mutations() {
        for command in [
            "git diff --stat; python3 -c 'open(\"out\", \"w\").write(\"x\")'",
            "cat README.md; sed -i '' 's/a/b/' README.md",
            "sed --in-place= README.md",
            "git diff --output=out",
            "git diff '--output=out'",
            "git diff -o out",
            "git diff -oout",
            "git log --output=out",
            "git show --textconv",
            // `-C <dir> <read-only sub>` is an Inspection now (the redirect
            // only changes which repository is read); config injection stays
            // a Mutation.
            "git -c core.fsmonitor=touch status",
            "find . -fprint output.txt",
            "find . -fprintf output.txt '%p'",
            "rg --hostname-bin sh pattern",
            "rg --search-zip pattern",
            "rg -z pattern",
            "sort -o generated.txt README.md",
            "sort --compress-program=sh README.md",
            "date -s now",
            "awk -i inplace '{print}' README.md",
            "sed -n 's/a/b/e' README.md",
            "fd --exec sh -c 'touch output'",
            "tree -o output.txt",
            "ast-grep -r 'README.md'",
            "sed -n -fmalicious.sed -e '1p' src/main.rs",
            "sed -I '' 's/a/b/' src/main.rs",
            "sed -n '1p\nw leaked.txt' src/main.rs",
            "cargo check & rm output",
            "cargo check > build.log | rm output",
            "cargo check | head -40 > build.log",
            "cargo check | echo x > output.log",
            "cargo check | head -40",
            "cargo check > build.log &",
            "env -S 'cargo check'",
            "echo x > output.log && cargo check",
            "cargo check < build-input.log",
            "cat README.md > copied.txt",
            "git diff --check; rm output",
            "cat README.md\nrm output",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Mutation,
                "{command}"
            );
        }
    }

    #[test]
    fn quoted_output_text_does_not_change_inspection_classification() {
        for command in ["echo 'git diff --output=out'", "printf 'sort -o out input'"] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Inspection,
                "{command}"
            );
        }
    }

    #[test]
    fn cargo_fmt_check_is_verification_but_plain_fmt_is_not() {
        for command in [
            "cargo fmt --check",
            "cargo fmt --all -- --check",
            "cargo fmt -- --check",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Verification,
                "{command}"
            );
            assert!(
                shell_command_is_admitted_verification_attempt(&exec_command(command)),
                "expected admission: {command}"
            );
        }
        // Plain `cargo fmt` rewrites the worktree: it must stay a mutation and
        // must not ride through the verification gate.
        assert_eq!(classify_shell_activity(tools::EXEC_COMMAND, &exec_command("cargo fmt")), ShellActivity::Mutation);
        assert!(!shell_command_is_admitted_verification_attempt(&exec_command("cargo fmt")));
    }

    #[test]
    fn pure_and_chained_verifiers_are_verification() {
        for command in [
            "cargo fmt --all -- --check && cargo check --locked",
            "cargo check --locked && cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked && cargo clippy --locked -p vtcode-ui -- -D warnings",
            "git diff --check && cargo check --locked",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Verification,
                "{command}"
            );
            assert!(
                shell_command_is_admitted_verification_attempt(&exec_command(command)),
                "expected admission: {command}"
            );
        }
    }

    #[test]
    fn non_and_chained_verifiers_remain_mutations() {
        for command in [
            "cargo check --locked; cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked || cargo nextest run --locked -p vtcode-ui",
            "cargo check --locked | head -40",
            "cargo check --locked && cargo nextest run --locked -p vtcode-ui | head -40",
            "cargo check --locked &",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Mutation,
                "{command}"
            );
        }
    }

    #[test]
    fn expanded_verifiers_classify_as_verification() {
        for command in [
            "bun test",
            "bun run test",
            "deno lint",
            "deno check mod.ts",
            "make test",
            "make lint",
            "just verify",
            "ruff check src/",
            "ruff format --check",
            "tsc --noEmit",
            "eslint src/",
            "python3 -m pytest",
            "uv run pytest",
            "npm run lint",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Verification,
                "{command}"
            );
        }
        for command in [
            "make clean",
            "make test clean",
            "just fmt",
            "tsc",
            "eslint --fix src/",
            "ruff format src/",
            "bun install",
            "uv sync",
        ] {
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &exec_command(command)),
                ShellActivity::Mutation,
                "{command}"
            );
        }
    }

    #[test]
    fn escaped_quotes_fail_closed_instead_of_hiding_operators() {
        // A backslash-escaped quote is a literal for the shell, so the trailing
        // `;` is a live separator: the aggregate exit status can mask a failed
        // verifier, and the chain must not classify as pure `&&`.
        assert!(!shell_uses_only_and_chaining("cargo check --locked \\\"; echo ok"));
        assert!(!shell_uses_only_and_chaining("echo \\\" ; cargo check --locked && echo done"));
        // Escaped quotes inside real double quotes stay inert.
        assert!(shell_uses_only_and_chaining("echo \"a\\\"b\" && cargo check --locked"));
        assert!(shell_uses_only_and_chaining("echo \"path\" && cargo fmt --check"));
        // Backslash is literal inside single quotes.
        assert!(shell_uses_only_and_chaining("echo 'a\\b' && cargo check --locked"));
    }

    #[test]
    fn default_verifier_prefers_manifest_priority_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(default_verifier_for_workspace(dir.path()), None);

        std::fs::write(dir.path().join("justfile"), "test:\n\techo ok\n").expect("justfile");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("just test"));
        std::fs::write(dir.path().join("pyproject.toml"), "[tool.pytest]\n").expect("pyproject");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("pytest -q"));
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"test":"vitest"}}"#).expect("package.json");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("npm test"));
        std::fs::write(dir.path().join("go.mod"), "module example\n").expect("go.mod");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("go test ./..."));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").expect("Cargo.toml");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("cargo check --locked"));
    }

    #[test]
    fn default_verifier_reads_npm_script_fallbacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"lint":"eslint ."}}"#).expect("package.json");
        assert_eq!(default_verifier_for_workspace(dir.path()).as_deref(), Some("npm run lint"));
    }

    #[test]
    fn verification_recovery_directive_names_concrete_command() {
        let directive = verification_recovery_directive(Some("go test ./..."), 1, 2);
        assert!(directive.contains("go test ./..."));
        assert!(directive.contains("1/2"));
        assert!(directive.contains("max_output_tokens"));
        assert!(directive.contains(VERIFIER_SHELL_FORM_NOTE));
        assert!(directive.contains("counts as standalone"));
        let fallback = verification_recovery_directive(None, 2, 2);
        assert!(fallback.contains(GENERIC_VERIFIER_DESCRIPTION));
        assert!(fallback.contains("2/2"));
    }

    #[test]
    fn verifier_reference_names_resolved_command_or_generic_description() {
        assert_eq!(verifier_reference(Some("go test ./...")), "`go test ./...`");
        assert_eq!(verifier_reference(Some("  npm test ")), "`npm test`");
        for missing in [None, Some(""), Some("   ")] {
            let reference = verifier_reference(missing);
            assert_eq!(reference, GENERIC_VERIFIER_DESCRIPTION);
            assert!(!reference.starts_with('`'), "no single command is named: {reference}");
            for ecosystem in ["cargo check --locked", "go test ./...", "npm test", "pytest -q"] {
                assert!(reference.contains(ecosystem), "missing {ecosystem}: {reference}");
            }
        }
    }

    #[test]
    fn truncation_only_verifier_rewrites_to_standalone_prefix() {
        for (command, expected) in [
            ("cargo check --locked 2>&1 | head -c 4000", "cargo check --locked 2>&1"),
            ("cargo check --locked -p vtcode 2>&1 | tail -20", "cargo check --locked -p vtcode 2>&1"),
            ("cargo nextest run 2>&1 | tail -15", "cargo nextest run 2>&1"),
            (
                "cargo check --locked && cargo nextest run --locked -p vtcode | tail -5",
                "cargo check --locked && cargo nextest run --locked -p vtcode",
            ),
            ("cargo check | tail -5 | head -20", "cargo check"),
            ("RUSTFLAGS=-Dwarnings cargo check | head -20", "RUSTFLAGS=-Dwarnings cargo check"),
            // File-output redirects are preserved verbatim in the head slice
            // (only the status-masking pipe tail is elided), and `>` does not
            // mask the exit status — so the rewrite stays truthful here too.
            ("cargo check > build.log | tail -5", "cargo check > build.log"),
        ] {
            assert_eq!(
                rewrite_truncation_only_verifier(&exec_command(command)).as_deref(),
                Some(expected),
                "expected rewrite: {command}"
            );
        }
    }

    #[test]
    fn non_truncation_pipelines_and_mutations_are_never_rewritten() {
        // Filtering tails do real work; dropping them would discard evidence.
        // Smuggled mutations, joins, redirects, and array forms must run (or
        // be rejected) exactly as typed — the rewrite must stay silent.
        for command in [
            "cargo check | grep error",
            "cargo check | wc -l",
            "cargo check | sort | uniq",
            "cargo check && rm -rf target | tail -5",
            "cargo check; git status | tail -5",
            "cargo check || cargo test | tail -5",
            "cargo check | tail -5; rm foo.txt",
            "cargo check | tail &",
            "cargo check",
            "echo done | tail -5",
            "rg -n 'pattern' src | head -20",
            "echo $(date) | tail -5",
        ] {
            assert_eq!(rewrite_truncation_only_verifier(&exec_command(command)), None, "must not rewrite: {command}");
        }
        assert_eq!(
            rewrite_truncation_only_verifier(&serde_json::json!({"command": ["cargo", "check", "|", "tail"]})),
            None,
            "array-form commands keep today's behavior"
        );
        assert_eq!(rewrite_truncation_only_verifier(&serde_json::json!({})), None);
    }

    #[test]
    fn args_as_executed_classify_elided_truncation_verifiers_as_verification() {
        for command in [
            "cargo check --locked 2>&1 | tail -5",
            "cargo nextest run 2>&1 | head -c 4000",
        ] {
            let typed = exec_command(command);
            let executed = shell_args_as_executed(tools::EXEC_COMMAND, &typed);
            assert!(matches!(executed, std::borrow::Cow::Owned(_)), "expected rewrite: {command}");
            assert_eq!(
                classify_shell_activity(tools::EXEC_COMMAND, &executed),
                ShellActivity::Verification,
                "{command}"
            );
            // Idempotent: the executed form runs unchanged on a second pass.
            assert!(matches!(shell_args_as_executed(tools::EXEC_COMMAND, &executed), std::borrow::Cow::Borrowed(_)));
        }

        let unified = json!({"action": "run", "command": "cargo check 2>&1 | tail -5"});
        assert_eq!(shell_args_as_executed(tools::UNIFIED_EXEC, &unified)["command"], "cargo check 2>&1");
    }

    #[test]
    fn args_as_executed_keep_filtering_pipes_and_non_run_calls_as_typed() {
        for command in [
            "cargo check 2>&1 | grep error",
            "cargo check; git status",
            "cargo check || true",
        ] {
            let typed = exec_command(command);
            let executed = shell_args_as_executed(tools::EXEC_COMMAND, &typed);
            assert!(matches!(executed, std::borrow::Cow::Borrowed(_)), "must run as typed: {command}");
            assert_ne!(
                classify_shell_activity(tools::EXEC_COMMAND, &executed),
                ShellActivity::Verification,
                "{command}"
            );
        }
        let poll = json!({"action": "poll", "session_id": "s1", "command": "cargo check | tail -5"});
        assert!(matches!(shell_args_as_executed(tools::UNIFIED_EXEC, &poll), std::borrow::Cow::Borrowed(_)));
        let read = json!({"path": "src/lib.rs"});
        assert!(matches!(shell_args_as_executed(tools::READ_FILE, &read), std::borrow::Cow::Borrowed(_)));
    }
}
