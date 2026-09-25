//! Pure planning-artifact logic: plan/tracker marker handling, section
//! parsing, validation, and tracker generation.
//!
//! Everything here is side-effect-free and depends only on `std`/`serde`, so it
//! is independently testable (see `super::tests`). I/O and tool wiring live in
//! `persistence.rs` / `start.rs` / `finish.rs`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(super) const PLAN_TRACKER_START: &str = "<!-- vtcode:plan-tracker:start -->";
pub(super) const PLAN_TRACKER_END: &str = "<!-- vtcode:plan-tracker:end -->";

/// The canonical one-line step format that produces reliable validation and
/// tracker generation. All repair directives reference this so the model gets
/// the same contract from every prompt surface.
pub const CANONICAL_STEP_FORMAT: &str = "1. Action -> files: [path/to/file.rs] -> verify: [cargo check]";

/// Shared valid `verify:` examples for planning synthesis/repair prompts.
/// `repair_feedback()` embeds this list; binary runloop constants stay
/// compile-time literals but must keep these examples via presence tests.
pub const PLANNING_VERIFY_VALID_EXAMPLES: &str = "`verify: [cargo nextest run -p vtcode]`, `verify: [cargo check --locked]`, `verify: [rg -n 'symbol' src/file.rs]`, `verify: [sed -n '1,40p' docs/file.md]`, `verify: [grep -n 'symbol' src/file.rs]`, `verify: [git show --stat HEAD]`";

/// Shared invalid `verify:` examples. Kept adjacent to
/// [`PLANNING_VERIFY_VALID_EXAMPLES`] so every prompt surface pairs them.
pub const PLANNING_VERIFY_INVALID_EXAMPLES: &str =
    "`verify: [run checks]`, `verify: [check later]`, `verify: [git diff --check]`";

const PLACEHOLDER_TOKENS: [&str; 21] = [
    "[step]",
    "[paths]",
    "[check]",
    "[explicit assumption]",
    "[default chosen when user did not specify]",
    "[out-of-scope items intentionally not changed]",
    "[file, symbol, or behavior confirmed from the repo]",
    "[observed command output -> the insight it establishes]",
    "[existing pattern or constraint verified before planning]",
    "[if any], otherwise: no remaining scope decisions",
    "[project build and lint command",
    "[project test command",
    "[2-4 lines: goal, user impact, what will change, what will not]",
    "[explicit commands/manual checks]",
    "[what must not break]",
    "[observable end state the implementation must produce]",
    "[required tooling, configuration, or prior work]",
    "[todo]",
    "todo:",
    "[decision needed]",
    "tbd",
];

const SUMMARY_SECTION_ALIASES: &[&str] = &["Summary"];
const IMPLEMENTATION_SECTION_ALIASES: &[&str] = &["Implementation Steps", "Steps"];
const VALIDATION_SECTION_ALIASES: &[&str] = &["Test Cases and Validation", "Validation"];
const ASSUMPTIONS_SECTION_ALIASES: &[&str] = &["Assumptions and Defaults", "Assumptions"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanValidationReport {
    pub missing_sections: Vec<String>,
    pub placeholder_tokens: Vec<String>,
    pub open_decisions: Vec<String>,
    pub invalid_implementation_steps: Vec<String>,
    pub implementation_step_count: usize,
    pub validation_item_count: usize,
    pub assumption_count: usize,
    pub summary_present: bool,
}

impl PlanValidationReport {
    pub fn is_ready(&self) -> bool {
        self.missing_sections.is_empty()
            && self.placeholder_tokens.is_empty()
            && self.open_decisions.is_empty()
            && self.invalid_implementation_steps.is_empty()
            && self.summary_present
            && self.implementation_step_count > 0
            && self.validation_item_count > 0
            && self.assumption_count > 0
    }

    pub fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if !self.missing_sections.is_empty() {
            reasons.push(format!("missing sections: {}", self.missing_sections.join(", ")));
        }
        if !self.placeholder_tokens.is_empty() {
            reasons.push(format!("placeholder tokens: {}", self.placeholder_tokens.join(", ")));
        }
        if !self.open_decisions.is_empty() {
            reasons.push(format!("unresolved decisions: {}", self.open_decisions.join("; ")));
        }
        if !self.invalid_implementation_steps.is_empty() {
            reasons.push(format!("invalid implementation steps: {}", self.invalid_implementation_steps.join("; ")));
        }
        if !self.summary_present {
            reasons.push("summary is empty".to_string());
        }
        if self.implementation_step_count == 0 {
            reasons.push("no implementation steps".to_string());
        }
        if self.validation_item_count == 0 {
            reasons.push("no validation items".to_string());
        }
        if self.assumption_count == 0 {
            reasons.push("no assumptions or defaults".to_string());
        }
        reasons
    }

    /// Produce bounded, validator-owned diagnostics for a repair directive.
    ///
    /// Unlike `reasons()`, which joins raw `open_decisions` lines (user/model
    /// controlled text), this method emits only validator-owned category
    /// summaries and step counts. It always includes the canonical step format
    /// so the model knows the exact contract to follow. This is safe to inject
    /// into a system message because every string here is validator-authored.
    pub fn repair_feedback(&self) -> String {
        let mut feedback = Vec::new();

        if !self.missing_sections.is_empty() {
            feedback.push(format!("missing required section(s): {}", self.missing_sections.join(", ")));
        }
        if !self.placeholder_tokens.is_empty() {
            feedback.push(format!("contains {} unresolved placeholder token(s)", self.placeholder_tokens.len()));
        }
        if !self.open_decisions.is_empty() {
            feedback.push(format!("contains {} unresolved decision marker(s)", self.open_decisions.len()));
        }
        if !self.invalid_implementation_steps.is_empty() {
            // Step numbers are parsed digits and reason strings are
            // validator-owned, so this is safe to echo.
            feedback.push(format!(
                "{} of {} implementation step(s) lack a concrete target or verification: {}",
                self.invalid_implementation_steps.len(),
                self.implementation_step_count,
                self.invalid_implementation_steps.join("; ")
            ));
        }
        if !self.summary_present {
            feedback.push("summary is empty".to_string());
        }
        if self.implementation_step_count == 0 {
            feedback.push("no implementation steps".to_string());
        }
        if self.validation_item_count == 0 {
            feedback.push("no validation items".to_string());
        }
        if self.assumption_count == 0 {
            feedback.push("no assumptions or defaults".to_string());
        }

        let mut result = if feedback.is_empty() {
            "The plan has validation issues".to_string()
        } else {
            format!("Plan validation issues: {}", feedback.join("; "))
        };
        result.push_str("\n\nRewrite every implementation step in this canonical one-line form:\n");
        result.push_str(CANONICAL_STEP_FORMAT);
        result.push_str(&format!(
            "\nEach step must name a concrete file path or symbol (not prose) and one concrete verify command or observable check. \
             Comma-separated verify entries must each be a command or an observable check; commas inside single or double quotes stay inside one item. \
             Valid examples: {PLANNING_VERIFY_VALID_EXAMPLES}. \
             Invalid examples: {PLANNING_VERIFY_INVALID_EXAMPLES}; vague prose and generic VCS-only checks do not satisfy this validator. \
             Command heads that are common English words (`file`, `sort`, `find`, `ls`, `wc`, …) also need a flag or path-like argument.",
        ));
        result
    }
}

pub fn tracker_file_for_plan_file(plan_file: &Path) -> Option<PathBuf> {
    let stem = plan_file.file_stem()?.to_str()?;
    Some(plan_file.with_file_name(format!("{stem}.tasks.md")))
}

pub fn plan_file_for_tracker_file(tracker_file: &Path) -> Option<PathBuf> {
    let file_name = tracker_file.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(".tasks.md")?;
    Some(tracker_file.with_file_name(format!("{stem}.md")))
}

fn strip_embedded_tracker(plan_content: &str) -> String {
    let Some(start) = plan_content.find(PLAN_TRACKER_START) else {
        return plan_content.trim().to_string();
    };
    let end = plan_content[start..]
        .find(PLAN_TRACKER_END)
        .map(|offset| start + offset + PLAN_TRACKER_END.len())
        .unwrap_or(plan_content.len());
    let mut merged = String::new();
    merged.push_str(plan_content[..start].trim_end());
    if !merged.is_empty() && !plan_content[end..].trim().is_empty() {
        merged.push_str("\n\n");
    }
    merged.push_str(plan_content[end..].trim_start());
    merged.trim().to_string()
}

pub(super) fn extract_embedded_tracker(plan_content: &str) -> Option<String> {
    let start = plan_content.find(PLAN_TRACKER_START)?;
    let end = plan_content.find(PLAN_TRACKER_END)?;
    if end <= start {
        return None;
    }
    let content = plan_content[start + PLAN_TRACKER_START.len()..end].trim();
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

pub(super) fn render_plan_with_tracker(plan_markdown: &str, tracker_markdown: Option<&str>) -> String {
    let base_plan = strip_embedded_tracker(plan_markdown);
    let Some(tracker_markdown) = tracker_markdown.map(str::trim).filter(|value| !value.is_empty()) else {
        return format!("{}\n", base_plan.trim_end());
    };
    format!("{}\n\n{}\n{}\n{}\n", base_plan.trim_end(), PLAN_TRACKER_START, tracker_markdown, PLAN_TRACKER_END)
}

/// Merge plan markdown with an optional tracker sidecar into the canonical
/// on-disk representation.
///
/// This deliberately delegates to `render_plan_with_tracker` so the result is
/// identical to what `persist_plan_draft` writes: the plan body with the
/// tracker embedded between `PLAN_TRACKER_START`/`PLAN_TRACKER_END` markers.
/// Previously this module appended the tracker as a bare trailing block, which
/// produced a *different* serialization than `persist_plan_draft` and could
/// double-embed the tracker when the plan file was already persisted.
pub fn merge_plan_content(plan_content: Option<String>, tracker_content: Option<String>) -> Option<String> {
    match (plan_content, tracker_content) {
        (Some(plan), Some(tracker)) => Some(render_plan_with_tracker(&plan, Some(&tracker))),
        (Some(plan), None) => Some(render_plan_with_tracker(&plan, None)),
        (None, Some(tracker)) => Some(render_plan_with_tracker("", Some(&tracker))),
        (None, None) => None,
    }
}

fn section_body(content: &str, header: &str) -> Option<String> {
    let mut capture = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if capture && is_plan_section_boundary(trimmed) {
            break;
        }
        if let Some(found) = trimmed.strip_prefix("## ") {
            if capture {
                break;
            }
            capture = strip_emphasis(found.trim().trim_end_matches(':'))
                .trim()
                .eq_ignore_ascii_case(header);
            continue;
        }
        if capture {
            lines.push(line.to_string());
        }
    }
    let body = lines.join("\n").trim().to_string();
    (!body.is_empty()).then_some(body)
}

fn section_body_for_aliases(content: &str, headers: &[&str]) -> Option<String> {
    headers
        .iter()
        .find_map(|header| section_body(content, header).or_else(|| standalone_section_body(content, header)))
}

/// Strip Markdown emphasis markers (`**bold**`, `` `code` ``) that models
/// frequently wrap around plan labels such as `**Files/symbols:**`. Emphasis
/// carries no semantics for validation; leaving it in place makes label
/// prefix matching reject well-formed plans (checkpoint turn_912).
fn strip_emphasis(value: &str) -> &str {
    value.trim_matches(['*', '`'])
}

/// Leading-edge variant for label prefixes: `**Files/symbols:** value`.
fn strip_leading_emphasis(value: &str) -> &str {
    value.trim_start_matches(['*', '`'])
}

fn normalized_section_label(line: &str) -> &str {
    let mut normalized = line.trim().trim_start_matches('>').trim_start();
    while let Some(stripped) = normalized.strip_prefix('#') {
        normalized = stripped.trim_start();
    }
    strip_emphasis(strip_list_marker(normalized).trim()).trim()
}

fn is_standalone_section_label(line: &str, header: &str) -> bool {
    let normalized = normalized_section_label(line);
    normalized.eq_ignore_ascii_case(header)
}

fn standalone_section_body(content: &str, header: &str) -> Option<String> {
    let mut capture = false;
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if is_standalone_section_label(trimmed, header) {
            if capture {
                break;
            }
            capture = true;
            continue;
        }
        if capture && is_plan_section_boundary(trimmed) {
            break;
        }
        if capture {
            lines.push(line.to_string());
        }
    }
    let body = lines.join("\n").trim().to_string();
    (!body.is_empty()).then_some(body)
}

fn strip_ascii_case_insensitive_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let prefix_end = prefix.len();
    value
        .get(..prefix_end)
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .and_then(|_| value.get(prefix_end..).map(str::trim_start))
}

fn labeled_body_for_aliases(content: &str, labels: &[&str]) -> Option<String> {
    let lines = content
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            labels
                .iter()
                .find_map(|label| strip_ascii_case_insensitive_prefix(line, &format!("{label}:")))
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn meaningful_section_lines(body: &str) -> Vec<&str> {
    body.lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with('>')
                && !line.starts_with("<!--")
                && *line != PLAN_TRACKER_START
                && *line != PLAN_TRACKER_END
        })
        .collect()
}

fn numbered_line_parts(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    // Tolerate a `Step ` prefix (`Step 1: ...`), a common model variant. The
    // digit check requires a leading digit after whitespace trimming so words
    // like `stepwise` are never mistaken for a step prefix.
    let trimmed = strip_ascii_case_insensitive_prefix(trimmed, "step")
        .map(str::trim_start)
        .filter(|rest| rest.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(trimmed);
    let digit_end = trimmed
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    if digit_end == 0 {
        return None;
    }

    let rest = trimmed.get(digit_end..)?.trim_start();
    let punctuation = rest.chars().next()?;
    if punctuation != '.' && punctuation != ')' && punctuation != ':' {
        return None;
    }

    Some((trimmed.get(..digit_end)?, rest.get(punctuation.len_utf8()..)?.trim_start()))
}

fn is_numbered_line(line: &str) -> bool {
    numbered_line_parts(line).is_some()
}

#[derive(Debug, Clone)]
struct ImplementationStepBlock {
    number: String,
    lines: Vec<String>,
}

fn strip_list_marker(line: &str) -> &str {
    let mut current = line.trim();
    loop {
        let Some(stripped) = current
            .strip_prefix("- ")
            .or_else(|| current.strip_prefix("* "))
            .or_else(|| current.strip_prefix("• "))
        else {
            return current;
        };
        current = stripped.trim_start();
    }
}

fn is_plan_section_boundary(line: &str) -> bool {
    let mut normalized = line.trim();
    while let Some(stripped) = normalized.strip_prefix('#') {
        normalized = stripped.trim_start();
    }
    normalized = strip_emphasis(strip_list_marker(normalized).trim()).trim();
    SUMMARY_SECTION_ALIASES
        .iter()
        .chain(IMPLEMENTATION_SECTION_ALIASES.iter())
        .chain(VALIDATION_SECTION_ALIASES.iter())
        .chain(ASSUMPTIONS_SECTION_ALIASES.iter())
        .any(|alias| {
            normalized.eq_ignore_ascii_case(alias)
                || strip_ascii_case_insensitive_prefix(normalized, &format!("{alias}:")).is_some()
        })
}

fn collect_implementation_step_blocks(content: &str, stop_at_section_boundaries: bool) -> Vec<ImplementationStepBlock> {
    let mut blocks = Vec::new();
    let mut current: Option<ImplementationStepBlock> = None;
    let mut collecting = true;
    let mut started = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('>')
            || trimmed.starts_with("<!--")
            || trimmed == PLAN_TRACKER_START
            || trimmed == PLAN_TRACKER_END
        {
            continue;
        }

        if let Some((number, step)) = numbered_line_parts(trimmed) {
            if !collecting {
                continue;
            }
            if let Some(previous) = current.take() {
                blocks.push(previous);
            }
            started = true;
            current = Some(ImplementationStepBlock {
                number: number.to_string(),
                lines: vec![step.to_string()],
            });
            continue;
        }

        if stop_at_section_boundaries && is_plan_section_boundary(trimmed) {
            if started {
                if let Some(previous) = current.take() {
                    blocks.push(previous);
                }
                collecting = false;
            }
            continue;
        }

        if collecting
            && started
            && let Some(step) = current.as_mut()
        {
            step.lines.push(trimmed.to_string());
        }
    }

    if let Some(last) = current {
        blocks.push(last);
    }
    blocks
}

fn marker_value<'a>(line: &'a str, labels: &[&str]) -> Option<&'a str> {
    let line = strip_list_marker(line);
    // Tolerate emphasis around the label (`**Files/symbols:** value`) — models
    // routinely bold marker labels, and the `**` otherwise breaks both the
    // label prefix and the trailing `:` match (checkpoint turn_912).
    let line = strip_leading_emphasis(line);
    labels.iter().find_map(|label| {
        strip_ascii_case_insensitive_prefix(line, &format!("{label}:"))
            .or_else(|| {
                // Emphasis between label and colon: `**Files/symbols:**`.
                strip_ascii_case_insensitive_prefix(line, label)
                    .and_then(|rest| strip_leading_emphasis(rest).strip_prefix(':').map(str::trim_start))
            })
            .map(|value| strip_leading_emphasis(value).trim_start())
    })
}

fn is_concrete_value(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != "[]" && find_placeholder_tokens(value).is_empty()
}

fn is_concrete_target(value: &str) -> bool {
    let value = value.trim();
    if !is_concrete_value(value) {
        return false;
    }

    let target = marker_value(value, &["files/symbols", "files", "symbols", "target", "behavior", "behaviour"])
        .unwrap_or(value)
        .trim();
    if !is_concrete_value(target) {
        return false;
    }

    let lower = target.to_ascii_lowercase();
    if lower.starts_with('[') && lower.ends_with(']') {
        let items = parse_bracket_list(target);
        return !items.is_empty() && items.iter().any(|item| is_concrete_target(item));
    }

    let has_structural_reference = target.split_whitespace().any(|token| {
        let token = token.trim_matches(|ch: char| ch.is_ascii_punctuation() && ch != '_' && ch != '/');
        token.contains('/')
            || token.contains('\\')
            || token.contains("::")
            || token.contains('_')
            || token.chars().skip(1).any(char::is_uppercase)
            || token.rsplit_once('.').is_some_and(|(_, suffix)| !suffix.is_empty())
    });
    if has_structural_reference {
        return true;
    }

    const GENERIC_TARGETS: &[&str] = &[
        "file",
        "files",
        "path",
        "paths",
        "symbol",
        "symbols",
        "files/symbols",
        "files or symbols",
        "file/symbol",
        "file or symbol",
        "behavior",
        "behaviour",
        "code",
        "codebase",
        "implementation",
        "feature",
        "workflow",
        "module",
        "modules",
        "component",
        "components",
        "target",
        "relevant files",
        "relevant code",
        "relevant modules",
        "relevant symbols",
        "appropriate files",
        "appropriate code",
        "appropriate modules",
        "affected files",
        "affected code",
        "affected modules",
        "the file",
        "the files",
        "the path",
        "the symbol",
        "the symbols",
        "the behavior",
        "the behaviour",
        "the code",
        "the codebase",
        "the implementation",
        "the feature",
        "the workflow",
        "the module",
        "the modules",
        "the component",
        "the components",
        "the relevant files",
        "the relevant code",
        "the relevant modules",
        "the relevant symbols",
        "the affected files",
        "the affected code",
        "the affected modules",
        "existing code",
        "existing files",
        "existing modules",
        "changed code",
        "changed files",
        "changed modules",
        "all relevant files",
        "all relevant code",
        "all relevant modules",
    ];
    if GENERIC_TARGETS.iter().any(|generic| lower == *generic) {
        return false;
    }

    let generic_prefixes = [
        "relevant ",
        "appropriate ",
        "affected ",
        "the relevant ",
        "the affected ",
        "existing ",
        "changed ",
        "all relevant ",
        "the ",
        "a ",
        "an ",
        "some ",
        "any ",
    ];
    if generic_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        return false;
    }

    // A behavior target may be prose, but it still needs two recognizable
    // domain terms. This rejects arbitrary filler such as `foo bar` or
    // `implementation details` while allowing concrete behavior names such
    // as `approval handoff`, `startup latency`, and `cache invalidation`.
    const CONCRETE_BEHAVIOR_WORDS: &[&str] = &[
        "agent",
        "assertion",
        "approval",
        "artifact",
        "benchmark",
        "benchmarks",
        "bootstrap",
        "budget",
        "cache",
        "check",
        "command",
        "configuration",
        "confirmation",
        "context",
        "defer",
        "deferred",
        "duration",
        "durations",
        "error",
        "event",
        "execution",
        "fallback",
        "flow",
        "handoff",
        "initialization",
        "initialize",
        "input",
        "instrumentation",
        "interview",
        "latency",
        "launch",
        "launches",
        "lifecycle",
        "logic",
        "markup",
        "measure",
        "measurement",
        "measurements",
        "memory",
        "milestone",
        "milestones",
        "output",
        "parser",
        "parsing",
        "path",
        "performance",
        "permission",
        "persistence",
        "phase",
        "phases",
        "plan",
        "planning",
        "policy",
        "prompt",
        "question",
        "read",
        "recovery",
        "refresh",
        "report",
        "reports",
        "request",
        "response",
        "runtime",
        "state",
        "startup",
        "step",
        "stream",
        "symbol",
        "task",
        "test",
        "timeout",
        "timing",
        "tracker",
        "trace",
        "traces",
        "transition",
        "tool",
        "ui",
        "validation",
        "workflow",
        "write",
    ];
    let concrete_word_count = lower
        .split_whitespace()
        .map(|word| word.trim_matches(|character: char| character.is_ascii_punctuation()))
        .filter(|word| CONCRETE_BEHAVIOR_WORDS.contains(word))
        .count();
    lower.split_whitespace().count() >= 2 && concrete_word_count >= 2
}

fn verification_words(value: &str) -> Vec<&str> {
    value
        .split_whitespace()
        .map(|word| {
            // Preserve flag-shaped tokens (`-n`, `--locked`): verification
            // validation uses them as command-invocation evidence, and
            // stripping the leading hyphen would turn `-l` into prose `l`.
            if word.len() > 1 && word.starts_with('-') {
                return word;
            }
            word.trim_matches(|character: char| {
                character.is_ascii_punctuation()
                    && !matches!(
                        character,
                        '_' | '/'
                            | '.'
                            | ';'
                            | '&'
                            | '|'
                            | '<'
                            | '>'
                            | '$'
                            | '('
                            | ')'
                            | '{'
                            | '}'
                            | '['
                            | ']'
                            | '!'
                            | '*'
                            | '?'
                            | '~'
                            | '\\'
                    )
            })
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn is_invocation_cue(word: &str) -> bool {
    word.eq_ignore_ascii_case("run")
        || word.eq_ignore_ascii_case("running")
        || word.eq_ignore_ascii_case("execute")
        || word.eq_ignore_ascii_case("executing")
        || word.eq_ignore_ascii_case("invoke")
        || word.eq_ignore_ascii_case("invoking")
        || word.eq_ignore_ascii_case("use")
        || word.eq_ignore_ascii_case("using")
        || word.eq_ignore_ascii_case("with")
        || word.eq_ignore_ascii_case("by")
        || word.eq_ignore_ascii_case("via")
        || word.eq_ignore_ascii_case("through")
        || word.eq_ignore_ascii_case("then")
        || word.eq_ignore_ascii_case("plus")
        || word.eq_ignore_ascii_case("after")
        || word.eq_ignore_ascii_case("rerun")
}

fn is_verification_wrapper(word: &str) -> bool {
    ["command", "execute", "invoke", "run", "use"]
        .iter()
        .any(|wrapper| word.eq_ignore_ascii_case(wrapper))
}

fn is_actual_command_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    let bare_word = word.trim_matches(|character: char| character.is_ascii_punctuation());
    const COMMAND_NAMES: &[&str] = &[
        "awk",
        "bun",
        "cargo",
        "cat",
        "cmake",
        "clippy",
        "cut",
        "deno",
        "diff",
        "dotnet",
        "egrep",
        "eslint",
        "fgrep",
        "file",
        "find",
        "go",
        "gradle",
        "grep",
        "head",
        "jq",
        "just",
        "ls",
        "make",
        "meson",
        "mvn",
        "mypy",
        "ninja",
        "nextest",
        "npm",
        "pnpm",
        "python",
        "python3",
        "pytest",
        "rg",
        "ruff",
        "rustfmt",
        "sed",
        "shellcheck",
        "sort",
        "stat",
        "swiftlint",
        "tail",
        "tr",
        "tsc",
        "uniq",
        "wc",
        "xcodebuild",
        "yarn",
    ];
    (!word.contains('/') && COMMAND_NAMES.iter().any(|candidate| bare_word.eq_ignore_ascii_case(candidate)))
        || word.starts_with('/')
        || is_safe_workspace_relative_command_token(word)
        || (!word.contains('/') && is_script_command_token(word))
}

/// Command heads that are also common English words. Expanding `COMMAND_NAMES`
/// with inspection tools introduced false accepts such as `file changes` or
/// `sort order` — multi-word phrases that look like commands only because the
/// first token is allowlisted. Those heads now require a flag or path-like
/// later token; tooling names like `cargo`/`rg` keep the multi-token rule.
const AMBIGUOUS_COMMAND_HEADS: &[&str] = &[
    "cat", "cut", "diff", "file", "find", "head", "just", "ls", "make", "sort", "stat", "tr", "uniq", "wc",
];

fn is_flag_like_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    word.len() > 1 && word.starts_with('-')
}

/// Filename-shaped evidence for ambiguous command heads: `README.md`,
/// `notes.txt`, `Cargo.toml`. `is_pathlike_command_token` only accepts
/// slash/`./`/absolute shapes, so slash-less inspection args would otherwise
/// false-reject (`wc README.md`).
fn is_filename_like_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    if word.is_empty() || word.starts_with('-') {
        return false;
    }
    match word.rsplit_once('.') {
        Some((stem, suffix)) => {
            !stem.is_empty()
                && !suffix.is_empty()
                && suffix.len() <= 12
                && suffix.chars().all(|character| character.is_ascii_alphanumeric())
                && stem
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
        }
        None => false,
    }
}

/// True when a command-head phrase still looks like a real invocation after
/// the allowlist match. Ambiguous English heads need flag/path evidence;
/// unambiguous heads keep existing semantics.
fn command_head_has_invocation_shape(words: &[&str]) -> bool {
    let Some(head) = words.first() else {
        return false;
    };
    let bare = head.trim_matches(|character: char| {
        character.is_ascii_punctuation() && !matches!(character, '_' | '/') || matches!(character, '`' | '"' | '\'')
    });
    if !AMBIGUOUS_COMMAND_HEADS
        .iter()
        .any(|candidate| bare.eq_ignore_ascii_case(candidate))
    {
        return true;
    }
    words.iter().skip(1).any(|word| {
        is_flag_like_token(word)
            || is_pathlike_command_token(word)
            || word.contains('/')
            || is_filename_like_token(word)
    })
}

fn is_safe_workspace_relative_command_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    let (word, dot_relative) = match word.strip_prefix("./") {
        Some(relative_word) => (relative_word, true),
        None => (word, false),
    };

    if has_url_scheme(word) || word.chars().any(is_shell_metacharacter) {
        return false;
    }

    if word.contains('/') {
        return word.split('/').all(is_safe_workspace_path_component);
    }

    dot_relative && is_safe_workspace_path_component(word)
}

fn is_safe_workspace_path_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
}

fn has_url_scheme(word: &str) -> bool {
    let Some((scheme, _)) = word.split_once(':') else {
        return false;
    };
    let mut scheme_characters = scheme.chars();
    matches!(scheme_characters.next(), Some(first) if first.is_ascii_alphabetic())
        && scheme_characters.all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
}

fn is_shell_metacharacter(character: char) -> bool {
    matches!(
        character,
        ';' | '&' | '|' | '<' | '>' | '$' | '(' | ')' | '{' | '}' | '[' | ']' | '!' | '*' | '?' | '~' | '\\'
    )
}

fn is_script_command_token(word: &str) -> bool {
    word.ends_with(".sh") || word.ends_with(".cmd") || word.ends_with(".ps1") || word.ends_with(".bat")
}

fn is_shell_assignment_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    let Some((name, value)) = word.split_once('=') else {
        return false;
    };

    if name.is_empty() || value.is_empty() {
        return false;
    }

    let mut name_chars = name.chars();
    match name_chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }

    name_chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_pathlike_command_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    word.starts_with('/')
        || is_safe_workspace_relative_command_token(word)
        || (!word.contains('/') && is_script_command_token(word))
}

fn contains_actual_command_invocation(value: &str) -> bool {
    let words = verification_words(value);
    if words.len() < 2 {
        return false;
    }

    let mut assignment_prefix_end = 0;
    while words
        .get(assignment_prefix_end)
        .is_some_and(|word| is_shell_assignment_token(word))
    {
        assignment_prefix_end += 1;
    }
    if assignment_prefix_end > 0
        && words
            .get(assignment_prefix_end)
            .is_some_and(|word| is_actual_command_token(word))
        && (is_pathlike_command_token(words[assignment_prefix_end]) || words.len() > assignment_prefix_end + 1)
        && command_head_has_invocation_shape(&words[assignment_prefix_end..])
    {
        return true;
    }

    for index in 0..words.len() {
        let word = words[index];
        if !is_actual_command_token(word) {
            continue;
        }

        if index == 0 {
            return (words.len() > 1 || is_pathlike_command_token(word))
                && command_head_has_invocation_shape(words.as_slice());
        }

        if (is_pathlike_command_token(word) || words.get(index + 1).is_some())
            && words[..index].iter().rev().take(3).any(|previous| is_invocation_cue(previous))
            && command_head_has_invocation_shape(&words[index..])
        {
            return true;
        }
    }

    value
        .split('`')
        .skip(1)
        .step_by(2)
        .any(|span| contains_actual_command_invocation(span.trim()))
}

/// History inspection can verify a review step when it selects actual evidence.
/// Keep this narrower than the shell read-only policy: a bare Git command or
/// whitespace check alone does not establish the step's outcome.
fn is_git_command_token(raw_word: &str) -> bool {
    let word = raw_word.trim_matches(|character: char| matches!(character, '`' | '"' | '\''));
    word.rsplit([';', '&', '|'])
        .next()
        .unwrap_or(word)
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| matches!(name, "git" | "git.exe"))
}

fn contains_shell_metacharacter(value: &str) -> bool {
    value
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '$' | '`' | '<' | '>' | '\\'))
}

fn git_verification_command_start(words: &[&str]) -> Option<usize> {
    let mut assignment_prefix_end = 0;
    while words
        .get(assignment_prefix_end)
        .is_some_and(|word| is_shell_assignment_token(word))
    {
        assignment_prefix_end += 1;
    }

    if words.get(assignment_prefix_end).is_some_and(|word| is_git_command_token(word)) {
        return Some(assignment_prefix_end);
    }
    if is_verification_wrapper(words.get(assignment_prefix_end)?)
        && words
            .get(assignment_prefix_end + 1)
            .is_some_and(|word| is_git_command_token(word))
    {
        return Some(assignment_prefix_end + 1);
    }

    words.iter().enumerate().find_map(|(index, word)| {
        (is_git_command_token(word)
            && (contains_shell_metacharacter(word)
                || words[..index].iter().any(|previous| contains_shell_metacharacter(previous))
                || words[..index]
                    .iter()
                    .rev()
                    .take(3)
                    .any(|previous| is_invocation_cue(previous) || is_verification_wrapper(previous))))
        .then_some(index)
    })
}

fn is_concrete_git_verification(value: &str) -> bool {
    if value.contains(['\n', '\r']) {
        return false;
    }
    let words = verification_words(value);
    let Some(command_start) = git_verification_command_start(&words) else {
        return false;
    };
    if words[..command_start].iter().any(|prefix| contains_shell_metacharacter(prefix)) {
        return false;
    }
    let Some((command, words)) = words[command_start..].split_first() else {
        return false;
    };
    if !is_git_command_token(command) || contains_shell_metacharacter(command) {
        return false;
    }
    let Some((subcommand, args)) = words.split_first() else {
        return false;
    };
    if !matches!(*subcommand, "log" | "show" | "diff" | "blame")
        || args.is_empty()
        || args.iter().any(|arg| {
            ["--check", "--output", "--ext-diff", "--exec", "--format=%x"]
                .iter()
                .any(|unsafe_arg| arg.contains(unsafe_arg))
                || contains_shell_metacharacter(arg)
        })
    {
        return false;
    }
    args.iter().enumerate().any(|(index, arg)| {
        let arg = arg.trim_matches(|ch| matches!(ch, '\'' | '"'));
        (arg == "--" && args.get(index + 1).is_some())
            || (arg.starts_with('-')
                && (["--since=", "--until=", "--author=", "--grep=", "--max-count="]
                    .iter()
                    .any(|prefix| arg.strip_prefix(prefix).is_some_and(|value| !value.is_empty()))
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|count| !count.is_empty() && count.chars().all(|ch| ch.is_ascii_digit()))))
            || (!arg.starts_with('-') && (index == 0 || args[index - 1] != "--"))
    })
}

/// Luu agentic-testing: fresh-context independent re-derivation counts as verification.
/// Accepts `independent-rederive <target> (fresh context, no helper reuse)`
/// so high-risk steps can require an oracle independent of production helpers.
fn is_independent_rederivation(value: &str) -> bool {
    let lowered = value.to_ascii_lowercase();
    let fresh = lowered.contains("fresh") || lowered.contains("independent");
    let rederive = lowered.contains("re-derive") || lowered.contains("rederive") || lowered.contains("re derive");
    let no_reuse = lowered.contains("without reusing")
        || lowered.contains("no helper reuse")
        || lowered.contains("no reuse")
        || lowered.contains("fresh context");
    if !(fresh && rederive && no_reuse) {
        return false;
    }
    // Require a concrete target beyond the keywords so vacuous
    // `independent rederive fresh context` cannot pass as verification.
    verification_words(value).len() >= 4
}

fn is_observable_manual_verification(value: &str) -> bool {
    let words = verification_words(value);
    if words.len() < 2 {
        return false;
    }

    const CUES: &[&str] = &[
        "benchmark",
        "benchmarks",
        "check",
        "checks",
        "compare",
        "compares",
        "confirm",
        "confirms",
        "instrument",
        "instrumented",
        "launch",
        "launches",
        "measure",
        "measures",
        "record",
        "records",
        "review",
        "reviews",
        "observe",
        "observes",
        "inspect",
        "inspects",
        "profile",
        "profiles",
        "run",
        "runs",
        "test",
        "tests",
        "validate",
        "validates",
        "verify",
        "verifies",
    ];
    const EVIDENCE: &[&str] = &[
        "after",
        "baseline",
        "before",
        "cold",
        "debug",
        "duration",
        "durations",
        "faster",
        "fewer",
        "improve",
        "improved",
        "improvement",
        "improves",
        "latency",
        "log",
        "logs",
        "metric",
        "metrics",
        "output",
        "outputs",
        "phase",
        "phases",
        "read",
        "reads",
        "reported",
        "result",
        "results",
        "speed",
        "startup",
        "time",
        "times",
        "timing",
        "unchanged",
        "warm",
        "launch",
        "launches",
        "prompt",
    ];

    let cue_count = words
        .iter()
        .filter(|word| CUES.iter().any(|cue| word.eq_ignore_ascii_case(cue)))
        .count();
    if cue_count == 0 {
        return false;
    }

    let evidence_count = words
        .iter()
        .filter(|word| {
            word.chars().any(|character| character.is_ascii_digit())
                || EVIDENCE.iter().any(|evidence| word.eq_ignore_ascii_case(evidence))
        })
        .count();
    let has_non_temporal_evidence = words.iter().any(|word| {
        !matches!(*word, "after" | "before") && EVIDENCE.iter().any(|evidence| word.eq_ignore_ascii_case(evidence))
    });
    let legacy_observable_marker = [
        "assert",
        "available",
        "completes",
        "contains",
        "deferred",
        "emits",
        "expected",
        "fails",
        "finishes",
        "holds",
        "includes",
        "matches",
        "manual",
        "measure",
        "never",
        "observable",
        "outputs",
        "persists",
        "preserves",
        "remains",
        "renders",
        "reports",
        "returns",
        "shows",
        "starts",
        "stays",
        "survives",
        "updates",
        "visible",
        "waits",
    ]
    .iter()
    .any(|marker| words.iter().any(|word| word.eq_ignore_ascii_case(marker)));
    let tests_pass = words
        .iter()
        .any(|word| word.eq_ignore_ascii_case("tests") || word.eq_ignore_ascii_case("checks"))
        && words.iter().any(|word| word.eq_ignore_ascii_case("pass"));

    (evidence_count >= 2 && has_non_temporal_evidence) || legacy_observable_marker || tests_pass
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationValidationError {
    NotConcrete,
    InvalidItem { ordinal: usize },
}

fn validate_concrete_verification(value: &str) -> Result<(), VerificationValidationError> {
    let value = value.trim();
    if !is_concrete_value(value) {
        return Err(VerificationValidationError::NotConcrete);
    }

    if value.starts_with('[') && value.ends_with(']') {
        let items = parse_bracket_list(value);
        if items.is_empty() {
            return Err(VerificationValidationError::NotConcrete);
        }
        for (index, item) in items.iter().enumerate() {
            if validate_concrete_verification(item).is_err() {
                return Err(VerificationValidationError::InvalidItem { ordinal: index + 1 });
            }
        }
        return Ok(());
    }

    let words = verification_words(value);
    if git_verification_command_start(&words).is_some()
        || (value.contains(['\n', '\r']) && words.iter().any(|word| is_git_command_token(word)))
    {
        return is_concrete_git_verification(value)
            .then_some(())
            .ok_or(VerificationValidationError::NotConcrete);
    }
    let leading_wrapper = words.first().is_some_and(|word| is_verification_wrapper(word));
    if (words.first().is_some_and(|word| is_actual_command_token(word))
        && (words.len() > 1 || words.first().is_some_and(|word| is_pathlike_command_token(word)))
        && command_head_has_invocation_shape(words.as_slice()))
        || (leading_wrapper
            && words.get(1).is_some_and(|word| is_actual_command_token(word))
            && (words.len() > 2 || words.get(1).is_some_and(|word| is_pathlike_command_token(word)))
            && command_head_has_invocation_shape(&words[1..]))
        || contains_actual_command_invocation(value)
    {
        return Ok(());
    }

    if is_independent_rederivation(value) {
        return Ok(());
    }

    if is_observable_manual_verification(value) {
        Ok(())
    } else {
        Err(VerificationValidationError::NotConcrete)
    }
}

/// Normalize a step action line for `->` segmentation, tolerating the
/// Unicode `→` arrow models occasionally emit. Validation and tracker
/// generation share this so the two paths can never disagree about which
/// arrows delimit a step's action/targets/verification. Returns
/// `Cow::Borrowed` (zero allocation) when no Unicode arrow is present — the
/// same normalize-or-borrow pattern as `summarizers`/`untrusted_data`.
fn normalize_step_action(action: &str) -> Cow<'_, str> {
    if action.contains('→') {
        Cow::Owned(action.replace('→', "->"))
    } else {
        Cow::Borrowed(action)
    }
}

/// Split a step action line into `->`-separated segments, tolerating the
/// Unicode `→` arrow. Owns its segments (`String`) so callers never juggle
/// the normalized temporary's lifetime; step lines are short, so the
/// per-segment allocation is immaterial.
fn step_action_segments(action: &str) -> Vec<String> {
    normalize_step_action(action)
        .split("->")
        .map(str::trim)
        .map(str::to_string)
        .collect()
}

fn implementation_step_shape_error(step: &ImplementationStepBlock) -> Option<String> {
    let first_line = step.lines.first().map(String::as_str).unwrap_or_default();
    let action = first_line.trim();
    if action.is_empty() {
        return Some("action is empty".to_string());
    }

    let segments = step_action_segments(action);
    let verify_index = segments
        .iter()
        .position(|segment| marker_value(segment, &["verify", "verification"]).is_some());
    let mut has_target = false;
    let mut invalid_target = false;

    if let Some(index) = verify_index {
        if index < 2 {
            return Some("must include a concrete target before the verification marker".to_string());
        }
        for target in segments.iter().skip(1).take(index.saturating_sub(1)) {
            if marker_value(target, &["outcome"]).is_some() {
                continue;
            }
            has_target = true;
            invalid_target |= !is_concrete_target(target);
        }
    } else if segments.len() > 1 {
        for target in segments.iter().skip(1) {
            if marker_value(target, &["outcome"]).is_some() {
                continue;
            }
            has_target = true;
            invalid_target |= !is_concrete_target(target);
        }
    }

    let mut has_verification = verify_index.is_some();
    let mut verification_error = verify_index
        .and_then(|index| marker_value(&segments[index], &["verify", "verification"]))
        .and_then(|verify| validate_concrete_verification(verify).err());
    for continuation in step.lines.iter().skip(1) {
        if let Some(target) = marker_value(continuation, &["files/symbols", "files", "symbols", "target"]) {
            has_target = true;
            invalid_target |= !is_concrete_target(target);
        }
        if let Some(verify) = marker_value(continuation, &["verify", "verification"]) {
            has_verification = true;
            if verification_error.is_none() {
                verification_error = validate_concrete_verification(verify).err();
            }
        }
    }

    if !has_target || invalid_target {
        return Some("must name a concrete file, symbol, or behavior target".to_string());
    }
    if !has_verification {
        return Some("must include a `verify:` or `verification:` marker".to_string());
    }
    if let Some(error) = verification_error {
        return Some(match error {
            VerificationValidationError::NotConcrete => {
                "verification marker must include a concrete command or check".to_string()
            }
            VerificationValidationError::InvalidItem { ordinal } => {
                format!("verification item {ordinal} must be a concrete command or check")
            }
        });
    }
    None
}

fn find_placeholder_tokens(content: &str) -> Vec<String> {
    let lower = content.to_ascii_lowercase();
    PLACEHOLDER_TOKENS
        .iter()
        .filter(|token| lower.contains(**token))
        .map(|token| token.to_string())
        .collect()
}

fn find_open_decisions(content: &str) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            (lower.contains("next open decision") || lower.contains("open question"))
                && ![
                    "none",
                    "no open",
                    "no remaining",
                    "no further",
                    "resolved",
                    "closed",
                    "n/a",
                    "not applicable",
                ]
                .iter()
                .any(|needle| lower.contains(needle))
        })
        .map(ToString::to_string)
        .collect()
}

pub fn validate_plan_content(content: &str) -> PlanValidationReport {
    let stripped = strip_embedded_tracker(content);
    let mut report = PlanValidationReport {
        placeholder_tokens: find_placeholder_tokens(&stripped),
        open_decisions: find_open_decisions(&stripped),
        ..PlanValidationReport::default()
    };

    let summary_body = section_body_for_aliases(&stripped, SUMMARY_SECTION_ALIASES)
        .or_else(|| labeled_body_for_aliases(&stripped, SUMMARY_SECTION_ALIASES));
    let implementation_section_body = section_body_for_aliases(&stripped, IMPLEMENTATION_SECTION_ALIASES);
    let implementation_labeled_body = labeled_body_for_aliases(&stripped, IMPLEMENTATION_SECTION_ALIASES);
    let implementation_blocks = if let Some(body) = implementation_section_body.as_deref() {
        collect_implementation_step_blocks(body, false)
    } else if implementation_labeled_body.is_some() {
        collect_implementation_step_blocks(implementation_labeled_body.as_deref().unwrap_or_default(), false)
    } else {
        // Older compact plans omit a Steps heading and put the numbered list
        // between labeled Summary/Validation/Assumptions lines. Keep that
        // compatibility, but stop collecting when the next labeled section
        // begins so validation bullets cannot masquerade as step details.
        collect_implementation_step_blocks(&stripped, true)
    };
    let validation_body = section_body_for_aliases(&stripped, VALIDATION_SECTION_ALIASES)
        .or_else(|| labeled_body_for_aliases(&stripped, VALIDATION_SECTION_ALIASES));
    let assumptions_body = section_body_for_aliases(&stripped, ASSUMPTIONS_SECTION_ALIASES)
        .or_else(|| labeled_body_for_aliases(&stripped, ASSUMPTIONS_SECTION_ALIASES));

    for (section, body) in [
        ("Summary", summary_body.as_ref()),
        ("Implementation Steps", (!implementation_blocks.is_empty()).then_some(&stripped)),
        ("Test Cases and Validation", validation_body.as_ref()),
        ("Assumptions and Defaults", assumptions_body.as_ref()),
    ] {
        if body.is_none() {
            report.missing_sections.push(section.to_string());
        }
    }

    if let Some(body) = summary_body.as_deref() {
        report.summary_present = !meaningful_section_lines(body).is_empty();
    }
    if !report.summary_present && !report.missing_sections.iter().any(|s| s == "Summary") {
        report.missing_sections.push("Summary".to_string());
    }

    report.implementation_step_count = implementation_blocks.len();
    report.invalid_implementation_steps = implementation_blocks
        .iter()
        .filter_map(|step| {
            implementation_step_shape_error(step).map(|reason| format!("step {}: {reason}", step.number))
        })
        .collect();
    if report.implementation_step_count == 0 && !report.missing_sections.iter().any(|s| s == "Implementation Steps") {
        report.missing_sections.push("Implementation Steps".to_string());
    }

    if let Some(body) = validation_body.as_deref() {
        let lines = meaningful_section_lines(body);
        report.validation_item_count = lines
            .iter()
            .filter(|line| is_numbered_line(line) || line.starts_with("- "))
            .count();
        if report.validation_item_count == 0 {
            report.validation_item_count = lines.len();
        }
    }
    if report.validation_item_count == 0 && !report.missing_sections.iter().any(|s| s == "Test Cases and Validation") {
        report.missing_sections.push("Test Cases and Validation".to_string());
    }

    if let Some(body) = assumptions_body.as_deref() {
        let lines = meaningful_section_lines(body);
        report.assumption_count = lines
            .iter()
            .filter(|line| is_numbered_line(line) || line.starts_with("- "))
            .count();
        if report.assumption_count == 0 {
            report.assumption_count = lines.len();
        }
    }
    if report.assumption_count == 0 && !report.missing_sections.iter().any(|s| s == "Assumptions and Defaults") {
        report.missing_sections.push("Assumptions and Defaults".to_string());
    }

    report
}

fn parse_bracket_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    split_bracket_items(trimmed)
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Split a bracket-list body on commas that sit outside single/double quotes.
///
/// Shell verifications routinely embed commas inside quoted `sed` address
/// ranges (`sed -n '/^## A/,/^## B/p'`) or `rg` alternations. A naive split
/// turns one concrete command into two fragments, and the first fragment
/// then fails verification as `verification item 1 must be a concrete
/// command or check` (session `session-vtcode-20260914T031505Z_075199-09813`,
/// step 4). Quote tracking keeps such commands intact.
pub fn split_bracket_items(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::with_capacity(body.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            // Backslash escapes the next char outside single quotes (shell:
            // literal inside `'…'`, escape inside `"…"` and unquoted so an
            // escaped comma/quote never splits or toggles).
            '\\' if !in_single => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ',' if !in_single && !in_double => {
                items.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    items.push(current);
    items
}

pub(super) fn tracker_has_progress_or_notes(tracker: &str) -> bool {
    let lower = tracker.to_ascii_lowercase();
    if lower.contains("## notes") {
        return true;
    }
    ["[x]", "[~]", "[!]", "[/]"].iter().any(|marker| lower.contains(marker))
}

pub fn generate_tracker_markdown_from_plan(plan_markdown: &str) -> Option<String> {
    let stripped = strip_embedded_tracker(plan_markdown);
    let implementation = section_body_for_aliases(&stripped, IMPLEMENTATION_SECTION_ALIASES).or_else(|| {
        let blocks = collect_implementation_step_blocks(&stripped, true);
        (!blocks.is_empty()).then(|| {
            blocks
                .into_iter()
                .filter_map(|block| block.lines.first().map(|line| format!("{}. {line}", block.number)))
                .collect::<Vec<_>>()
                .join("\n")
        })
    })?;
    let title = plan_markdown
        .lines()
        .find_map(|line| line.trim().strip_prefix("# ").map(str::trim))
        .filter(|line| !line.is_empty())
        .unwrap_or("Implementation Plan");

    let mut items = Vec::new();
    let mut seen_descriptions = HashSet::new();
    for line in implementation.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Some((_, description)) = numbered_line_parts(line) else {
            continue;
        };
        // Share arrow normalization with validation so an accepted `→`-style
        // step produces the same tracker segments the validator saw.
        let segments = step_action_segments(description);
        let main = segments.first().map(String::as_str).unwrap_or_default();
        if main.is_empty() {
            continue;
        }
        let description_key = main.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        if !seen_descriptions.insert(description_key) {
            continue;
        }

        let mut entry = format!("- [ ] {main}\n");
        for segment in segments.iter().skip(1) {
            // Reuse the validator's emphasis-tolerant marker parsing so an
            // accepted `**files:**`-style segment yields the same tracker
            // metadata the validator saw.
            if let Some(files) = marker_value(segment, &["files"]) {
                let values = parse_bracket_list(files);
                if !values.is_empty() {
                    entry.push_str(&format!("  files: {}\n", values.join(", ")));
                }
                continue;
            }
            if let Some(outcome) = marker_value(segment, &["outcome"]) {
                let outcome = outcome.trim().trim_start_matches('[').trim_end_matches(']');
                if !outcome.is_empty() {
                    entry.push_str(&format!("  outcome: {outcome}\n"));
                }
                continue;
            }
            if let Some(verify) = marker_value(segment, &["verify"]) {
                let values = parse_bracket_list(verify);
                if values.is_empty() {
                    let trimmed = verify.trim();
                    if !trimmed.is_empty() {
                        entry.push_str(&format!("  verify: {trimmed}\n"));
                    }
                } else {
                    for value in values {
                        entry.push_str(&format!("  verify: {value}\n"));
                    }
                }
            }
        }
        items.push(entry);
    }

    if items.is_empty() {
        return None;
    }

    Some(format!("# {}\n\n## Plan of Work\n\n{}", title, items.concat().trim_end()))
}

#[cfg(test)]
mod agentic_testing_tests {
    use super::{
        is_independent_rederivation, parse_bracket_list, split_bracket_items, validate_concrete_verification,
        validate_plan_content,
    };

    #[test]
    fn targeted_git_history_verifies_review_plan() {
        let plan = "## Summary\nReview the completed background-task work.\n\n## Implementation Steps\n1. Review completion changes -> files: [src/agent/runloop/unified/turn/session_loop_runner/background_completion.rs] -> verify: [git log -5 --oneline]\n2. Inspect the shipped commit -> files: [src/agent/runloop/unified/turn/session_loop_runner/support.rs] -> verify: [git show --stat HEAD]\n3. Compare the prior version -> files: [src/agent/runloop/unified/turn/session_loop_runner/support.rs] -> verify: [git diff HEAD~1 -- src/agent/runloop/unified/turn/session_loop_runner/support.rs]\n4. Trace ownership -> files: [src/agent/runloop/unified/turn/session_loop_runner/support.rs] -> verify: [git blame src/agent/runloop/unified/turn/session_loop_runner/support.rs]\n\n## Test Cases and Validation\nInspect selected history.\n\n## Assumptions and Defaults\nNo file edits are expected.";
        let report = validate_plan_content(plan);
        assert!(report.is_ready(), "targeted history checks should validate: {report:?}");
        assert!(validate_concrete_verification("run git show --stat HEAD").is_ok());
    }

    #[test]
    fn git_verification_rejects_mutation_and_unfocused_checks() {
        for verify in [
            "git log",
            "git show --stat",
            "git diff",
            "git diff --check",
            "git diff --check HEAD",
            "git log --grep=",
            "git reset --hard HEAD",
            "git checkout main",
            "git show HEAD; git reset --hard HEAD",
            "please run git reset --hard HEAD",
            "/usr/bin/git reset --hard HEAD",
            "command /usr/bin/git reset --hard HEAD",
            "git show HEAD\ngit reset --hard HEAD",
            "cargo check && git reset --hard HEAD",
            "cargo check; git reset --hard HEAD",
            "cargo check;git reset --hard HEAD",
            "cargo check\ngit reset --hard HEAD",
            "review history",
        ] {
            assert!(validate_concrete_verification(verify).is_err(), "must reject {verify}");
        }

        for verify in ["run git show --stat HEAD", "/usr/bin/git show --stat HEAD"] {
            assert!(validate_concrete_verification(verify).is_ok(), "must accept {verify}");
        }
    }

    #[test]
    fn independent_rederivation_counts_as_concrete_verification() {
        let verify = "independent-rederive bitstream order (fresh context, no helper reuse)";
        assert!(is_independent_rederivation(verify));
        assert!(validate_concrete_verification(verify).is_ok());
    }

    #[test]
    fn vague_verify_without_fresh_rederivation_still_rejected() {
        assert!(!is_independent_rederivation("verify later"));
        assert!(validate_concrete_verification("verify later").is_err());
    }

    #[test]
    fn vacuous_independent_compare_without_rederive_still_rejected() {
        // `compare` alone is manual verification (needs evidence), not an
        // independent re-derivation oracle.
        assert!(!is_independent_rederivation("independent compare fresh context"));
        assert!(!is_independent_rederivation("independent rederive fresh"));
    }

    #[test]
    fn bracket_split_keeps_quoted_commas_inside_one_item() {
        let items = split_bracket_items("rg -n 'a,b' README.md, cargo check --locked");
        assert_eq!(items.len(), 2);
        assert!(items[0].contains("rg -n"));
        assert!(items[1].contains("cargo check"));

        // Session session-vtcode-20260914T031505Z_075199-09813 step 4: the
        // comma inside the quoted sed range must not split the item.
        let single = parse_bracket_list(
            "[sed -n '/^## Documentation/,/^## Development/p' README.md | wc -l reports fewer lines]",
        );
        assert_eq!(single.len(), 1);
        assert!(
            validate_concrete_verification(&single[0]).is_ok(),
            "quoted-comma sed inspection command must be one concrete verify item"
        );

        let concrete = parse_bracket_list("[rg -n 'a,b' README.md]");
        assert_eq!(concrete.len(), 1);
        assert!(validate_concrete_verification(&concrete[0]).is_ok());
    }

    #[test]
    fn inspection_commands_count_as_concrete_verification() {
        // Live planning drafts (checkpoints turn_1234–1243) emitted sed/grep
        // verifies for README/docs changes and were rejected as
        // "verification item 1 must be a concrete command or check".
        for verify in [
            "sed -n '81,88p' README.md",
            "grep -n 'planning-workflow' docs/guides/planning-workflow.md",
            "head -40 docs/file.md",
            "tail -20 docs/file.md",
            "wc -l README.md",
            "wc README.md",
            "head README.md",
            "file src/main.rs",
            "ls src/",
            "cargo test",
        ] {
            assert!(
                validate_concrete_verification(verify).is_ok(),
                "inspection command should validate as concrete: {verify}"
            );
        }

        for invalid in [
            "run checks",
            "check later",
            "git diff --check",
            "review docs",
            // Ambiguous English command heads without flag/path evidence.
            "file changes",
            "sort order",
            "find files",
            "make sense",
            "ls files",
        ] {
            assert!(
                validate_concrete_verification(invalid).is_err(),
                "vague or VCS-only verify must stay rejected: {invalid}"
            );
        }
    }

    #[test]
    fn bracket_split_still_splits_unquoted_items_and_flags_vague_first_item() {
        let items = parse_bracket_list("[review docs, cargo check --locked]");
        assert_eq!(items.len(), 2);
        assert!(validate_concrete_verification(&items[0]).is_err());
        assert!(validate_concrete_verification(&items[1]).is_ok());
    }

    #[test]
    fn bracket_split_handles_backslash_escapes_outside_single_quotes() {
        // Escaped comma outside quotes stays inside one item.
        let items = split_bracket_items("foo\\,bar, baz");
        assert_eq!(items, vec!["foo\\,bar".to_string(), " baz".to_string()]);

        // Escaped quote inside double quotes neither toggles nor splits.
        let items = split_bracket_items("rg -n \"a\\\"b,c\", cargo check");
        assert_eq!(items.len(), 2);
        assert!(items[0].contains("rg -n"));
        assert!(items[1].contains("cargo check"));
    }
}
