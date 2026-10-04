//! System instructions and prompt management.
//!
//! Prompt variants share one canonical base contract plus thin mode deltas and
//! compact runtime addenda. Project-specific behavior comes from dynamically
//! loaded instruction maps (`AGENTS.md`/`CLAUDE.md`), dynamic tool guidance,
//! skill metadata, and runtime notices.

use crate::config::constants::prompt_budget as prompt_budget_constants;
use crate::config::types::ShellPromptProfile;
use crate::llm::providers::gemini::wire::Content;
use crate::prompts::context::PromptContext;
use crate::prompts::guidelines::{generate_tool_guidelines_for_profile, render_shell_profile_guidance};
use crate::prompts::output_styles::OutputStyleApplier;
use crate::prompts::render::render_environment_addenda;
use crate::prompts::resources::{apply_system_prompt_layers, resolve_system_prompt_layers};
pub use crate::prompts::static_prompts::{
    agent_identity_label, default_lightweight_prompt, default_system_prompt, lightweight_instruction_text,
    minimal_instruction_text, minimal_system_prompt, specialized_instruction_text, specialized_system_prompt,
    static_profile_prompt,
};
use crate::prompts::system_prompt_cache::PROMPT_CACHE;
use crate::skills::render::render_prompt_skills_section;
use std::path::Path;
use tracing::warn;
use vtcode_commons::estimate_tokens;

/// Shared Planning workflow header used by both static and incremental prompt builders.
pub const PLANNING_WORKFLOW_READ_ONLY_HEADER: &str = "# PLANNING WORKFLOW (READ-ONLY)";
/// Shared Planning workflow notice line describing strict read-only enforcement.
pub const PLANNING_WORKFLOW_READ_ONLY_NOTICE_LINE: &str = "Mutating file edits are blocked, including `apply_patch`. Use `exec_command.cmd` only for read-only repository inspection with the active shell profile's syntax; keep `task_tracker` current. Keep discovery commands static: for `find`, use literal paths and quoted patterns only; keep them free of `$()`, backticks, variable or brace expansion, and dynamically spliced options, and prefer `rg --files` when practical.";
/// Shared Planning workflow instruction line for transitioning to implementation.
pub const PLANNING_WORKFLOW_EXIT_INSTRUCTION_LINE: &str = "Only a validated plan persisted under `.vtcode/plans/` is ready for user approval. Mutating tools stay disabled until the user approves.";
/// Canonical contract for model-authored plan output and runtime-owned persistence.
pub const PLANNING_WORKFLOW_PLAN_PERSISTENCE_POLICY_LINE: &str = "Emit exactly one final `<proposed_plan>` block and no surrounding prose. Do not use shell commands or file-writing tools to create or modify `.vtcode/plans/`; runtime owns plan/tracker persistence and validation, and exposes approval controls only after successful persistence.";
/// Compact, spec-like plan quality line. The previous wording ("summary,
/// steps, test cases, assumptions") let the model emit verbosely large plans
/// that blew the generation token budget and were cut off mid-`<proposed_plan>`
/// — which previously re-triggered the recovery loop forever. This mandates a
/// tight spec that fits a small token budget and prefers file:symbol
/// references over prose. It also forbids wrapping those references in
/// markdown link syntax or editor/IDE URI schemes (e.g. `vscode-file://`,
/// `file://`) — plans are read in terminals and other non-hyperlink
/// surfaces, and a bare `path/to/file.rs:42` reference is portable while a
/// broken pseudo-link pointing at the editor binary itself is not.
/// The canonical one-line step format, mirrored from
/// `tools::handlers::planning_workflow::artifacts::CANONICAL_STEP_FORMAT`
/// (const contexts cannot concat!, so the sync is enforced by the
/// `plan_quality_line_shows_canonical_step_format` test below). Showing the
/// exact shape up front matters: the repair directive prints it only after a
/// rejection, and turn_912/913 showed planners repeatedly failing "step lacks
/// a concrete target or verification" without ever seeing an example. The
/// optional `## Expected Outcomes` / `## Dependencies and Prerequisites`
/// sections are requested only "when material" so plans carry outcomes and
/// prerequisites without inflating every plan past the token budget.
pub const PLANNING_WORKFLOW_PLAN_QUALITY_LINE: &str = "Keep the final proposed plan compact and spec-like, with these sections: `## Summary`; `## Scope` (In/Out — concrete surfaces changed vs explicit non-goals; Scope lines are plan context, never tracker steps); `## Implementation Steps` (or `## Steps`); `## Test Cases and Validation` (or `## Validation`); `## Assumptions and Defaults` (or `## Assumptions`). When material to the request, add `## Expected Outcomes` (observable end states the implementation must produce) and `## Dependencies and Prerequisites` (tooling, configuration, or prior work required before implementation); omit them when nothing material exists. Every numbered implementation step must name a concrete file, symbol, behavior, or other repository target and include one concrete `verify:`/`verification:` command or observable check, written in the canonical one-line form `1. Action -> files: [path/to/file.rs] -> verify: [cargo check]`; common inspection commands such as `sed -n`, `grep -n`, `rg -n`, and `wc` are valid when they are the command head with a flag or path-like argument (English-word heads like `file`/`sort`/`find` need that evidence too). Concrete read-only `git log`, `git show`, `git diff`, and `git blame` checks may verify review steps when they select a revision, path, or filter; bare Git commands and `git diff --check` do not. Reuse evidence already visible in the planning transcript and keep command output focused. Generic `1. Do the work` steps, vague prose, and comma-separated verify entries that are not commands or observable checks are not plans. Documentation checks such as `npx markdownlint-cli2 README.md` are concrete verification commands. Only Markdown-only steps may use unavailable lint as their sole check; code steps still require ordinary verification. List separate verification commands as comma-separated items, never as a semicolon chain. Commas inside single or double quotes stay inside one verify item. Prefer file:symbol references over prose, written as plain text or inline code (e.g. `src/main.rs:42`) — never as markdown links or editor/IDE URIs (no `[label](url)`, no `vscode-file://`/`file://` schemes). Resolve placeholders and open decisions before approval; use `Next open decision:` or `Open question:` only when a decision remains unresolved.";
/// Scale research effort to the request instead of always exhaustively
/// enumerating the repository. Checkpoint turn_647 showed a "make a simple
/// plan to improve launch time" request burn 70+ tool calls across dozens of
/// files until the turn's tool wall-clock budget was exhausted with no plan
/// delivered — the model had no signal to stop researching and draft. This
/// line gives it a concrete budget to self-regulate against.
pub const PLANNING_WORKFLOW_RESEARCH_SCOPE_LINE: &str = "Scale research to the request: for a narrow or simple ask, ~5-10 targeted reads/searches is usually enough before drafting `<proposed_plan>` — do not exhaustively enumerate the whole repository. For a broad or ambiguous ask, research proportionally more, but stop and draft as soon as scope/decomposition/verification decisions are closed.";
/// Shared Planning workflow policy line directing context-aware read-only research and plain-text question resolution.
pub const PLANNING_WORKFLOW_PLAN_POLICY_LINE: &str = "Continue exploring read-only, finish unblocked planning, and surface open decisions or questions directly in plain text. Monitor the available tool-loop budget; stop research when the plan is sufficiently specified or the limit is near, then synthesize one compact decision-ready plan from the evidence already gathered.";
pub const PLANNING_WORKFLOW_INTERVIEW_POLICY_LINE: &str = "Use repository evidence and reasonable engineering judgment to resolve ordinary ambiguity. Do not ask the user to choose files, implementation details, validation commands, or prioritization you can infer. Use `request_user_input` only for a critical blocker where proceeding could cause materially different, unsafe, or irreversible work; otherwise state the assumption and continue to the plan.";
pub const PLANNING_WORKFLOW_NO_REQUEST_USER_INPUT_POLICY_LINE: &str = "`request_user_input` is optional. If it is unavailable or denied, do not retry it: make reasonable assumptions, synthesize one valid plan from the evidence already gathered, and keep planning active until that plan is persisted and ready for approval.";
/// Shared Planning workflow guard line requiring explicit transition from planning to execution.
pub const PLANNING_WORKFLOW_NO_AUTO_EXIT_LINE: &str = "Do not auto-exit Planning workflow; wait for explicit implementation intent after a validated persisted plan exists.";
/// Shared Planning workflow task-tracking line clarifying availability and aliasing.
/// Implementation prompt used when transitioning from planning to execution.
pub const PLANNING_WORKFLOW_IMPLEMENTATION_PROMPT: &str = "Implement the approved plan. Finish with a concise execution summary covering outcome, changed files, verification performed, and remaining blockers.";
/// Hint shown when planning workflow is active.
pub const PLANNING_WORKFLOW_HINT: &str = "Planning workflow is active. Continue refining; approval controls appear only after a validated plan is persisted.";

pub const PLANNING_WORKFLOW_TASK_TRACKER_LINE: &str = "`task_tracker` remains available while planning.";
/// Shared reminder appended when presenting plans while still in Planning workflow.
pub const PLANNING_WORKFLOW_IMPLEMENT_REMINDER: &str = PLANNING_WORKFLOW_PLAN_PERSISTENCE_POLICY_LINE;

pub const PROMPT_TITLE: &str = "# VT Code";
/// Identity line. `apply_agent_identity` replaces the first `VT Code` in this
/// line with the active agent label, so the label must appear exactly once.
pub const PROMPT_INTRO: &str = "You are VT Code, a coding agent working in the user's repository and terminal.";
/// Product name that `apply_agent_identity` swaps for the agent label inside
/// [`PROMPT_TITLE`] and [`PROMPT_INTRO`].
const PROMPT_IDENTITY_NAME: &str = "VT Code";

/// Natural-language role framing inserted between the identity line and the
/// runtime guidance for the Default and Specialized profiles. It sets the
/// working posture and effort calibration; the concrete rules live in the
/// sections below it. Omitted from Minimal and Lightweight modes to respect
/// their compact budgets (see the parent-ratio guard in `subagents/config.rs`).
pub const PROMPT_ROLE_PARAGRAPH: &str = "Work the way a senior engineer on this codebase would: understand the relevant code before changing it, make the change the task calls for, and report what you actually observed. Scale effort to the ask. A quick question deserves a direct answer, and a multi-file change deserves a plan and real checks.";
pub const CONTRACT_HEADER: &str = "## Contract";

/// Contract rules shared across all prompt modes that are not universal
/// user-facing runtime guidance: state the harness must carry across
/// compaction. Grounding, honesty, and scope rules live in
/// `runtime_guidance::RUNTIME_GUIDANCE_SECTION` so they also survive a
/// workspace `system.md` override.
pub const SHARED_CONTRACT_LINES: &[&str] = &[
    "Across compaction, preserve the task goal, tracker state, touched files, verification status, and decisions made so far.",
];

/// Default/Lightweight/Specialized mode: extended working style beyond the
/// universal runtime guidance (instruction map, communication detail,
/// corrections, delegation, code style, and test design). Each rule has exactly one home;
/// do not restate runtime-guidance rules here.
pub const DEFAULT_SPECIFIC_LINES: &[&str] = &[
    "Start from the project instruction map (`AGENTS.md`/`CLAUDE.md`) and the code itself, and follow the conventions they show.",
    "Write updates and summaries for a teammate who is catching up: complete sentences, technical terms spelled out, and no fragments, arrow chains, or labels you invented along the way.",
    "Answer a simple question directly in prose. Use headers, lists, and tables only when the content has real structure.",
    "Correct an earlier statement only when the error changes the user's code, conclusions, or decisions, and do it in one plain sentence.",
    "Brief a subagent fully the first time, and use its findings rather than redoing the work.",
    "Match the surrounding code's naming, idiom, and comment density, and comment only on constraints the code cannot show.",
    "For tests, start from the risks: check boundaries and asymmetric cases from both sides, derive high-risk expected values without the code's own helpers, and assert observable behavior, not just the absence of a panic.",
];

/// Minimal mode has no additional contract lines; universal behavior lives in
/// the compiled runtime-guidance section shared by every profile.
pub const MINIMAL_SPECIFIC_LINES: &[&str] = &[];

/// Shared operating-profile sentences reused across modes.
///
/// These canonical wordings avoid drift between profiles that state the same
/// rule with slightly different phrasing. Mode deltas below must reuse them
/// verbatim; `operating_profile_deltas_share_canonical_sentences` enforces it.
pub const OPERATING_TASK_TRACKER: &str = "Track the work in `task_tracker` once it stops being trivial.";

pub const DEFAULT_OPERATING_PROFILE_DELTA: &str = r#"## Operating Profile

- The core tools are `exec_command`, `write_stdin`, and `apply_patch`; `code_search` becomes available in Planning workflow.
- Shell commands go in `exec_command.cmd` and are not separate tools. Follow the active shell profile's syntax.
- When the user asks for a change, make it with the tools rather than describing it, unless the active agent mode is read-only.
- Use Planning workflow for research and spec work, and stay read-only until the user states implementation intent."#;

pub const MINIMAL_OPERATING_PROFILE_DELTA: &str = r#"## Operating Profile

- Follow the project instruction map (`AGENTS.md`/`CLAUDE.md`).
- Track the work in `task_tracker` once it stops being trivial."#;

pub const LIGHTWEIGHT_OPERATING_PROFILE_DELTA: &str = r#"## Operating Profile

- This profile is for simple work: act directly in this thread and keep the loop short.
- Track the work in `task_tracker` once it stops being trivial."#;

pub const SPECIALIZED_OPERATING_PROFILE_DELTA: &str = r#"## Operating Profile

- This profile is for complex work: explore the relevant code, settle a plan, then execute it.
- Use `task_tracker` for multi-step work, and Planning workflow while scope or verification is still open.
- Stop only when the tracker state, verification results, and resumable state agree.
- End plan work with one `<proposed_plan>` block. During execution, re-plan only when the approved plan is stale; the runtime persists the new plan and continues.
- When repo-wide invariants matter, also read the architecture documents the instruction map points to."#;

const STRUCTURED_REASONING_INSTRUCTIONS: &str = r#"
## Structured Reasoning

When visible structure helps, you can tag your reasoning: `<analysis>` for facts and options, `<reasoning_plan>` for advisory steps, `<uncertainty>` for blockers, and `<verification>` for checks you ran. `<plan>` is reserved for the planning workflow's approval artifact. When code or tools will consume a decision, prefer JSON or a function call over prose.
"#;

/// System instruction configuration
#[derive(Debug, Clone, Default)]
pub struct SystemPromptConfig;

/// A named layer of the composed system prompt.
///
/// The token-budget trimmer (see [`SectionKind::trim_priority`]) drops whole
/// sections rather than truncating text mid-layer, so each section's text is
/// stored verbatim (including any leading/trailing whitespace baked into its
/// source constant) exactly as it would have been appended by the legacy
/// single-string builder.
struct PromptSection {
    kind: SectionKind,
    text: String,
}

/// Identifies which layer of the system prompt a `PromptSection` belongs to.
///
/// Variants mirror the layers `compose_system_instruction_text` actually
/// assembles today. Agent identity is not a separate variant: it is applied
/// as an in-place text substitution on the base contract (title/intro lines)
/// rather than an appended section, so it is folded into [`Self::BaseContract`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionKind {
    /// Compiled runtime guidance, canonical contract + operating profile (with
    /// any workspace prompt-layer override/append and agent-identity
    /// substitution already applied).
    /// Always present and never trimmed to satisfy the token budget.
    BaseContract,
    /// Optional `<analysis>/<reasoning_plan>/<uncertainty>/<verification>` tagging
    /// guidance. Advisory; trimmed first when over budget.
    StructuredReasoning,
    /// Lean "## Skills" routing section rendered from available skill
    /// metadata. Advisory; trimmed alongside structured reasoning.
    Skills,
    /// "## Environment" addenda (languages, interaction mode, MCP sources,
    /// temporal context, working directory).
    EnvironmentAddenda,
    /// "## Active Tools" dynamic tool guidance derived from the active tool
    /// catalog.
    ToolGuidelines,
    /// "## Shell Profile" guidance for the current command environment.
    ShellProfile,
}

impl SectionKind {
    /// Static section name used in [`SystemPromptReport::trimmed_sections`].
    const fn name(self) -> &'static str {
        match self {
            Self::BaseContract => "base_contract",
            Self::StructuredReasoning => "structured_reasoning",
            Self::Skills => "skills",
            Self::EnvironmentAddenda => "environment_addenda",
            Self::ToolGuidelines => "tool_guidelines",
            Self::ShellProfile => "shell_profile",
        }
    }

    /// Trim order: lower values are dropped first. `None` means the section
    /// is never dropped to satisfy the token budget.
    const fn trim_priority(self) -> Option<u8> {
        match self {
            Self::StructuredReasoning => Some(0),
            Self::Skills => Some(1),
            Self::EnvironmentAddenda => Some(2),
            // Shell safety guidance and the active-tool contract are required
            // for the model to use the available tools safely. They must never
            // disappear as a side effect of prompt budgeting.
            Self::ShellProfile | Self::ToolGuidelines => None,
            Self::BaseContract => None,
        }
    }

    /// Whether this section belongs to the stable instruction prefix.
    ///
    /// Runtime tool catalogs and environment observations are deliberately
    /// dynamic. Keeping this classification beside the section definition
    /// avoids relying on an earliest-header heuristic, which can accidentally
    /// make a later safety section part of (or outside) a cache boundary.
    const fn is_cache_stable(self) -> bool {
        matches!(self, Self::BaseContract | Self::StructuredReasoning | Self::Skills | Self::ShellProfile)
    }
}

/// Result of measuring a composed system prompt against the configured token
/// budget (`agent.max_system_prompt_tokens`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemPromptReport {
    /// `estimate_token_count` of the final composed text (after trimming, if
    /// trimming occurred).
    pub token_estimate: u64,
    /// Whether `token_estimate` exceeds `agent.max_system_prompt_tokens`.
    pub over_budget: bool,
    /// Names of sections dropped to satisfy the budget, in drop order. Empty
    /// unless `agent.trim_system_prompt` is enabled and trimming occurred.
    pub trimmed_sections: Vec<&'static str>,
}

impl SystemPromptReport {
    /// Measure `text` against `max_tokens` with no trimming applied.
    ///
    /// Useful when a system prompt was assembled or overridden outside the
    /// normal section-based pipeline (e.g. downstream embedders calling
    /// `AgentRunner::set_system_prompt`, or appendix text appended after
    /// [`compose_system_instruction_with_report`] already measured the
    /// sectioned prompt).
    #[must_use]
    pub fn measure(text: &str, max_tokens: u64) -> Self {
        let token_estimate = estimate_token_count(text);
        Self {
            token_estimate,
            over_budget: token_estimate > max_tokens,
            trimmed_sections: Vec::new(),
        }
    }
}

/// Compose the base system instruction plus compact tool/skill/environment addenda.
pub async fn compose_system_instruction_text(
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    prompt_context: Option<&PromptContext>,
) -> String {
    compose_system_instruction_with_report(project_root, vtcode_config, prompt_context)
        .await
        .0
}

/// Compose the system instruction and return the token-budget report
/// alongside it. See [`SystemPromptReport`] and `SectionKind::trim_priority`
/// for the budget/trim behavior driven by `agent.max_system_prompt_tokens`,
/// `agent.system_prompt_budget_warning`, and `agent.trim_system_prompt`.
pub async fn compose_system_instruction_with_report(
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    prompt_context: Option<&PromptContext>,
) -> (String, SystemPromptReport) {
    let (prompt, report, _) =
        compose_system_instruction_with_identity(project_root, vtcode_config, prompt_context).await;
    (prompt, report)
}

/// Compose a prompt and retain the stable instruction digest used by local and
/// provider-facing prompt caches.
async fn compose_system_instruction_with_identity(
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    prompt_context: Option<&PromptContext>,
) -> (String, SystemPromptReport, u64) {
    let sections = build_prompt_sections(project_root, vtcode_config, prompt_context).await;
    let instruction_digest = stable_prompt_sections_digest(&sections);
    let (max_tokens, warn_enabled, trim_enabled) = system_prompt_budget_settings(vtcode_config);
    let (prompt, report) = apply_token_budget(sections, max_tokens, warn_enabled, trim_enabled);
    (prompt, report, instruction_digest)
}

/// Measure the system prompt size without applying budget trimming or warnings.
///
/// This is used at startup to warn about potential token budget overruns
/// before the first request is made. Unlike [`compose_system_instruction_with_report`],
/// this function does not apply `agent.trim_system_prompt` and does not emit
/// budget-exceeded warnings.
pub async fn measure_system_prompt_size(
    project_root: &Path,
    vtcode_config: &crate::config::VTCodeConfig,
) -> SystemPromptReport {
    let sections = build_prompt_sections(project_root, Some(vtcode_config), None).await;
    let text = join_prompt_sections(&sections);
    let token_estimate = estimate_token_count(&text);
    SystemPromptReport {
        token_estimate,
        over_budget: token_estimate > vtcode_config.agent.max_system_prompt_tokens,
        trimmed_sections: Vec::new(),
    }
}

/// Resolve the effective `(max_system_prompt_tokens, budget_warning_enabled,
/// trim_enabled)` settings, falling back to the `AgentConfig` defaults when
/// no config is available.
fn system_prompt_budget_settings(vtcode_config: Option<&crate::config::VTCodeConfig>) -> (u64, bool, bool) {
    vtcode_config.map_or((prompt_budget_constants::DEFAULT_MAX_SYSTEM_PROMPT_TOKENS, true, true), |cfg| {
        (
            cfg.agent.max_system_prompt_tokens,
            cfg.agent.system_prompt_budget_warning,
            cfg.agent.trim_system_prompt,
        )
    })
}

/// Build the ordered prompt sections. Each section's text is stored exactly
/// as the legacy single-string builder would have appended it, so
/// [`join_prompt_sections`] reproduces byte-identical output when nothing is
/// trimmed.
async fn build_prompt_sections(
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    prompt_context: Option<&PromptContext>,
) -> Vec<PromptSection> {
    let prompt_mode = vtcode_config.map(|c| c.agent.system_prompt_mode).unwrap_or_default();
    let static_base_prompt = static_profile_prompt(prompt_mode);
    let resolved_layers = resolve_system_prompt_layers(project_root).await;
    let mut base_prompt = apply_system_prompt_layers(static_base_prompt, &resolved_layers);
    crate::prompts::runtime_guidance::ensure_runtime_guidance(&mut base_prompt);

    tracing::trace!(
        mode = ?prompt_mode,
        base_tokens = estimate_token_count(&base_prompt),
        "Selected system prompt mode"
    );

    // Apply agent identity based on the default primary agent configuration.
    // This combines "VT Code" with the active agent mode so the LLM knows its role.
    if let Some(cfg) = vtcode_config {
        let agent_label = agent_identity_label(&cfg.default_primary_agent);
        base_prompt = apply_agent_identity(&base_prompt, &agent_label);
    }

    let mut sections = vec![PromptSection { kind: SectionKind::BaseContract, text: base_prompt }];

    if should_include_structured_reasoning(vtcode_config) {
        sections.push(PromptSection {
            kind: SectionKind::StructuredReasoning,
            text: STRUCTURED_REASONING_INSTRUCTIONS.to_string(),
        });
    }

    let shell_profile = vtcode_config
        .map(|cfg| cfg.agent.shell_prompt_profile)
        .unwrap_or(ShellPromptProfile::Auto)
        .resolve_for_current_platform();
    sections.push(PromptSection {
        kind: SectionKind::ShellProfile,
        text: render_shell_profile_guidance(shell_profile),
    });

    if let Some(ctx) = prompt_context {
        // Prompt-caching discipline: static content first, dynamic last.
        // `Skills` is session-stable routing metadata while `ToolGuidelines`
        // derives from the live tool catalog (planning toggles, MCP refreshes),
        // so skills must precede tool guidelines. Otherwise every catalog
        // change would invalidate the cached skills section that follows it.
        if let Some(skills_section) = render_prompt_skills_section(&ctx.available_skill_metadata) {
            sections.push(PromptSection { kind: SectionKind::Skills, text: skills_section });
        }
        // Static prompts ship the parallel-call hint unconditionally; only the
        // runtime per-turn path re-resolves it against the provider.
        let guidelines =
            generate_tool_guidelines_for_profile(&ctx.available_tools, ctx.capability_level, shell_profile, true);
        if !guidelines.is_empty() {
            sections.push(PromptSection {
                kind: SectionKind::ToolGuidelines,
                text: guidelines.trim_start_matches('\n').to_string(),
            });
        }
    }

    if let Some(environment_section) = render_environment_addenda(vtcode_config, prompt_context) {
        sections.push(PromptSection {
            kind: SectionKind::EnvironmentAddenda,
            text: environment_section,
        });
    }

    sections
}

/// Join ordered prompt sections exactly as the legacy single-string builder
/// did: the first section verbatim, then each subsequent section separated
/// by a blank line.
fn join_prompt_sections(sections: &[PromptSection]) -> String {
    let capacity = sections.iter().map(|section| section.text.len() + 2).sum();
    let mut joined = String::with_capacity(capacity);
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            joined.push_str("\n\n");
        }
        joined.push_str(&section.text);
    }
    joined
}

/// Enforce the configured system-prompt token budget against the composed
/// sections.
///
/// When under budget, sections are joined and returned unchanged. When over
/// budget and `trim_enabled` is false, the full untrimmed text is still used
/// but a warning is logged (gated on `warn_enabled`). When over budget and
/// `trim_enabled` is true, whole sections are dropped in
/// [`SectionKind::trim_priority`] order (lowest first), re-measuring after
/// each drop, until the prompt fits or only untrimmable sections remain.
fn apply_token_budget(
    mut sections: Vec<PromptSection>,
    max_tokens: u64,
    warn_enabled: bool,
    trim_enabled: bool,
) -> (String, SystemPromptReport) {
    let mut text = join_prompt_sections(&sections);
    let mut token_estimate = estimate_token_count(&text);
    let mut trimmed_sections: Vec<&'static str> = Vec::new();

    if token_estimate > max_tokens {
        if trim_enabled {
            while token_estimate > max_tokens {
                let drop_index = sections
                    .iter()
                    .enumerate()
                    .filter_map(|(index, section)| section.kind.trim_priority().map(|priority| (priority, index)))
                    .min_by_key(|(priority, _)| *priority)
                    .map(|(_, index)| index);
                let Some(drop_index) = drop_index else {
                    break;
                };
                let dropped = sections.remove(drop_index);
                trimmed_sections.push(dropped.kind.name());
                text = join_prompt_sections(&sections);
                token_estimate = estimate_token_count(&text);
            }

            if !trimmed_sections.is_empty() {
                tracing::warn!(
                    token_estimate,
                    max_system_prompt_tokens = max_tokens,
                    dropped_sections = ?trimmed_sections,
                    "Trimmed system prompt sections to satisfy token budget"
                );
            }
        } else if warn_enabled {
            tracing::warn!(
                token_estimate,
                max_system_prompt_tokens = max_tokens,
                "System prompt exceeds configured token budget"
            );
        }
    }

    let report = SystemPromptReport {
        token_estimate,
        over_budget: token_estimate > max_tokens,
        trimmed_sections,
    };
    (text, report)
}

/// Hash only the explicitly stable prompt sections.
fn stable_prompt_sections_digest(sections: &[PromptSection]) -> u64 {
    let stable_sections = sections
        .iter()
        .filter(|section| section.kind.is_cache_stable())
        .map(|section| (section.kind.name(), section.text.as_str()))
        .collect::<Vec<_>>();
    crate::core::agent::hash_utils::hash_value(&stable_sections)
}

/// Apply agent identity to the system prompt by replacing the title and intro lines.
/// This combines the "VT Code" identity with the active agent mode so the LLM
/// knows its role (e.g., "VT Code (Build mode)" or "VT Code (Auto mode)").
fn apply_agent_identity(prompt: &str, agent_label: &str) -> String {
    let mut result = prompt.to_string();
    let old_title = PROMPT_TITLE;
    let labeled_title = old_title.replacen(PROMPT_IDENTITY_NAME, agent_label, 1);
    let old_intro = PROMPT_INTRO;

    let title_found = if let Some(pos) = result.find(old_title) {
        result.replace_range(pos..pos + old_title.len(), &labeled_title);
        true
    } else {
        warn!("Could not find prompt title '{}' to apply agent identity", old_title);
        false
    };

    let intro_found = if let Some(pos) = result.find(old_intro) {
        let labeled_intro = old_intro.replacen(PROMPT_IDENTITY_NAME, agent_label, 1);
        result.replace_range(pos..pos + old_intro.len(), &labeled_intro);
        true
    } else {
        warn!("Could not find prompt intro '{}' to apply agent identity", old_intro);
        false
    };

    if !title_found || !intro_found {
        warn!(
            agent_label = %agent_label,
            title_replaced = title_found,
            intro_replaced = intro_found,
            "agent identity partially applied"
        );
    }

    result
}

/// Structured reasoning tags are opt-in (`agent.include_structured_reasoning_tags`)
/// in every prompt mode; without a config there is nothing to opt in.
fn should_include_structured_reasoning(vtcode_config: Option<&crate::config::VTCodeConfig>) -> bool {
    vtcode_config.is_some_and(|cfg| cfg.agent.should_include_structured_reasoning_tags())
}

/// Generate the stable base system instruction with configuration-aware sections.
///
/// Note: This function maintains backward compatibility by not accepting prompt_context.
/// For enhanced prompts with dynamic guidelines, call `compose_system_instruction_text` directly.
pub async fn generate_system_instruction_with_config(
    config: &SystemPromptConfig,
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
) -> Content {
    let (content, _report) =
        generate_system_instruction_with_config_and_report(config, project_root, vtcode_config).await;
    content
}

/// Same as [`generate_system_instruction_with_config`] but also returns the
/// [`SystemPromptReport`] for the composed prompt, whether served from cache
/// or freshly built.
pub async fn generate_system_instruction_with_config_and_report(
    _config: &SystemPromptConfig,
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
) -> (Content, SystemPromptReport) {
    generate_system_instruction_with_context_and_report(_config, project_root, vtcode_config, None).await
}

/// Generate a system instruction using a context-aware cache identity.
///
/// Context-free and context-aware prompts intentionally use different cache
/// keys. This prevents a prompt assembled with workspace tools or skill
/// metadata from being returned to a caller that requested the static prompt.
pub async fn generate_system_instruction_with_context_and_report(
    _config: &SystemPromptConfig,
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    prompt_context: Option<&PromptContext>,
) -> (Content, SystemPromptReport) {
    let (built_instruction, built_report, instruction_digest) =
        compose_system_instruction_with_identity(project_root, vtcode_config, prompt_context).await;
    let cache_key = cache_key_for_identity(
        project_root,
        vtcode_config,
        instruction_digest,
        prompt_context_digest(prompt_context),
        0,
    );
    let (instruction, report) = match PROMPT_CACHE.get(&cache_key) {
        Some(cached) => cached,
        None => {
            let built = (built_instruction, built_report);
            PROMPT_CACHE.insert(cache_key, built.clone());
            built
        }
    };

    // Apply output style if configured
    let styled_instruction = apply_output_style(instruction, vtcode_config, project_root).await;
    (Content::system_text(styled_instruction), report)
}

/// Apply output style to a generated system instruction
pub async fn apply_output_style(
    instruction: String,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    project_root: &Path,
) -> String {
    if let Some(config) = vtcode_config {
        let output_style_applier = OutputStyleApplier::new();
        if let Err(e) = output_style_applier.load_styles_from_config(config, project_root).await {
            tracing::warn!("Failed to load output styles: {}", e);
            instruction // Return original if loading fails
        } else {
            output_style_applier
                .apply_style(&config.output_style.active_style, &instruction, config)
                .await
        }
    } else {
        instruction // Return original if no config
    }
}

/// Build a cache key for the system prompt.
///
/// `catalog_epoch` is the tool-catalog version at the time of the request. When
/// the tool set changes (e.g. planning workflow is toggled, MCP tools are refreshed), the
/// epoch advances and the old cached prompt is superseded rather than served stale.
#[cfg(test)]
fn cache_key(project_root: &Path, vtcode_config: Option<&crate::config::VTCodeConfig>, catalog_epoch: u64) -> String {
    let mode = vtcode_config.map(|cfg| cfg.agent.system_prompt_mode).unwrap_or_default();
    let instruction_digest =
        crate::core::agent::hash_utils::hash_value(&("context-free", format!("{mode:?}"), static_profile_prompt(mode)));
    cache_key_for_identity(project_root, vtcode_config, instruction_digest, 0, catalog_epoch)
}

/// Construct one cache key from independent prompt, context, configuration,
/// provider-capability, and catalog-epoch digests.
fn cache_key_for_identity(
    project_root: &Path,
    vtcode_config: Option<&crate::config::VTCodeConfig>,
    instruction_digest: u64,
    context_digest: u64,
    catalog_epoch: u64,
) -> String {
    let config_digest = prompt_config_digest(vtcode_config);
    let capability_digest = vtcode_config
        .map(|cfg| {
            let catalog = crate::config::models::model_catalog_entry(&cfg.agent.provider, &cfg.agent.default_model);
            crate::core::agent::hash_utils::PromptCapabilityIdentity::from_catalog(
                &cfg.agent.provider,
                &cfg.agent.default_model,
                Some(cfg.agent.reasoning_effort),
                0,
                catalog,
            )
            .digest()
        })
        .unwrap_or_else(|| {
            crate::core::agent::hash_utils::PromptCapabilityIdentity::from_catalog("default", "default", None, 0, None)
                .digest()
        });
    let catalog_epoch_digest = crate::core::agent::hash_utils::hash_value(&catalog_epoch);
    let project_digest = crate::core::agent::hash_utils::hash_value(&project_root.to_string_lossy().as_ref());

    format!(
        "sys_prompt:{project_digest:016x}:{instruction_digest:016x}:{config_digest:016x}:{context_digest:016x}:cap{capability_digest:016x}:catalog{catalog_epoch_digest:016x}"
    )
}

/// Digest configuration fields that affect prompt text or budget behavior.
fn prompt_config_digest(vtcode_config: Option<&crate::config::VTCodeConfig>) -> u64 {
    let Some(cfg) = vtcode_config else {
        return crate::core::agent::hash_utils::hash_value(&"default-config");
    };

    use std::hash::{Hash, Hasher};
    let mut hasher = crate::core::agent::hash_utils::StableHasher::new();
    cfg.agent.provider.hash(&mut hasher);
    cfg.agent.default_model.hash(&mut hasher);
    format!("{:?}", cfg.agent.reasoning_effort).hash(&mut hasher);
    cfg.agent.include_working_directory.hash(&mut hasher);
    cfg.agent.include_temporal_context.hash(&mut hasher);
    cfg.agent.temporal_context_use_utc.hash(&mut hasher);
    cfg.agent.include_structured_reasoning_tags.hash(&mut hasher);
    format!("{:?}", cfg.agent.system_prompt_mode).hash(&mut hasher);
    format!("{:?}", cfg.agent.tool_documentation_mode).hash(&mut hasher);
    format!("{:?}", cfg.agent.shell_prompt_profile).hash(&mut hasher);
    cfg.agent.max_system_prompt_tokens.hash(&mut hasher);
    cfg.agent.system_prompt_budget_warning.hash(&mut hasher);
    cfg.agent.trim_system_prompt.hash(&mut hasher);
    cfg.chat.ask_questions.enabled.hash(&mut hasher);
    cfg.mcp.enabled.hash(&mut hasher);
    cfg.prompt_cache.cache_friendly_prompt_shaping.hash(&mut hasher);
    cfg.default_primary_agent.hash(&mut hasher);
    hasher.finish()
}

/// Digest the prompt-bearing parts of a context without relying on pointer or
/// insertion order identity. Vectors are sorted because discovery order is not
/// a semantic part of the rendered prompt contract.
fn prompt_context_digest(prompt_context: Option<&PromptContext>) -> u64 {
    let Some(context) = prompt_context else {
        return crate::core::agent::hash_utils::hash_value(&"context-free");
    };

    let mut languages = context.languages.clone();
    languages.sort();
    let mut tools = context.available_tools.clone();
    tools.sort();
    let mut skills = context.available_skills.clone();
    skills.sort();
    let mut metadata = context
        .available_skill_metadata
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.description.clone(),
                skill.short_description.clone(),
                skill.path.clone(),
                skill.scope,
                skill.manifest.as_ref().map(|manifest| format!("{manifest:?}")),
            )
        })
        .collect::<Vec<_>>();
    metadata.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.3.cmp(&right.3)));

    let preferences = context.user_preferences.as_ref().map(|preferences| {
        let mut preferred_languages = preferences.preferred_languages.clone();
        preferred_languages.sort();
        let mut preferred_frameworks = preferences.preferred_frameworks.clone();
        preferred_frameworks.sort();
        (preferred_languages, preferences.coding_style.clone(), preferred_frameworks)
    });

    crate::core::agent::hash_utils::hash_value(&(
        context.workspace.as_ref(),
        languages,
        context.project_type.as_deref(),
        tools,
        skills,
        metadata,
        preferences,
        context.capability_level.map(|level| format!("{level:?}")),
        context.current_directory.as_ref(),
        context.editor_context.as_ref(),
    ))
}

/// Generate a minimal system instruction (pi-inspired, <1K tokens)
pub fn generate_minimal_instruction() -> Content {
    Content::system_text(minimal_instruction_text())
}

/// Generate a lightweight system instruction for simple operations
pub fn generate_lightweight_instruction() -> Content {
    Content::system_text(lightweight_instruction_text())
}

/// Generate a specialized system instruction for advanced operations
pub fn generate_specialized_instruction() -> Content {
    Content::system_text(specialized_instruction_text())
}

// ─── Token Estimation ────────────────────────────────────────────────────────

/// Estimate prompt tokens through the shared workspace tokenizer.
///
/// Keeping prompt budgeting on the common estimator makes prompt reports and
/// the other runtime token budgets use the same tokenization semantics.
#[must_use]
pub fn estimate_token_count(text: &str) -> u64 {
    estimate_tokens(text) as u64
}

#[cfg(test)]
mod tests;
