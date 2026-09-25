use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::config::constants::tools;
use crate::config::types::{CapabilityLevel, ResolvedShellPromptProfile, ShellPromptProfile};
use crate::core::agent::harness_kernel::SessionToolCatalogSnapshot;
use crate::llm::provider::ToolDefinition;
use crate::prompts::sections::SectionBoundaryMode;
use crate::tools::registry::tool_groups;

const TOOL_EXEC_COMMAND: &str = tools::EXEC_COMMAND;
const TOOL_WRITE_STDIN: &str = tools::WRITE_STDIN;
const TOOL_CODE_SEARCH: &str = tools::CODE_SEARCH;
const TOOL_READ_FILE: &str = tools::READ_FILE;
const TOOL_LIST_FILES: &str = tools::LIST_FILES;
const TOOL_GREP_FILE: &str = tools::GREP_FILE;
const TOOL_APPLY_PATCH: &str = tools::APPLY_PATCH;
const TOOL_REQUEST_USER_INPUT: &str = tools::REQUEST_USER_INPUT;
const TOOL_TASK_TRACKER: &str = tools::TASK_TRACKER;
const TOOL_START_PLANNING: &str = tools::START_PLANNING;

/// Shared cross-turn resume pointer (invariant #22). The hint body itself stays
/// transient via `append_transient_turn_notes`; tool guidance only advertises
/// that a turn-start `Exec session resume:` note carries the live ids so a
/// resumed or compacted session needs zero identity reconstruction.
const CROSS_TURN_RESUME_HINT_CLAUSE: &str =
    "; a turn-start `Exec session resume:` hint carries the live ids when a prior turn ended mid-run.";

/// The single home of `start_planning` guidance, shared by the Minimal and
/// Default Active Tools. The tool itself asks the user before entering
/// planning (`require_confirmation`), except under full-auto or
/// skip-confirmations, so the model calls it rather than proposing it in prose.
const START_PLANNING_GUIDANCE_LINE: &str = "- For demanding, ambiguous, or multi-phase tasks, call `start_planning`; it asks the user before entering the read-only Planning workflow unless the session runs in full-auto or skips confirmations. Skip it for straightforward changes.";

/// Planning-workflow `task_tracker` index rules. While planning, the tracker
/// routes to the plan sidecar, which rejects index 0 (`index_path components
/// must be >= 1`); checklist-level `index: 0` completion exists only outside
/// planning, so neither line advertises it.
const PLANNING_TASK_TRACKER_COMPACT_LINE: &str = "- Keep blockers and verification open in `task_tracker`; updates use positive indices or index_path, and index 0 is invalid while planning.";
const PLANNING_TASK_TRACKER_INDEX_LINE: &str = "- Use `task_tracker` action=update with positive flat indices or positive hierarchical index_path values (index 0 is invalid while planning), and use items for bulk updates.";

/// Documentation density is independent of the tools a session may execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolGuidanceProfile {
    Minimal,
    Default,
}

impl ToolGuidanceProfile {
    #[must_use]
    pub fn resolve(
        context_tokens: usize,
        default_prompt_tokens: usize,
        max_prompt_tokens: usize,
        input_usd_per_token: Option<f64>,
        max_budget_usd: Option<f64>,
    ) -> Self {
        let exceeds_cost = input_usd_per_token.zip(max_budget_usd).is_some_and(|(price, budget)| {
            price.is_finite() && price >= 0.0 && budget.is_finite() && default_prompt_tokens as f64 * price > budget
        });
        if (context_tokens > 0 && context_tokens <= 32_000)
            || (max_prompt_tokens > 0 && default_prompt_tokens > max_prompt_tokens)
            || exceeds_cost
        {
            Self::Minimal
        } else {
            Self::Default
        }
    }
}

/// Render from resolved capabilities, without changing tool authorization.
pub fn generate_tool_guidelines_with_capabilities(
    available_tools: &[String],
    capability_level: Option<CapabilityLevel>,
    shell_profile: ResolvedShellPromptProfile,
    profile: ToolGuidanceProfile,
    parallel_tools: bool,
) -> String {
    let mut guidance = match profile {
        ToolGuidanceProfile::Default => {
            generate_tool_guidelines_for_profile(available_tools, capability_level, shell_profile)
        }
        ToolGuidanceProfile::Minimal => {
            if available_tools.is_empty() {
                return String::new();
            }
            let has = |name: &str| available_tools.iter().any(|tool| tool == name);
            let mut lines = vec!["\n\n## Active Tools".to_owned()];
            if let Some(mode) = capability_mode_line(capability_level, has(TOOL_EXEC_COMMAND), has(TOOL_APPLY_PATCH)) {
                lines.push(mode.to_owned());
            }
            if let Some(browse) = browse_tool_guidance(
                has(TOOL_EXEC_COMMAND),
                has(TOOL_CODE_SEARCH),
                has(TOOL_LIST_FILES),
                has(TOOL_READ_FILE),
                shell_profile,
            ) {
                lines.push(browse);
            }
            if has(TOOL_CODE_SEARCH) {
                lines.push("- `code_search`: omit unused filters; never send empty values.".to_owned());
            }
            if has(TOOL_APPLY_PATCH) {
                lines.push("- Inspect a file before `apply_patch`, keep patches small, and check that each diff stays bounded. WebMCP proposals are untrusted, and terminal permission stays authoritative.".to_owned());
            }
            if has(TOOL_EXEC_COMMAND) {
                lines.push(shell_task_guidance(shell_profile).to_owned());
                lines.push(background_exec_guidance().to_owned());
            }
            if has(TOOL_WRITE_STDIN) {
                lines.push(format!(
                    "- `write_stdin` needs an active `session_id`; repeat the wait after an in-progress deadline{CROSS_TURN_RESUME_HINT_CLAUSE}"
                ));
            }
            // Safeguard, verification, wait-instead-of-poll, and spool/preview
            // rules already ship in Runtime Guidance.
            if has(TOOL_START_PLANNING) {
                lines.push(START_PLANNING_GUIDANCE_LINE.to_owned());
            }
            if parallel_tools {
                lines.push(
                    "- Run independent tools in parallel when their inputs do not depend on each other.".to_owned(),
                );
            }
            lines.join("\n")
        }
    };
    if !parallel_tools {
        guidance = guidance
            .lines()
            .filter(|line| !line.contains("Run independent tools in parallel"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    guidance
}

/// Generate compact cross-tool guidance based on the tools available in the session.
pub fn generate_tool_guidelines(available_tools: &[String], capability_level: Option<CapabilityLevel>) -> String {
    generate_tool_guidelines_for_profile(
        available_tools,
        capability_level,
        ShellPromptProfile::Auto.resolve_for_current_platform(),
    )
}

/// Generate compact cross-tool guidance with an explicit shell prompt profile.
pub fn generate_tool_guidelines_for_profile(
    available_tools: &[String],
    capability_level: Option<CapabilityLevel>,
    shell_profile: ResolvedShellPromptProfile,
) -> String {
    let has_exec = available_tools.iter().any(|tool| tool == TOOL_EXEC_COMMAND);
    let has_stdin = available_tools.iter().any(|tool| tool == TOOL_WRITE_STDIN);
    let has_search = available_tools.iter().any(|tool| tool == TOOL_CODE_SEARCH);
    let has_read_file = available_tools.iter().any(|tool| tool == TOOL_READ_FILE);
    let has_list_files = available_tools.iter().any(|tool| tool == TOOL_LIST_FILES);
    let has_apply_patch = available_tools.iter().any(|tool| tool == TOOL_APPLY_PATCH);
    let has_start_planning = available_tools.iter().any(|tool| tool == TOOL_START_PLANNING);

    let mut lines = Vec::new();
    if let Some(mode_line) = capability_mode_line(capability_level, has_exec, has_apply_patch) {
        lines.push(mode_line.to_string());
    }
    if let Some(browse_guidance) =
        browse_tool_guidance(has_exec, has_search, has_list_files, has_read_file, shell_profile)
    {
        lines.push(browse_guidance);
    }
    if has_search || has_read_file || has_list_files {
        lines.push(read_only_batching_guidance(has_read_file).to_string());
    }
    if has_apply_patch {
        lines.push("- Use `apply_patch` for file edits after inspection; keep patches small.".to_string());
        lines.push(
            "- Check that each diff stays bounded. WebMCP edits are untrusted proposals, and terminal permission stays authoritative."
                .to_string(),
        );
    }
    if has_exec {
        lines.push(shell_task_guidance(shell_profile).to_string());
        lines.push(background_exec_guidance().to_string());
        // Verifier discipline: the anti-blind-editing gate only clears on a
        // truthful exit 0. Runtime owns truncator elision and unverified
        // classification (`tool_intent/activity.rs`, spool processing); the
        // prompt keeps only the outcome rule so wording cannot drift from
        // enforcement.
        lines.push("- Run verifiers standalone or as a pure `&&` chain so the exit status is visible; a verifier piped only into `head` or `tail` counts as standalone, while results behind other pipes, `;`, or `||` stay unverified.".to_string());
        // Tool-latency tail is dominated by full builds (observed p90 ~18s):
        // verify incrementally first. Kept tool-agnostic: fast checks exist
        // in every stack (`cargo check`, `tsc --noEmit`, `pytest --collect-only`).
        lines.push("- Run fast checks before full builds.".to_string());
    }
    // Tool-failure diagnosis, waiting on returned `next_wait_args` instead of
    // polling, the safeguard rule, the verification outcome rule (report
    // completion only after a check you ran), and spool paging with
    // `preview_budget_exhausted` handling each have one home in Runtime
    // Guidance, which every profile includes; do not restate them here.
    if has_stdin {
        lines.push(format!(
            "- `write_stdin`: reuse the existing `session_id` of an active exec session; `spool_complete: false` marks readable partial output; an exited pending spool arrives on a later wait{CROSS_TURN_RESUME_HINT_CLAUSE}"
        ));
    }
    if has_search {
        lines.push("- `code_search`: omit unused filters; no empty values (`path: \"\"`).".to_string());
        lines.push(code_search_guidance(has_exec, shell_profile));
    }
    if has_apply_patch || has_exec {
        lines.push(
            "- Build and Auto share tools and safety gates; Auto changes confirmation behavior only after explicit approval or full-auto policy."
                .to_string(),
        );
    }
    if has_search || has_exec {
        lines.push("- Run independent tools in parallel when inputs do not depend on each other.".to_string());
    }
    if has_start_planning {
        lines.push(START_PLANNING_GUIDANCE_LINE.to_string());
    }

    if lines.is_empty() {
        return String::new();
    }

    format!("\n\n## Active Tools\n{}", lines.join("\n"))
}

pub fn append_runtime_tool_prompt_sections(
    prompt: &mut String,
    tool_snapshot: &SessionToolCatalogSnapshot,
    include_catalog_metadata: bool,
) {
    append_runtime_tool_prompt_sections_for_profile(
        prompt,
        tool_snapshot,
        include_catalog_metadata,
        ShellPromptProfile::Auto.resolve_for_current_platform(),
    );
}

pub fn append_runtime_tool_prompt_sections_for_profile(
    prompt: &mut String,
    tool_snapshot: &SessionToolCatalogSnapshot,
    include_catalog_metadata: bool,
    shell_profile: ResolvedShellPromptProfile,
) {
    remove_prompt_section(prompt, "## Active Tools");
    remove_prompt_section(prompt, "[Runtime Tool Catalog]");
    while prompt.ends_with('\n') {
        prompt.pop();
    }

    let available_tools = snapshot_tool_names(tool_snapshot);
    let guidelines =
        generate_runtime_tool_guidelines_for_profile(&available_tools, tool_snapshot.planning_active, shell_profile);
    if !guidelines.is_empty() {
        append_prompt_block(prompt, guidelines.trim_start_matches('\n'));
    }

    if include_catalog_metadata && tool_snapshot.snapshot.is_some() {
        let active_tools = if tool_snapshot.active_tool_names.is_empty() {
            "none".to_string()
        } else {
            tool_snapshot.active_tool_names.join(", ")
        };
        let catalog_metadata = format!(
            "[Runtime Tool Catalog]\n- version: {}\n- epoch: {}\n- catalog_tools: {}\n- available_tools: {}\n- currently_available_tools: {}\n- request_user_input_enabled: {}",
            tool_snapshot.version,
            tool_snapshot.epoch,
            tool_snapshot.catalog_tools(),
            tool_snapshot.available_tools(),
            active_tools,
            tool_snapshot.request_user_input_enabled,
        );
        append_prompt_block(prompt, &catalog_metadata);
    }
}

/// Select documentation density using the active route and session budgets.
pub fn append_runtime_tool_prompt_sections_for_model(
    prompt: &mut String,
    tool_snapshot: &SessionToolCatalogSnapshot,
    include_catalog_metadata: bool,
    shell_profile: ResolvedShellPromptProfile,
    provider: &dyn crate::llm::provider::LLMProvider,
    model: &str,
    config: Option<&crate::config::VTCodeConfig>,
) {
    append_runtime_tool_prompt_sections_for_profile(prompt, tool_snapshot, include_catalog_metadata, shell_profile);
    let pricing = crate::config::models::model_catalog_entry(provider.name(), model).map(|entry| entry.pricing);
    let profile = ToolGuidanceProfile::resolve(
        crate::compaction::effective_context_budget(config, provider, model),
        vtcode_commons::estimate_tokens(prompt),
        config.map_or(0, |cfg| cfg.agent.max_system_prompt_tokens as usize),
        pricing.and_then(|price| price.input),
        config.and_then(|cfg| cfg.agent.harness.max_budget_usd),
    );
    let parallel_tools = provider.supports_parallel_tool_config(model);
    // The detailed planning contract contains no parallel-call hint and remains
    // intact in Default. Minimal retains the same read-only and output contract.
    if tool_snapshot.planning_active && profile == ToolGuidanceProfile::Default {
        return;
    }
    remove_prompt_section(prompt, "## Active Tools");
    let names = snapshot_tool_names(tool_snapshot);
    let capability_level = Some(infer_capability_level(&names));
    let mut guidance =
        generate_tool_guidelines_with_capabilities(&names, capability_level, shell_profile, profile, parallel_tools);
    if tool_snapshot.planning_active {
        append_minimal_planning_addendum(&mut guidance, &names);
    }
    append_prompt_block(prompt, guidance.trim_start_matches('\n'));
}

/// Planning addendum for the compact (Minimal) tool guidance, where the
/// detailed planning contract is dropped to fit the budget.
fn append_minimal_planning_addendum(guidance: &mut String, names: &[String]) {
    guidance.push_str("\n- Planning is read-only. Stop research when the plan is specified or the budget is near; emit one `<proposed_plan>` block with concrete targets and verification for each step.");
    let read_tools = [TOOL_READ_FILE, TOOL_GREP_FILE, TOOL_CODE_SEARCH, TOOL_LIST_FILES]
        .into_iter()
        .filter(|tool| names.iter().any(|name| name == tool))
        .collect::<Vec<_>>();
    if read_tools.is_empty() {
        guidance.push_str("\n- Keep inspections small: keep `max_output_tokens` small, avoid batching multiple large inspections in parallel; start git history with `git log --oneline` before targeted `git show --stat`.");
    } else {
        guidance.push_str(&format!(
            "\n- Keep inspections small: prefer `{}` over `exec_command` shell reads; keep `max_output_tokens` small, avoid batching multiple large inspections in parallel; start git history with `git log --oneline` before targeted `git show --stat`.",
            read_tools.join("`/`")
        ));
    }
    if names.iter().any(|name| name == TOOL_TASK_TRACKER) {
        guidance.push('\n');
        guidance.push_str(PLANNING_TASK_TRACKER_COMPACT_LINE);
    }
    if names.iter().any(|name| name == TOOL_REQUEST_USER_INPUT) {
        guidance.push_str(
            "\n- Use `request_user_input` only for material blockers remaining after repository exploration.",
        );
    }
}

/// Append a compact summary of tools omitted from a client-local wire payload.
///
/// The listing is bounded like the `## Skills` routing section: at most
/// `DEFERRED_TOOLS_MAX_GROUPS` groups are named and the rest collapse into one
/// overflow line, so MCP-heavy sessions cannot bloat every request. Group
/// descriptions are server-provided and unbounded, so each line is truncated
/// to `DEFERRED_TOOLS_MAX_DESC_CHARS` characters.
pub fn append_deferred_tools_prompt_section(prompt: &mut String, tools: &[ToolDefinition]) {
    remove_prompt_section(prompt, "[Deferred Tools]");

    let mut groups: Vec<_> = tool_groups(tools)
        .into_iter()
        .filter(|group| group.deferred_count > 0)
        .collect();
    let overflow = groups.len().saturating_sub(DEFERRED_TOOLS_MAX_GROUPS);
    groups.truncate(DEFERRED_TOOLS_MAX_GROUPS);

    let mut lines: Vec<String> = groups
        .into_iter()
        .map(|group| {
            let description = truncate_deferred_description(group.description.as_deref().unwrap_or_default());
            format!("- {} ({} tools): {}", group.name, group.deferred_count, description)
        })
        .collect();
    if overflow > 0 {
        lines.push(format!("(+{overflow} more deferred groups available — use `search_tools` to find them)"));
    }

    let unnamespaced_deferred = tools
        .iter()
        .filter(|tool| tool.namespace.is_none() && tool.defer_loading == Some(true))
        .count();
    if unnamespaced_deferred > 0 {
        lines.push(format!("- {unnamespaced_deferred} additional deferred tools"));
    }

    if lines.is_empty() {
        return;
    }

    let section = format!(
        "[Deferred Tools]\n{}\nUse `search_tools` to find a deferred capability. Selected definitions become available in the next request segment.",
        lines.join("\n")
    );
    append_prompt_block(prompt, &section);
}

/// Maximum deferred-tool groups named in the `[Deferred Tools]` prompt
/// section before the rest collapse into one overflow line.
const DEFERRED_TOOLS_MAX_GROUPS: usize = 8;
/// Maximum characters of a group description in the section. Mirrors the
/// `## Skills` line truncation: server-provided text must not bloat the prompt.
const DEFERRED_TOOLS_MAX_DESC_CHARS: usize = 120;

fn truncate_deferred_description(description: &str) -> String {
    if description.chars().count() <= DEFERRED_TOOLS_MAX_DESC_CHARS {
        return description.to_string();
    }
    let truncated: String = description.chars().take(DEFERRED_TOOLS_MAX_DESC_CHARS).collect();
    format!("{}...", truncated.trim_end())
}

fn append_prompt_block(prompt: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }

    if prompt.is_empty() {
        prompt.push_str(block);
    } else {
        let _ = write!(prompt, "\n\n{block}");
    }
}

fn remove_prompt_section(prompt: &mut String, section_header: &str) {
    while let Some((section_start, section_end)) = find_prompt_section_bounds(prompt, section_header) {
        prompt.replace_range(section_start..section_end, "");
    }
}

fn find_prompt_section_bounds(prompt: &str, section_header: &str) -> Option<(usize, usize)> {
    crate::prompts::sections::find_prompt_section_bounds(prompt, section_header, SectionBoundaryMode::BracketOrMarkdown)
}

fn generate_runtime_tool_guidelines_for_profile(
    available_tools: &[String],
    planning_active: bool,
    shell_profile: ResolvedShellPromptProfile,
) -> String {
    if !planning_active {
        return generate_tool_guidelines_for_profile(available_tools, None, shell_profile);
    }

    let has_exec = available_tools.iter().any(|tool| tool == TOOL_EXEC_COMMAND);
    let has_search = available_tools.iter().any(|tool| tool == TOOL_CODE_SEARCH);
    let has_read_file = available_tools.iter().any(|tool| tool == TOOL_READ_FILE);
    let has_list_files = available_tools.iter().any(|tool| tool == TOOL_LIST_FILES);
    let has_request_user_input = available_tools.iter().any(|tool| tool == TOOL_REQUEST_USER_INPUT);
    let has_task_tracker = available_tools.iter().any(|tool| matches!(tool.as_str(), TOOL_TASK_TRACKER));

    let mut lines = vec!["- Planning workflow active: stay within the read-safe tool list.".to_string()];
    lines.push("- Monitor the available planning tool-loop budget; stop research when the plan is specified or the limit is near, then synthesize one compact decision-ready plan from existing evidence.".to_string());
    lines.push("- Every implementation step in the final plan must name a concrete repository target and include a concrete verification command or observable check.".to_string());
    lines.push("- When the plan is ready, emit only one `<proposed_plan>` block; do not repeat planning policy text or add surrounding prose.".to_string());
    if let Some(browse_guidance) =
        browse_tool_guidance(has_exec, has_search, has_list_files, has_read_file, shell_profile)
    {
        lines.push(browse_guidance);
    }
    if has_exec {
        lines.push("- In Planning workflow, use `exec_command` only for read-only verification.".to_string());
    }
    if has_search {
        lines.push("- `code_search`: omit unused filters; no empty values (`path: \"\"`).".to_string());
    }
    if has_task_tracker {
        lines.push("- Keep `task_tracker` updated as you refine the plan.".to_string());
        lines.push("- Keep blockers and verification open in `task_tracker` until resolved.".to_string());
        lines.push(PLANNING_TASK_TRACKER_INDEX_LINE.to_string());
    }
    if has_request_user_input {
        lines.push(
            "- Use `request_user_input` only for material blockers that remain after repository exploration."
                .to_string(),
        );
    }
    if has_search || has_exec {
        lines.push("- If calls repeat without progress, tighten the plan instead of retrying identically.".to_string());
    }

    format!("\n\n## Active Tools\n{}", lines.join("\n"))
}

fn snapshot_tool_names(tool_snapshot: &SessionToolCatalogSnapshot) -> Vec<String> {
    tool_snapshot
        .active_tool_names
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn browse_tool_guidance(
    has_exec: bool,
    has_search: bool,
    has_list_files: bool,
    has_read_file: bool,
    shell_profile: ResolvedShellPromptProfile,
) -> Option<String> {
    if has_exec {
        return Some(shell_browse_guidance(shell_profile, has_search));
    }

    if !(has_search || has_list_files || has_read_file) {
        return None;
    }

    Some("- Use available read-only repository tools for browsing; do not modify files.".to_string())
}

pub fn render_shell_profile_guidance(shell_profile: ResolvedShellPromptProfile) -> String {
    match shell_profile {
        ResolvedShellPromptProfile::UnixLike => {
            "## Shell Profile\n- Active shell profile: `unix_like`. Use Unix-like command syntax in `exec_command.cmd`, for example `ls`, `rg`, `find`, `cat`, `sed`, and `awk`.\n- On macOS, write BSD-compatible flags for BSD tools. VT Code does not rewrite GNU flags for macOS BSD tools.\n- The shell profile controls prompt examples and expected command syntax only; command policy, sandboxing, and approvals remain separate runtime checks.\n- VT Code does not translate GNU-to-BSD, BSD-to-GNU, Unix-to-PowerShell, or PowerShell-to-Unix command flags.".to_string()
        }
        ResolvedShellPromptProfile::PowerShell => {
            "## Shell Profile\n- Active shell profile: `powershell`. Use native PowerShell syntax in `exec_command.cmd`, for example `Get-ChildItem`, `Select-String`, `Get-Content`, and `Where-Object`.\n- On native Windows, use WSL when you need Unix-like workflows or Unix command examples.\n- The shell profile controls prompt examples and expected command syntax only; command policy, sandboxing, and approvals remain separate runtime checks.\n- VT Code does not translate GNU-to-BSD, BSD-to-GNU, Unix-to-PowerShell, or PowerShell-to-Unix command flags.".to_string()
        }
    }
}

fn shell_browse_guidance(shell_profile: ResolvedShellPromptProfile, has_search: bool) -> String {
    // Sessions show `exec_command` + `rg` crowding out `code_search`
    // (observed 654 exec vs 15 code_search in one run): `rg` scans text
    // while `code_search` resolves definitions and exact usages, so route
    // code search to the dedicated tool whenever it is available.
    const SEARCH_PREFERENCE_UNIX: &str = " Prefer `code_search` over `rg`/`grep` for code.";
    const SEARCH_PREFERENCE_POWERSHELL: &str = " Prefer `code_search` over `Select-String` for code.";
    match shell_profile {
        ResolvedShellPromptProfile::UnixLike => {
            let mut line = if has_search {
                "- Use `exec_command.cmd` with `ls`, `find`, `cat`, `sed`, and `awk` for repository browsing."
                    .to_string()
            } else {
                "- Use `exec_command.cmd` with `ls`, `rg`, `find`, `cat`, `sed`, and `awk` for repository browsing."
                    .to_string()
            };
            if has_search {
                line.push_str(SEARCH_PREFERENCE_UNIX);
            }
            line
        }
        ResolvedShellPromptProfile::PowerShell => {
            let mut line = "- Use `exec_command.cmd` with native PowerShell commands such as `Get-ChildItem`, `Select-String`, `Get-Content`, and `Where-Object` for repository browsing.".to_string();
            if has_search {
                line.push_str(SEARCH_PREFERENCE_POWERSHELL);
            }
            line
        }
    }
}

fn shell_task_guidance(shell_profile: ResolvedShellPromptProfile) -> &'static str {
    match shell_profile {
        ResolvedShellPromptProfile::UnixLike => {
            "- Use `exec_command.cmd` for build tools, test tools, `git diff -- <path>`, and shell-only tasks. In one-shot `exec_command` calls, do not use `!!`, `!$`, `!ssh`, or `fc`; write full command arguments explicitly from conversation or tool results. Interactive shells: review-safe history expansion (Bash `histverify`, zsh `HIST_VERIFY`)."
        }
        ResolvedShellPromptProfile::PowerShell => {
            "- Use `exec_command.cmd` for build tools, test tools, `git diff -- <path>`, and shell-only tasks using native PowerShell syntax."
        }
    }
}

fn background_exec_guidance() -> &'static str {
    "- For long-lived commands, set `background: true` on `exec_command`; it returns a bounded preview plus a stable `session_id` and wait arguments. At most three live background processes are retained per runtime, with no automatic eviction; reuse the session operations to wait, poll, write, inspect, terminate, or close."
}

fn read_only_batching_guidance(has_read_file: bool) -> &'static str {
    if has_read_file {
        "- Batch independent read-only calls; use bounded `read_file` ranges, order dependencies, serialize mutations; narrow the range on `line_truncated`."
    } else {
        "- Batch independent read-only calls; order dependent reads, and serialize mutations."
    }
}

fn code_search_guidance(has_exec: bool, _shell_profile: ResolvedShellPromptProfile) -> String {
    const BASE: &str = "- Advanced `code_search` takes `query`; filters `path`, `file_types`, `result_types`, `max_results`; results: definitions, exact syntactic usages. Queries use literal smart-case and `|`-separated literals; truncated: narrow. Example: `{\"query\":\"TurnLoop\",\"path\":\"src\",\"result_types\":[\"definition\"]}`. Do not JSON-encode arrays or integers as strings. Prefer `code_search` over `rg` on `.vtcode/context/tool_outputs/`.";
    if has_exec {
        format!("{BASE} Use `exec_command` or a skill for syntax patterns.")
    } else {
        BASE.to_string()
    }
}

fn capability_mode_line(
    capability_level: Option<CapabilityLevel>,
    has_exec: bool,
    has_file: bool,
) -> Option<&'static str> {
    match capability_level {
        Some(CapabilityLevel::Basic) => {
            Some("- Capabilities: limited. Ask the user to enable more capabilities if file work is required.")
        }
        Some(CapabilityLevel::FileReading | CapabilityLevel::FileListing) => {
            Some("- Capabilities: read-only. Analyze and search, but do not modify files or run shell commands.")
        }
        _ if !has_exec && !has_file => {
            Some("- Capabilities: read-only. Analyze and search, but do not modify files or run shell commands.")
        }
        _ => None,
    }
}

/// Infer capability level from available tools.
pub fn infer_capability_level(available_tools: &[String]) -> CapabilityLevel {
    let has_search = available_tools.iter().any(|t| t == TOOL_CODE_SEARCH);
    let has_edit = available_tools.iter().any(|t| t == TOOL_APPLY_PATCH);
    let has_read = has_edit || available_tools.iter().any(|t| t == TOOL_READ_FILE);
    let has_list = has_search || available_tools.iter().any(|t| t == TOOL_LIST_FILES);
    let has_exec = available_tools.iter().any(|t| t == TOOL_EXEC_COMMAND);

    if has_search {
        CapabilityLevel::CodeSearch
    } else if has_edit {
        CapabilityLevel::Editing
    } else if has_exec {
        CapabilityLevel::Bash
    } else if has_list {
        CapabilityLevel::FileListing
    } else if has_read {
        CapabilityLevel::FileReading
    } else {
        CapabilityLevel::Basic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Universal rules have one home in Runtime Guidance (or the shared
    /// contract). Compose every static profile with each tool-guidance variant
    /// and the Harness Limits section, and check each rule marker lands once.
    #[test]
    fn universal_rules_have_one_home_across_composed_prompt_sections() {
        use crate::config::types::SystemPromptMode;
        use crate::prompts::harness_limits::upsert_harness_limits_section;
        use crate::prompts::static_prompts::static_profile_prompt;
        use crate::prompts::system::PLANNING_WORKFLOW_READ_ONLY_NOTICE_LINE;

        let shell = ResolvedShellPromptProfile::UnixLike;
        let execution_tools = [
            TOOL_EXEC_COMMAND,
            TOOL_WRITE_STDIN,
            TOOL_APPLY_PATCH,
            TOOL_CODE_SEARCH,
            TOOL_TASK_TRACKER,
            TOOL_START_PLANNING,
            TOOL_REQUEST_USER_INPUT,
        ]
        .map(str::to_owned)
        .to_vec();
        let planning_tools = [
            TOOL_EXEC_COMMAND,
            TOOL_WRITE_STDIN,
            TOOL_CODE_SEARCH,
            TOOL_GREP_FILE,
            TOOL_TASK_TRACKER,
            TOOL_REQUEST_USER_INPUT,
        ]
        .map(str::to_owned)
        .to_vec();
        let mut minimal_planning = generate_tool_guidelines_with_capabilities(
            &planning_tools,
            None,
            shell,
            ToolGuidanceProfile::Minimal,
            false,
        );
        append_minimal_planning_addendum(&mut minimal_planning, &planning_tools);
        let variants = [
            ("default", generate_tool_guidelines_for_profile(&execution_tools, None, shell), false),
            (
                "minimal",
                generate_tool_guidelines_with_capabilities(
                    &execution_tools,
                    None,
                    shell,
                    ToolGuidanceProfile::Minimal,
                    true,
                ),
                false,
            ),
            ("default planning", generate_runtime_tool_guidelines_for_profile(&planning_tools, true, shell), true),
            ("minimal planning", minimal_planning, true),
        ];
        // Each marker names one universal rule that lives in Runtime Guidance
        // or the shared contract and must not be restated by tool sections.
        let markers = [
            "never claim a check passed",
            "diagnose it and change approach",
            "rather than polling",
            "small ranges",
            "preview_budget_exhausted",
            "additional_permissions",
            "bypass safeguards",
            "Delegate only sizeable",
            "Across compaction",
        ];

        for mode in [
            SystemPromptMode::Default,
            SystemPromptMode::Minimal,
            SystemPromptMode::Lightweight,
            SystemPromptMode::Specialized,
        ] {
            for (variant, guidance, planning) in &variants {
                let mut prompt = static_profile_prompt(mode).to_owned();
                if *planning {
                    prompt.push_str("\n\n");
                    prompt.push_str(PLANNING_WORKFLOW_READ_ONLY_NOTICE_LINE);
                }
                prompt.push_str(guidance);
                upsert_harness_limits_section(&mut prompt, 32, 600, 2);
                for marker in markers {
                    assert_eq!(
                        prompt.matches(marker).count(),
                        1,
                        "{mode:?} with {variant} tool guidance should state {marker:?} exactly once"
                    );
                }
                let start_planning_mentions = prompt.matches("start_planning").count();
                assert_eq!(start_planning_mentions, usize::from(!*planning), "{mode:?} with {variant} tool guidance");
            }
        }
    }

    #[test]
    fn documentation_profile_respects_context_tokens_and_known_cost() {
        assert_eq!(ToolGuidanceProfile::resolve(32_000, 100, 1000, None, None), ToolGuidanceProfile::Minimal);
        assert_eq!(ToolGuidanceProfile::resolve(1_000_000, 100, 1000, None, Some(0.0)), ToolGuidanceProfile::Default);
        assert_eq!(ToolGuidanceProfile::resolve(1_000_000, 1001, 1000, None, None), ToolGuidanceProfile::Minimal);
        assert_eq!(
            ToolGuidanceProfile::resolve(1_000_000, 1000, 2000, Some(0.00001), Some(0.001)),
            ToolGuidanceProfile::Minimal
        );
    }

    #[test]
    fn minimal_and_default_tool_guidance_snapshots() {
        let tools = vec![TOOL_READ_FILE.to_owned()];
        let minimal = generate_tool_guidelines_with_capabilities(
            &tools,
            None,
            ResolvedShellPromptProfile::UnixLike,
            ToolGuidanceProfile::Minimal,
            false,
        );
        assert_eq!(
            minimal,
            "\n\n## Active Tools\n- Capabilities: read-only. Analyze and search, but do not modify files or run shell commands.\n- Use available read-only repository tools for browsing; do not modify files."
        );
        let default = generate_tool_guidelines_with_capabilities(
            &tools,
            None,
            ResolvedShellPromptProfile::UnixLike,
            ToolGuidanceProfile::Default,
            false,
        );
        assert_eq!(
            default,
            "\n\n## Active Tools\n- Capabilities: read-only. Analyze and search, but do not modify files or run shell commands.\n- Use available read-only repository tools for browsing; do not modify files.\n- Batch independent read-only calls; use bounded `read_file` ranges, order dependencies, serialize mutations; narrow the range on `line_truncated`."
        );
    }

    #[test]
    fn tool_guidance_uses_actual_parallel_capabilities_for_three_families() {
        use crate::config::constants::models;
        use crate::llm::provider::LLMProvider;
        use crate::llm::providers::{AnthropicProvider, GeminiProvider, OpenAIProvider};
        let providers: [(Box<dyn LLMProvider>, &str); 3] = [
            (Box::new(OpenAIProvider::new("offline-fixture".into())), models::openai::DEFAULT_MODEL),
            (Box::new(AnthropicProvider::new("offline-fixture".into())), models::anthropic::DEFAULT_MODEL),
            (Box::new(GeminiProvider::new("offline-fixture".into())), models::google::DEFAULT_MODEL),
        ];
        for (provider, model) in providers {
            for profile in [ToolGuidanceProfile::Minimal, ToolGuidanceProfile::Default] {
                let parallel = provider.supports_parallel_tool_config(model);
                let text = generate_tool_guidelines_with_capabilities(
                    &[TOOL_EXEC_COMMAND.to_owned()],
                    None,
                    ResolvedShellPromptProfile::UnixLike,
                    profile,
                    parallel,
                );
                assert_eq!(text.contains("tools in parallel"), parallel);
                let serial = generate_tool_guidelines_with_capabilities(
                    &[TOOL_EXEC_COMMAND.to_owned()],
                    None,
                    ResolvedShellPromptProfile::UnixLike,
                    profile,
                    false,
                );
                assert!(!serial.contains("tools in parallel"));
            }
        }
    }

    #[test]
    fn test_read_only_capability_detection() {
        let tools = vec![TOOL_CODE_SEARCH.to_string()];
        let guidelines = generate_tool_guidelines(&tools, None);
        assert!(guidelines.contains("Capabilities: read-only"));
        assert!(guidelines.contains("do not modify files"));
    }

    #[test]
    fn test_tool_preference_guidance() {
        let tools = vec![TOOL_EXEC_COMMAND.to_string(), TOOL_CODE_SEARCH.to_string()];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);
        assert!(guidelines.contains("Advanced `code_search` takes `query`"));
        assert!(guidelines.contains("literal smart-case"));
        assert!(guidelines.contains("exact syntactic usages"));
        assert!(guidelines.contains("\"result_types\":[\"definition\"]"));
        assert!(guidelines.contains("Do not JSON-encode arrays or integers as strings"));
        assert!(guidelines.contains("omit unused filters"));
        assert!(guidelines.contains("path: \"\""));
        assert!(guidelines.contains("git diff -- <path>"));
        assert!(guidelines.contains("build tools"));
        assert!(guidelines.contains("test tools"));
        // Search steering: with both tools present the browse line must
        // prefer `code_search` over `rg` for code (session evidence showed
        // `rg`-via-exec crowding out `code_search` 654:15).
        assert!(guidelines.contains("Prefer `code_search` over `rg`/`grep` for code"));
        // Latency steering: fast checks before full builds (tool-agnostic).
        assert!(guidelines.contains("Run fast checks before full builds"));
        // Completion-as-checkpoint guidance lives in the operating profiles;
        // the guidelines section no longer repeats it.
        assert!(!guidelines.contains("Completion is a checkpoint"));
    }

    #[test]
    fn test_edit_workflow_guidance() {
        let tools = vec![TOOL_APPLY_PATCH.to_string()];
        let guidelines = generate_tool_guidelines(&tools, None);
        assert!(guidelines.contains("Use `apply_patch`"));
        assert!(guidelines.contains("patches small"));
        // Completion-as-checkpoint guidance lives in the operating profiles;
        // the guidelines section no longer repeats it.
        assert!(!guidelines.contains("verification resolved"));
    }

    #[test]
    fn test_vt_code_guidance_omits_task_tracker() {
        let tools = vec![
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_WRITE_STDIN.to_string(),
            TOOL_APPLY_PATCH.to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);

        assert!(guidelines.contains("exec_command.cmd"));
        for command in ["ls", "rg", "find", "cat", "sed", "awk"] {
            assert!(
                guidelines.contains(&format!("`{command}`")),
                "{command} should be shown as an exec_command.cmd example"
            );
        }
        assert!(guidelines.contains("`write_stdin`"));
        assert!(guidelines.contains("At most three live background processes"));
        assert!(guidelines.contains("`apply_patch`"));
        assert!(!guidelines.contains("task_tracker"));
        assert!(!guidelines.contains("list_files"));
        assert!(!guidelines.contains("read_file"));
    }

    #[test]
    fn task_tracker_guidance_explains_action_aware_indices() {
        let guidelines = generate_runtime_tool_guidelines_for_profile(
            &[TOOL_TASK_TRACKER.to_string()],
            true,
            ResolvedShellPromptProfile::UnixLike,
        );

        assert!(guidelines.contains(&format!("\n{PLANNING_TASK_TRACKER_INDEX_LINE}")));
        assert!(PLANNING_TASK_TRACKER_INDEX_LINE.contains("positive flat indices"));
        assert!(PLANNING_TASK_TRACKER_INDEX_LINE.contains("(index 0 is invalid while planning)"));
        assert!(PLANNING_TASK_TRACKER_INDEX_LINE.contains("use items for bulk updates"));
        // The planning sidecar rejects index 0, so no planning line may present
        // it as a valid checklist-completion index.
        for line in [PLANNING_TASK_TRACKER_INDEX_LINE, PLANNING_TASK_TRACKER_COMPACT_LINE] {
            assert!(line.contains("index 0 is invalid while planning"), "{line}");
            assert!(!line.contains("reserved"), "{line}");
            assert!(!line.contains("index: 0"), "{line}");
        }
    }

    #[test]
    fn unix_like_guidance_makes_command_reuse_explicit() {
        let tools = vec![TOOL_EXEC_COMMAND.to_string(), TOOL_WRITE_STDIN.to_string()];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);

        assert!(guidelines.contains("one-shot `exec_command` calls"));
        assert!(guidelines.contains("`!!`, `!$`, `!ssh`, or `fc`"));
        assert!(guidelines.contains("write full command arguments explicitly"));
        assert!(guidelines.contains("conversation or tool results"));
        assert!(guidelines.contains("existing `session_id`"));
        assert!(guidelines.contains("background: true"));
        assert!(guidelines.contains("Bash `histverify`"));
        assert!(guidelines.contains("zsh `HIST_VERIFY`"));
        // Cross-turn resume (invariant #22): the live id arrives via a
        // turn-start `Exec session resume:` hint when a prior turn ended mid-run.
        assert!(guidelines.contains("`Exec session resume:`"));
        assert!(guidelines.contains("prior turn ended mid-run"));
        // No `code_search` in this profile: the search-preference clause
        // must not spend budget naming an unavailable tool.
        assert!(!guidelines.contains("Prefer `code_search` over `rg`"));
    }

    #[test]
    fn write_stdin_guidance_advertises_cross_turn_resume_hint() {
        let tools = vec![TOOL_WRITE_STDIN.to_string()];
        let default_guidance = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);
        assert!(default_guidance.contains("`Exec session resume:`"));
        assert!(default_guidance.contains("prior turn ended mid-run"));

        let minimal = generate_tool_guidelines_with_capabilities(
            &tools,
            None,
            ResolvedShellPromptProfile::UnixLike,
            ToolGuidanceProfile::Minimal,
            false,
        );
        assert!(minimal.contains("`Exec session resume:`"));
        assert!(minimal.contains("prior turn ended mid-run"));
    }

    #[test]
    fn powershell_guidance_uses_native_command_examples() {
        let tools = vec![
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
            TOOL_APPLY_PATCH.to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::PowerShell);

        assert!(guidelines.contains("native PowerShell commands"));
        assert!(guidelines.contains("`Get-ChildItem`"));
        assert!(guidelines.contains("`Select-String`"));
        assert!(guidelines.contains("native PowerShell syntax"));
        assert!(guidelines.contains("Prefer `code_search` over `Select-String` for code"));
        assert!(guidelines.contains("Advanced `code_search` takes `query`"));
        assert!(guidelines.contains("literal smart-case"));
        assert!(guidelines.contains("omit unused filters"));
        assert!(!guidelines.contains("`ls`, `rg`, `find`, `cat`, `sed`, and `awk`"));
        assert!(!guidelines.contains("shell history expansion"));
        assert!(!guidelines.contains("histverify"));
        assert!(!guidelines.contains("HIST_VERIFY"));
    }

    #[test]
    fn shell_profile_prompt_keeps_policy_and_syntax_separate() {
        let unix = render_shell_profile_guidance(ResolvedShellPromptProfile::UnixLike);
        assert!(unix.contains("Active shell profile: `unix_like`"));
        assert!(unix.contains("does not rewrite GNU flags for macOS BSD tools"));
        assert!(unix.contains("controls prompt examples and expected command syntax only"));
        assert!(unix.contains("does not translate GNU-to-BSD"));

        let powershell = render_shell_profile_guidance(ResolvedShellPromptProfile::PowerShell);
        assert!(powershell.contains("Active shell profile: `powershell`"));
        assert!(powershell.contains("WSL"));
        assert!(powershell.contains("Unix-like workflows"));
        assert!(powershell.contains("PowerShell-to-Unix"));
    }

    #[test]
    fn test_harness_browse_tool_guidance() {
        let tools = vec![TOOL_LIST_FILES.to_string(), TOOL_READ_FILE.to_string()];
        let guidelines = generate_tool_guidelines(&tools, None);
        assert!(guidelines.contains("available read-only repository tools"));
        assert!(guidelines.contains("bounded `read_file` ranges"));
        assert!(!guidelines.contains("list_files"));
        assert!(!guidelines.contains("offset"));
        assert!(!guidelines.contains("per_page"));
    }

    #[test]
    fn test_canonical_browse_tool_guidance_prefers_public_tools() {
        let tools = vec![
            TOOL_CODE_SEARCH.to_string(),
            TOOL_LIST_FILES.to_string(),
            "read_file".to_string(),
        ];
        let guidelines = generate_tool_guidelines(&tools, None);
        assert!(guidelines.contains("available read-only repository tools"));
        assert!(guidelines.contains("code_search"));
        assert!(guidelines.contains("bounded `read_file` ranges"));
    }

    #[test]
    fn test_capability_basic_guidance() {
        let tools = vec![];
        let guidelines = generate_tool_guidelines(&tools, Some(CapabilityLevel::Basic));
        assert!(guidelines.contains("Capabilities: limited"));
        assert!(guidelines.contains("enable more capabilities"));
    }

    #[test]
    fn test_capability_file_reading_guidance() {
        let tools = vec![TOOL_APPLY_PATCH.to_string()];
        let guidelines = generate_tool_guidelines(&tools, Some(CapabilityLevel::FileReading));
        assert!(guidelines.contains("Capabilities: read-only"));
        assert!(guidelines.contains("do not modify"));
    }

    #[test]
    fn test_full_capabilities_no_special_guidance() {
        let tools = vec![
            TOOL_APPLY_PATCH.to_string(),
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(
            &tools,
            Some(CapabilityLevel::Editing),
            ResolvedShellPromptProfile::UnixLike,
        );

        assert!(!guidelines.contains("Capabilities: limited"));
        assert!(!guidelines.contains("Capabilities: read-only"));
    }

    #[test]
    fn test_empty_tools_shows_read_only_capabilities() {
        let tools = vec![];
        let guidelines = generate_tool_guidelines(&tools, None);
        assert!(guidelines.contains("Capabilities: read-only"));
    }

    #[test]
    fn test_planning_workflow_guidance_keeps_verification_open() {
        let tools = vec![
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_TASK_TRACKER.to_string(),
            TOOL_CODE_SEARCH.to_string(),
        ];
        let guidelines =
            generate_runtime_tool_guidelines_for_profile(&tools, true, ResolvedShellPromptProfile::UnixLike);
        assert!(guidelines.contains("Keep `task_tracker` updated"));
        assert!(guidelines.contains("blockers and verification open"));
    }

    #[test]
    fn test_capability_inference_precedence() {
        let tools = vec![TOOL_APPLY_PATCH.to_string(), TOOL_CODE_SEARCH.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::CodeSearch);

        let tools = vec![TOOL_EXEC_COMMAND.to_string(), TOOL_APPLY_PATCH.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::Editing);
    }

    #[test]
    fn test_capability_inference_variants() {
        let tools = vec![TOOL_APPLY_PATCH.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::Editing);

        let tools = vec![TOOL_EXEC_COMMAND.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::Bash);

        let tools = vec![TOOL_CODE_SEARCH.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::CodeSearch);

        let tools = vec![TOOL_LIST_FILES.to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::FileListing);

        let tools = vec!["read_file".to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::FileReading);

        let tools = vec!["unknown_tool".to_string()];
        assert_eq!(infer_capability_level(&tools), CapabilityLevel::Basic);
    }

    #[test]
    fn test_guidelines_stay_compact() {
        let tools = vec![
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
            "read_file".to_string(),
            TOOL_LIST_FILES.to_string(),
            "apply_patch".to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);
        assert!(guidelines.contains("Batch independent read-only calls"));
        assert!(guidelines.contains("code_search"));
        // Shipped verifier discipline: every exec-capable profile must carry
        // the truthful-status outcome rule (standalone/pure-`&&`, a pure
        // `head`/`tail` truncator counts as standalone, other pipes stay
        // unverified), matching `VERIFIER_SHELL_FORM_NOTE`. Elision and `max_output_tokens` detail
        // lives in runtime enforcement, not prompt text.
        assert!(guidelines.contains("stay unverified"));
        assert!(guidelines.contains("pure `&&`"));
        assert!(!guidelines.contains("elided"));
        assert!(!guidelines.contains("max_output_tokens"));
        assert!(guidelines.contains("Build and Auto share tools and safety gates"));
        let approx_tokens = vtcode_commons::estimate_tokens(&guidelines);
        // The batching, bounded-diff, and verifier-discipline guardrails are
        // intentionally part of the compact shared prompt. Raised from 500 so
        // the verifier rule can state its reason (a visible exit status).
        assert!(approx_tokens < 520, "got ~{approx_tokens} tokens");
    }

    #[test]
    fn deferred_tools_section_caps_groups_and_truncates_descriptions() {
        use crate::llm::provider::ToolNamespace;

        fn deferred_mcp_tool(server: &str, tool: &str, description: &str) -> ToolDefinition {
            let mut definition = ToolDefinition::function(
                format!("mcp__{server}__{tool}"),
                "deferred".to_string(),
                serde_json::json!({"type": "object"}),
            );
            definition.namespace = Some(ToolNamespace {
                name: server.to_string(),
                description: description.to_string(),
            });
            definition.defer_loading = Some(true);
            definition
        }

        let mut tools = Vec::new();
        for index in 0..10 {
            tools.push(deferred_mcp_tool(&format!("server-{index:02}"), "search", "Tools provided by MCP server"));
        }
        // One group with an unbounded server-provided description.
        tools.push(deferred_mcp_tool("server-long", "search", &"d".repeat(500)));

        let mut prompt = "Base prompt".to_string();
        append_deferred_tools_prompt_section(&mut prompt, &tools);

        assert!(prompt.contains("[Deferred Tools]"));
        assert!(prompt.contains("server-00 (1 tools)"));
        assert!(prompt.contains("server-07 (1 tools)"));
        assert!(!prompt.contains("server-08 (1 tools)"), "groups past the cap must collapse into overflow");
        assert!(!prompt.contains("server-09 (1 tools)"));
        assert!(
            prompt.contains("(+3 more deferred groups available"),
            "10 named groups + 1 long group over the 8-group cap overflows by 3"
        );
        assert!(!prompt.contains(&"d".repeat(121)), "group descriptions must stay truncated");
        assert!(prompt.contains("Use `search_tools` to find a deferred capability"));

        // Idempotent: re-appending replaces the section instead of duplicating it.
        append_deferred_tools_prompt_section(&mut prompt, &tools);
        assert_eq!(prompt.matches("[Deferred Tools]").count(), 1);
    }

    #[test]
    fn deferred_tools_section_omits_empty_and_fully_loaded_groups() {
        let mut prompt = "Base prompt".to_string();
        append_deferred_tools_prompt_section(&mut prompt, &[]);
        assert!(!prompt.contains("[Deferred Tools]"));

        let loaded = ToolDefinition::function(
            "exec_command".to_string(),
            "Shell".to_string(),
            serde_json::json!({"type": "object"}),
        );
        let mut prompt = "Base prompt".to_string();
        append_deferred_tools_prompt_section(&mut prompt, &[loaded]);
        assert!(!prompt.contains("[Deferred Tools]"));
    }

    #[test]
    fn test_parallel_tool_call_guidance() {
        let tools = vec![
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
            TOOL_APPLY_PATCH.to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);
        assert!(guidelines.contains("parallel"), "Should include parallel tool call guidance");
        assert!(guidelines.contains("inputs do not depend"), "Should mention independent inputs");
    }

    #[test]
    fn test_read_only_batching_guidance_is_explicit() {
        let tools = vec![
            TOOL_CODE_SEARCH.to_string(),
            TOOL_READ_FILE.to_string(),
            TOOL_LIST_FILES.to_string(),
        ];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);

        assert!(guidelines.contains("Batch independent read-only calls"));
        assert!(guidelines.contains("`read_file` ranges"));
        assert!(guidelines.contains("serialize mutations"));
        // No exec tools in this profile: the verifier truthful-status rule is
        // exec-conditional and must not spend budget here.
        assert!(!guidelines.contains("stay unverified"));
    }

    #[test]
    fn execution_agents_can_suggest_planning_for_demanding_tasks() {
        let tools = vec![TOOL_START_PLANNING.to_string(), TOOL_EXEC_COMMAND.to_string()];
        let guidelines = generate_tool_guidelines_for_profile(&tools, None, ResolvedShellPromptProfile::UnixLike);

        assert_eq!(guidelines.matches(START_PLANNING_GUIDANCE_LINE).count(), 1);
        let minimal = generate_tool_guidelines_with_capabilities(
            &tools,
            None,
            ResolvedShellPromptProfile::UnixLike,
            ToolGuidanceProfile::Minimal,
            false,
        );
        assert_eq!(minimal.matches(START_PLANNING_GUIDANCE_LINE).count(), 1);
        // Without the tool, no profile mentions it.
        let without = generate_tool_guidelines_for_profile(
            &[TOOL_EXEC_COMMAND.to_string()],
            None,
            ResolvedShellPromptProfile::UnixLike,
        );
        assert!(!without.contains("start_planning"));
    }

    #[test]
    fn planning_workflow_runtime_guidance_keeps_exec_read_only() {
        let tools = vec![
            TOOL_APPLY_PATCH.to_string(),
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
        ];
        let guidelines =
            generate_runtime_tool_guidelines_for_profile(&tools, true, ResolvedShellPromptProfile::UnixLike);

        assert!(guidelines.contains("Planning workflow active"));
        assert!(guidelines.contains("`exec_command` only for read-only verification"));
        assert!(guidelines.contains("concrete repository target"));
        assert!(guidelines.contains("emit only one `<proposed_plan>` block"));
        assert!(guidelines.contains("omit unused filters"));
        assert!(!guidelines.contains("Inspect before edit"));
    }

    #[test]
    fn runtime_tool_guidance_uses_explicit_powershell_profile() {
        let tools = vec![
            TOOL_APPLY_PATCH.to_string(),
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
        ];
        let guidelines =
            generate_runtime_tool_guidelines_for_profile(&tools, false, ResolvedShellPromptProfile::PowerShell);

        assert!(guidelines.contains("native PowerShell commands"));
        assert!(guidelines.contains("`Get-ChildItem`"));
        assert!(guidelines.contains("`Select-String`"));
        assert!(guidelines.contains("native PowerShell syntax"));
        assert!(!guidelines.contains("`ls`, `rg`, `find`, `cat`, `sed`, and `awk`"));
    }

    #[test]
    fn runtime_tool_guidance_uses_explicit_unix_like_profile() {
        let tools = vec![
            TOOL_APPLY_PATCH.to_string(),
            TOOL_EXEC_COMMAND.to_string(),
            TOOL_CODE_SEARCH.to_string(),
        ];
        let guidelines =
            generate_runtime_tool_guidelines_for_profile(&tools, false, ResolvedShellPromptProfile::UnixLike);

        assert!(guidelines.contains("`ls`, `find`, `cat`, `sed`, and `awk` for repository browsing"));
        assert!(guidelines.contains("Prefer `code_search` over `rg`/`grep` for code"));
        assert!(guidelines.contains("Advanced `code_search` takes `query`"));
        assert!(guidelines.contains("literal smart-case"));
        assert!(guidelines.contains("shell-only tasks"));
        assert!(!guidelines.contains("native PowerShell commands"));
        assert!(!guidelines.contains("`Get-ChildItem`"));
    }

    #[test]
    fn runtime_tool_prompt_sections_use_explicit_profile_for_active_tools() {
        let mut powershell_prompt = "Base prompt".to_string();
        let mut unix_prompt = "Base prompt".to_string();
        let snapshot = SessionToolCatalogSnapshot::new(
            7,
            9,
            false,
            false,
            Some(std::sync::Arc::new(vec![
                ToolDefinition::function(
                    TOOL_EXEC_COMMAND.to_string(),
                    "Shell".to_string(),
                    serde_json::json!({"type": "object"}),
                ),
                ToolDefinition::function(
                    TOOL_CODE_SEARCH.to_string(),
                    "Bounded source search".to_string(),
                    serde_json::json!({"type": "object"}),
                ),
            ])),
            false,
        );

        append_runtime_tool_prompt_sections_for_profile(
            &mut powershell_prompt,
            &snapshot,
            false,
            ResolvedShellPromptProfile::PowerShell,
        );
        append_runtime_tool_prompt_sections_for_profile(
            &mut unix_prompt,
            &snapshot,
            false,
            ResolvedShellPromptProfile::UnixLike,
        );

        assert!(powershell_prompt.contains("## Active Tools"));
        assert!(powershell_prompt.contains("`Get-ChildItem`"));
        assert!(powershell_prompt.contains("`Select-String`"));
        assert!(!powershell_prompt.contains("`ls`, `rg`, `find`, `cat`, `sed`, and `awk`"));

        assert!(unix_prompt.contains("## Active Tools"));
        assert!(unix_prompt.contains("`ls`, `find`, `cat`, `sed`, and `awk` for repository browsing"));
        assert!(unix_prompt.contains("Prefer `code_search` over `rg`/`grep` for code"));
        assert!(unix_prompt.contains("Advanced `code_search` takes `query`"));
        assert!(unix_prompt.contains("literal smart-case"));
        assert!(!unix_prompt.contains("`Get-ChildItem`"));
    }

    #[test]
    fn runtime_tool_prompt_sections_include_catalog_metadata() {
        let mut prompt = "Base prompt".to_string();
        let snapshot = SessionToolCatalogSnapshot::new(
            7,
            9,
            true,
            false,
            Some(std::sync::Arc::new(vec![
                ToolDefinition::function(
                    TOOL_EXEC_COMMAND.to_string(),
                    "Search".to_string(),
                    serde_json::json!({"type": "object"}),
                ),
                ToolDefinition::function(
                    TOOL_APPLY_PATCH.to_string(),
                    "File".to_string(),
                    serde_json::json!({"type": "object"}),
                ),
            ])),
            false,
        );

        append_runtime_tool_prompt_sections(&mut prompt, &snapshot, true);

        assert!(prompt.contains("## Active Tools"));
        assert!(prompt.contains("[Runtime Tool Catalog]"));
        assert!(prompt.contains("catalog_tools: 2"));
        assert!(prompt.contains("currently_available_tools: exec_command, apply_patch"));
        assert!(prompt.contains("request_user_input_enabled: false"));
    }

    #[test]
    fn runtime_tool_prompt_sections_replace_existing_runtime_sections() {
        let mut prompt = "Base prompt".to_string();
        let first = SessionToolCatalogSnapshot::new(
            1,
            2,
            false,
            false,
            Some(std::sync::Arc::new(vec![ToolDefinition::function(
                TOOL_EXEC_COMMAND.to_string(),
                "Search".to_string(),
                serde_json::json!({"type": "object"}),
            )])),
            false,
        );
        let second = SessionToolCatalogSnapshot::new(
            7,
            9,
            true,
            true,
            Some(std::sync::Arc::new(vec![ToolDefinition::function(
                TOOL_APPLY_PATCH.to_string(),
                "File".to_string(),
                serde_json::json!({"type": "object"}),
            )])),
            false,
        );

        append_runtime_tool_prompt_sections(&mut prompt, &first, true);
        append_runtime_tool_prompt_sections(&mut prompt, &second, true);

        assert_eq!(prompt.matches("## Active Tools").count(), 1);
        assert_eq!(prompt.matches("[Runtime Tool Catalog]").count(), 1);
        assert!(prompt.contains("version: 7"));
        assert!(!prompt.contains("version: 1"));
        assert!(prompt.contains("request_user_input_enabled: true"));
        assert!(!prompt.contains("request_user_input_enabled: false"));
    }
}
