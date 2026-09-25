mod commands;
mod commands_processing;
mod files;
pub(crate) mod large_output;
#[cfg(test)]
mod large_output_tests;
mod mcp;
mod panels;
mod streams;
mod styles;

// Re-export stream utilities
use anyhow::Result;
use commands::render_terminal_command_panel;
use files::{
    format_diff_content_lines_with_numbers, render_apply_patch_diff_preview, render_list_dir_output,
    render_read_file_output, render_write_file_preview,
};
use mcp::{render_context7_output, render_generic_output, render_sequential_output, resolve_renderer_profile};
use serde_json::Value;
use streams::render_stream_section;
pub(crate) use streams::{render_code_fence_blocks, resolve_stdout_tail_limit};
use styles::{GitStyles, LsStyles};
use vtcode_commons::ui_protocol::TaskItemStatus;
use vtcode_core::config::constants::tools;
use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::config::mcp::McpRendererProfile;
use vtcode_core::config::{ToolDisplayMode, ToolOutputMode};
use vtcode_core::tools::continuation::{
    NEXT_CONTINUE_PROMPT, NEXT_READ_PROMPT, PtyContinuationArgs, ReadChunkContinuationArgs,
};
use vtcode_core::tools::handlers::task_tracking::{compact_task_tree_view_from_items, short_task_description};
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};
use vtcode_core::utils::style_helpers::{ColorPalette, render_styled};
use vtcode_ui::tui::app::TaskPanelMetadata;

pub(crate) fn spooled_output_hint(path: &str) -> String {
    format!(
        "Large output was spooled to \"{path}\". Use exec_command with shell tools such as cat, sed, or rg to inspect details."
    )
}

/// Render a detail line with tree prefix styling (└ prefix).
/// Used to unify output details with the tree structure used by other tools.
pub(crate) fn render_tree_detail(renderer: &mut AnsiRenderer, detail: &str) -> Result<()> {
    let palette = ColorPalette::default();
    let mut styled = String::new();
    push_tree_prefix(&mut styled, &palette);
    styled.push_str(&render_styled(detail, palette.muted, None));
    renderer.line(MessageStyle::Info, &styled)?;
    Ok(())
}

/// Push the shared `  └ ` tree-detail prefix (two-space indent, dim `└`, trailing
/// space) into `styled`. Centralized so the prefix style can change in one place
/// across both detail lines and command-line renderings.
pub(crate) fn push_tree_prefix(styled: &mut String, palette: &ColorPalette) {
    styled.push_str("  ");
    styled.push_str(&render_styled("└", palette.muted, Some("dim".to_string())));
    styled.push(' ');
}

fn tool_recovery_hint(val: &Value) -> Option<&'static str> {
    if !val.get("loop_detected").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    if val.get("spool_path").and_then(Value::as_str).is_some() {
        return Some("Loop detected; continue from spooled output.");
    }
    if val.get("fallback_tool").and_then(Value::as_str).is_some() {
        return Some("Loop detected; fallback is available.");
    }
    Some("Loop detected; change approach before retrying.")
}

fn push_tool_follow_up_hint(hints: &mut Vec<String>, hint: impl Into<String>) {
    let hint = hint.into();
    if hint.trim().is_empty() || hints.iter().any(|existing| existing == &hint) {
        return;
    }
    hints.push(hint);
}

fn tool_follow_up_hints(val: &Value) -> Vec<String> {
    let mut hints = Vec::with_capacity(5);
    if let Some(hint) = tool_recovery_hint(val) {
        push_tool_follow_up_hint(&mut hints, hint);
    }
    if let Some(next_action) = val.get("next_action").and_then(Value::as_str) {
        push_tool_follow_up_hint(&mut hints, next_action);
    }
    if let Some(path) = val.get("spool_path").and_then(Value::as_str) {
        push_tool_follow_up_hint(&mut hints, spooled_output_hint(path));
    }
    if val
        .get("next_continue_args")
        .and_then(PtyContinuationArgs::from_value)
        .is_some()
    {
        push_tool_follow_up_hint(&mut hints, NEXT_CONTINUE_PROMPT);
    } else if val
        .get("next_read_args")
        .and_then(ReadChunkContinuationArgs::from_value)
        .is_some()
    {
        push_tool_follow_up_hint(&mut hints, NEXT_READ_PROMPT);
    }
    hints
}

/// Return follow-up guidance in the same order used by the live renderer.
///
/// The transcript viewer stores complete command captures separately from the
/// compact live row. Keep this metadata in that capture so exporting or
/// reviewing the whole conversation does not lose recovery instructions that
/// were rendered after the command body.
pub(crate) fn tool_follow_up_hints_for_capture(val: &Value, rendered_output: Option<&str>) -> Vec<String> {
    tool_follow_up_hints(val)
        .into_iter()
        .filter(|hint| !rendered_output.is_some_and(|output| output.contains(hint.as_str())))
        .collect()
}

pub(super) fn render_tool_follow_up_hints(
    renderer: &mut AnsiRenderer,
    val: &Value,
    rendered_output: Option<&str>,
) -> Result<()> {
    let mut rendered_any = false;
    for hint in tool_follow_up_hints(val) {
        if rendered_output.is_some_and(|output| output.contains(hint.as_str())) {
            continue;
        }
        if !rendered_any {
            renderer.line(MessageStyle::ToolDetail, "")?;
            rendered_any = true;
        }
        renderer.line(MessageStyle::ToolDetail, &hint)?;
    }
    Ok(())
}

fn preferred_follow_up_rendered_body(val: &Value) -> Option<&str> {
    val.get("output")
        .and_then(Value::as_str)
        .or_else(|| val.get("content").and_then(Value::as_str))
}

fn render_tool_follow_up_hints_for_value(renderer: &mut AnsiRenderer, val: &Value) -> Result<()> {
    render_tool_follow_up_hints(renderer, val, preferred_follow_up_rendered_body(val))
}

async fn render_terminal_tool_output(
    renderer: &mut AnsiRenderer,
    val: &Value,
    vt_config: Option<&VTCodeConfig>,
    allow_tool_ansi: bool,
) -> Result<()> {
    let git_styles = GitStyles::new();
    let ls_styles = LsStyles::from_env();
    render_terminal_command_panel(renderer, val, &git_styles, &ls_styles, vt_config, allow_tool_ansi).await
}

pub(crate) async fn render_tool_output(
    renderer: &mut AnsiRenderer,
    tool_name: Option<&str>,
    val: &Value,
    vt_config: Option<&VTCodeConfig>,
) -> Result<()> {
    let allow_tool_ansi = vt_config.map(|cfg| cfg.ui.allow_tool_ansi).unwrap_or(false);
    let is_git_diff_output = is_git_diff_payload(val);

    match tool_name {
        Some(tools::WRITE_FILE) | Some(tools::CREATE_FILE) => {
            let git_styles = GitStyles::new();
            let ls_styles = LsStyles::from_env();
            return render_write_file_preview(renderer, val, &git_styles, &ls_styles);
        }
        Some(tools::EDIT_FILE) | Some(tools::SEARCH_REPLACE) | Some(tools::DELETE_FILE)
            if val.get("diff").is_some() || val.get("diff_preview").is_some() =>
        {
            let git_styles = GitStyles::new();
            let ls_styles = LsStyles::from_env();
            return render_write_file_preview(renderer, val, &git_styles, &ls_styles);
        }
        Some(tools::APPLY_PATCH) => {
            let git_styles = GitStyles::new();
            let ls_styles = LsStyles::from_env();
            return render_apply_patch_diff_preview(renderer, val, &git_styles, &ls_styles);
        }
        Some(tools::UNIFIED_FILE) => {
            if val.get("diff").is_some() || val.get("diff_preview").is_some() {
                let git_styles = GitStyles::new();
                let ls_styles = LsStyles::from_env();
                return render_write_file_preview(renderer, val, &git_styles, &ls_styles);
            }
            if val.get("content").is_some() {
                render_read_file_output(renderer, val)?;
                render_tool_follow_up_hints(renderer, val, val.get("content").and_then(Value::as_str))?;
                return Ok(());
            }
        }
        Some(tools::RUN_PTY_CMD)
        | Some(tools::READ_PTY_SESSION)
        | Some(tools::CREATE_PTY_SESSION)
        | Some(tools::SEND_PTY_INPUT)
        | Some(tools::CLOSE_PTY_SESSION)
        | Some(tools::RESIZE_PTY_SESSION)
        | Some(tools::LIST_PTY_SESSIONS)
        | Some(tools::EXEC_COMMAND)
        | Some(tools::EXEC_PTY_CMD) => {
            return render_terminal_tool_output(renderer, val, vt_config, allow_tool_ansi).await;
        }
        Some(tools::UNIFIED_EXEC) if !is_git_diff_output && should_render_command_session_terminal_panel(val) => {
            return render_terminal_tool_output(renderer, val, vt_config, allow_tool_ansi).await;
        }
        Some(tools::WEB_FETCH) => {
            render_generic_output(renderer, val)?;
            render_tool_follow_up_hints_for_value(renderer, val)?;
            return Ok(());
        }
        Some(tools::LIST_FILES) => {
            let ls_styles = LsStyles::from_env();
            render_list_dir_output(renderer, val, &ls_styles)?;
            render_tool_follow_up_hints_for_value(renderer, val)?;
            return Ok(());
        }
        Some(tools::READ_FILE) => {
            render_read_file_output(renderer, val)?;
            render_tool_follow_up_hints(renderer, val, val.get("content").and_then(Value::as_str))?;
            return Ok(());
        }
        Some(tools::EXECUTE_CODE) => {
            return render_terminal_tool_output(renderer, val, vt_config, allow_tool_ansi).await;
        }
        Some(tools::TASK_TRACKER) if render_tracker_view(renderer, val)? => {
            return Ok(());
        }
        _ => {}
    }

    render_simple_tool_status(renderer, tool_name, val)?;

    if let Some(notice) = val.get("security_notice").and_then(Value::as_str) {
        renderer.line(MessageStyle::ToolDetail, notice)?;
    }

    render_tool_follow_up_hints_for_value(renderer, val)?;

    if let Some(tool) = tool_name
        && tool.starts_with("mcp_")
    {
        if let Some(profile) = resolve_renderer_profile(tool, vt_config) {
            match profile {
                McpRendererProfile::Context7 => render_context7_output(renderer, val)?,
                McpRendererProfile::SequentialThinking => render_sequential_output(renderer, val)?,
            }
        } else {
            render_generic_output(renderer, val)?;
        }
        // Early return for MCP tools - don't fall through to other rendering logic
        return Ok(());
    }

    let output_mode = vt_config.map(|cfg| cfg.ui.tool_output_mode).unwrap_or(ToolOutputMode::Compact);
    let tail_limit = resolve_stdout_tail_limit(vt_config);
    let git_styles = GitStyles::new();
    let ls_styles = LsStyles::from_env();
    let disable_spool = val.get("no_spool").and_then(Value::as_bool).unwrap_or(false);

    // PTY tools use "output" field instead of "stdout"
    let stream_tool_name = if is_git_diff_output { None } else { tool_name };

    if let Some(output) = val.get("output").and_then(Value::as_str) {
        render_stream_section(
            renderer,
            "",
            output,
            output_mode,
            tail_limit,
            stream_tool_name,
            &git_styles,
            &ls_styles,
            MessageStyle::ToolOutput,
            allow_tool_ansi,
            disable_spool,
            vt_config,
        )
        .await?;
    } else if let Some(stdout) = val.get("stdout").and_then(Value::as_str) {
        render_stream_section(
            renderer,
            "stdout",
            stdout,
            output_mode,
            tail_limit,
            stream_tool_name,
            &git_styles,
            &ls_styles,
            MessageStyle::ToolOutput,
            allow_tool_ansi,
            disable_spool,
            vt_config,
        )
        .await?;
    }
    if let Some(stderr) = val.get("stderr").and_then(Value::as_str) {
        render_stream_section(
            renderer,
            "stderr",
            stderr,
            output_mode,
            tail_limit,
            tool_name,
            &git_styles,
            &ls_styles,
            MessageStyle::ToolError,
            allow_tool_ansi,
            disable_spool,
            vt_config,
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn format_unified_diff_lines(diff_content: &str) -> Vec<String> {
    format_diff_content_lines_with_numbers(diff_content)
}

fn is_git_diff_payload(val: &Value) -> bool {
    val.get("content_type")
        .and_then(Value::as_str)
        .is_some_and(|content_type| content_type == "git_diff")
}

/// User-facing transcript surface for a tracker payload.
///
/// Successful checklists collapse to a header line (`• Release 2/5`) when
/// progress counts exist (explicit or derived from items), even if the tree
/// body is empty. Errors and empty trackers keep diagnostic lines. Row-level
/// detail travels through [`tracker_transcript_lines`] with typed statuses.
pub(crate) fn tracker_progress_lines(val: &Value) -> Vec<String> {
    if tracker_response_is_successful(val)
        && (!tracker_visible_tree_rows(val).is_empty() || tracker_progress_counts(val).is_some())
    {
        return vec![tracker_progress_header(val)];
    }
    let diagnostics = tracker_summary_lines(val);
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(diagnostics.len() + 1);
    lines.push("• Tasks".to_string());
    lines.extend(diagnostics);
    lines
}

/// Max tree rows shown inline in expanded transcript mode; overflow collapses
/// to a single `  … N more` row so large checklists stay bounded.
pub(crate) const TRACKER_TRANSCRIPT_MAX_ROWS: usize = 30;

/// Max bytes of one visible tracker row. A plan step can carry a long
/// `Action -> files: [...] -> verify: [...]` body; the row keeps only its
/// leading description so the TODO panel and the compact current row stay
/// scannable. The full step text remains in the structured checklist payload.
pub(crate) const TRACKER_ROW_DESCRIPTION_MAX_BYTES: usize = 96;

/// One user-facing tracker row: glyphless display text plus its typed status.
///
/// `status` is `None` for headers, diagnostics, and truncation rows, which
/// render with the default style. Carrying status alongside text (instead of
/// re-parsing glyphs) keeps styling exact after glyph removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrackerLine {
    pub(crate) text: String,
    pub(crate) status: Option<TaskItemStatus>,
}

impl TrackerLine {
    pub(crate) fn plain(text: String) -> Self {
        Self { text, status: None }
    }

    pub(crate) fn row(text: String, status: TaskItemStatus) -> Self {
        Self { text, status: Some(status) }
    }
}

/// One glyphless tree row with its typed status and leaf flag.
///
/// Parents summarize children and carry no status glyph; `leaf` distinguishes
/// them so the current-task picker prefers actionable leaf rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrackerRow {
    pub(crate) text: String,
    pub(crate) status: TaskItemStatus,
    pub(crate) leaf: bool,
}

/// Transcript lines for a tracker payload, honoring display mode.
///
/// Status surfaces through text styling only (no leaf glyphs): done rows
/// render struck-through, the current row renders bold in the theme accent,
/// blocked rows use the warning token. Compact (`expanded = false`) shows
/// header plus the single current task row (`  ▶ …`); expanded appends the
/// glyphless tree body (truncated). Errors and diagnostics never expand.
pub(crate) fn tracker_transcript_lines(val: &Value, expanded: bool) -> Vec<TrackerLine> {
    if !expanded {
        let progress = tracker_progress_lines(val);
        if progress.len() != 1 || !tracker_response_is_successful(val) {
            return progress.into_iter().map(TrackerLine::plain).collect();
        }
        let Some(current) = tracker_current_tree_row(val) else {
            return progress.into_iter().map(TrackerLine::plain).collect();
        };
        let header = progress.into_iter().next().unwrap_or_else(|| "• Tasks".to_string());
        return vec![TrackerLine::plain(header), current];
    }
    if !tracker_response_is_successful(val) {
        return tracker_progress_lines(val).into_iter().map(TrackerLine::plain).collect();
    }
    let progress = tracker_progress_lines(val);
    if progress.len() != 1 {
        return progress.into_iter().map(TrackerLine::plain).collect();
    }
    let tree = tracker_rich_tree_rows(val);
    if tree.is_empty() {
        return progress.into_iter().map(TrackerLine::plain).collect();
    }
    let header = progress.into_iter().next().unwrap_or_else(|| "• Tasks".to_string());
    let mut lines = Vec::with_capacity(1 + TRACKER_TRANSCRIPT_MAX_ROWS + 1);
    lines.push(TrackerLine::plain(header));
    if tree.len() > TRACKER_TRANSCRIPT_MAX_ROWS {
        let overflow = tree.len() - TRACKER_TRANSCRIPT_MAX_ROWS;
        lines.extend(
            tree.into_iter()
                .take(TRACKER_TRANSCRIPT_MAX_ROWS)
                .map(|row| TrackerLine::row(row.text, row.status)),
        );
        lines.push(TrackerLine::plain(format!("  … {overflow} more")));
    } else {
        lines.extend(tree.into_iter().map(|row| TrackerLine::row(row.text, row.status)));
    }
    lines
}

/// Panel body rows for a tracker payload: glyphless tree text only (no header).
///
/// Statuses travel separately via [`tracker_tree_body_statuses`]; the panel
/// maps them to theme styles per row.
pub(crate) fn tracker_tree_body_lines(val: &Value) -> Vec<String> {
    tracker_rich_tree_rows(val).into_iter().map(|row| row.text).collect()
}

/// Panel rows with typed statuses plus the focused-row index.
///
/// Returns `(texts, statuses, current)`: display lines for the panel body,
/// parallel statuses for per-row styling, and the index of the focused
/// current task (leaf-aware pick, same priority as the transcript) for accent
/// emphasis. The focused index is `None` when nothing is actionable.
pub(crate) fn tracker_panel_rows(val: &Value) -> (Vec<String>, Vec<TaskItemStatus>, Option<usize>) {
    let rows = tracker_rich_tree_rows(val);
    let mut current = None;
    for want_leaf in [true, false] {
        for status in [
            TaskItemStatus::InProgress,
            TaskItemStatus::Pending,
            TaskItemStatus::Blocked,
        ] {
            if let Some(index) = rows.iter().position(|row| row.status == status && row.leaf == want_leaf) {
                current = Some(index);
                break;
            }
        }
        if current.is_some() {
            break;
        }
    }
    let texts = rows.iter().map(|row| row.text.clone()).collect::<Vec<_>>();
    let statuses = rows.into_iter().map(|row| row.status).collect::<Vec<_>>();
    (texts, statuses, current)
}

/// Glyphless tree rows with typed statuses, in tree order.
///
/// Checklist items render through the shared compact tree formatter; the
/// leading status glyph is stripped so status surfaces through text styling
/// only. The legacy `view.lines` fallback derives status from its glyphs.
fn tracker_rich_tree_rows(val: &Value) -> Vec<TrackerRow> {
    let checklist_items = val
        .get("checklist")
        .and_then(Value::as_object)
        .and_then(|checklist| checklist.get("items"))
        .and_then(Value::as_array);
    let compact_rows = checklist_items
        .filter(|items| !items.is_empty())
        .map(|items| compact_task_tree_view_from_items(items))
        .unwrap_or_default();
    if compact_rows.is_empty() {
        return val
            .get("view")
            .and_then(Value::as_object)
            .and_then(|view| view.get("lines"))
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(visible_tracker_view_row)
                    .map(|display| match tracker_tree_row_glyph(&display) {
                        Some((glyph, _)) => TrackerRow {
                            text: tracker_row_text(strip_tracker_status_glyph(&display)),
                            status: task_status_from_glyph(glyph),
                            leaf: true,
                        },
                        // Glyph-free context rows are preserved (never dropped)
                        // with the neutral pending style, matching the legacy
                        // plain rendering.
                        None => TrackerRow {
                            text: tracker_row_text(display),
                            status: TaskItemStatus::Pending,
                            leaf: false,
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    let mut rows = Vec::with_capacity(compact_rows.len());
    for row in &compact_rows {
        let Some(display) = visible_tracker_view_row(row) else {
            continue;
        };
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<TaskItemStatus>().ok())
            .unwrap_or(TaskItemStatus::Pending);
        let leaf = tracker_tree_row_glyph(&display).is_some();
        rows.push(TrackerRow {
            text: tracker_row_text(strip_tracker_status_glyph(&display)),
            status,
            leaf,
        });
    }
    rows
}

fn tracker_visible_tree_rows(val: &Value) -> Vec<String> {
    tracker_tree_body_lines(val)
}

/// Whether a transcript line is the compact-mode focused TODO row.
pub(crate) fn is_tracker_current_row(line: &str) -> bool {
    line.trim_start().starts_with("▶ ")
}

/// Compact-mode focused row: the single most actionable task.
///
/// Reuses the leaf-aware pick from [`tracker_panel_rows`] so transcript and
/// panel always agree on which task is focused. All-completed and empty
/// checklists return `None` so compact stays header-only. The returned row
/// uses the distinct `  ▶ ` visual without a status glyph; styling carries
/// the status.
pub(crate) fn tracker_current_tree_row(val: &Value) -> Option<TrackerLine> {
    let (texts, statuses, current) = tracker_panel_rows(val);
    let index = current?;
    Some(TrackerLine::row(format_tracker_current_row(&texts[index]), statuses[index]))
}

/// Map a status glyph token to its typed status.
fn task_status_from_glyph(glyph: &str) -> TaskItemStatus {
    match glyph {
        "[x] " => TaskItemStatus::Completed,
        "[-] " => TaskItemStatus::InProgress,
        "[!] " => TaskItemStatus::Blocked,
        _ => TaskItemStatus::Pending,
    }
}

/// Split a tree display row into its status glyph and body, if it is a leaf.
///
/// Only the leading glyph token is inspected, so descriptions containing
/// bracket text mid-string (e.g. `Fix [-] flag handling`) are never mangled.
fn tracker_tree_row_glyph(display: &str) -> Option<(&str, &str)> {
    let mut rest = display.trim_start();
    loop {
        if let Some(after) = rest
            .strip_prefix("├ ")
            .or_else(|| rest.strip_prefix("└ "))
            .or_else(|| rest.strip_prefix("│ "))
        {
            rest = after;
            continue;
        }
        break;
    }
    for glyph in ["□ ", "[x] ", "[-] ", "[!] "] {
        if let Some(body) = rest.strip_prefix(glyph) {
            return Some((glyph, body));
        }
    }
    None
}

/// Strip the leading status glyph from a tree row, keeping branch structure.
///
/// `  ├ [-] Defer setup` → `  ├ Defer setup`. Parent rows and glyph-free
/// rows pass through unchanged.
/// Bound one visible tracker row to a single short description.
///
/// Applied after glyph stripping so the tree prefix and row structure survive;
/// only the trailing description is shortened to its leading clause and then
/// trimmed to the byte budget. Never lets a row wrap into multiple lines in
/// the panel or the compact current-task row.
fn tracker_row_text(text: String) -> String {
    let (prefix, body) = split_tree_prefix_for_shortening(&text);
    let short = short_task_description(body);
    let base = if short.trim().is_empty() {
        body.trim()
    } else {
        short.trim()
    };
    let recombined = format!("{prefix}{base}");
    vtcode_commons::formatting::truncate_byte_budget(&recombined, TRACKER_ROW_DESCRIPTION_MAX_BYTES, "…")
}

/// Split a glyphless tree row into its branch prefix and description body so
/// shortening preserves `  ├ `/`  └ `/`  │ ` structure.
fn split_tree_prefix_for_shortening(text: &str) -> (&str, &str) {
    let mut index = 0;
    while index < text.len() && text.as_bytes()[index] == b' ' {
        index += 1;
    }
    loop {
        let rest = &text[index..];
        if let Some(after) = rest
            .strip_prefix("├ ")
            .or_else(|| rest.strip_prefix("└ "))
            .or_else(|| rest.strip_prefix("│ "))
        {
            index = text.len() - after.len();
            continue;
        }
        break;
    }
    text.split_at(index)
}

fn strip_tracker_status_glyph(display: &str) -> String {
    let Some((_, body)) = tracker_tree_row_glyph(display) else {
        return display.to_string();
    };
    let prefix_len = display.len() - body.len();
    let (prefix_with_glyph, _) = display.split_at(prefix_len);
    let glyph_len = ["□ ", "[x] ", "[-] ", "[!] "]
        .iter()
        .find_map(|glyph| prefix_with_glyph.strip_suffix(glyph).map(|_| glyph.len()))
        .unwrap_or(0);
    format!("{}{}", &prefix_with_glyph[..prefix_with_glyph.len() - glyph_len], body.trim_start())
}

/// Reframe a glyphless tree row with the distinct current-task visual.
fn format_tracker_current_row(glyphless_text: &str) -> String {
    let mut rest = glyphless_text.trim_start();
    loop {
        if let Some(after) = rest
            .strip_prefix("├ ")
            .or_else(|| rest.strip_prefix("└ "))
            .or_else(|| rest.strip_prefix("│ "))
        {
            rest = after;
            continue;
        }
        break;
    }
    format!("  ▶ {}", rest.trim_start())
}

/// Title + progress only — no next-step snippet, no tree rows.
fn tracker_progress_header(val: &Value) -> String {
    let Some((completed, total)) = tracker_progress_counts(val) else {
        return "• Tasks".to_string();
    };
    let label = resolve_tracker_title(val, "Tasks");
    format!("• {label} {completed}/{total}")
}

fn tracker_progress_counts(val: &Value) -> Option<(usize, usize)> {
    let checklist = val.get("checklist")?.as_object()?;
    if let (Some(completed), Some(total)) =
        (checklist.get("completed").and_then(Value::as_u64), checklist.get("total").and_then(Value::as_u64))
    {
        return Some((usize::try_from(completed).unwrap_or(usize::MAX), usize::try_from(total).unwrap_or(usize::MAX)));
    }
    let items = checklist.get("items")?.as_array()?;
    // Only count renderable tasks (non-empty description/text). Malformed
    // entries without descriptions must not produce misleading `0/N` counts.
    let renderable = items
        .iter()
        .filter(|item| {
            item.get("description")
                .and_then(Value::as_str)
                .or_else(|| item.get("text").and_then(Value::as_str))
                .is_some_and(|desc| !desc.trim().is_empty())
        })
        .collect::<Vec<_>>();
    if renderable.is_empty() {
        return None;
    }
    let total = renderable.len();
    let completed = renderable
        .iter()
        .filter(|item| item.get("status").and_then(Value::as_str) == Some("completed"))
        .count();
    Some((completed, total))
}

/// Humanize generated tracker titles (`1789108823046-kind-lagoon` → `Kind
/// Lagoon`) so the transcript header reads as a name instead of a file-stem
/// ID. Only millisecond-timestamp-prefixed slugs are rewritten; user titles
/// (`Release`, paths, sentences) pass through verbatim.
pub(crate) fn humanize_tracker_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Task tracker".to_string();
    }
    let mut parts = trimmed.splitn(2, ['-', '_']);
    let prefix = parts.next().unwrap_or_default();
    let slug = parts.next().unwrap_or_default();
    let is_generated_id = prefix.len() >= 10
        && prefix.chars().all(|character| character.is_ascii_digit())
        && !slug.is_empty()
        && slug
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_');
    if !is_generated_id {
        return trimmed.to_string();
    }
    let humanized = slug
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if humanized.is_empty() {
        trimmed.to_string()
    } else {
        humanized
    }
}

fn visible_tracker_view_row(value: &Value) -> Option<String> {
    let display = value.get("display").and_then(Value::as_str).or_else(|| value.as_str())?;
    let trimmed = display.trim_start();
    if trimmed.starts_with("files:") || trimmed.starts_with("outcome:") || trimmed.starts_with("verify:") {
        return None;
    }
    Some(display.to_string())
}

pub(crate) fn tracker_panel_metadata(val: &Value) -> Option<TaskPanelMetadata> {
    val.get("checklist").and_then(Value::as_object)?;
    let title = resolve_tracker_title(val, "Task tracker");
    let (completed, total) = tracker_progress_counts(val)?;
    Some(TaskPanelMetadata { title, completed, total })
}

/// Descriptive header title: never surface a generated plan codename.
///
/// Checklist titles created from approved plans already carry the plan-summary
/// clause, but older/manual payloads may still carry a timestamp slug
/// (`1789108823046-jolly-forest`) or its humanized form (`Jolly Forest`).
/// Those read as random names and say nothing about the work, so derive a
/// purpose-indicating title from the first checklist item instead. User titles
/// pass through verbatim (humanized only for timestamp slugs).
fn resolve_tracker_title(val: &Value, fallback_default: &str) -> String {
    let raw = val
        .get("checklist")
        .and_then(Value::as_object)
        .and_then(|checklist| checklist.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    let Some(raw) = raw else {
        return first_item_descriptive_title(val).unwrap_or_else(|| fallback_default.to_string());
    };
    if is_generated_tracker_title(raw) {
        return first_item_descriptive_title(val).unwrap_or_else(|| fallback_default.to_string());
    }
    humanize_tracker_title(raw)
}

fn is_generated_tracker_title(raw: &str) -> bool {
    let trimmed = raw.trim();
    let mut parts = trimmed.splitn(2, ['-', '_']);
    let prefix = parts.next().unwrap_or_default();
    let slug = parts.next().unwrap_or_default();
    let is_timestamped =
        prefix.len() >= 10 && prefix.chars().all(|character| character.is_ascii_digit()) && !slug.is_empty();
    if is_timestamped {
        return true;
    }
    vtcode_commons::slug::is_humanized_codename(&humanize_tracker_title(trimmed))
}

/// Fallback descriptive title from the first actionable checklist item.
/// Bounds to the same 60-byte header budget used at tracker creation.
fn first_item_descriptive_title(val: &Value) -> Option<String> {
    let items = val.get("checklist")?.get("items")?.as_array()?;
    for item in items {
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| item.get("text").and_then(Value::as_str))?;
        let short = short_task_description(description);
        let short = short.trim();
        if !short.is_empty() {
            return Some(vtcode_commons::formatting::truncate_byte_budget(short, 60, "…"));
        }
    }
    None
}

fn render_tracker_view(renderer: &mut AnsiRenderer, val: &Value) -> Result<bool> {
    // Non-inline fallback honors display mode: compact shows header plus the
    // current task, expanded appends the truncated tree. Glyphless plain text;
    // status styling applies on the inline surface.
    let expanded = renderer.tool_display_mode() != ToolDisplayMode::Compact;
    let lines: Vec<String> = tracker_transcript_lines(val, expanded)
        .into_iter()
        .map(|line| line.text)
        .collect();
    if lines.is_empty() {
        return Ok(false);
    }

    // Render through the markdown pipeline so inline formatting displays styled
    // instead of raw source on the single progress/diagnostic line.
    for line in lines {
        renderer.render_markdown_output(MessageStyle::ToolDetail, &line)?;
    }

    Ok(true)
}

fn tracker_summary_lines(val: &Value) -> Vec<String> {
    let has_valid_checklist_items = val
        .get("checklist")
        .and_then(Value::as_object)
        .and_then(|checklist| checklist.get("items"))
        .and_then(Value::as_array)
        .is_some_and(|items| !compact_task_tree_view_from_items(items).is_empty());
    if has_valid_checklist_items && tracker_response_is_successful(val) {
        return Vec::new();
    }

    tracker_diagnostic_lines(val)
}

fn tracker_response_is_successful(val: &Value) -> bool {
    if val.get("error").is_some() || val.get("error_type").is_some() {
        return false;
    }

    match val.get("status").and_then(Value::as_str) {
        Some("created" | "replaced" | "updated" | "unchanged" | "ok" | "added") => true,
        Some(_) => false,
        None => true,
    }
}

fn tracker_diagnostic_lines(val: &Value) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(status) = val.get("status").and_then(Value::as_str)
        && !status.trim().is_empty()
    {
        lines.push(format!("  Tracker status: {status}"));
    }

    if let Some(message) = val.get("message").and_then(Value::as_str)
        && !message.trim().is_empty()
    {
        lines.push(format!("  Update: {message}"));
    }

    lines
}

fn render_simple_tool_status(renderer: &mut AnsiRenderer, _tool_name: Option<&str>, val: &Value) -> Result<()> {
    let has_error = val.get("error").is_some() || val.get("error_type").is_some();

    if has_error {
        render_error_details(renderer, val)?;
    }

    Ok(())
}

fn should_render_command_session_terminal_panel(val: &Value) -> bool {
    let has_command = val
        .get("command")
        .map(|command| match command {
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(parts) => !parts.is_empty(),
            _ => false,
        })
        .unwrap_or(false);
    let has_terminal_stream = val
        .get("output")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
        || val
            .get("stdout")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
        || val
            .get("stderr")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty());
    let has_session_context = ["id", "session_id", "process_id", "is_exited", "exit_code"]
        .iter()
        .any(|key| val.get(*key).is_some());

    !is_git_diff_payload(val) && (has_command || has_terminal_stream || has_session_context)
}

fn render_error_details(renderer: &mut AnsiRenderer, val: &Value) -> Result<()> {
    if let Some(error_msg) = val
        .get("message")
        .and_then(|v| v.as_str())
        .filter(|msg| !msg.trim().is_empty())
        .or_else(|| val.get("error").and_then(|v| v.as_str()).filter(|msg| !msg.trim().is_empty()))
    {
        renderer.line(MessageStyle::ToolError, &format!("Error: {error_msg}"))?;
    }

    if let Some(error_type) = val.get("error_type").and_then(|v| v.as_str()) {
        let type_description = match error_type {
            "InvalidParameters" => "Invalid parameters provided",
            "ToolNotFound" => "Tool not found",
            "ResourceNotFound" => "Resource not found",
            "PermissionDenied" => "Permission denied",
            "ExecutionError" => "Execution error",
            "PolicyViolation" => "Policy violation",
            "Timeout" => "Operation timed out",
            "NetworkError" => "Network error",
            "EncodingError" => "Encoding error",
            "FileSystemError" => "File system error",
            _ => error_type,
        };
        renderer.line(MessageStyle::ToolDetail, &format!("Type: {type_description}"))?;
    }

    if let Some(original) = val.get("original_error").and_then(|v| v.as_str())
        && !original.trim().is_empty()
    {
        let display_error = vtcode_commons::formatting::truncate_byte_budget(original, 197, "...");
        renderer.line(MessageStyle::ToolDetail, &format!("Details: {display_error}"))?;
    }

    if let Some(path) = val.get("path").and_then(|v| v.as_str()) {
        renderer.line(MessageStyle::ToolDetail, &format!("Path: {path}"))?;
    }

    if let Some(line) = val.get("line").and_then(|v| v.as_u64()) {
        if let Some(col) = val.get("column").and_then(|v| v.as_u64()) {
            renderer.line(MessageStyle::ToolDetail, &format!("Location: line {line}, column {col}"))?;
        } else {
            renderer.line(MessageStyle::ToolDetail, &format!("Location: line {line}"))?;
        }
    }

    if let Some(suggestions) = val.get("recovery_suggestions").and_then(|v| v.as_array())
        && !suggestions.is_empty()
    {
        renderer.line(MessageStyle::ToolDetail, "")?;
        renderer.line(MessageStyle::ToolDetail, "Suggestions:")?;
        for (idx, suggestion) in suggestions.iter().take(5).enumerate() {
            if let Some(text) = suggestion.as_str() {
                renderer.line(MessageStyle::ToolDetail, &format!("{}. {}", idx + 1, text))?;
            }
        }
        if suggestions.len() > 5 {
            renderer.line(MessageStyle::ToolDetail, &format!("    ... and {} more", suggestions.len() - 5))?;
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn collect_inline_output(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<vtcode_core::ui::InlineCommand>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        match command {
            vtcode_core::ui::InlineCommand::AppendLine { segments, .. } => {
                lines.push(segments.into_iter().map(|segment| segment.text).collect::<String>());
            }
            vtcode_core::ui::InlineCommand::ReplaceLast { lines: replacement_lines, .. } => {
                lines.extend(
                    replacement_lines
                        .into_iter()
                        .map(|line| line.into_iter().map(|segment| segment.text).collect::<String>()),
                );
            }
            vtcode_core::ui::InlineCommand::RecordDiffReview(anchor) => {
                lines.push(format!("RecordDiffReview {}", anchor.file_path));
                lines.push(anchor.notice);
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Drain test-sink commands and return recorded diff-review anchors.
#[cfg(test)]
pub(crate) fn collect_inline_diff_review_anchors(
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<vtcode_core::ui::InlineCommand>,
) -> Vec<vtcode_commons::ui_protocol::DiffReviewAnchor> {
    let mut anchors = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        if let vtcode_core::ui::InlineCommand::RecordDiffReview(anchor) = command {
            anchors.push(anchor);
        }
    }
    anchors
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use vtcode_commons::ui_protocol::TaskItemStatus;
    use vtcode_core::config::ToolDisplayMode;
    use vtcode_core::ui::InlineHandle;
    use vtcode_core::utils::ansi::AnsiRenderer;

    use super::{
        TRACKER_ROW_DESCRIPTION_MAX_BYTES, TRACKER_TRANSCRIPT_MAX_ROWS, TrackerLine, collect_inline_output,
        humanize_tracker_title, is_tracker_current_row, preferred_follow_up_rendered_body, render_tool_output,
        should_render_command_session_terminal_panel, spooled_output_hint, tracker_current_tree_row,
        tracker_panel_metadata, tracker_panel_rows, tracker_progress_lines, tracker_row_text, tracker_summary_lines,
        tracker_transcript_lines, tracker_tree_body_lines,
    };

    #[test]
    fn tracker_row_text_bounds_long_descriptions_only() {
        let long = "Add `vtcode exec resume` to the Commands section — document the cross-turn exec-session \
                    resume contract in the second-tier command table row for `vtcode exec`";
        let bounded = tracker_row_text(long.to_string());

        assert_eq!(bounded, "Add `vtcode exec resume` to the Commands section");
        assert!(
            bounded.len() <= TRACKER_ROW_DESCRIPTION_MAX_BYTES + '…'.len_utf8(),
            "row must stay within the budget: {} bytes",
            bounded.len()
        );

        // Long clauses without a detail separator still truncate with an ellipsis.
        let unbroken = "Implement a very long refactor across many modules and services that keeps going without a separator at all";
        let truncated = tracker_row_text(unbroken.to_string());
        assert!(truncated.ends_with('…'), "unbroken row must be bounded: {truncated:?}");

        // Short descriptions stay verbatim so the panel does not add noise.
        assert_eq!(tracker_row_text("Verify with cargo check".to_string()), "Verify with cargo check");
    }

    #[test]
    fn tracker_rows_and_current_row_share_one_short_description() {
        let long = "Update the Everyday recipes block — add a headless resume example next to the existing \
                    `vtcode continue --session-id` recipe and align the schedule example with the canonical flag order";
        let payload = json!({
            "checklist": {
                "title": "Refine README",
                "completed": 0,
                "total": 1,
                "items": [{"index": 1, "description": long, "status": "pending"}]
            }
        });

        let rows = tracker_tree_body_lines(&payload);
        assert_eq!(rows.len(), 1, "single item yields a single row: {rows:?}");
        assert_eq!(rows[0], "  └ Update the Everyday recipes block");
        assert!(!rows[0].contains("headless"), "detail tail must not surface: {rows:?}");

        let current = tracker_current_tree_row(&payload).expect("pending row is the current task");
        assert_eq!(current.text, "  ▶ Update the Everyday recipes block");
    }

    #[test]
    fn command_session_terminal_panel_detects_command_payload() {
        let payload = json!({
            "command": "cargo check",
            "output": "Checking vtcode"
        });
        assert!(should_render_command_session_terminal_panel(&payload));
    }

    #[test]
    fn command_session_terminal_panel_detects_session_payload() {
        let payload = json!({
            "session_id": "run-123",
            "is_exited": true
        });
        assert!(should_render_command_session_terminal_panel(&payload));
    }

    #[test]
    fn command_session_terminal_panel_ignores_non_terminal_payload() {
        let payload = json!({
            "sessions": [],
            "success": true
        });
        assert!(!should_render_command_session_terminal_panel(&payload));
    }

    #[test]
    fn command_session_terminal_panel_skips_git_diff_payload() {
        let payload = json!({
            "command": "git diff -- src/main.rs",
            "output": "diff --git a/src/main.rs b/src/main.rs",
            "content_type": "git_diff"
        });
        assert!(!should_render_command_session_terminal_panel(&payload));
    }

    #[test]
    fn preferred_follow_up_rendered_body_prefers_output_over_content() {
        let payload = json!({
            "output": "stdout body",
            "content": "content body"
        });

        assert_eq!(preferred_follow_up_rendered_body(&payload), Some("stdout body"));
    }

    #[test]
    fn preferred_follow_up_rendered_body_falls_back_to_content() {
        let payload = json!({
            "content": "content body"
        });

        assert_eq!(preferred_follow_up_rendered_body(&payload), Some("content body"));
    }

    #[tokio::test]
    async fn render_tool_output_command_session_git_diff_renders_diff_not_command_preview() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "git diff -- src/main.rs",
            "output": "diff --git a/src/main.rs b/src/main.rs\n+added\n-removed\n",
            "content_type": "git_diff",
            "is_exited": true,
            "exit_code": 0
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::UNIFIED_EXEC), &payload, None)
            .await
            .expect("git diff payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("diff --git a/src/main.rs b/src/main.rs"));
        assert!(!inline_output.contains("└ "), "run-command preview prefix should not appear for git diff payload");
    }

    #[tokio::test]
    async fn render_tool_output_command_session_git_diff_stdout_renders_diff_not_command_preview() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "git diff -- src/lib.rs",
            "stdout": "diff --git a/src/lib.rs b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
            "content_type": "git_diff",
            "is_exited": true,
            "exit_code": 0
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::UNIFIED_EXEC), &payload, None)
            .await
            .expect("git diff stdout payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("diff --git a/src/lib.rs b/src/lib.rs"));
        assert!(inline_output.contains("@@ -1 +1 @@"));
        assert!(inline_output.contains("new"));
        assert!(!inline_output.contains("└ "), "run-command preview prefix should not appear for git diff payload");
    }

    #[tokio::test]
    async fn render_tool_output_apply_patch_renders_diff_content() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "success": true,
            "diff": [{
                "path": "README.md",
                "content": "diff --git a/README.md b/README.md\n-before\n+after\n",
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("apply_patch diff payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("README.md"));
        assert!(inline_output.contains("-before"));
        assert!(inline_output.contains("+after"));
    }

    #[tokio::test]
    async fn render_tool_output_apply_patch_parses_ansi_diff_payloads() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "success": true,
            "diff": [{
                "path": "README.md",
                "operation": "updated",
                "content": "\u{1b}[36mdiff --git a/README.md b/README.md\u{1b}[0m\n\u{1b}[36m@@ -1 +1 @@\u{1b}[0m\n\u{1b}[31m-before\u{1b}[0m\n\u{1b}[32m+after\u{1b}[0m\n",
                "additions": 1,
                "deletions": 1,
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("ANSI apply_patch diff payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("-    1 │ before"));
        assert!(inline_output.contains("+    1 │ after"));
    }

    #[tokio::test]
    async fn render_tool_output_command_session_renders_structured_hints() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "cargo check",
            "output": "tail preview",
            "session_id": "run-123",
            "is_exited": false,
            "next_continue_args": {
                "session_id": "run-123"
            },
            "spool_path": ".vtcode/context/tool_outputs/run-123.txt"
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::UNIFIED_EXEC), &payload, None)
            .await
            .expect("structured hint payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Large output was spooled to"));
        assert!(inline_output.contains("exec_command"));
        assert!(inline_output.contains("cat, sed, or rg"));
        assert!(!inline_output.contains("read_file/grep_file"));
        assert!(inline_output.contains("Reuse `next_continue_args`."));
    }

    #[tokio::test]
    async fn render_tool_output_exec_command_renders_terminal_panel_with_output() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        renderer.set_tool_display_mode(ToolDisplayMode::Expanded);
        let payload = json!({
            "command": "cargo check",
            "output": "Compiling vtcode v0.135.9",
            "stdout": "Compiling vtcode v0.135.9",
            "stderr": "",
            "is_exited": true,
            "exit_code": 0
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::EXEC_COMMAND), &payload, None)
            .await
            .expect("exec_command payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(
            inline_output.contains("Compiling vtcode v0.135.9"),
            "exec_command output must be rendered in the terminal panel, got: {inline_output}"
        );
        assert!(
            !inline_output.contains("(no output)"),
            "exec_command output must not fall through to the no-output status renderer"
        );
    }

    #[tokio::test]
    async fn render_tool_output_exec_command_compact_hides_completed_stdout() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "cargo check",
            "stdout": "verbose completed output",
            "stderr": ""
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::EXEC_COMMAND), &payload, None)
            .await
            .expect("exec_command payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(!inline_output.contains("verbose completed output"));
    }

    #[tokio::test]
    async fn render_tool_output_exec_pty_cmd_renders_terminal_panel_with_output() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "ls -la",
            "output": "total 0",
            "session_id": "run-456",
            "is_exited": true,
            "exit_code": 0
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::EXEC_PTY_CMD), &payload, None)
            .await
            .expect("exec_pty_cmd payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(
            inline_output.contains("total") && inline_output.contains("✓ exit 0"),
            "exec_pty_cmd output must be rendered in the terminal panel, got: {inline_output}"
        );
    }

    #[tokio::test]
    async fn render_tool_output_run_pty_completed_spooled_output_is_reference_only() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "cargo check",
            "output": "preview text that should not render inline",
            "session_id": "run-123",
            "is_exited": true,
            "exit_code": 0,
            "spool_path": ".vtcode/context/tool_outputs/run-123.txt"
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::RUN_PTY_CMD), &payload, None)
            .await
            .expect("spooled PTY payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("✓ exit 0"));
        assert!(inline_output.contains("Large output was spooled to"));
        assert!(!inline_output.contains("preview text that should not render inline"));
        assert!(!inline_output.contains("(no output)"));
    }

    #[tokio::test]
    async fn render_tool_output_read_file_renders_spool_hint_on_early_return_path() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "path": "README.md",
            "content": "preview",
            "spool_path": ".vtcode/context/tool_outputs/readme.txt"
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::READ_FILE), &payload, None)
            .await
            .expect("read_file payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Large output was spooled to"));
        assert!(inline_output.contains("exec_command"));
        assert!(inline_output.contains("cat, sed, or rg"));
        assert!(!inline_output.contains("read_file/grep_file"));
    }

    #[tokio::test]
    async fn render_tool_output_web_fetch_content_fallback_renders_follow_up_hint() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "content": "preview",
            "spool_path": ".vtcode/context/tool_outputs/web.txt"
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::WEB_FETCH), &payload, None)
            .await
            .expect("web_fetch payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Large output was spooled to"));
        assert!(inline_output.contains("exec_command"));
        assert!(inline_output.contains("cat, sed, or rg"));
        assert!(!inline_output.contains("read_file/grep_file"));
    }

    #[tokio::test]
    async fn render_tool_output_does_not_duplicate_spooled_output_hint() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let spool_path = ".vtcode/context/tool_outputs/web.txt";
        let hint = spooled_output_hint(spool_path);
        let payload = json!({
            "output": hint,
            "spool_path": spool_path
        });

        render_tool_output(&mut renderer, Some("custom_tool"), &payload, None)
            .await
            .expect("spooled hint payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert_eq!(inline_output.matches("Large output was spooled to").count(), 1);
        assert!(inline_output.contains("exec_command"));
        assert!(inline_output.contains("cat, sed, or rg"));
    }

    #[tokio::test]
    async fn render_tool_output_read_file_long_preview_keeps_preview_limits() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let content = (1..=100).map(|idx| format!("{idx}: line {idx}")).collect::<Vec<_>>().join("\n");
        let payload = json!({
            "path": "src/main.rs",
            "content": content
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::READ_FILE), &payload, None)
            .await
            .expect("read_file preview payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        // read_file now shows a summary line instead of code preview
        assert!(inline_output.contains("Read 100 lines"));
        assert!(inline_output.contains("└ Read 100 lines"));
        assert!(!inline_output.contains("    Read 100 lines"));
    }

    #[tokio::test]
    async fn render_tool_output_renders_loop_recovery_hint_from_structured_fields() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "loop_detected": true,
            "fallback_tool": vtcode_core::config::constants::tools::CODE_SEARCH
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::CODE_SEARCH), &payload, None)
            .await
            .expect("loop recovery hint payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Loop detected; fallback is available."));
    }

    #[tokio::test]
    async fn render_tool_output_renders_spooled_loop_recovery_hint() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "loop_detected": true,
            "spool_path": ".vtcode/context/tool_outputs/readme.txt",
            "next_read_args": {
                "path": ".vtcode/context/tool_outputs/readme.txt",
                "offset": 81,
                "limit": 40
            }
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::READ_FILE), &payload, None)
            .await
            .expect("spooled loop recovery hint payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Loop detected; continue from spooled output."));
    }

    #[tokio::test]
    async fn render_tool_output_does_not_duplicate_loop_recovery_hint() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "loop_detected": true,
            "fallback_tool": vtcode_core::config::constants::tools::CODE_SEARCH,
            "output": "Loop detected; fallback is available."
        });

        render_tool_output(&mut renderer, Some("custom_tool"), &payload, None)
            .await
            .expect("duplicate hint payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert_eq!(inline_output.matches("Loop detected; fallback is available.").count(), 1);
    }

    #[tokio::test]
    async fn render_tool_output_command_session_keeps_exit_127_output_and_guidance() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "command": "pip install pymupdf",
            "output": "bash: pip: command not found",
            "session_id": "run-127",
            "is_exited": true,
            "exit_code": 127,
            "critical_note": "Command `pip` was not found in PATH.",
            "next_action": "Check the command name or install the missing binary, then rerun the command."
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::UNIFIED_EXEC), &payload, None)
            .await
            .expect("exit 127 payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("bash: pip: command not found"));
        assert!(inline_output.contains("not found in PATH."));
        assert!(
            inline_output.contains("Check the command name or install the missing binary, then rerun the command.")
        );
        assert!(inline_output.contains("✓ exit 127"));
        assert!(!inline_output.contains("Solution:"));
        assert_eq!(
            inline_output
                .matches("Check the command name or install the missing binary, then rerun the command.")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn render_tool_output_renders_generic_recoverable_failure_guidance() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "error": "Tool preflight validation failed: x",
            "is_recoverable": true,
            "next_action": "Retry with fallback_tool_args."
        });

        render_tool_output(&mut renderer, Some("custom_tool"), &payload, None)
            .await
            .expect("generic recoverable failure should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("Tool preflight validation failed: x"));
        assert!(inline_output.contains("Retry with fallback_tool_args."));
        assert_eq!(inline_output.matches("Retry with fallback_tool_args.").count(), 1);
        assert!(!inline_output.contains("\"error\""));
        assert!(!inline_output.contains("\"next_action\""));
    }

    #[tokio::test]
    async fn render_tool_output_write_file_diff_truncation_does_not_claim_full_review() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff_preview": {
                "content": "@@ -1 +1 @@\n-old\n+new\n",
                "truncated": true,
                "omitted_line_count": 5
            }
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::WRITE_FILE), &payload, None)
            .await
            .expect("write file diff payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(
            inline_output.contains("preview excerpt retained") || inline_output.contains("lines omitted"),
            "tool-truncated previews must not claim full-diff review: {inline_output:?}"
        );
        assert!(
            !inline_output.contains("RecordDiffReview"),
            "excerpt payloads must not record expand anchors: {inline_output:?}"
        );
        assert!(!inline_output.contains("review full diff"));
        assert!(!inline_output.contains("use read_file for full view"));
    }

    #[tokio::test]
    async fn render_tool_output_write_file_truncated_preview_does_not_record_anchor() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff_preview": {
                "content": "@@ -1 +1 @@\n-old\n+new\n",
                "truncated": true,
                "omitted_line_count": 5,
                "path": "src/main.rs"
            }
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::WRITE_FILE), &payload, None)
            .await
            .expect("write file diff payload should render");

        let anchors = super::collect_inline_diff_review_anchors(&mut receiver);
        assert!(
            anchors.is_empty(),
            "tool-level truncated previews are excerpts and must not record full-diff anchors: {anchors:?}"
        );
    }

    #[tokio::test]
    async fn render_tool_output_write_file_untruncated_preview_can_record_expand_from_streams() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        renderer.set_table_max_width(Some(40));
        // Complete unified body (not a registry excerpt) whose rows exceed
        // DIFF_WRAP_SOURCE_MAX_WIDTH so safety-cap truncation advertises expand.
        let body = format!("@@ -1 +1 @@\n-{}\n+{}\n", "old ".repeat(600), "new ".repeat(600));
        let payload = json!({
            "diff_preview": {
                "content": body,
                "truncated": false,
                "path": "src/main.rs"
            }
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::WRITE_FILE), &payload, None)
            .await
            .expect("write file diff payload should render");

        let anchors = super::collect_inline_diff_review_anchors(&mut receiver);
        assert!(!anchors.is_empty(), "safety-capped full body should record an expand anchor");
        assert!(
            anchors
                .iter()
                .any(|a| a.unified.contains("old old") || a.unified.contains("new new"))
        );
    }

    #[tokio::test]
    async fn render_tool_output_write_file_uses_canonical_diff_entries() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "path": "README.md",
            "diff": [{
                "path": "README.md",
                "operation": "updated",
                "content": "@@ -1 +1 @@\n-before\n+after\n",
                "additions": 1,
                "deletions": 1,
                "truncated": false,
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::WRITE_FILE), &payload, None)
            .await
            .expect("canonical write diff should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("• Edited README.md (+1 -1)"));
        assert!(inline_output.contains("-    1 │ before"));
        assert!(inline_output.contains("+    1 │ after"));
    }

    #[tokio::test]
    async fn render_tool_output_groups_multiple_file_edits_in_compact_summary() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff": [
                {
                    "path": "src/a.rs",
                    "operation": "updated",
                    "content": "@@ -1 +1 @@\n-old a\n+new a\n",
                    "additions": 2,
                    "deletions": 3,
                    "skipped": false
                },
                {
                    "path": "src/b.rs",
                    "operation": "updated",
                    "content": "@@ -1 +1 @@\n-old b\n+new b\n",
                    "summary": {"additions": 2, "deletions": 2},
                    "skipped": false
                }
            ]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("multi-file diff payload should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("• Edited 2 files (+4 -5)"));
        assert!(inline_output.contains("  ├ src/a.rs (+2 -3)"));
        assert!(inline_output.contains("  └ src/b.rs (+2 -2)"));
        assert!(!inline_output.contains("• Edited src/a.rs"));
        assert!(!inline_output.contains("• Edited src/b.rs"));
        assert!(inline_output.find("  ├ src/a.rs").unwrap() < inline_output.find("  └ src/b.rs").unwrap());
    }

    #[tokio::test]
    async fn render_tool_output_apply_patch_strips_duplicated_file_headers() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff": [{
                "path": "src/main.rs",
                "operation": "updated",
                "content": "diff --git a/src/main.rs b/src/main.rs\nindex 1111111..2222222 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n",
                "additions": 1,
                "deletions": 1,
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("apply_patch diff with headers should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("• Edited src/main.rs (+1 -1)"));
        assert!(inline_output.contains("@@ -1 +1 @@"));
        assert!(inline_output.contains("-    1 │ old"));
        assert!(inline_output.contains("+    1 │ new"));
        assert!(!inline_output.contains("--- a/src/main.rs"), "heading already shows the path: {inline_output}");
        assert!(!inline_output.contains("+++ b/src/main.rs"), "heading already shows the path: {inline_output}");
        assert!(!inline_output.contains("diff --git"), "git header must not duplicate the heading: {inline_output}");
    }

    #[tokio::test]
    async fn render_tool_output_apply_patch_header_only_shows_no_changes() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff": [{
                "path": "src/main.rs",
                "operation": "updated",
                "content": "--- a/src/main.rs\n+++ b/src/main.rs\n",
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("header-only diff should render a friendly row");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("• Edited src/main.rs"));
        assert!(inline_output.contains("no changes"), "header-only preview must not render blank: {inline_output}");
        assert!(!inline_output.contains("--- a/src/main.rs"));
    }

    #[tokio::test]
    async fn render_tool_output_apply_patch_skipped_shows_user_message_not_reason_code() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff": [{
                "path": "src/big.rs",
                "operation": "updated",
                "skipped": true,
                "reason": "too_many_changes",
                "summary": {"additions": 12, "deletions": 3}
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("skipped diff should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(!inline_output.contains("too_many_changes"), "stable reason code must not surface: {inline_output}");
        assert!(inline_output.contains("+12 -3"), "friendly message keeps counts: {inline_output}");
    }

    #[tokio::test]
    async fn render_tool_output_empty_content_keeps_truncation_notice() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut renderer = AnsiRenderer::with_inline_ui(InlineHandle::new_for_tests(sender), Default::default());
        let payload = json!({
            "diff": [{
                "path": "src/big.rs",
                "operation": "updated",
                "content": "",
                "truncated": true,
                "omitted_line_count": 9,
                "skipped": false
            }]
        });

        render_tool_output(&mut renderer, Some(vtcode_core::config::constants::tools::APPLY_PATCH), &payload, None)
            .await
            .expect("empty truncated diff should render");

        let inline_output = collect_inline_output(&mut receiver);
        assert!(inline_output.contains("+9 lines"), "omission metadata must survive empty bodies: {inline_output}");
    }

    #[test]
    fn tracker_summary_lines_hide_successful_tracker_details() {
        let payload = json!({
            "status": "updated",
            "message": "Item 2 status changed: pending -> in_progress",
            "checklist": {
                "total": 4,
                "completed": 1,
                "in_progress": 2,
                "pending": 1,
                "blocked": 0,
                "progress_percent": 25,
                "items": [
                    { "index": 1, "description": "A", "status": "completed" },
                    { "index": 2, "description": "B", "status": "in_progress" },
                    { "index": 3, "description": "C", "status": "in_progress" },
                    { "index": 4, "description": "D", "status": "pending" }
                ]
            }
        });

        assert!(tracker_summary_lines(&payload).is_empty());
    }

    #[test]
    fn tracker_summary_lines_still_show_message_without_checklist() {
        let payload = json!({
            "status": "empty",
            "message": "No active checklist."
        });
        let lines = tracker_summary_lines(&payload);
        assert!(lines.iter().any(|line| line == "  Tracker status: empty"));
        assert!(lines.iter().any(|line| line == "  Update: No active checklist."));
    }

    #[test]
    fn tracker_progress_lines_keep_counts_when_tree_body_is_empty() {
        // Explicit completed/total without renderable step titles still answers
        // "how far along is this work?" on the user-facing surface.
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "completed": 2,
                "total": 5,
                "items": []
            }
        });

        let rows = tracker_progress_lines(&payload);

        assert_eq!(rows, vec!["• Release 2/5"]);
        assert!(tracker_tree_body_lines(&payload).is_empty());
    }

    #[test]
    fn tracker_progress_lines_show_title_and_progress_only() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate", "status": "pending" },
                    { "index_path": "2", "description": "Implement", "status": "in_progress" },
                    { "index_path": "3", "description": "Verify", "status": "completed" },
                    { "index_path": "4", "description": "Resolve dependency", "status": "blocked" }
                ]
            }
        });

        let rows = tracker_progress_lines(&payload);

        assert_eq!(rows, vec!["• Release 1/4"]);
        assert!(rows.iter().all(|row| !row.contains("next:")));
        assert!(rows.iter().all(|row| !row.contains("├") && !row.contains("└")));
    }

    #[test]
    fn tracker_tree_body_lines_are_panel_only_compact_tree() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate", "status": "pending" },
                    { "index_path": "2", "description": "Implement", "status": "in_progress" },
                    { "index_path": "3", "description": "Verify", "status": "completed" },
                    { "index_path": "4", "description": "Resolve dependency", "status": "blocked" }
                ]
            }
        });

        let rows = tracker_tree_body_lines(&payload);

        assert_eq!(
            rows,
            vec![
                "  ├ Investigate",
                "  ├ Implement",
                "  ├ Verify",
                "  └ Resolve dependency",
            ]
        );
        assert!(rows.iter().all(|row| !row.starts_with("• ")));
        assert!(
            rows.iter()
                .all(|row| !row.contains("□") && !row.contains("[x]") && !row.contains("[-]")),
            "status surfaces through styling, not glyphs: {rows:?}"
        );
        assert_eq!(
            tracker_panel_rows(&payload),
            (
                vec![
                    "  ├ Investigate".to_string(),
                    "  ├ Implement".to_string(),
                    "  ├ Verify".to_string(),
                    "  └ Resolve dependency".to_string(),
                ],
                vec![
                    TaskItemStatus::Pending,
                    TaskItemStatus::InProgress,
                    TaskItemStatus::Completed,
                    TaskItemStatus::Blocked,
                ],
                Some(1),
            )
        );
    }

    #[test]
    fn humanize_tracker_title_strips_timestamp_prefix_from_generated_ids() {
        assert_eq!(humanize_tracker_title("1789108823046-kind-lagoon"), "Kind Lagoon");
        assert_eq!(humanize_tracker_title("1789108823046-kind_lagoon"), "Kind Lagoon");
    }

    #[test]
    fn humanize_tracker_title_keeps_user_titles_verbatim() {
        assert_eq!(humanize_tracker_title("Release"), "Release");
        assert_eq!(humanize_tracker_title("2024-report"), "2024-report");
        assert_eq!(humanize_tracker_title("  Task tracker  "), "Task tracker");
    }

    #[test]
    fn tracker_progress_lines_humanize_generated_title_without_raw_slug() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "1789108823046-kind-lagoon",
                "items": [
                    { "index_path": "1", "description": "Investigate", "status": "pending" },
                ]
            }
        });

        let rows = tracker_progress_lines(&payload);

        assert_eq!(rows, vec!["• Investigate 0/1"]);
        assert!(!rows[0].contains("1789108823046"));
        assert!(!rows[0].contains("Lagoon"));
        assert!(!rows[0].contains("next:"));
    }

    #[test]
    fn tracker_progress_lines_prefer_explicit_counts_over_derived() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "1789108823046-kind-lagoon",
                "completed": 2,
                "total": 5,
                "items": [
                    { "index_path": "1", "description": "Done one", "status": "completed" },
                    { "index_path": "2", "description": "Done two", "status": "completed" },
                    { "index_path": "3", "description": "Audit heavy-crate linkage", "status": "pending" },
                ]
            }
        });

        let rows = tracker_progress_lines(&payload);

        assert_eq!(rows, vec!["• Done one 2/5"]);
        assert!(!rows[0].contains("1789108823046"));
        assert!(!rows[0].contains("Lagoon"));
        assert!(!rows[0].contains("Audit heavy-crate linkage"));
    }

    #[test]
    fn tracker_header_replaces_humanized_codename_with_first_task() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Jolly Forest",
                "items": [
                    { "index_path": "1", "description": "Add vtcode exec resume to the Commands section – document the contract", "status": "pending" },
                    { "index_path": "2", "description": "Verify links", "status": "pending" },
                ]
            }
        });

        let rows = tracker_progress_lines(&payload);
        assert_eq!(rows, vec!["• Add vtcode exec resume to the Commands section 0/2"]);
        assert!(!rows[0].contains("Jolly"));

        let metadata = tracker_panel_metadata(&payload).expect("metadata");
        assert_eq!(metadata.title, "Add vtcode exec resume to the Commands section");
    }

    #[test]
    fn tracker_header_keeps_user_title_verbatim() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate", "status": "pending" },
                ]
            }
        });

        let rows = tracker_progress_lines(&payload);
        assert_eq!(rows, vec!["• Release 0/1"]);
    }

    #[test]
    fn tracker_tree_body_lines_strip_metadata_but_keep_task_titles() {
        // Parents summarize their children, so showing their stored leaf status
        // would be misleading. Metadata remains in the structured payload but
        // must not turn into visible detail rows.
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    {
                        "index_path": "1",
                        "level": 0,
                        "description": "Prepare release",
                        "status": "in_progress",
                        "files": ["Cargo.toml"],
                        "outcome": "Version is ready",
                        "verify": ["cargo nextest run -p vtcode"]
                    },
                    { "index_path": "1.1", "level": 1, "description": "Update version", "status": "completed" },
                    { "index_path": "1.2", "level": 1, "description": "Run checks", "status": "in_progress" },
                    { "index_path": "2", "level": 0, "description": "Publish", "status": "pending" }
                ]
            }
        });

        let rows = tracker_tree_body_lines(&payload);

        assert_eq!(
            rows,
            vec![
                "  ├ Prepare release",
                "  │ Update version",
                "  │ Run checks",
                "  └ Publish",
            ]
        );
        assert!(
            rows.iter()
                .all(|line| { !line.contains("files:") && !line.contains("outcome:") && !line.contains("verify:") })
        );
        assert_eq!(payload["checklist"]["items"][0]["files"], json!(["Cargo.toml"]));
        assert_eq!(payload["checklist"]["items"][0]["outcome"], "Version is ready");
        assert_eq!(payload["checklist"]["items"][0]["verify"], json!(["cargo nextest run -p vtcode"]));
        let metadata = tracker_panel_metadata(&payload).expect("structured panel metadata");
        assert_eq!(metadata.title, "Release");
        assert_eq!((metadata.completed, metadata.total), (1, 4));
    }

    #[test]
    fn tracker_progress_lines_keep_diagnostics_for_empty_or_malformed_tracker_responses() {
        // Compact rendering applies only to successful structured checklists.
        // Empty and malformed responses must remain diagnosable instead of
        // silently presenting a blank task panel.
        let empty = json!({});
        let malformed = json!({
            "status": "error",
            "message": "Tracker response did not include checklist items.",
            "view": { "lines": "not an array" }
        });
        let malformed_items = json!({
            "status": "error",
            "message": "Tracker response contained invalid checklist items.",
            "checklist": { "items": [{}] }
        });

        assert!(tracker_progress_lines(&empty).is_empty());
        assert_eq!(
            tracker_progress_lines(&malformed),
            vec![
                "• Tasks",
                "  Tracker status: error",
                "  Update: Tracker response did not include checklist items.",
            ]
        );
        assert_eq!(
            tracker_progress_lines(&malformed_items),
            vec![
                "• Tasks",
                "  Tracker status: error",
                "  Update: Tracker response contained invalid checklist items.",
            ]
        );

        let partial_failure = json!({
            "status": "error",
            "message": "Tracker response was only partially applied.",
            "checklist": {
                "items": [
                    { "index": 1, "description": "Still present", "status": "completed" }
                ]
            }
        });
        assert_eq!(
            tracker_progress_lines(&partial_failure),
            vec![
                "• Tasks",
                "  Tracker status: error",
                "  Update: Tracker response was only partially applied.",
            ]
        );
        // Failed updates stay diagnosable in the transcript; remaining checklist
        // rows remain available on the panel body path.
        assert_eq!(tracker_tree_body_lines(&partial_failure), vec!["  └ Still present"]);
    }

    fn transcript_texts(lines: &[TrackerLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    fn transcript_statuses(lines: &[TrackerLine]) -> Vec<Option<TaskItemStatus>> {
        lines.iter().map(|line| line.status).collect()
    }

    #[test]
    fn tracker_transcript_lines_compact_shows_header_plus_current_task() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate cache miss", "status": "completed" },
                    { "index_path": "2", "description": "Defer eager setup", "status": "in_progress" },
                    { "index_path": "3", "description": "Verify with cargo check", "status": "pending" },
                ]
            }
        });

        let rows = tracker_transcript_lines(&payload, false);

        assert_eq!(transcript_texts(&rows), vec!["• Release 1/3", "  ▶ Defer eager setup"]);
        assert_eq!(transcript_statuses(&rows), vec![None, Some(TaskItemStatus::InProgress)]);
        assert!(rows.iter().all(|row| !row.text.contains("[-]") && !row.text.contains("□")));
    }

    #[test]
    fn tracker_current_tree_row_prefers_in_progress_over_pending_and_blocked() {
        // Asymmetric statuses: pending comes first in document order, but the
        // in-progress leaf later must win as current.
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Verify with cargo check", "status": "pending" },
                    { "index_path": "2", "description": "Defer eager setup", "status": "in_progress" },
                    { "index_path": "3", "description": "Resolve dependency", "status": "blocked" },
                ]
            }
        });

        assert_eq!(
            tracker_current_tree_row(&payload),
            Some(TrackerLine::row("  ▶ Defer eager setup".to_string(), TaskItemStatus::InProgress))
        );
    }

    #[test]
    fn tracker_current_tree_row_falls_back_to_pending_then_blocked() {
        let pending_only = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Verify with cargo check", "status": "completed" },
                    { "index_path": "2", "description": "Defer eager setup", "status": "pending" },
                ]
            }
        });
        assert_eq!(
            tracker_current_tree_row(&pending_only),
            Some(TrackerLine::row("  ▶ Defer eager setup".to_string(), TaskItemStatus::Pending))
        );

        let blocked_only = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Verify with cargo check", "status": "completed" },
                    { "index_path": "2", "description": "Resolve dependency", "status": "blocked" },
                ]
            }
        });
        assert_eq!(
            tracker_current_tree_row(&blocked_only),
            Some(TrackerLine::row("  ▶ Resolve dependency".to_string(), TaskItemStatus::Blocked))
        );
    }

    #[test]
    fn tracker_current_tree_row_stays_header_only_when_all_completed_or_empty() {
        let all_done = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate cache miss", "status": "completed" },
                    { "index_path": "2", "description": "Defer eager setup", "status": "completed" },
                ]
            }
        });
        assert_eq!(tracker_current_tree_row(&all_done), None);
        assert_eq!(tracker_transcript_lines(&all_done, false), vec![TrackerLine::plain("• Release 2/2".to_string())]);

        let empty = json!({
            "status": "updated",
            "checklist": { "title": "Release", "completed": 0, "total": 0, "items": [] }
        });
        assert_eq!(tracker_current_tree_row(&empty), None);
    }

    #[test]
    fn tracker_current_row_marker_is_detected() {
        assert!(is_tracker_current_row("  ▶ Defer eager setup"));
        assert!(is_tracker_current_row("  ▶ Verify with cargo check"));
        assert!(!is_tracker_current_row("  ├ Defer eager setup"));
        assert!(!is_tracker_current_row("• Release 1/3"));
    }

    #[test]
    fn tracker_glyph_strip_keeps_mid_string_brackets_and_branches() {
        // Only the leading status glyph is stripped; bracket text inside the
        // description and the branch structure survive verbatim.
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Fix [-] flag handling", "status": "pending" },
                    { "index_path": "2", "description": "Ship [x] marked docs", "status": "completed" },
                ]
            }
        });

        let rows = tracker_transcript_lines(&payload, true);

        assert_eq!(
            transcript_texts(&rows),
            vec!["• Release 1/2", "  ├ Fix [-] flag handling", "  └ Ship [x] marked docs"]
        );
        assert_eq!(
            transcript_statuses(&rows),
            vec![None, Some(TaskItemStatus::Pending), Some(TaskItemStatus::Completed)]
        );
    }

    #[test]
    fn tracker_transcript_lines_expanded_shows_each_task_item() {
        let payload = json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Investigate cache miss", "status": "completed" },
                    { "index_path": "2", "description": "Defer eager setup", "status": "in_progress" },
                    { "index_path": "3", "description": "Verify with cargo check", "status": "pending" },
                ]
            }
        });

        let rows = tracker_transcript_lines(&payload, true);

        assert_eq!(transcript_texts(&rows)[0], "• Release 1/3");
        assert_eq!(
            transcript_texts(&rows)[1..],
            [
                "  ├ Investigate cache miss",
                "  ├ Defer eager setup",
                "  └ Verify with cargo check",
            ]
        );
        assert_eq!(
            transcript_statuses(&rows),
            vec![
                None,
                Some(TaskItemStatus::Completed),
                Some(TaskItemStatus::InProgress),
                Some(TaskItemStatus::Pending),
            ]
        );
    }

    #[test]
    fn tracker_transcript_lines_expanded_truncates_large_checklists() {
        let items: Vec<serde_json::Value> = (1..=(TRACKER_TRANSCRIPT_MAX_ROWS + 5))
            .map(|index| {
                json!({
                    "index_path": index.to_string(),
                    "description": format!("Distinct task {index}"),
                    "status": if index == 1 { "completed" } else { "pending" },
                })
            })
            .collect();
        let payload = json!({
            "status": "updated",
            "checklist": { "title": "Release", "items": items }
        });

        let rows = tracker_transcript_lines(&payload, true);

        assert_eq!(rows.len(), 1 + TRACKER_TRANSCRIPT_MAX_ROWS + 1);
        assert_eq!(rows[0].text, format!("• Release 1/{}", TRACKER_TRANSCRIPT_MAX_ROWS + 5));
        assert_eq!(rows[0].status, None);
        assert!(rows[1].text.contains("Distinct task 1"));
        assert_eq!(rows[1].status, Some(TaskItemStatus::Completed));
        assert!(
            rows[TRACKER_TRANSCRIPT_MAX_ROWS]
                .text
                .contains(format!("Distinct task {TRACKER_TRANSCRIPT_MAX_ROWS}").as_str())
        );
        assert_eq!(rows[TRACKER_TRANSCRIPT_MAX_ROWS + 1].text, "  … 5 more");
        assert_eq!(rows[TRACKER_TRANSCRIPT_MAX_ROWS + 1].status, None);
        assert!(
            rows.iter()
                .all(|row| !row.text.contains("Distinct task 31") || row.text.starts_with("  …"))
        );
    }

    #[test]
    fn tracker_transcript_lines_expanded_keeps_diagnostics_without_tree() {
        let payload = json!({
            "status": "error",
            "message": "Tracker response was only partially applied.",
            "checklist": {
                "items": [
                    { "index": 1, "description": "Still present", "status": "completed" }
                ]
            }
        });

        let rows = tracker_transcript_lines(&payload, true);

        assert_eq!(
            transcript_texts(&rows),
            vec![
                "• Tasks",
                "  Tracker status: error",
                "  Update: Tracker response was only partially applied.",
            ]
        );
        assert!(rows.iter().all(|row| row.status.is_none()));
        assert!(rows.iter().all(|row| !row.text.contains("Still present")));
    }
}
