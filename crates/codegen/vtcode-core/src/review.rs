use anyhow::{Result, bail};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewTarget {
    CurrentDiff,
    LastDiff,
    Files(Vec<String>),
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSpec {
    pub target: ReviewTarget,
    pub style: Option<String>,
    pub instructions: Option<String>,
}

pub fn build_review_spec(
    last_diff: bool,
    target: Option<String>,
    files: Vec<String>,
    style: Option<String>,
) -> Result<ReviewSpec> {
    build_review_spec_with_instructions(last_diff, target, files, style, None)
}

pub fn build_review_spec_with_instructions(
    last_diff: bool,
    target: Option<String>,
    files: Vec<String>,
    style: Option<String>,
    instructions: Option<String>,
) -> Result<ReviewSpec> {
    if last_diff && target.is_some() {
        bail!("--last-diff cannot be combined with --target");
    }
    if last_diff && !files.is_empty() {
        bail!("--last-diff cannot be combined with explicit files");
    }
    if target.is_some() && !files.is_empty() {
        bail!("--target cannot be combined with explicit files");
    }

    let style = style.map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
    let instructions = instructions
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let target = if last_diff {
        ReviewTarget::LastDiff
    } else if let Some(target) = target {
        let target = target.trim();
        if target.is_empty() {
            bail!("--target cannot be empty");
        }
        ReviewTarget::Custom(target.to_string())
    } else if !files.is_empty() {
        let files = files
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if files.is_empty() {
            bail!("review files cannot be empty");
        }
        ReviewTarget::Files(files)
    } else {
        ReviewTarget::CurrentDiff
    };

    Ok(ReviewSpec { target, style, instructions })
}

/// Returns true only when the natural-language instructions explicitly ask the
/// reviewer to implement or apply fixes. `/review` stays read-only otherwise.
///
/// Matching is whole-word: substring matching would escalate ordinary prose
/// such as "review the implementation" (`implement` ⊂ `implementation`) or
/// "how we apply theming" (`apply them` ⊂ `apply theming`).
pub fn review_allows_mutation(spec: &ReviewSpec) -> bool {
    let Some(instructions) = spec.instructions.as_deref() else {
        return false;
    };
    let lowered = instructions.to_ascii_lowercase();
    let words = split_instruction_words(&lowered);
    // Intentionally narrow: "fix" alone (e.g. "focus on fixes") must not escalate.
    // Require an action verb directed at making the change.
    const MARKERS: &[&[&str]] = &[
        &["implement", "the", "fix"],
        &["implement", "fixes"],
        &["implement", "it"],
        &["implement", "them"],
        &["implement"],
        &["apply", "the", "fix"],
        &["apply", "fixes"],
        &["apply", "them"],
        &["go", "ahead", "and", "fix"],
        &["please", "fix"],
        &["fix", "it"],
        &["fix", "them"],
        &["fix", "all"],
        &["make", "the", "fix"],
        &["make", "the", "changes"],
    ];
    MARKERS.iter().any(|marker| contains_word_sequence(&words, marker))
}

fn split_instruction_words(lowered: &str) -> Vec<&str> {
    lowered
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn contains_word_sequence(words: &[&str], marker: &[&str]) -> bool {
    !marker.is_empty() && marker.len() <= words.len() && words.windows(marker.len()).any(|window| window == marker)
}

pub fn build_review_prompt(spec: &ReviewSpec) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are a code reviewer. Your job is to review code changes and provide actionable feedback.\n");
    match spec.instructions.as_deref() {
        Some(instructions) => {
            let _ = writeln!(prompt, "\n---\n\nInput: {instructions}\n\n---");
        }
        None => {
            prompt.push_str("\n---\n\nInput: (none - default review)\n\n---");
        }
    }
    prompt.push_str(
        "\n## Determining What to Review\n\n\
         Based on the input provided, determine which type of review to perform:\n\n\
         1. **No arguments (default)**: Review all uncommitted changes\n\
         - Run: `git diff` for unstaged changes\n\
         - Run: `git diff --cached` for staged changes\n\
         - Run: `git status --short` to identify untracked (net new) files\n\n\
         2. **Commit hash** (40-char SHA or short hash): Review that specific commit\n\
         - First verify with `git rev-parse --verify <sha>`; then run `git show <verified-sha>`\n\
         - Never interpolate free-form prose into `git show`. If verification fails, fall back to the default diff and say so.\n\n\
         3. **Branch name or range** (e.g. `feature`, `main..HEAD`): Compare current branch to the specified branch\n\
         - First verify with `git rev-parse --verify <branch>`; then run `git diff <verified-target>...HEAD`\n\
         - Never interpolate free-form prose into `git diff`. If verification fails, fall back to the default diff and say so.\n\n\
         4. **PR URL or number**: Review the pull request\n\
         - Run: `gh pr view <verified-number-or-url>` to get PR context\n\
         - Run: `gh pr diff <verified-number-or-url>` to get the diff\n\
         - Extract only the PR number/URL from the input; never pass prose as the argument.\n\n\
         Use best judgement when processing input. Free-form instructions after `/review` describe review *focus* (correctness, regressions, style); they are not shell arguments.\n",
    );
    prompt.push_str("\nExplicit target for this run:\n");
    match &spec.target {
        ReviewTarget::CurrentDiff => {
            prompt.push_str("- Target: review the current git diff in the workspace.\n");
        }
        ReviewTarget::LastDiff => {
            prompt.push_str("- Target: review the last committed git diff.\n");
        }
        ReviewTarget::Files(files) => {
            prompt.push_str("- Target: review these files:\n");
            for file in files {
                let _ = writeln!(prompt, "  - {file}");
            }
        }
        ReviewTarget::Custom(target) => {
            let _ = writeln!(prompt, "- Target: review `{target}`.");
        }
    }

    if let Some(style) = &spec.style {
        let _ = writeln!(prompt, "- Style: {style}.");
    }

    prompt.push_str(
        "\n## Gathering Context\n\n\
         **Diffs alone are not enough.** After getting the diff, read the entire file(s) being modified to understand the full context. Code that looks wrong in isolation may be correct given surrounding logic - and vice versa.\n\n\
         - Use the diff to identify which files changed\n\
         - Use `git status --short` to identify untracked files, then read their full contents\n\
         - Read the full file to understand existing patterns, control flow, and error handling\n\
         - Check for existing style guide or conventions files (CONVENTIONS.md, AGENTS.md, .editorconfig, etc.)\n",
    );

    prompt.push_str(
        "\n## What to Look For\n\n\
         **Bugs** - Your primary focus.\n\n\
         - Logic errors, off-by-one mistakes, incorrect conditionals\n\
         - If-else guards: missing guards, incorrect branching, unreachable code paths\n\
         - Edge cases: null/empty/undefined inputs, error conditions, race conditions\n\
         - Security issues: injection, auth bypass, data exposure\n\
         - Broken error handling that swallows failures, throws unexpectedly or returns error types that are not caught.\n\n\
         **Structure** - Does the code fit the codebase?\n\n\
         - Does it follow existing patterns and conventions?\n\
         - Are there established abstractions it should use but doesn't?\n\
         - Excessive nesting that could be flattened with early returns or extraction\n\n\
         **Performance** - Only flag if obviously problematic.\n\n\
         - O(n^2) on unbounded data, N+1 queries, blocking I/O on hot paths\n\n\
         **Behavior Changes** - If a behavioral change is introduced, raise it (especially if it's possibly unintentional).\n",
    );

    prompt.push_str(
        "\n## Reporting Findings\n\n\
         Report every suspected bug with the concrete scenario (inputs, state, or environment) where it fails and your confidence (high, medium, or low). A lower-confidence finding with a clear scenario still helps the reader decide what to check.\n\n\
         - Only review the changes - do not review pre-existing code that wasn't modified\n\
         - Use available tools (callers, tests, docs) to raise or lower your confidence before reporting\n\
         - Each finding needs a realistic scenario where it breaks; do not invent hypothetical problems\n\n\
         **Don't be a zealot about style.** When checking code against conventions:\n\n\
         - Verify the code is _actually_ in violation. Don't complain about else statements if early returns are already being used correctly.\n\
         - Some \"violations\" are acceptable when they're the simplest option. A `let` statement is fine if the alternative is convoluted.\n\
         - Excessive nesting is a legitimate concern regardless of other style choices.\n",
    );

    if review_allows_mutation(spec) {
        prompt.push_str(
            "\n## Fix Mode (explicitly requested)\n\n\
             The input explicitly asks you to implement fixes. After presenting ranked findings, you may apply justified DRY/KISS refactors, fix, test, and re-review until clean.\n",
        );
    } else {
        prompt.push_str(
            "\n## Requirements\n\n\
             - Review only. Do not modify files or run mutating commands.\n\
             - Fix, refactor, or implement changes only when the input explicitly asks for it (standalone word \"implement\", or phrases like \"apply the fix\" or \"fix it\"). Otherwise report findings only.\n\
             - Focus on bugs, regressions, security issues, performance issues, and missing tests.\n\
             - Present findings first, ordered by severity.\n\
             - Include concrete file paths and line numbers when possible.\n\
             - If there are no findings, say that explicitly.\n",
        );
    }

    prompt.push_str(
        "\n## Output\n\n\
         1. For each finding, state why it is a bug, the scenarios, environments, or inputs needed for it to arise, its severity, and your confidence.\n\
         2. Do not overstate severity; when severity depends on the scenario, say so up front.\n\
         3. Keep the tone neutral and specific: describe what the code does, not the author.\n\
         4. Write so the reader can quickly understand the issue without reading too closely.\n\
         5. Leave out praise and comments that do not help the reader act.\n",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::{ReviewSpec, ReviewTarget, build_review_prompt, build_review_spec, review_allows_mutation};

    #[test]
    fn review_spec_defaults_to_current_diff() {
        let spec = build_review_spec(false, None, Vec::new(), None).expect("spec");
        assert_eq!(
            spec,
            ReviewSpec {
                target: ReviewTarget::CurrentDiff,
                style: None,
                instructions: None
            }
        );
    }

    #[test]
    fn review_spec_rejects_conflicting_target_selectors() {
        let err = build_review_spec(true, Some("HEAD~1..HEAD".to_string()), Vec::new(), None)
            .expect_err("conflicting selectors should fail");

        assert!(err.to_string().contains("--last-diff"));
    }

    #[test]
    fn review_prompt_includes_files_and_style() {
        let spec = build_review_spec(
            false,
            None,
            vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
            Some("security".to_string()),
        )
        .expect("spec");
        let prompt = build_review_prompt(&spec);

        assert!(prompt.contains("Target: review these files"));
        assert!(prompt.contains("- src/main.rs"));
        assert!(prompt.contains("Style: security."));
        assert!(prompt.contains("Review only."));
    }

    #[test]
    fn review_prompt_asks_for_confidence_instead_of_certainty() {
        let spec = build_review_spec(false, None, Vec::new(), None).expect("spec");
        let prompt = build_review_prompt(&spec);

        assert!(prompt.contains("Report every suspected bug"));
        assert!(prompt.contains("your confidence (high, medium, or low)"));
        assert!(!prompt.contains("Be certain"));
        assert!(!prompt.contains("if you're unsure"));
    }

    #[test]
    fn review_prompt_never_interpolates_prose_into_shell_commands() {
        let spec = super::build_review_spec_with_instructions(
            false,
            None,
            Vec::new(),
            None,
            Some("Review the full diff and nearby code for correctness".to_string()),
        )
        .expect("spec");
        let prompt = build_review_prompt(&spec);

        assert!(prompt.contains("Input: Review the full diff"));
        assert!(!prompt.contains("git show Review the full diff"));
        assert!(!prompt.contains("git diff Review the full diff"));
        assert!(prompt.contains("they are not shell arguments"));
        assert!(prompt.contains("Review only."));
    }

    #[test]
    fn review_fix_escalation_requires_explicit_action_verb() {
        let read_only = super::build_review_spec_with_instructions(
            false,
            None,
            Vec::new(),
            None,
            Some("Review the full diff for correctness and regressions".to_string()),
        )
        .expect("spec");
        assert!(!review_allows_mutation(&read_only));
        assert!(build_review_prompt(&read_only).contains("Review only."));

        let fix_mode = super::build_review_spec_with_instructions(
            false,
            None,
            Vec::new(),
            None,
            Some("Review the full diff, then implement fixes for each one".to_string()),
        )
        .expect("spec");
        assert!(review_allows_mutation(&fix_mode));
        assert!(build_review_prompt(&fix_mode).contains("Fix Mode"));
    }

    #[test]
    fn review_fix_escalation_ignores_substring_false_positives() {
        // Asymmetric regression tests: "implement" ⊂ "implementation" and
        // "apply them" ⊂ "apply theming" must not escalate.
        for prose in [
            "Review the implementation for correctness",
            "Check how we apply theming in the TUI",
            "Focus on suffix handling and prefix fixes",
        ] {
            let spec =
                super::build_review_spec_with_instructions(false, None, Vec::new(), None, Some(prose.to_string()))
                    .expect("spec");
            assert!(!review_allows_mutation(&spec), "{prose:?} must stay read-only");
            assert!(build_review_prompt(&spec).contains("Review only."));
        }
    }
}
