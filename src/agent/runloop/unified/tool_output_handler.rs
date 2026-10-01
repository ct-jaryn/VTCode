use crate::agent::runloop::git::normalize_workspace_path;
use crate::agent::runloop::mcp_events::McpPanelState;
use crate::agent::runloop::tool_output::TrackerLine;
use crate::agent::runloop::unified::state::SessionStats;
use anstyle::Effects;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use vtcode_commons::ui_protocol::TaskItemStatus;
use vtcode_core::config::ToolDisplayMode;
use vtcode_core::config::constants::tools;
use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::tools::tool_intent;
use vtcode_core::utils::ansi::AnsiRenderer;
use vtcode_core::utils::ansi::MessageStyle;
use vtcode_core::utils::style_helpers::ColorPalette;
use vtcode_core::utils::transcript;
use vtcode_ui::tui::app::{InlineHandle, InlineMessageKind, InlineSegment, InlineTextStyle, ToolOutputId};

use crate::agent::runloop::unified::run_loop_context::RunLoopContext;
use crate::agent::runloop::unified::tool_pipeline::{
    ToolDisplayStatus, ToolExecutionStatus, ToolPipelineOutcome, is_exec_session_call, renders_pty_command_header,
    streams_pty_output,
};
use crate::agent::runloop::unified::tool_summary_helpers::{
    COMPACT_PREVIEW_LEN, display_command_text, preview_command, preview_full_command, relativize_command_paths,
};
use vtcode_commons::canonicalize;

fn record_mcp_outcome_event(
    mcp_panel_state: &mut McpPanelState,
    tool_name: &str,
    args_val: &serde_json::Value,
    command_success: bool,
) {
    let mut mcp_event = crate::agent::runloop::mcp_events::McpEvent::new(
        "mcp".to_string(),
        tool_name.to_string(),
        Some(args_val.to_string()),
    );
    if command_success {
        mcp_event.success(None);
    } else {
        mcp_event.failure(Some("Command returned a non-zero exit code".to_string()));
    }
    mcp_panel_state.add_event(mcp_event);
}

fn collect_modified_files(modified_files: &[String]) -> Vec<PathBuf> {
    modified_files.iter().map(PathBuf::from).collect()
}

fn collect_instruction_activity_paths(
    workspace_root: &Path,
    args_val: &serde_json::Value,
    output: &serde_json::Value,
    modified_files: &[String],
) -> Vec<PathBuf> {
    let canonical_workspace = canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let mut paths = BTreeSet::new();
    for modified in modified_files {
        push_activity_path(workspace_root, &canonical_workspace, modified, &mut paths);
    }
    collect_paths_from_value(workspace_root, &canonical_workspace, Some("args"), args_val, &mut paths);
    collect_paths_from_value(workspace_root, &canonical_workspace, Some("output"), output, &mut paths);
    paths.into_iter().collect()
}

fn collect_paths_from_value(
    workspace_root: &Path,
    canonical_workspace: &Path,
    key: Option<&str>,
    value: &serde_json::Value,
    paths: &mut BTreeSet<PathBuf>,
) {
    match value {
        serde_json::Value::String(text) => {
            if key.is_some_and(path_like_key) {
                push_activity_path(workspace_root, canonical_workspace, text, paths);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_paths_from_value(workspace_root, canonical_workspace, key, value, paths);
            }
        }
        serde_json::Value::Object(map) => {
            for (child_key, child_value) in map {
                collect_paths_from_value(
                    workspace_root,
                    canonical_workspace,
                    Some(child_key.as_str()),
                    child_value,
                    paths,
                );
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn path_like_key(key: &str) -> bool {
    matches!(
        key,
        "path"
            | "paths"
            | "file"
            | "files"
            | "file_path"
            | "file_paths"
            | "cwd"
            | "workdir"
            | "directory"
            | "directories"
            | "root"
            | "workspace"
    )
}

fn push_activity_path(workspace_root: &Path, canonical_workspace: &Path, raw: &str, paths: &mut BTreeSet<PathBuf>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains("://") || trimmed.starts_with("untitled:") {
        return;
    }

    let normalized = normalize_workspace_path(workspace_root, Path::new(trimmed));
    if normalized.starts_with(canonical_workspace) || normalized.starts_with(workspace_root) {
        paths.insert(normalized);
    }
}

fn is_run_pty_tool(name: &str, args_val: &serde_json::Value) -> bool {
    renders_pty_command_header(name, args_val)
}

fn is_command_output_call(name: &str, args_val: &serde_json::Value) -> bool {
    name != tools::SEND_PTY_INPUT
        && (name == tools::EXECUTE_CODE
            || tool_intent::is_command_run_tool_call(name, args_val)
            || is_run_pty_tool(name, args_val))
}

fn compact_run_completion_line(output: &serde_json::Value, status: ToolDisplayStatus) -> Option<String> {
    if let Some(exit_code) = output.get("exit_code").and_then(serde_json::Value::as_i64) {
        if matches!(status, ToolDisplayStatus::Success) && exit_code == 0 {
            return Some("✓ run completed (exit code: 0)".to_string());
        }
        if matches!(status, ToolDisplayStatus::Warning) && exit_code == 0 {
            return Some("⚠ run completed with warnings (exit code: 0)".to_string());
        }
        return Some(format!("✗ run error, exit code: {exit_code}"));
    }

    if output.get("is_exited").and_then(serde_json::Value::as_bool) == Some(true) {
        if matches!(status, ToolDisplayStatus::Success) {
            return Some("✓ done".to_string());
        }
        if matches!(status, ToolDisplayStatus::Warning) {
            return Some("⚠ done with warnings".to_string());
        }
        return Some("✗ failed".to_string());
    }

    match status {
        ToolDisplayStatus::Failure => Some("✗ failed".to_string()),
        ToolDisplayStatus::Warning => Some("⚠ completed with warnings".to_string()),
        ToolDisplayStatus::Success => None,
    }
}

fn is_git_diff_payload(output: &serde_json::Value) -> bool {
    output
        .get("content_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|content_type| content_type == "git_diff")
}

fn has_renderable_stream_content(output: &serde_json::Value) -> bool {
    ["output", "stdout", "stderr", "content"].iter().any(|key| {
        output
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn is_task_tracker_tool(name: &str) -> bool {
    matches!(name, tools::TASK_TRACKER)
}

fn task_tracker_block_lines(output: &serde_json::Value, expanded: bool) -> Vec<TrackerLine> {
    crate::agent::runloop::tool_output::tracker_transcript_lines(output, expanded)
}

/// Style map for tracker rows: status surfaces through text styling only.
///
/// Done rows render struck-through, italic, and dimmed; the focused current
/// row renders bold in the theme `primary` accent; blocked rows use the
/// `warning` token; other in-progress rows use `primary`; pending rows keep
/// the default style. All colors come from theme tokens through the shared
/// style bridge, so WCAG contrast holds by construction.
fn task_tracker_block_segments(lines: &[TrackerLine]) -> Vec<Vec<InlineSegment>> {
    use vtcode_core::ui::markdown::RenderMarkdownOptions;
    use vtcode_core::ui::theme;
    use vtcode_core::ui::tui::convert_style;

    let default_style = std::sync::Arc::new(InlineTextStyle::default());
    let base_style = MessageStyle::Info.style();
    let theme_styles = theme::active_styles();
    let mut info_fallback = convert_style(base_style);
    if info_fallback.color.is_none() {
        info_fallback = info_fallback.merge_color(Some(theme_styles.foreground));
    }
    // `primary`/`warning` meet the WCAG AA floor in every built-in theme.
    let primary = convert_style(theme_styles.primary);
    let warning = convert_style(theme_styles.warning);
    let render_options = RenderMarkdownOptions {
        preserve_code_indentation: true,
        disable_code_block_table_reparse: false,
        table_max_width: None,
    };
    lines
        .iter()
        .map(|line| {
            render_tracker_line(
                line,
                &theme_styles,
                &info_fallback,
                &primary,
                &warning,
                &default_style,
                &render_options,
            )
            .unwrap_or_else(|| {
                vec![InlineSegment {
                    text: line.text.clone(),
                    style: default_style.clone(),
                }]
            })
        })
        .collect()
}

/// Render one tracker line with its status style.
///
/// Headers, diagnostics, truncation, and pending rows keep the default
/// rendering; other statuses tint through [`render_tracker_status_row`].
fn render_tracker_line(
    line: &TrackerLine,
    theme_styles: &vtcode_core::ui::theme::ThemeStyles,
    info_fallback: &InlineTextStyle,
    primary: &InlineTextStyle,
    warning: &InlineTextStyle,
    default_style: &std::sync::Arc<InlineTextStyle>,
    render_options: &vtcode_core::ui::markdown::RenderMarkdownOptions,
) -> Option<Vec<InlineSegment>> {
    let row_style = match line.status {
        None | Some(TaskItemStatus::Pending) => {
            return render_tracker_inline_row(&line.text, theme_styles, info_fallback, default_style, render_options);
        }
        Some(TaskItemStatus::Completed) => InlineTextStyle {
            color: None,
            bg_color: None,
            effects: Effects::STRIKETHROUGH | Effects::ITALIC | Effects::DIMMED,
        },
        Some(TaskItemStatus::Blocked) => warning.clone(),
        Some(TaskItemStatus::InProgress) if crate::agent::runloop::tool_output::is_tracker_current_row(&line.text) => {
            let mut current = primary.clone();
            current.effects |= Effects::BOLD;
            current
        }
        Some(TaskItemStatus::InProgress) => primary.clone(),
    };
    render_tracker_status_row(&line.text, &row_style, theme_styles, info_fallback, render_options)
}

/// Split a tree row into its structural prefix and markdown body.
///
/// Handles the current-task marker (`  ▶ `), branch prefixes (`  ├ `), and —
/// for tolerance with legacy payloads — one leading status glyph. Returns
/// `None` for title/diagnostic lines so they keep their plain style.
fn split_tracker_row_prefix(line: &str) -> Option<(&str, &str)> {
    let mut rest = line;
    let mut consumed_prefix = false;
    let leading_spaces = rest.len() - rest.trim_start_matches(' ').len();
    rest = &rest[leading_spaces..];
    if let Some(after) = rest.strip_prefix("▶ ") {
        rest = after;
        consumed_prefix = true;
    }
    loop {
        if let Some(after) = rest
            .strip_prefix("├ ")
            .or_else(|| rest.strip_prefix("└ "))
            .or_else(|| rest.strip_prefix("│ "))
        {
            rest = after;
            consumed_prefix = true;
            continue;
        }
        break;
    }
    for token in ["□ ", "[x] ", "[-] ", "[!] "] {
        if let Some(after) = rest.strip_prefix(token) {
            rest = after;
            consumed_prefix = true;
            break;
        }
    }
    if !consumed_prefix || rest.is_empty() {
        return None;
    }
    let prefix_len = line.len() - rest.len();
    Some((&line[..prefix_len], rest))
}

/// Render one compact tree row as styled segments: plain tree prefix plus a
/// markdown-rendered body so `` `code` ``, **bold**, and file paths display
/// styled instead of raw source. Returns `None` when the row has no tree
/// prefix or markdown yields no visible output (caller falls back to plain).
fn render_tracker_inline_row(
    line: &str,
    theme_styles: &vtcode_core::ui::theme::ThemeStyles,
    fallback: &InlineTextStyle,
    default_style: &std::sync::Arc<InlineTextStyle>,
    render_options: &vtcode_core::ui::markdown::RenderMarkdownOptions,
) -> Option<Vec<InlineSegment>> {
    use vtcode_core::ui::markdown::render_markdown_to_lines_with_options;
    use vtcode_core::ui::tui::convert_style;

    let (prefix, body) = split_tracker_row_prefix(line)?;
    if body.trim().is_empty() {
        return None;
    }
    let rendered =
        render_markdown_to_lines_with_options(body, MessageStyle::Info.style(), theme_styles, None, *render_options);
    let mut segments = Vec::with_capacity(4);
    segments.push(InlineSegment {
        text: prefix.to_string(),
        style: default_style.clone(),
    });
    // Single-line descriptions stay one row so inline replacement counts stay
    // aligned; join any extra markdown lines with a space.
    let rendered_lines = rendered
        .iter()
        .filter(|rendered_line| !rendered_line.is_empty())
        .collect::<Vec<_>>();
    let mut wrote_body = false;
    for (line_index, rendered_line) in rendered_lines.iter().enumerate() {
        if line_index > 0 {
            segments.push(InlineSegment {
                text: " ".to_string(),
                style: default_style.clone(),
            });
        }
        for seg in &rendered_line.segments {
            if seg.text.is_empty() {
                continue;
            }
            let converted = convert_style(seg.style);
            let mut inline_style = fallback.clone();
            inline_style.color = None;
            if let Some(color) = converted.color
                && Some(color) != fallback.color
            {
                inline_style.color = Some(color);
            }
            if let Some(bg) = converted.bg_color {
                inline_style.bg_color = Some(bg);
            }
            inline_style.effects = converted.effects | fallback.effects;
            segments.push(InlineSegment {
                text: seg.text.clone(),
                style: std::sync::Arc::new(inline_style),
            });
            wrote_body = true;
        }
    }
    wrote_body.then_some(segments)
}

/// Render one status-styled tree row: structural prefix in the row style plus
/// a markdown-rendered body so `` `code` ``, **bold**, and file paths display
/// styled instead of raw source. Unstyled body text takes the row style (done
/// rows strike through, current rows glow in the accent); genuinely distinct
/// markdown spans keep their colors. Returns `None` when the row has no tree
/// prefix or markdown yields no visible output (caller falls back to plain).
fn render_tracker_status_row(
    line: &str,
    row_style: &InlineTextStyle,
    theme_styles: &vtcode_core::ui::theme::ThemeStyles,
    info_fallback: &InlineTextStyle,
    render_options: &vtcode_core::ui::markdown::RenderMarkdownOptions,
) -> Option<Vec<InlineSegment>> {
    use vtcode_core::ui::markdown::render_markdown_to_lines_with_options;
    use vtcode_core::ui::tui::convert_style;

    let (prefix, body) = split_tracker_row_prefix(line)?;
    if body.trim().is_empty() {
        return None;
    }
    let rendered =
        render_markdown_to_lines_with_options(body, MessageStyle::Info.style(), theme_styles, None, *render_options);
    let prefix_style = std::sync::Arc::new(row_style.clone());
    let mut segments = Vec::with_capacity(4);
    segments.push(InlineSegment {
        text: prefix.to_string(),
        style: prefix_style.clone(),
    });
    let rendered_lines = rendered
        .iter()
        .filter(|rendered_line| !rendered_line.is_empty())
        .collect::<Vec<_>>();
    let mut wrote_body = false;
    for (line_index, rendered_line) in rendered_lines.iter().enumerate() {
        if line_index > 0 {
            segments.push(InlineSegment { text: " ".to_string(), style: prefix_style.clone() });
        }
        for seg in &rendered_line.segments {
            if seg.text.is_empty() {
                continue;
            }
            let converted = convert_style(seg.style);
            let mut inline_style = row_style.clone();
            if converted.color.is_none_or(|color| Some(color) == info_fallback.color) {
                inline_style.color = row_style.color;
            } else {
                inline_style.color = converted.color;
            }
            if let Some(bg) = converted.bg_color {
                inline_style.bg_color = Some(bg);
            }
            inline_style.effects = converted.effects | row_style.effects;
            // Unstyled body text keeps the row style; the cleared color above
            // already resolves it when markdown matches the Info base.
            if inline_style.color.is_none() {
                inline_style.color = row_style.color;
            }
            segments.push(InlineSegment {
                text: seg.text.clone(),
                style: std::sync::Arc::new(inline_style),
            });
            wrote_body = true;
        }
    }
    wrote_body.then_some(segments)
}

/// Single writer for user-facing tracker transcript blocks.
///
/// Approval handoff and the tool pipeline must share replace/dedupe so progress
/// updates never stack a second block. UI `replace_last` uses the UI write
/// length recorded by this helper — never a TRANSCRIPT-derived count, which
/// can clobber unrelated UI lines when the two stores diverge.
pub(crate) fn write_tracker_progress_transcript(handle: &InlineHandle, lines: Vec<TrackerLine>) {
    if lines.is_empty() {
        return;
    }
    let texts: Vec<String> = lines.iter().map(|line| line.text.clone()).collect();
    let ui_write_len = lines.len();
    if transcript::tail_matches(&texts) {
        transcript::remember_tracker_block_with_ui_len(texts, ui_write_len);
        return;
    }
    let segments = task_tracker_block_segments(&lines);
    if let Some(transcript_count) = transcript::tracker_block_len_if_at_tail() {
        let ui_count = transcript::tracker_ui_write_len().unwrap_or(ui_write_len);
        handle.replace_last(ui_count, InlineMessageKind::Tool, segments);
        transcript::replace_last(transcript_count, &texts);
        transcript::remember_tracker_block_with_ui_len(texts, ui_write_len);
        return;
    }
    // Fallback: remembered block drifted. Only replace a tail that is clearly
    // our tracker shape (progress header, tree row, or truncation line), never
    // generic `• Foo N/M` UI summaries such as Questions answered lines.
    // UI replace uses the remembered UI length; transcript replace uses the
    // trailing tracker-like count so expanded blocks replace fully.
    if transcript::last_line().is_some_and(|last| looks_like_tracker_content(&last)) {
        let ui_count = transcript::tracker_ui_write_len().unwrap_or(1).max(1);
        let transcript_count = trailing_tracker_content_len().max(1);
        handle.replace_last(ui_count, InlineMessageKind::Tool, segments);
        transcript::replace_last(transcript_count, &texts);
        transcript::remember_tracker_block_with_ui_len(texts, ui_write_len);
        return;
    }
    for (segments, plain_line) in segments.into_iter().zip(texts.iter()) {
        handle.append_line(InlineMessageKind::Tool, segments);
        transcript::append(plain_line);
    }
    transcript::remember_tracker_block_with_ui_len(texts, ui_write_len);
}

fn looks_like_tracker_progress_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with("• ") {
        return false;
    }
    if trimmed.starts_with("• Plan") || trimmed.starts_with("• Ran") || trimmed.starts_with("• Questions") {
        return false;
    }
    if trimmed.starts_with("• Tasks") {
        return true;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return false;
    }
    if tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case("answered") || token.eq_ignore_ascii_case("questions"))
    {
        return false;
    }
    let Some(last) = tokens.last() else {
        return false;
    };
    let mut parts = last.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(a), Some(b), None) if !a.is_empty()
            && !b.is_empty()
            && a.chars().all(|c| c.is_ascii_digit())
            && b.chars().all(|c| c.is_ascii_digit())
    )
}

fn looks_like_tracker_tree_row(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("▶ ") {
        return true;
    }
    if trimmed.starts_with("├ ") || trimmed.starts_with("└ ") || trimmed.starts_with("│ ") {
        return true;
    }
    if trimmed.starts_with("□ ")
        || trimmed.starts_with("[x] ")
        || trimmed.starts_with("[-] ")
        || trimmed.starts_with("[!] ")
    {
        return true;
    }
    // Expanded truncation row (`  … N more`).
    trimmed.starts_with('…') || trimmed.starts_with("...")
}

fn looks_like_tracker_content(line: &str) -> bool {
    looks_like_tracker_progress_line(line) || looks_like_tracker_tree_row(line)
}

fn trailing_tracker_content_len() -> usize {
    const MAX_SCAN: usize = 40;
    transcript::snapshot()
        .iter()
        .rev()
        .take(MAX_SCAN)
        .take_while(|line| looks_like_tracker_content(line))
        .count()
}

fn apply_task_tracker_block(handle: &InlineHandle, lines: Vec<TrackerLine>) {
    write_tracker_progress_transcript(handle, lines);
}

/// Extract the command string from tool call arguments for display.
///
/// Uses display-safe joining (bare `|`, `>`, `;`; quotes only whitespace) so
/// `• Ran` headers read as shell, not as quoted tokens like `'|'`/`'2>'`.
fn extract_command_line(args: &serde_json::Value) -> Option<String> {
    display_command_text(args)
}

/// Shared display → relativize → single-line preview pipeline for `• Ran …`
/// headers. Keeps the collapsed row and the viewer header consistent instead
/// of echoing a multi-line script.
fn compact_command_preview(args: &serde_json::Value, workspace_root: Option<&Path>) -> Option<String> {
    display_command_text(args)
        .map(|command| relativize_command_paths(&command, workspace_root))
        .map(|command| preview_command(&command, COMPACT_PREVIEW_LEN))
        .filter(|command| !command.is_empty())
}

fn compact_command_text(name: &str, args: &serde_json::Value, workspace_root: Option<&Path>) -> String {
    // Display join (no shell_words quoting) plus a first-line head-truncated
    // preview: the collapsed row must stay readable, not executable-looking.
    compact_command_preview(args, workspace_root).unwrap_or_else(|| name.to_string())
}

fn compact_hidden_line_count(output: &serde_json::Value, complete_capture: Option<&str>) -> usize {
    if let Some(capture) = complete_capture {
        return normalize_terminal_output_lines(capture).len();
    }

    canonical_pipe_streams(output)
        .into_iter()
        .map(|stream| {
            if stream.label == Some("stderr") {
                return 0;
            }

            let line_count = normalize_terminal_output_lines(stream.text).len();
            if stream.label.is_none()
                && let Some(stderr) = output_text(output, "stderr")
                && streams_are_aliases(stream.text, stderr)
            {
                return line_count.saturating_sub(normalize_terminal_output_lines(stderr).len());
            }
            line_count
        })
        .sum()
}

fn render_command_summary(
    renderer: &mut AnsiRenderer,
    name: &str,
    args_val: &serde_json::Value,
    output: &serde_json::Value,
    command_success: bool,
    workspace_root: Option<&Path>,
    viewer_id: Option<ToolOutputId>,
    force_expanded: bool,
) -> Result<()> {
    if let Some(viewer_id) = viewer_id {
        // Carry the identity on the summary command itself. Text/order
        // matching is ambiguous when async calls run the same command.
        renderer.set_next_tool_output_anchor(viewer_id);
    }
    let stream_label = crate::agent::runloop::unified::tool_summary::stream_label_from_output(output, command_success);
    let summary_ctx = crate::agent::runloop::unified::tool_summary::ToolSummaryRenderContext { workspace_root };
    let status = ToolDisplayStatus::from_command_output(output, command_success);
    let bullet_color = status.color(ColorPalette::default());
    if force_expanded {
        crate::agent::runloop::unified::tool_summary::render_expanded_tool_call_summary(
            renderer,
            name,
            args_val,
            stream_label,
            &summary_ctx,
            bullet_color,
        )
    } else {
        crate::agent::runloop::unified::tool_summary::render_tool_call_summary(
            renderer,
            name,
            args_val,
            stream_label,
            &summary_ctx,
            bullet_color,
        )
    }
}

fn value_has_content(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::String(value) => !value.trim().is_empty(),
        serde_json::Value::Array(values) => !values.is_empty(),
        serde_json::Value::Object(values) => !values.is_empty(),
        serde_json::Value::Number(_) => true,
    }
}

const STRUCTURED_COMMAND_OUTPUT_FIELDS: &[&str] = &[
    "output",
    "stdout",
    "stderr",
    "content",
    "command",
    "critical_note",
    "next_action",
    "exit_code",
];

const COMPACT_COMMAND_ARTIFACT_FIELDS: &[&str] = &[
    "generated_files",
    "json_result",
    "modified_files",
    "diff",
    "diff_preview",
    "failure_diagnostics",
    "security_notice",
    "artifacts",
];

fn structured_command_context(output: &serde_json::Value) -> Option<String> {
    let object = output.as_object()?;
    let metadata = object
        .iter()
        .filter(|(key, value)| {
            !STRUCTURED_COMMAND_OUTPUT_FIELDS.contains(&key.as_str()) && !matches!(value, serde_json::Value::Null)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();

    if metadata.is_empty() {
        return None;
    }

    serde_json::to_string_pretty(&serde_json::Value::Object(metadata)).ok()
}

fn append_structured_command_context(lines: &mut Vec<String>, output: &serde_json::Value) {
    let Some(context) = structured_command_context(output) else {
        return;
    };

    lines.push("  structured output:".to_string());
    lines.extend(context.lines().map(|line| format!("    {line}")));
}

fn render_structured_command_context(renderer: &mut AnsiRenderer, output: &serde_json::Value) -> Result<()> {
    let Some(context) = structured_command_context(output) else {
        return Ok(());
    };

    renderer.line(MessageStyle::ToolDetail, "structured output:")?;
    for line in context.lines() {
        renderer.line(MessageStyle::ToolDetail, &format!("  {line}"))?;
    }
    Ok(())
}

fn complete_capture_unavailable(output: &serde_json::Value, complete_capture: Option<&str>) -> bool {
    output.get("spool_path").is_some() && complete_capture.is_none()
}

fn has_compact_command_artifact(output: &serde_json::Value, complete_capture: Option<&str>) -> bool {
    output_text(output, "critical_note").is_some()
        || stderr_for_inline_display(output).is_some()
        || complete_capture_unavailable(output, complete_capture)
        || COMPACT_COMMAND_ARTIFACT_FIELDS
            .iter()
            .any(|key| output.get(*key).is_some_and(value_has_content))
        || [
            "security_notice",
            "next_action",
            "next_continue_args",
            "next_read_args",
            "fallback_tool",
            "fallback_tool_args",
        ]
        .iter()
        .any(|key| output.get(*key).is_some_and(value_has_content))
        || output.get("loop_detected").and_then(serde_json::Value::as_bool) == Some(true)
}

fn has_file_operation_diff(output: &serde_json::Value) -> bool {
    !vtcode_core::tools::file_ops::canonical_diff_previews(output).is_empty()
}

fn warning_message(output: &serde_json::Value) -> Option<String> {
    let warning = output.get("warning")?;
    match warning {
        serde_json::Value::String(message) => {
            let message = message.trim();
            (!message.is_empty()).then(|| message.to_string())
        }
        serde_json::Value::Number(number) if number.as_f64().is_some_and(|value| value != 0.0) => {
            Some(format!("warning count: {number}"))
        }
        serde_json::Value::Bool(true) => Some("completed with warnings".to_string()),
        serde_json::Value::Array(values) if !values.is_empty() => Some("completed with warnings".to_string()),
        serde_json::Value::Object(values) if !values.is_empty() => {
            let message = warning
                .as_object()
                .and_then(|fields| fields.get("message"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty());
            Some(message.unwrap_or("completed with warnings").to_string())
        }
        _ => None,
    }
}

fn append_warning_line(lines: &mut Vec<String>, output: &serde_json::Value) {
    if let Some(message) = warning_message(output) {
        lines.push(format!("    ⚠ {message}"));
    }
}

fn append_capture_status_line(lines: &mut Vec<String>, output: &serde_json::Value, complete_capture: Option<&str>) {
    if complete_capture_unavailable(output, complete_capture) {
        lines.push("    Complete command output capture unavailable.".to_string());
    }
}

/// Record the tool-call summary line ("• Ran ...") to the transcript only.
fn record_summary_line(name: &str, args: &serde_json::Value, _output: &serde_json::Value, _command_success: bool) {
    let action_label = if tool_intent::is_command_run_tool_call(name, args) {
        "Run command"
    } else {
        name
    };
    let headline = if action_label == "Run command" {
        if let Some(cmd) = extract_command_line(args) {
            format!("Ran {cmd}")
        } else {
            "Ran command".to_string()
        }
    } else {
        format!("• {action_label}")
    };
    transcript::append(&headline);
}

fn contains_line_block(container: &str, candidate: &str) -> bool {
    !line_block_ranges(container, candidate).is_empty()
}

fn line_block_ranges(container: &str, candidate: &str) -> Vec<(usize, usize)> {
    let container_lines = container.lines().collect::<Vec<_>>();
    let candidate_lines = candidate.lines().collect::<Vec<_>>();
    if candidate_lines.is_empty() || candidate_lines.len() > container_lines.len() {
        return Vec::new();
    }

    container_lines
        .windows(candidate_lines.len())
        .enumerate()
        .filter_map(|(start, window)| {
            (window == candidate_lines.as_slice()).then_some((start, start + candidate_lines.len()))
        })
        .collect()
}

fn contains_distinct_line_blocks(container: &str, first: &str, second: &str) -> bool {
    let first_ranges = line_block_ranges(container, first);
    let second_ranges = line_block_ranges(container, second);
    first_ranges.iter().any(|&(first_start, first_end)| {
        second_ranges
            .iter()
            .any(|&(second_start, second_end)| first_end <= second_start || second_end <= first_start)
    })
}

fn streams_are_aliases(left: &str, right: &str) -> bool {
    contains_line_block(left, right) || contains_line_block(right, left)
}

fn output_text<'a>(output: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    output
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim_end)
        .filter(|text| !text.trim().is_empty())
}

fn stderr_for_inline_display(output: &serde_json::Value) -> Option<&str> {
    let stderr = output_text(output, "stderr")?;
    // Named streams are distinct unless the authoritative merged `output`
    // field proves that stderr is already present in the terminal capture.
    // stdout and stderr can legitimately contain identical text.
    let already_visible = output_text(output, "output").is_some_and(|merged| {
        output_text(output, "stdout").map_or_else(
            || contains_line_block(merged, stderr),
            |stdout| contains_distinct_line_blocks(merged, stdout, stderr),
        )
    });
    if already_visible { None } else { Some(stderr) }
}

fn ordered_stream_texts(output: &serde_json::Value) -> Vec<&str> {
    canonical_pipe_streams(output).into_iter().map(|stream| stream.text).collect()
}

#[derive(Clone, Copy)]
struct CanonicalOutputStream<'a> {
    label: Option<&'static str>,
    text: &'a str,
}

fn append_named_streams<'a>(
    streams: &mut Vec<CanonicalOutputStream<'a>>,
    stdout: Option<&'a str>,
    stderr: Option<&'a str>,
) {
    if let Some(stdout) = stdout {
        streams.push(CanonicalOutputStream { label: Some("stdout"), text: stdout });
    }
    if let Some(stderr) = stderr {
        streams.push(CanonicalOutputStream { label: Some("stderr"), text: stderr });
    }
}

fn append_content_stream<'a>(streams: &mut Vec<CanonicalOutputStream<'a>>, content: Option<&'a str>) {
    let Some(content) = content else {
        return;
    };
    if !streams.iter().any(|stream| contains_line_block(stream.text, content)) {
        streams.push(CanonicalOutputStream { label: None, text: content });
    }
}

fn canonical_pipe_streams(output: &serde_json::Value) -> Vec<CanonicalOutputStream<'_>> {
    let merged = output_text(output, "output");
    let stdout = output_text(output, "stdout");
    let stderr = output_text(output, "stderr");
    let content = output_text(output, "content");
    let mut streams = Vec::new();

    if let Some(merged) = merged {
        let stdout_is_in_merged = stdout.is_some_and(|text| contains_line_block(merged, text));
        let stderr_is_in_merged = stderr.is_some_and(|text| contains_line_block(merged, text));
        let stdout_contains_merged = stdout.is_some_and(|text| contains_line_block(text, merged));
        let stderr_contains_merged = stderr.is_some_and(|text| contains_line_block(text, merged));

        // A combined `output` field is authoritative when it contains both
        // named streams as distinct blocks. Requiring non-overlapping blocks
        // matters when stdout and stderr happen to be identical or one is a
        // prefix of the other: one occurrence cannot prove both are copies.
        if let (Some(stdout), Some(stderr)) = (stdout, stderr)
            && contains_distinct_line_blocks(merged, stdout, stderr)
        {
            streams.push(CanonicalOutputStream { label: None, text: merged });
            append_content_stream(&mut streams, content);
            return streams;
        }

        // When both named streams contain the merged value, the merged field
        // is a bounded preview. Keep each complete, labeled stream instead of
        // guessing that the single preview occurrence represents both pipes.
        if stdout_contains_merged && stderr_contains_merged {
            append_named_streams(&mut streams, stdout, stderr);
            append_content_stream(&mut streams, content);
            return streams;
        }

        // A preview nested in either one named stream is best represented by
        // the complete named values. The other stream remains labeled even
        // when its content is not present in the preview.
        if stdout_contains_merged || stderr_contains_merged {
            append_named_streams(&mut streams, stdout, stderr);
            append_content_stream(&mut streams, content);
            return streams;
        }

        // If both named streams are present in the merged field but overlap,
        // preserve their labels and retain merged-only lines when neither
        // named value covers the whole merged field.
        if stdout_is_in_merged && stderr_is_in_merged {
            append_named_streams(&mut streams, stdout, stderr);
            if !stdout_contains_merged && !stderr_contains_merged {
                streams.push(CanonicalOutputStream { label: None, text: merged });
            }
            append_content_stream(&mut streams, content);
            return streams;
        }

        // A merged value containing only one named stream still carries
        // unlabelled content. Keep that merged value and append the other
        // named stream rather than dropping it as an apparent alias.
        streams.push(CanonicalOutputStream { label: None, text: merged });
        if let Some(stdout) = stdout
            && !stdout_is_in_merged
        {
            streams.push(CanonicalOutputStream { label: Some("stdout"), text: stdout });
        }
        if let Some(stderr) = stderr
            && !stderr_is_in_merged
        {
            streams.push(CanonicalOutputStream { label: Some("stderr"), text: stderr });
        }
        append_content_stream(&mut streams, content);
        return streams;
    }

    if let Some(stdout) = stdout {
        streams.push(CanonicalOutputStream { label: Some("stdout"), text: stdout });
    }
    if let Some(stderr) = stderr {
        // Without a merged authoritative field, stdout and stderr are
        // separate pipes even when their contents happen to match.
        streams.push(CanonicalOutputStream { label: Some("stderr"), text: stderr });
    }
    append_content_stream(&mut streams, content);
    streams
}

async fn load_complete_output(output: &serde_json::Value, workspace_root: Option<&Path>) -> Option<String> {
    // A present but malformed spool marker is still a spool reference. Never
    // reinterpret its inline preview as a trustworthy complete capture.
    if output.get("spool_path").is_some() && vtcode_core::tools::SpooledOutputReference::from_value(output).is_none() {
        return None;
    }
    if let Some(reference) = vtcode_core::tools::SpooledOutputReference::from_value(output) {
        let root = workspace_root?;
        let owned = reference.spool_path.to_string();
        let root = root.to_path_buf();
        let state = reference.state;
        let byte_count = reference.original_bytes;
        let digest = reference.sha256.map(str::to_string);
        return tokio::task::spawn_blocking(move || {
            let value = serde_json::json!({
                "spool_path": owned,
                "spooled_bytes": byte_count,
                "spool_sha256": digest,
                "spool_state": match state {
                    vtcode_core::tools::SpoolState::Pending => "pending",
                    vtcode_core::tools::SpoolState::Completed => "completed",
                    vtcode_core::tools::SpoolState::Unknown => "unknown",
                },
            });
            let reference = vtcode_core::tools::SpooledOutputReference::from_value(&value)?;
            match state {
                vtcode_core::tools::SpoolState::Completed => reference.read_verified_completed(&root).ok(),
                vtcode_core::tools::SpoolState::Pending => reference.read_pending_bounded(&root, 64 * 1024).ok(),
                vtcode_core::tools::SpoolState::Unknown => None,
            }
        })
        .await
        .ok()
        .flatten();
    }

    if output_text(output, "output").is_none()
        && (output_text(output, "stdout").is_some() || output_text(output, "stderr").is_some())
    {
        // Named pipe streams remain labeled in the viewer. Joining them here
        // would make the later capture renderer mistake stderr for a copy of
        // stdout and drop it.
        return None;
    }

    let texts = ordered_stream_texts(output);
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn normalize_terminal_output_lines(capture: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = capture.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let final_byte = loop {
                        let Some(next) = chars.next() else {
                            break None;
                        };
                        if ('@'..='~').contains(&next) {
                            break Some(next);
                        }
                        params.push(next);
                    };

                    match final_byte {
                        // Clear-screen sequences mean that the earlier text
                        // was only a stale terminal frame, not command output
                        // that should remain in the readable viewer.
                        Some('J') if params.starts_with('2') || params.starts_with('3') => {
                            lines.clear();
                            current.clear();
                        }
                        // Erase the current line for the common progress-bar
                        // rewrite sequence. Styling and cursor movement are
                        // intentionally omitted from the plain-text viewer.
                        Some('K') if params.starts_with('2') => current.clear(),
                        _ => {}
                    }
                }
                Some(']') => {
                    // Skip OSC title/hyperlink sequences through BEL or ST.
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            let _ = chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            },
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    let _ = chars.next();
                    lines.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            '\n' => lines.push(std::mem::take(&mut current)),
            '\u{8}' => {
                let _ = current.pop();
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn normalized_lines_contain_subsequence(container: &[String], candidate: &[String]) -> bool {
    if candidate.is_empty() {
        return false;
    }

    let mut candidate_index = 0;
    for line in container {
        if line == &candidate[candidate_index] {
            candidate_index += 1;
            if candidate_index == candidate.len() {
                return true;
            }
        }
    }
    false
}

fn command_output_header(name: &str, args: &serde_json::Value, workspace_root: Option<&Path>) -> String {
    // Transcript Review shows the complete command: viewport-aware wrapping
    // (TUI reflow) owns overflow instead of a `…` truncation, so long chained
    // commands remain readable in full. Uses the same script-runner folding as
    // compact previews but applies no length cap.
    display_command_text(args)
        .map(|command| relativize_command_paths(&command, workspace_root))
        .map(|command| preview_full_command(&command))
        .filter(|command| !command.is_empty())
        .map(|command| format!("• Ran {command}"))
        .unwrap_or_else(|| format!("• Ran {name}"))
}

fn append_merged_output_lines(lines: &mut Vec<String>, output_lines: impl IntoIterator<Item = String>) {
    for (index, line) in output_lines.into_iter().enumerate() {
        if index == 0 {
            lines.push(format!("  └ {line}"));
        } else {
            lines.push(format!("    {line}"));
        }
    }
}

fn append_labeled_output_lines(lines: &mut Vec<String>, label: &str, output_lines: impl IntoIterator<Item = String>) {
    lines.push(format!("  {label}:"));
    for line in output_lines {
        lines.push(format!("    {line}"));
    }
}

fn append_viewer_status_line(lines: &mut Vec<String>, output: &serde_json::Value, status: ToolDisplayStatus) {
    if !matches!(status, ToolDisplayStatus::Success)
        && let Some(completion) = compact_run_completion_line(output, status)
    {
        lines.push(format!("    {completion}"));
    }
}

fn build_merged_command_output_lines(
    name: &str,
    args: &serde_json::Value,
    capture: &str,
    workspace_root: Option<&Path>,
    output: &serde_json::Value,
    status: ToolDisplayStatus,
) -> Vec<String> {
    let mut lines = vec![command_output_header(name, args, workspace_root)];
    let named_streams = canonical_pipe_streams(output);
    let capture_lines = normalize_terminal_output_lines(capture);
    let has_spool_metadata = output.get("spool_path").is_some();

    if has_spool_metadata {
        // A successfully loaded spool is the complete capture; the inline
        // output field is only a bounded preview and must not be shown beside
        // it. If the spool could not be loaded, keep the path fail-closed and
        // avoid presenting the untrusted preview as complete output.
        if !capture_lines.is_empty() {
            append_merged_output_lines(&mut lines, capture_lines.clone());
            for stream in &named_streams {
                let Some(label) = stream.label else {
                    continue;
                };
                let stream_lines = normalize_terminal_output_lines(stream.text);
                if !normalized_lines_contain_subsequence(&capture_lines, &stream_lines) {
                    append_labeled_output_lines(&mut lines, label, stream_lines);
                }
            }
        }
    } else if named_streams.is_empty() {
        append_merged_output_lines(&mut lines, capture_lines);
    } else {
        for stream in &named_streams {
            let output_lines = normalize_terminal_output_lines(stream.text);
            if let Some(label) = stream.label {
                append_labeled_output_lines(&mut lines, label, output_lines);
            } else {
                append_merged_output_lines(&mut lines, output_lines);
            }
        }

        // A PTY spool can contain terminal data not represented by the named
        // pipe fields. Preserve that extra capture explicitly, but do not use
        // it to deduplicate stdout and stderr when no merged field is present.
        let named_lines = named_streams
            .iter()
            .flat_map(|stream| normalize_terminal_output_lines(stream.text))
            .collect::<Vec<_>>();
        if !capture_lines.is_empty() && capture_lines != named_lines {
            append_labeled_output_lines(&mut lines, "output", capture_lines);
        }
    }
    if let Some(note) = output_text(output, "critical_note") {
        lines.push(format!("    {note}"));
    }
    append_warning_line(&mut lines, output);
    append_viewer_status_line(&mut lines, output, status);
    append_structured_command_context(&mut lines, output);
    lines
}

fn build_pipe_command_output_lines(
    name: &str,
    args: &serde_json::Value,
    output: &serde_json::Value,
    workspace_root: Option<&Path>,
    status: ToolDisplayStatus,
) -> Vec<String> {
    let mut lines = vec![command_output_header(name, args, workspace_root)];
    for stream in canonical_pipe_streams(output) {
        let output_lines = normalize_terminal_output_lines(stream.text);
        if output_lines.is_empty() {
            continue;
        }
        if let Some(label) = stream.label {
            append_labeled_output_lines(&mut lines, label, output_lines);
        } else {
            append_merged_output_lines(&mut lines, output_lines);
        }
    }
    if let Some(note) = output_text(output, "critical_note") {
        lines.push(format!("    {note}"));
    }
    append_warning_line(&mut lines, output);
    append_viewer_status_line(&mut lines, output, status);
    append_structured_command_context(&mut lines, output);
    lines
}

fn append_follow_up_capture_lines(lines: &mut Vec<String>, output: &serde_json::Value, rendered_output: Option<&str>) {
    for hint in crate::agent::runloop::tool_output::tool_follow_up_hints_for_capture(output, rendered_output) {
        lines.push(format!("    {hint}"));
    }
}

async fn render_tool_output_common(
    renderer: &mut AnsiRenderer,
    handle: &InlineHandle,
    name: &str,
    args_val: &serde_json::Value,
    output: &serde_json::Value,
    command_success: bool,
    vt_config: Option<&VTCodeConfig>,
    workspace_root: Option<&Path>,
) -> Result<()> {
    let inline_run_tool = renderer.supports_inline_ui() && streams_pty_output(name, args_val);
    let git_diff_payload = is_git_diff_payload(output);
    let status = ToolDisplayStatus::from_command_output(output, command_success);
    let has_spool_path = output.get("spool_path").is_some();
    let complete_capture = if renderer.supports_inline_ui()
        && is_command_output_call(name, args_val)
        && (inline_run_tool || has_spool_path)
    {
        load_complete_output(output, workspace_root).await
    } else {
        None
    };

    // For streamed inline PTY tools, retain the complete capture separately.
    // Expanded mode renders a bounded live block; compact mode waits for the
    // completion row so the transcript does not jump through transient PTY
    // replacements.
    let inline_pty_command = inline_run_tool && is_command_output_call(name, args_val);
    let compact_pty_without_live_preview =
        inline_pty_command && renderer.tool_display_mode() == ToolDisplayMode::Compact;
    if inline_pty_command && !git_diff_payload {
        // Prefer the complete PTY spool (or the complete inline result) for
        // the session-local tool-output viewer. The live PTY block, when
        // enabled, remains bounded separately.
        let viewer_id = if let Some(capture) = complete_capture.as_deref() {
            let mut viewer_lines =
                build_merged_command_output_lines(name, args_val, capture, workspace_root, output, status);
            append_capture_status_line(&mut viewer_lines, output, complete_capture.as_deref());
            append_follow_up_capture_lines(&mut viewer_lines, output, Some(capture));
            Some(handle.record_tool_output(viewer_lines))
        } else {
            // A rejected or unavailable spool must not fall back to a
            // potentially untrusted path. Keep the command call visible in
            // the viewer while retaining fail-closed spool handling.
            let mut viewer_lines = if has_spool_path {
                build_merged_command_output_lines(name, args_val, "", workspace_root, output, status)
            } else {
                build_pipe_command_output_lines(name, args_val, output, workspace_root, status)
            };
            append_capture_status_line(&mut viewer_lines, output, complete_capture.as_deref());
            append_follow_up_capture_lines(&mut viewer_lines, output, complete_capture.as_deref());
            Some(handle.record_tool_output(viewer_lines))
        };

        let compact_success = renderer.tool_display_mode() == ToolDisplayMode::Compact
            && is_command_output_call(name, args_val)
            && matches!(status, ToolDisplayStatus::Success)
            && !has_compact_command_artifact(output, complete_capture.as_deref());
        if compact_success {
            renderer.collapse_pty_block_to_compact_activity(
                compact_command_text(name, args_val, workspace_root),
                compact_hidden_line_count(output, complete_capture.as_deref()),
                None,
                viewer_id,
            )?;
            return Ok(());
        }

        // A completed PTY result with a warning, diagnostic, diff, or failed
        // capture is a hard boundary regardless of whether a live preview was
        // shown. The next successful command must start a new group.
        renderer.flush_compact_command_group();

        if complete_capture_unavailable(output, complete_capture.as_deref()) {
            renderer.line(MessageStyle::Warning, "Complete command output capture unavailable.")?;
        }
        if let Some(message) = warning_message(output) {
            renderer.line(MessageStyle::Warning, &format!("⚠ {message}"))?;
        }

        // Expanded mode retains the existing live PTY block and makes the
        // post-execution summary available to the normal transcript path.
        // Compact mode has no live block, so render an anchored summary now
        // to keep attention-worthy results identifiable in the inline UI.
        if compact_pty_without_live_preview {
            render_command_summary(renderer, name, args_val, output, command_success, workspace_root, viewer_id, true)?;
        } else {
            record_summary_line(name, args_val, output, command_success);
        }

        if let Some(note) = output_text(output, "critical_note") {
            renderer.line(MessageStyle::ToolError, note)?;
            transcript::append(note);
        }

        // A distinct stderr field is not part of the live PTY block. Keep it
        // visible after completion, while alias detection avoids repeating a
        // stderr stream already included in the terminal capture.
        if let Some(stderr) = stderr_for_inline_display(output) {
            let stderr_lines = normalize_terminal_output_lines(stderr);
            if !stderr_lines.is_empty() {
                renderer.line(MessageStyle::ToolError, &format!("stderr: {}", stderr_lines.join("\n")))?;
            }
        }

        if !has_renderable_stream_content(output) && matches!(status, ToolDisplayStatus::Success) {
            if renderer.tool_display_mode() != ToolDisplayMode::Compact {
                renderer.line(MessageStyle::Info, "(no output)")?;
            }
            return Ok(());
        }

        // Send completion as a status line only when the command needs
        // attention; on success the colored header bullet is sufficient.
        if !matches!(status, ToolDisplayStatus::Success) {
            if let Some(completion) = compact_run_completion_line(output, status) {
                let indented = format!("    {}", completion);
                renderer.line(MessageStyle::Status, &indented)?;
                transcript::append(&completion);
            }
        }
        return Ok(());
    }

    // Session follow-ups need a viewer capture too: the inline body is capped
    // at 10 rows and the expand notice opens this record, so the complete
    // stdin/stdout capture stays reachable without leaving the TUI.
    let is_session_followup = is_exec_session_call(name, args_val);
    // Drop any unused expand anchor from a prior call that did not overflow.
    let _ = renderer.take_session_expand_anchor();
    renderer.set_session_body(is_session_followup);
    let viewer_id = if renderer.supports_inline_ui() && (is_command_output_call(name, args_val) || is_session_followup)
    {
        let mut viewer_lines = if is_session_followup {
            build_pipe_command_output_lines(name, args_val, output, workspace_root, status)
        } else if inline_run_tool || has_spool_path {
            complete_capture.as_deref().map_or_else(
                || build_merged_command_output_lines(name, args_val, "", workspace_root, output, status),
                |capture| build_merged_command_output_lines(name, args_val, capture, workspace_root, output, status),
            )
        } else {
            build_pipe_command_output_lines(name, args_val, output, workspace_root, status)
        };
        append_capture_status_line(&mut viewer_lines, output, complete_capture.as_deref());
        append_follow_up_capture_lines(&mut viewer_lines, output, complete_capture.as_deref());
        let viewer_id = handle.record_tool_output(viewer_lines);
        // Session expand notices consume this when the 10-row body overflows.
        if is_session_followup {
            renderer.set_session_expand_anchor(viewer_id);
        }
        Some(viewer_id)
    } else {
        None
    };

    let compact_command = renderer.supports_inline_ui()
        && is_command_output_call(name, args_val)
        && renderer.tool_display_mode() == ToolDisplayMode::Compact
        && matches!(status, ToolDisplayStatus::Success)
        && !git_diff_payload;
    let compact_file_diff = renderer.supports_inline_ui()
        && renderer.tool_display_mode() == ToolDisplayMode::Compact
        && matches!(status, ToolDisplayStatus::Success)
        && crate::agent::runloop::unified::tool_summary::is_file_modification_tool(name, args_val)
        && has_file_operation_diff(output);
    let compact_artifact = has_compact_command_artifact(output, complete_capture.as_deref());
    if !matches!(status, ToolDisplayStatus::Success) {
        // Warnings and failures are hard boundaries even for command aliases
        // that do not use the live PTY path (for example, `bash`).
        renderer.flush_compact_command_group();
    }
    if git_diff_payload || compact_command && compact_artifact {
        // Attention-worthy output is a hard boundary: do not let a command
        // with visible diagnostics or a diff merge into the preceding group.
        renderer.flush_compact_command_group();
    }
    if compact_file_diff {
        // File changes are glanceable activity, not command-group members.
        // Flush before rendering the file heading so a preceding command row
        // cannot absorb it and the following command starts a fresh group.
        renderer.flush_compact_command_group();
    }
    if compact_command {
        renderer.render_compact_command_activity(
            compact_command_text(name, args_val, workspace_root),
            compact_hidden_line_count(output, complete_capture.as_deref()),
            None,
            viewer_id,
        )?;
        if !compact_artifact {
            return Ok(());
        }
    }

    // Streamed PTY tools with a diff retain the existing live summary in
    // expanded mode. Compact mode suppresses that live row, so render an
    // anchored summary before the diff body instead.
    let skip_live_pty_summary = inline_run_tool && git_diff_payload && !compact_pty_without_live_preview;
    if !(compact_command || skip_live_pty_summary || compact_file_diff) {
        render_command_summary(
            renderer,
            name,
            args_val,
            output,
            command_success,
            workspace_root,
            viewer_id,
            !matches!(status, ToolDisplayStatus::Success) || git_diff_payload,
        )?;
    }

    if complete_capture_unavailable(output, complete_capture.as_deref()) {
        renderer.line(MessageStyle::Warning, "Complete command output capture unavailable.")?;
    }
    if let Some(message) = warning_message(output) {
        renderer.line(MessageStyle::Warning, &format!("⚠ {message}"))?;
    }

    let result = crate::agent::runloop::tool_output::render_tool_output(renderer, Some(name), output, vt_config).await;
    if result.is_ok() && compact_command && compact_artifact {
        render_structured_command_context(renderer, output)?;
    }
    if !matches!(status, ToolDisplayStatus::Success) {
        // The warning/failure row itself is visible, but it must not remain
        // the active tail that a later successful command could extend.
        renderer.flush_compact_command_group();
    }
    if compact_command && compact_artifact {
        // Some attention-worthy metadata (for example, a critical note) can
        // be rendered without emitting another line. End the active compact
        // tail explicitly so the next command cannot merge into this row.
        renderer.flush_compact_command_group();
    }
    result
}

fn render_error_common(renderer: &mut AnsiRenderer, name: &str, error: &str, error_type: &str) -> Result<()> {
    let err_msg = format!("Tool '{name}' {error_type}: {error}");
    renderer.line(MessageStyle::Error, &err_msg)?;
    Ok(())
}

#[derive(Default)]
struct OutcomeState {
    turn_modified_files: Vec<PathBuf>,
    turn_touched_files: Vec<PathBuf>,
    last_tool_stdout: Option<String>,
}

impl OutcomeState {
    fn into_full_tuple(self) -> (Vec<PathBuf>, Vec<PathBuf>, Option<String>) {
        (self.turn_modified_files, self.turn_touched_files, self.last_tool_stdout)
    }
}

struct OutcomeContext<'a> {
    session_stats: &'a mut SessionStats,
    renderer: &'a mut AnsiRenderer,
    handle: &'a InlineHandle,
    mcp_panel_state: &'a mut McpPanelState,
    vt_config: Option<&'a VTCodeConfig>,
    workspace_root: Option<&'a Path>,
}

struct SuccessPayload<'a> {
    output: &'a serde_json::Value,
    stdout: &'a Option<String>,
    modified_files: &'a [String],
    command_success: bool,
}

async fn handle_success_common(
    ctx: &mut OutcomeContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    payload: SuccessPayload<'_>,
    state: &mut OutcomeState,
) -> Result<()> {
    ctx.session_stats.record_tool(name);

    if let Some(tool_name) = name.strip_prefix("mcp_") {
        ctx.renderer.flush_compact_command_group();
        let tool_name = tool_name.trim_start_matches('_');
        let tool_name = tool_name.split("__").last().unwrap_or(tool_name);
        record_mcp_outcome_event(ctx.mcp_panel_state, tool_name, args_val, payload.command_success);
    } else if is_task_tracker_tool(name) && ctx.renderer.supports_inline_ui() {
        ctx.renderer.flush_compact_command_group();
        // Display-mode split: compact transcript shows header plus the current
        // task; expanded appends the truncated tree. The docked panel body
        // always keeps the full tree with per-row statuses for text-styling.
        let expanded = ctx.renderer.tool_display_mode() != ToolDisplayMode::Compact;
        let (panel_lines, panel_statuses, panel_current) =
            crate::agent::runloop::tool_output::tracker_panel_rows(payload.output);
        let progress_lines = task_tracker_block_lines(payload.output, expanded);
        if !panel_lines.is_empty() || !progress_lines.is_empty() {
            ctx.handle.update_task_panel_with_statuses(
                panel_lines,
                panel_statuses,
                panel_current,
                crate::agent::runloop::tool_output::tracker_panel_metadata(payload.output),
            );
            if !progress_lines.is_empty() {
                apply_task_tracker_block(ctx.handle, progress_lines);
            }
        }
    } else {
        render_tool_output_common(
            ctx.renderer,
            ctx.handle,
            name,
            args_val,
            payload.output,
            payload.command_success,
            ctx.vt_config,
            ctx.workspace_root,
        )
        .await?;
    }

    state.last_tool_stdout = if payload.command_success {
        payload.stdout.clone()
    } else {
        None
    };

    if !payload.modified_files.is_empty() {
        state.turn_modified_files.extend(collect_modified_files(payload.modified_files));
    }

    // Track read/touched paths for checkpoint replay even when nothing was
    // modified. Reuses the existing activity-path extractor so read_file,
    // grep, and search turns leave visible evidence without snapshotting
    // file contents.
    if let Some(workspace_root) = ctx.workspace_root {
        let touched =
            collect_instruction_activity_paths(workspace_root, args_val, payload.output, payload.modified_files);
        if !touched.is_empty() {
            ctx.session_stats
                .record_touched_files(touched.iter().map(|path| path.display().to_string()));
            state.turn_touched_files.extend(touched);
        }
    }

    Ok(())
}

fn handle_non_success_common(
    ctx: &mut OutcomeContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    status: &ToolExecutionStatus,
) -> Result<()> {
    ctx.renderer.flush_compact_command_group();

    // Expanded PTY tools already rendered "• Ran ..." in the pre-execution
    // inline block. Compact PTY tools suppress that block, so retain the
    // command summary for failures and cancellations instead of relying on a
    // header that was never emitted.
    let has_live_pty_preview = ctx.renderer.supports_inline_ui()
        && is_run_pty_tool(name, args_val)
        && ctx.renderer.tool_display_mode() != ToolDisplayMode::Compact;

    match status {
        ToolExecutionStatus::Failure { error } | ToolExecutionStatus::Timeout { error } => {
            let user_message = error.user_message();
            let viewer_id = if ctx.renderer.supports_inline_ui() && is_command_output_call(name, args_val) {
                Some(ctx.handle.record_tool_output(vec![
                    command_output_header(name, args_val, ctx.workspace_root),
                    format!(
                        "    {}: {}",
                        if matches!(status, ToolExecutionStatus::Timeout { .. }) {
                            "timed out"
                        } else {
                            "failed"
                        },
                        user_message
                    ),
                ]))
            } else {
                None
            };
            if !has_live_pty_preview {
                if let Some(viewer_id) = viewer_id {
                    ctx.renderer.set_next_tool_output_anchor(viewer_id);
                }
                render_non_success_summary(
                    ctx.renderer,
                    name,
                    args_val,
                    Some("error"),
                    ctx.workspace_root,
                    ToolDisplayStatus::Failure,
                )?;
            }
            render_error_common(
                ctx.renderer,
                name,
                &user_message,
                if matches!(status, ToolExecutionStatus::Timeout { .. }) {
                    "timed out"
                } else {
                    "failure"
                },
            )?;
        }
        ToolExecutionStatus::Cancelled => {
            let viewer_id = if ctx.renderer.supports_inline_ui() && is_command_output_call(name, args_val) {
                Some(ctx.handle.record_tool_output(vec![
                    command_output_header(name, args_val, ctx.workspace_root),
                    "    warning: tool execution cancelled".to_string(),
                ]))
            } else {
                None
            };
            if !has_live_pty_preview {
                if let Some(viewer_id) = viewer_id {
                    ctx.renderer.set_next_tool_output_anchor(viewer_id);
                }
                render_non_success_summary(
                    ctx.renderer,
                    name,
                    args_val,
                    Some("cancelled"),
                    ctx.workspace_root,
                    ToolDisplayStatus::Warning,
                )?;
            }
            ctx.renderer.line(MessageStyle::Info, "Tool execution cancelled")?;
        }
        ToolExecutionStatus::Success { .. } => {}
    };

    Ok(())
}

fn render_non_success_summary(
    renderer: &mut AnsiRenderer,
    name: &str,
    args_val: &serde_json::Value,
    stream_label: Option<&str>,
    workspace_root: Option<&Path>,
    status: ToolDisplayStatus,
) -> Result<()> {
    let summary_ctx = crate::agent::runloop::unified::tool_summary::ToolSummaryRenderContext { workspace_root };
    crate::agent::runloop::unified::tool_summary::render_expanded_tool_call_summary(
        renderer,
        name,
        args_val,
        stream_label,
        &summary_ctx,
        status.color(ColorPalette::default()),
    )
}

async fn process_outcome_common(
    ctx: &mut OutcomeContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    outcome: &ToolPipelineOutcome,
) -> Result<OutcomeState> {
    let mut state = OutcomeState::default();

    match &outcome.status {
        ToolExecutionStatus::Success {
            output, stdout, modified_files, command_success, ..
        } => {
            handle_success_common(
                ctx,
                name,
                args_val,
                SuccessPayload {
                    output,
                    stdout,
                    modified_files,
                    command_success: *command_success,
                },
                &mut state,
            )
            .await?;
        }
        _ => handle_non_success_common(ctx, name, args_val, &outcome.status)?,
    }

    Ok(state)
}

pub(crate) async fn handle_pipeline_output(
    ctx: &mut RunLoopContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    outcome: &ToolPipelineOutcome,
    vt_config: Option<&VTCodeConfig>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let (modified, _touched, stdout) = handle_pipeline_output_full(ctx, name, args_val, outcome, vt_config).await?;
    Ok((modified, stdout))
}

pub(crate) async fn handle_pipeline_output_full(
    ctx: &mut RunLoopContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    outcome: &ToolPipelineOutcome,
    vt_config: Option<&VTCodeConfig>,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>, Option<String>)> {
    // The registry owns the workspace used by the executor and the spooler.
    // Use it here even on the Copilot path, whose lightweight run-loop
    // context intentionally does not carry an auto-permission context.
    let workspace_root = Some(ctx.tool_registry.workspace_root().as_path());
    let mut output_ctx = OutcomeContext {
        session_stats: ctx.session_stats,
        renderer: ctx.renderer,
        handle: ctx.handle,
        mcp_panel_state: ctx.mcp_panel_state,
        vt_config,
        workspace_root,
    };
    let state = process_outcome_common(&mut output_ctx, name, args_val, outcome).await?;
    Ok(state.into_full_tuple())
}

// Adapter for TurnLoopContext (to avoid duplication when handling tool output in the turn loop)
pub(crate) async fn handle_pipeline_output_from_turn_ctx(
    ctx: &mut crate::agent::runloop::unified::turn::TurnLoopContext<'_>,
    name: &str,
    args_val: &serde_json::Value,
    outcome: &ToolPipelineOutcome,
    vt_config: Option<&VTCodeConfig>,
) -> Result<(Vec<PathBuf>, Option<String>)> {
    let mut run_ctx = ctx.as_run_loop_context();
    // `handle_pipeline_output_full` already records touched files into
    // `session_stats` via `handle_success_common` (same underlying stats
    // object); do not record a second time here. Modified files remain the
    // snapshot content source while touched files only feed checkpoint replay.
    let (modified_files, _touched_files, last_stdout) =
        handle_pipeline_output_full(&mut run_ctx, name, args_val, outcome, vt_config).await?;

    if let ToolExecutionStatus::Success { output, modified_files, command_success: true, .. } = &outcome.status {
        let activity_paths =
            collect_instruction_activity_paths(ctx.config.workspace.as_path(), args_val, output, modified_files);
        if !activity_paths.is_empty() {
            ctx.context_manager.record_instruction_activity_paths(activity_paths);
        }
    }

    Ok((modified_files, last_stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{IsTerminal, stdin};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::{RwLock, mpsc::unbounded_channel};
    use vtcode_core::acp::ToolPermissionCache;
    use vtcode_core::config::loader::VTCodeConfig;
    use vtcode_core::core::decision_tracker::DecisionTracker;
    use vtcode_core::core::trajectory::TrajectoryLogger;
    use vtcode_core::tools::ApprovalRecorder;
    use vtcode_core::tools::registry::{ToolExecutionError, ToolRegistry};
    use vtcode_core::tools::result_cache::{ToolCacheKey, ToolResultCache};
    use vtcode_core::ui::inline_theme_from_core_styles;
    use vtcode_core::ui::theme;
    use vtcode_ui::tui::app::{InlineCommand, InlineHandle, SessionOptions, spawn_session_with_options};

    fn build_harness_state() -> crate::agent::runloop::unified::run_loop_context::HarnessTurnState {
        crate::agent::runloop::unified::run_loop_context::HarnessTurnState::new(
            crate::agent::runloop::unified::run_loop_context::TurnRunId("test-run".to_string()),
            crate::agent::runloop::unified::run_loop_context::TurnId("test-turn".to_string()),
            4,
            60,
            0,
        )
    }

    fn dummy_handle() -> InlineHandle {
        InlineHandle::new_for_tests(unbounded_channel().0)
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn successful_task_tracker_replacement_contains_header_plus_current_task() {
        // Compact transcript contract: header plus the single current-task row
        // with the distinct `▶` visual. The full compact tree stays panel-only.
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let first = serde_json::json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "level": 0, "description": "Release", "status": "in_progress" },
                    { "index_path": "1.1", "level": 1, "description": "Update version", "status": "completed" },
                    { "index_path": "1.2", "level": 1, "description": "Run checks", "status": "in_progress" }
                ]
            }
        });
        let second = serde_json::json!({
            "status": "updated",
            "checklist": {
                "title": "Release",
                "items": [
                    { "index_path": "1", "level": 0, "description": "Release", "status": "completed" },
                    { "index_path": "1.1", "level": 1, "description": "Update version", "status": "completed" },
                    { "index_path": "1.2", "level": 1, "description": "Run checks", "status": "completed" }
                ]
            }
        });

        let first_progress = task_tracker_block_lines(&first, false);
        let second_progress = task_tracker_block_lines(&second, false);
        let first_panel = crate::agent::runloop::tool_output::tracker_tree_body_lines(&first);
        let second_panel = crate::agent::runloop::tool_output::tracker_tree_body_lines(&second);

        assert_eq!(tracker_texts(&first_progress), vec!["• Release 1/3", "  ▶ Run checks"]);
        assert_eq!(
            first_progress[1].status,
            Some(TaskItemStatus::InProgress),
            "current row carries its status for accent styling"
        );
        assert_eq!(tracker_texts(&second_progress), vec!["• Release 3/3"]);
        assert!(
            first_panel.iter().any(|line| line.contains("Update version")),
            "panel body keeps the compact tree: {first_panel:?}"
        );
        assert!(
            second_panel.iter().all(|line| !line.starts_with("• ")),
            "panel body must not re-include the transcript progress header"
        );

        apply_task_tracker_block(&handle, first_progress);
        apply_task_tracker_block(&handle, second_progress);

        let replacement = std::iter::from_fn(|| receiver.try_recv().ok()).find_map(|command| match command {
            InlineCommand::ReplaceLast { count, lines, .. } => Some((count, lines)),
            _ => None,
        });
        let (count, rows) = replacement.expect("second tracker update should replace the previous progress line");
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(|segment| segment.text).collect::<String>())
            .collect::<Vec<_>>();

        assert_eq!(count, 2);
        assert_eq!(rows, vec!["• Release 3/3"]);
        assert!(rows.iter().all(|row| !row.contains("next:") && !row.contains("├")));
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn identical_task_tracker_block_is_not_appended_twice() {
        // Approval handoff + pipeline replay (or repeated `list` calls) emit
        // the same payload. The transcript must keep one block, not two.
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let payload = serde_json::json!({
            "status": "updated",
            "checklist": {
                "completed": 1,
                "total": 3,
                "title": "Release",
                "items": [
                    { "index_path": "1", "description": "Release", "status": "in_progress" },
                    { "index_path": "1.1", "description": "Update version", "status": "completed" },
                    { "index_path": "1.2", "description": "Run checks", "status": "pending" }
                ]
            }
        });
        let lines = task_tracker_block_lines(&payload, false);

        apply_task_tracker_block(&handle, lines.clone());
        // Drain the initial append so only post-repeat commands remain.
        while receiver.try_recv().is_ok() {}
        apply_task_tracker_block(&handle, lines);

        assert!(receiver.try_recv().is_err(), "identical tracker repeat must not emit another transcript command");
        transcript::clear();
    }

    fn tracker_texts(lines: &[TrackerLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    fn plain_tracker_lines(texts: &[&str]) -> Vec<TrackerLine> {
        texts.iter().map(|text| TrackerLine::plain((*text).to_string())).collect()
    }

    #[test]
    fn looks_like_tracker_progress_line_rejects_questions_summaries() {
        assert!(looks_like_tracker_progress_line("• Tasks 0/2"));
        assert!(looks_like_tracker_progress_line("• Release 1/4"));
        assert!(!looks_like_tracker_progress_line("• Questions 2/5 answered"));
        assert!(!looks_like_tracker_progress_line("• Plan — research/synthesis"));
        assert!(!looks_like_tracker_progress_line("• Ran cargo check"));
    }

    #[test]
    fn looks_like_tracker_content_accepts_tree_rows_and_truncation() {
        assert!(looks_like_tracker_content("• Release 1/3"));
        assert!(looks_like_tracker_content("  ├ Investigate cache miss"));
        assert!(looks_like_tracker_content("  └ Verify with cargo check"));
        assert!(looks_like_tracker_content("  │ Defer eager setup"));
        assert!(looks_like_tracker_content("  ▶ Defer eager setup"));
        // Legacy glyph rows remain recognizable so in-flight blocks still
        // replace cleanly across the upgrade.
        assert!(looks_like_tracker_content("  ├ □ Investigate cache miss"));
        assert!(looks_like_tracker_content("  ▶ [-] Defer eager setup"));
        assert!(looks_like_tracker_content("  … 5 more"));
        assert!(!looks_like_tracker_content("• Questions 2/5 answered"));
        assert!(!looks_like_tracker_content("• Ran cargo check"));
        assert!(!looks_like_tracker_content("Now applying the edit"));
    }

    #[test]
    fn task_tracker_block_lines_expanded_shows_items_with_asymmetric_statuses() {
        let payload = serde_json::json!({
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

        let compact = task_tracker_block_lines(&payload, false);
        let expanded = task_tracker_block_lines(&payload, true);

        assert_eq!(tracker_texts(&compact), vec!["• Release 1/3", "  ▶ Defer eager setup"]);
        assert_eq!(compact[1].status, Some(TaskItemStatus::InProgress));
        assert_eq!(
            expanded,
            vec![
                TrackerLine::plain("• Release 1/3".to_string()),
                TrackerLine::row("  ├ Investigate cache miss".to_string(), TaskItemStatus::Completed),
                TrackerLine::row("  ├ Defer eager setup".to_string(), TaskItemStatus::InProgress),
                TrackerLine::row("  └ Verify with cargo check".to_string(), TaskItemStatus::Pending),
            ]
        );
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn write_tracker_progress_transcript_replaces_compact_with_expanded_block() {
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 0/2"]));
        while receiver.try_recv().is_ok() {}
        let expanded = vec![
            TrackerLine::plain("• Release 1/2".to_string()),
            TrackerLine::row("  ├ Investigate cache miss".to_string(), TaskItemStatus::Completed),
            TrackerLine::row("  └ Defer eager setup".to_string(), TaskItemStatus::Pending),
        ];

        write_tracker_progress_transcript(&handle, expanded.clone());

        let replacement = std::iter::from_fn(|| receiver.try_recv().ok()).find_map(|command| match command {
            InlineCommand::ReplaceLast { count, lines, .. } => Some((count, lines)),
            _ => None,
        });
        let (count, rows) = replacement.expect("compact block should be replaced by expanded block");
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(|segment| segment.text).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(count, 1);
        assert_eq!(rows, tracker_texts(&expanded));
        assert_eq!(
            transcript::snapshot(),
            tracker_texts(&expanded).into_iter().map(str::to_string).collect::<Vec<_>>()
        );
        transcript::clear();
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn write_tracker_progress_transcript_uses_ui_write_len_not_transcript_only_tail() {
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 0/2"]));
        while receiver.try_recv().is_ok() {}
        // UI-only append after tracker (Questions-style) must not be clobbered
        // via TRANSCRIPT-derived replace count once Questions dual-writes; when
        // TRANSCRIPT still has tracker as tail but UI has an extra line, the
        // helper uses recorded UI write length.
        handle.append_line(
            InlineMessageKind::Info,
            vec![InlineSegment {
                text: "• Questions 1/2 answered".to_string(),
                style: Arc::new(InlineTextStyle::default()),
            }],
        );

        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 1/2"]));

        let commands: Vec<InlineCommand> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        let replaced_ui_len = commands.iter().find_map(|command| match command {
            InlineCommand::ReplaceLast { count, lines, .. } => {
                let rows: Vec<String> = lines
                    .iter()
                    .map(|row| row.iter().map(|segment| segment.text.clone()).collect::<String>())
                    .collect();
                Some((*count, rows))
            }
            _ => None,
        });
        // TRANSCRIPT still sees tracker at tail (Questions not mirrored here),
        // so replace happens — but only the helper's UI write length (1), and
        // the replacement content is the new progress line.
        let (count, rows) = replaced_ui_len.expect("tracker update should replace remembered UI write");
        assert_eq!(count, 1);
        assert_eq!(rows, vec!["• Release 1/2".to_string()]);
        transcript::clear();
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn write_tracker_progress_transcript_replaces_previous_block_on_change() {
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let first = plain_tracker_lines(&["• Release 0/2"]);
        let second = plain_tracker_lines(&["• Release 1/2"]);

        write_tracker_progress_transcript(&handle, first);
        while receiver.try_recv().is_ok() {}
        write_tracker_progress_transcript(&handle, second.clone());

        let replacement = std::iter::from_fn(|| receiver.try_recv().ok()).find_map(|command| match command {
            InlineCommand::ReplaceLast { count, lines, .. } => Some((count, lines)),
            _ => None,
        });
        let (count, rows) = replacement.expect("changed progress must replace the previous tracker block");
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(|segment| segment.text).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(count, 1);
        assert_eq!(rows, tracker_texts(&second));
        transcript::clear();
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn write_tracker_progress_transcript_does_not_clobber_intervening_lines() {
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 0/2"]));
        while receiver.try_recv().is_ok() {}
        // Intervening transcript content after the tracker block.
        transcript::append("Now applying the edit");

        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 1/2"]));

        let commands: Vec<InlineCommand> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        let appended = commands
            .iter()
            .any(|command| matches!(command, InlineCommand::AppendLine { .. }));
        let replaced = commands
            .iter()
            .any(|command| matches!(command, InlineCommand::ReplaceLast { .. }));
        assert!(appended && !replaced, "must append after intervening lines instead of replace_last clobber");
        let snap = transcript::snapshot();
        assert!(snap.contains(&"Now applying the edit".to_string()));
        assert_eq!(
            snap.iter().filter(|line| line.starts_with("• Release")).count(),
            2,
            "old block remains under intervening line; new block appended (no silent clobber)"
        );
        transcript::clear();
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn write_tracker_progress_transcript_replaces_tail_tracker_progress_line() {
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        transcript::append("• Tasks 0/1");
        while receiver.try_recv().is_ok() {}

        write_tracker_progress_transcript(&handle, plain_tracker_lines(&["• Release 1/1"]));

        let replacement = std::iter::from_fn(|| receiver.try_recv().ok()).find_map(|command| match command {
            InlineCommand::ReplaceLast { count, lines, .. } => Some((count, lines)),
            _ => None,
        });
        let (count, rows) = replacement.expect("tail tracker progress line should be replaced");
        let rows = rows
            .into_iter()
            .map(|row| row.into_iter().map(|segment| segment.text).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(count, 1);
        assert_eq!(rows, vec!["• Release 1/1".to_string()]);
        assert_eq!(transcript::snapshot(), vec!["• Release 1/1".to_string()]);
        transcript::clear();
    }

    #[test]
    #[serial_test::serial(transcript_state)]
    fn pipeline_replay_after_approval_handoff_is_not_appended_twice() {
        // `render_created_task_tracker` appends the approval block without
        // updating `HarnessTurnState`. The pipeline replay of the identical
        // payload must still collapse to one block, and the replayed block
        // must be remembered so the next update replaces it.
        transcript::clear();
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let payload = serde_json::json!({
            "status": "updated",
            "checklist": {
                "completed": 1,
                "total": 3,
                "items": [
                    { "index_path": "1", "description": "Release", "status": "in_progress" },
                    { "index_path": "1.1", "description": "Update version", "status": "completed" },
                    { "index_path": "1.2", "description": "Run checks", "status": "pending" }
                ]
            }
        });
        let lines = task_tracker_block_lines(&payload, false);
        // Simulate the approval handoff's transcript write.
        for line in &lines {
            transcript::append(&line.text);
        }

        apply_task_tracker_block(&handle, lines.clone());

        assert!(
            receiver.try_recv().is_err(),
            "pipeline replay after approval handoff must not emit another transcript command"
        );
        assert!(transcript::tracker_block_matches(&lines.iter().map(|line| line.text.clone()).collect::<Vec<_>>()));
        transcript::clear();
    }

    #[test]
    fn task_tracker_row_segments_render_inline_code_without_backticks() {
        let rows = task_tracker_block_segments(&[
            TrackerLine::plain("• Task tracker".to_string()),
            TrackerLine::row("  └ Use `src/main.rs` parser".to_string(), TaskItemStatus::Pending),
        ]);

        assert_eq!(rows.len(), 2);
        // Title keeps its plain single segment.
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[0][0].text, "• Task tracker");
        // Tree prefix stays intact while the code span renders styled without
        // literal backticks.
        let text = rows[1].iter().map(|segment| segment.text.as_str()).collect::<String>();
        assert_eq!(text, "  └ Use src/main.rs parser");
        assert!(rows[1].len() > 1, "code span should produce distinct styled segments");
        assert!(!text.contains('`'));
    }

    #[test]
    fn task_tracker_row_segments_keep_diagnostics_plain() {
        let rows = task_tracker_block_segments(&[
            TrackerLine::plain("• Task tracker".to_string()),
            TrackerLine::plain("  Tracker status: error".to_string()),
            TrackerLine::row("  └ Plain action".to_string(), TaskItemStatus::Pending),
        ]);

        assert_eq!(rows[1].len(), 1);
        assert_eq!(rows[1][0].text, "  Tracker status: error");
        let text = rows[2].iter().map(|segment| segment.text.as_str()).collect::<String>();
        assert_eq!(text, "  └ Plain action");
    }

    #[test]
    fn task_tracker_done_rows_render_struck_through_dimmed_italic() {
        use vtcode_core::ui::tui::convert_style;

        let rows = task_tracker_block_segments(&[
            TrackerLine::plain("• Release 1/3".to_string()),
            TrackerLine::row("  ├ Investigate cache miss".to_string(), TaskItemStatus::Completed),
            TrackerLine::row("  ├ Defer eager setup".to_string(), TaskItemStatus::InProgress),
            TrackerLine::row("  └ Verify with cargo check".to_string(), TaskItemStatus::Pending),
        ]);

        let done_style = &rows[1][1].style;
        assert!(done_style.effects.contains(Effects::STRIKETHROUGH), "done rows must strike through: {rows:?}");
        assert!(done_style.effects.contains(Effects::ITALIC), "done rows must italicize: {rows:?}");
        assert!(done_style.effects.contains(Effects::DIMMED), "done rows must dim: {rows:?}");
        // No glyphs leak into any row.
        for row in &rows {
            let text = row.iter().map(|segment| segment.text.as_str()).collect::<String>();
            assert!(!text.contains("□") && !text.contains("[x]") && !text.contains("[-]"), "{text:?}");
        }
        // Pending keeps the default style (no accent, no strike).
        assert_eq!(rows[3][1].style.color, None);
        assert!(!rows[3][1].style.effects.contains(Effects::STRIKETHROUGH));
        // Non-current in-progress rows use the primary accent (the markdown
        // base style contributes weight pipeline-wide; the focused current row
        // adds the `▶` marker on top).
        let expected = convert_style(theme::active_styles().primary);
        assert_eq!(rows[2][1].style.color, expected.color);
    }

    #[test]
    fn task_tracker_current_row_segments_carry_primary_accent() {
        use vtcode_core::ui::tui::convert_style;

        let rows = task_tracker_block_segments(&[
            TrackerLine::plain("• Release 1/3".to_string()),
            TrackerLine::row("  ▶ Defer eager setup".to_string(), TaskItemStatus::InProgress),
        ]);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text, "• Release 1/3");
        let text = rows[1].iter().map(|segment| segment.text.as_str()).collect::<String>();
        assert_eq!(text, "  ▶ Defer eager setup");
        let expected = convert_style(theme::active_styles().primary);
        assert!(expected.color.is_some(), "primary accent must resolve to a concrete color for the TODO visual");
        assert_eq!(
            rows[1][0].style.color, expected.color,
            "current-task marker must use the primary accent, not the default style"
        );
        assert!(rows[1][0].style.effects.contains(Effects::BOLD), "current-task marker must be bold: {rows:?}");
        assert!(
            rows[1].iter().skip(1).any(|segment| segment.style.color == expected.color),
            "current-task body must keep the accent tint: {rows:?}"
        );
    }

    #[test]
    fn split_tracker_row_prefix_handles_nested_and_parent_rows() {
        assert_eq!(split_tracker_row_prefix("  ├ Investigate"), Some(("  ├ ", "Investigate")));
        assert_eq!(split_tracker_row_prefix("  │ Update version"), Some(("  │ ", "Update version")));
        assert_eq!(split_tracker_row_prefix("  ▶ Defer eager setup"), Some(("  ▶ ", "Defer eager setup")));
        // Legacy glyph rows still split so in-flight blocks degrade gracefully.
        assert_eq!(split_tracker_row_prefix("  ├ □ Investigate"), Some(("  ├ □ ", "Investigate")));
        assert_eq!(split_tracker_row_prefix("  ├ Prepare release"), Some(("  ├ ", "Prepare release")));
        assert_eq!(split_tracker_row_prefix("• Task tracker"), None);
        assert_eq!(split_tracker_row_prefix("  Tracker status: error"), None);
    }

    // Use Tokio runtime for async test blocks
    #[tokio::test]
    async fn test_renderer_records_tool_and_collects_modified_files() {
        // Setup a stdout renderer
        let mut renderer = AnsiRenderer::stdout();

        // Prepare session stats and mcp state
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::default();

        // Create an outcome that indicates write to /tmp/foo.txt
        let output_json = serde_json::json!({"result":"ok"});
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: output_json.clone(),
            stdout: None,
            modified_files: vec!["/tmp/foo.txt".to_string()],
            command_success: true,
        });

        // Invoke the shared outcome processor via a minimal output context.
        let handle = dummy_handle();
        let mut output_ctx = OutcomeContext {
            workspace_root: None,
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };
        let (mod_files, _touched, _last_stdout) =
            process_outcome_common(&mut output_ctx, "write_file", &serde_json::json!({}), &outcome)
                .await
                .expect("render should succeed")
                .into_full_tuple();

        // Confirm the function recorded the tool call
        let recorded = stats.sorted_tools();
        assert!(recorded.contains(&"write_file".to_string()));

        // Confirm the modified files list contains our path
        assert_eq!(mod_files, vec![PathBuf::from("/tmp/foo.txt")]);
    }

    #[test]
    fn tool_call_visual_status_colors_success_failure_and_warning() {
        let palette = ColorPalette::default();
        assert_eq!(ToolDisplayStatus::Success.color(palette), palette.success);
        assert_eq!(ToolDisplayStatus::Failure.color(palette), palette.error);
        assert_eq!(ToolDisplayStatus::Warning.color(palette), palette.warning);

        assert!(matches!(
            ToolDisplayStatus::from_command_output(&serde_json::json!({}), true),
            ToolDisplayStatus::Success
        ));
        assert!(matches!(
            ToolDisplayStatus::from_command_output(&serde_json::json!({}), false),
            ToolDisplayStatus::Failure
        ));
        assert!(matches!(
            ToolDisplayStatus::from_command_output(&serde_json::json!({"warning": "no results"}), true),
            ToolDisplayStatus::Warning
        ));
        assert!(matches!(
            ToolDisplayStatus::from_command_output(&serde_json::json!({"warning": null}), true),
            ToolDisplayStatus::Success
        ));

        assert!(
            compact_run_completion_line(&serde_json::json!({"exit_code": 0}), ToolDisplayStatus::Success).is_some()
        );
        assert!(
            compact_run_completion_line(&serde_json::json!({"warning": "no results"}), ToolDisplayStatus::Warning)
                .is_some()
        );
        assert!(compact_run_completion_line(&serde_json::json!({}), ToolDisplayStatus::Success).is_none());
    }

    #[test]
    fn compact_hidden_line_count_excludes_distinct_stderr() {
        let output = serde_json::json!({
            "output": "stdout line\nstderr line",
            "stdout": "stdout line",
            "stderr": "stderr line"
        });

        assert_eq!(compact_hidden_line_count(&output, None), 1);
    }

    #[test]
    fn command_extraction_uses_canonical_command_text_shapes() {
        assert_eq!(
            extract_command_line(&serde_json::json!({"command": ["git", "status", "--short"]})),
            Some("git status --short".to_string())
        );
        assert_eq!(
            extract_command_line(&serde_json::json!({"command.0": "git", "command.1": "status"})),
            Some("git status".to_string())
        );
        assert_eq!(
            command_output_header(
                tools::EXECUTE_CODE,
                &serde_json::json!({"command": ["git", "status", "--short"]}),
                None
            ),
            "• Ran git status --short"
        );
    }

    #[test]
    fn command_extraction_leaves_shell_operators_bare() {
        // Screenshot 2026-09-02: `• Ran cat … '2>' '/dev/null' '|' …` quoted
        // every operator. Display joining must keep `|`, `>`, `;` bare and
        // quote only words containing whitespace.
        let args = serde_json::json!({
            "command": [
                "cat", "docs/guides/agent-loop-contract.md",
                "2>/dev/null", "|", "head", "-120",
                ";", "echo", "---"
            ]
        });
        assert_eq!(
            extract_command_line(&args),
            Some("cat docs/guides/agent-loop-contract.md 2>/dev/null | head -120 ; echo ---".to_string())
        );
        assert_eq!(
            command_output_header(tools::EXEC_COMMAND, &args, None),
            "• Ran cat docs/guides/agent-loop-contract.md 2>/dev/null | head -120 ; echo ---"
        );
        // String commands preserve raw shell text (no re-quoting).
        let string_args = serde_json::json!({
            "command": "cat docs/guides/agent-loop-contract.md 2>/dev/null | head -120; echo ---"
        });
        let header = command_output_header(tools::EXEC_COMMAND, &string_args, None);
        assert_eq!(header, "• Ran cat docs/guides/agent-loop-contract.md 2>/dev/null | head -120; echo ---");
        assert!(!header.contains("'|'"), "pipe must not be quoted: {header}");
        assert!(!header.contains("'2>'"), "redirection must not be quoted: {header}");
    }

    #[test]
    fn command_header_for_multiline_python_stays_readable() {
        // Screenshot 2026-09-02: `• Ran python3 -c "` with `tur…ool_calls`
        // continuations and `\'\'` quoting noise. The viewer header must stay
        // single-line with real script content and no nested-quote artifacts.
        let args = serde_json::json!({
            "command": "python3 -c \"\nimport json\nwith open('.vtcode/checkpoints/turn_1032.json') as f: d = json.load(f)\""
        });
        let header = command_output_header(tools::EXEC_COMMAND, &args, None);
        assert!(header.starts_with("• Ran python3 -c "), "got: {header}");
        assert!(header.contains("import json"), "got: {header}");
        assert!(!header.contains('\n'), "newlines leaked: {header:?}");
        assert!(!header.contains("tur…ool"), "mid-string ellipsis leaked: {header}");
        assert!(!header.contains("\\'"), "escaped quotes leaked: {header}");
        assert_ne!(header, "• Ran python3 -c \"");
    }

    #[test]
    fn command_header_shows_full_command_without_truncation() {
        // Transcript Review must show the complete command: a long chained
        // `git add && git commit && git log` must survive in full instead of
        // head-truncating at the 120-char compact preview budget.
        let command = "git add README.md && git commit -m 'docs(readme): expand contributors and sponsors by default' && git log --oneline -1 && git status --short && git diff --stat";
        assert!(command.chars().count() > COMPACT_PREVIEW_LEN, "fixture must overflow compact cap");
        let args = serde_json::json!({ "command": command });
        let header = command_output_header(tools::EXEC_COMMAND, &args, None);
        assert_eq!(header, format!("• Ran {command}"), "got: {header:?}");
        assert!(!header.contains('…'), "truncation ellipsis leaked: {header:?}");
    }

    #[test]
    fn ordered_stream_texts_deduplicates_merged_output_aliases() {
        let output = serde_json::json!({
            "output": "stdout line\nstderr line",
            "stdout": "stdout line",
            "stderr": "stderr line"
        });

        assert_eq!(ordered_stream_texts(&output), vec!["stdout line\nstderr line"]);
    }

    #[test]
    fn ordered_stream_texts_preserves_distinct_pipe_streams() {
        let output = serde_json::json!({
            "output": "merged line",
            "stdout": "stdout line",
            "stderr": "stderr line"
        });

        assert_eq!(ordered_stream_texts(&output), vec!["merged line", "stdout line", "stderr line"]);
    }

    #[test]
    fn canonical_pipe_streams_preserve_unrepresented_content() {
        let output = serde_json::json!({
            "stdout": "command output",
            "content": "additional structured content"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![
                (Some("stdout"), "command output"),
                (None, "additional structured content")
            ]
        );
    }

    #[test]
    fn canonical_pipe_streams_keep_merged_output_once() {
        let output = serde_json::json!({
            "output": "stdout line\nstderr line",
            "stdout": "stdout line",
            "stderr": "stderr line"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label, None);
        assert_eq!(streams[0].text, "stdout line\nstderr line");
    }

    #[test]
    fn canonical_pipe_streams_label_separate_streams() {
        let output = serde_json::json!({
            "stdout": "stdout line",
            "stderr": "stderr line"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![(Some("stdout"), "stdout line"), (Some("stderr"), "stderr line")]
        );
    }

    #[test]
    fn canonical_pipe_streams_preserve_identical_named_streams() {
        let output = serde_json::json!({
            "stdout": "same output",
            "stderr": "same output"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![(Some("stdout"), "same output"), (Some("stderr"), "same output")]
        );
    }

    #[test]
    fn canonical_pipe_streams_require_distinct_merged_occurrences() {
        let single_occurrence = serde_json::json!({
            "output": "same output",
            "stdout": "same output",
            "stderr": "same output"
        });
        let streams = canonical_pipe_streams(&single_occurrence);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![(Some("stdout"), "same output"), (Some("stderr"), "same output")]
        );
        assert_eq!(stderr_for_inline_display(&single_occurrence), Some("same output"));

        let distinct_occurrences = serde_json::json!({
            "output": "same output\nsame output",
            "stdout": "same output",
            "stderr": "same output"
        });
        let streams = canonical_pipe_streams(&distinct_occurrences);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].label, None);
        assert_eq!(streams[0].text, "same output\nsame output");
        assert_eq!(stderr_for_inline_display(&distinct_occurrences), None);
    }

    #[test]
    fn canonical_pipe_streams_preserve_merged_lines_when_named_streams_overlap() {
        let output = serde_json::json!({
            "output": "same output\nmerged-only output",
            "stdout": "same output",
            "stderr": "same output"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![
                (Some("stdout"), "same output"),
                (Some("stderr"), "same output"),
                (None, "same output\nmerged-only output")
            ]
        );
    }

    #[test]
    fn canonical_pipe_streams_preserve_full_named_alias() {
        let output = serde_json::json!({
            "output": "stdout line",
            "stdout": "stdout line\nsecond stdout line",
            "stderr": "stderr line"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![
                (Some("stdout"), "stdout line\nsecond stdout line"),
                (Some("stderr"), "stderr line")
            ]
        );
    }

    #[test]
    fn canonical_pipe_streams_preserve_distinct_streams_when_output_is_preview() {
        let output = serde_json::json!({
            "output": "preview",
            "stdout": "preview\nstdout line",
            "stderr": "preview\nstderr line"
        });

        let streams = canonical_pipe_streams(&output);
        assert_eq!(
            streams.iter().map(|stream| (stream.label, stream.text)).collect::<Vec<_>>(),
            vec![
                (Some("stdout"), "preview\nstdout line"),
                (Some("stderr"), "preview\nstderr line")
            ]
        );
    }

    #[test]
    fn normalize_terminal_output_lines_handles_ansi_rewrites_and_blanks() {
        let capture = "stale\n\x1b[2J\x1b[H\x1b[31mred\x1b[0m\rfinal\n\nlast\n";

        assert_eq!(normalize_terminal_output_lines(capture), vec!["final", "", "last"]);
        assert_eq!(normalize_terminal_output_lines("abc\x08d\n"), vec!["abd"]);
    }

    #[test]
    fn build_pipe_command_output_lines_labels_stderr_once() {
        let output = serde_json::json!({
            "stdout": "normal output",
            "stderr": "diagnostic output",
            "exit_code": 1
        });

        assert_eq!(
            build_pipe_command_output_lines(
                tools::EXECUTE_CODE,
                &serde_json::json!({"command": "printf test"}),
                &output,
                None,
                ToolDisplayStatus::Failure,
            ),
            vec![
                "• Ran printf test",
                "  stdout:",
                "    normal output",
                "  stderr:",
                "    diagnostic output",
                "    ✗ run error, exit code: 1",
            ]
        );
    }

    #[test]
    fn build_merged_command_output_lines_keeps_complete_capture_and_status_once() {
        let output = serde_json::json!({
            "exit_code": 2,
            "critical_note": "output was retained in the current session"
        });
        let capture = "stdout line\nstderr line\n";

        let lines = build_merged_command_output_lines(
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "long command"}),
            capture,
            None,
            &output,
            ToolDisplayStatus::Failure,
        );

        assert_eq!(lines[0], "• Ran long command");
        assert!(lines.contains(&"  └ stdout line".to_string()));
        assert!(lines.contains(&"    stderr line".to_string()));
        assert!(lines.contains(&"    output was retained in the current session".to_string()));
        assert_eq!(lines.iter().filter(|line| line.contains("stderr line")).count(), 1);
        assert_eq!(lines.iter().filter(|line| line.contains("exit code: 2")).count(), 1);
    }

    #[test]
    fn build_merged_command_output_lines_keeps_distinct_stderr_without_capture() {
        let output = serde_json::json!({"stderr": "diagnostic output"});

        let lines = build_merged_command_output_lines(
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "long command"}),
            "",
            None,
            &output,
            ToolDisplayStatus::Success,
        );

        assert_eq!(lines, vec!["• Ran long command", "  stderr:", "    diagnostic output",]);
    }

    #[test]
    fn build_merged_command_output_lines_labels_distinct_stderr_with_merged_output() {
        let output = serde_json::json!({
            "output": "normal output",
            "stderr": "diagnostic output"
        });

        let lines = build_merged_command_output_lines(
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "long command"}),
            "normal output\ndiagnostic output\n",
            None,
            &output,
            ToolDisplayStatus::Success,
        );

        assert_eq!(
            lines,
            vec![
                "• Ran long command",
                "  └ normal output",
                "  stderr:",
                "    diagnostic output",
            ]
        );
    }

    #[test]
    fn build_merged_command_output_lines_labels_identical_named_streams_without_merged_output() {
        let output = serde_json::json!({
            "stdout": "same output",
            "stderr": "same output"
        });

        let lines = build_merged_command_output_lines(
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "long command"}),
            "same output\nsame output",
            None,
            &output,
            ToolDisplayStatus::Success,
        );

        assert_eq!(
            lines,
            vec![
                "• Ran long command",
                "  stdout:",
                "    same output",
                "  stderr:",
                "    same output",
            ]
        );
    }

    #[tokio::test]
    async fn pty_capture_reads_complete_workspace_spool() {
        let workspace = TempDir::new().expect("workspace temp dir");
        let spool_path = workspace.path().join(".vtcode/context/tool_outputs/pty.txt");
        tokio::fs::create_dir_all(spool_path.parent().expect("spool parent"))
            .await
            .expect("create spool parent");
        let complete_output = "first complete line\nsecond complete line\n";
        tokio::fs::write(&spool_path, complete_output).await.expect("write spool");

        let output = serde_json::json!({
            "spool_path": ".vtcode/context/tool_outputs/pty.txt",
            "spooled_bytes": complete_output.len(),
            "spool_sha256": vtcode_commons::utils::calculate_sha256(complete_output.as_bytes()),
            "spool_state": "completed",
            "output": "first preview line"
        });

        assert_eq!(
            load_complete_output(&output, Some(workspace.path())).await.as_deref(),
            Some("first complete line\nsecond complete line\n")
        );
    }

    #[tokio::test]
    async fn pty_capture_rejects_spool_outside_workspace() {
        let workspace = TempDir::new().expect("workspace temp dir");
        let outside = TempDir::new().expect("outside temp dir");
        let spool_path = outside.path().join("pty.txt");
        tokio::fs::write(&spool_path, "secret outside workspace")
            .await
            .expect("write outside spool");

        let output = serde_json::json!({ "spool_path": spool_path });

        assert!(load_complete_output(&output, Some(workspace.path())).await.is_none());
    }

    #[tokio::test]
    async fn pty_capture_rejects_malformed_spool_metadata_without_inline_fallback() {
        let workspace = TempDir::new().expect("workspace temp dir");
        let output = serde_json::json!({
            "spool_path": null,
            "output": "untrusted inline fallback"
        });

        assert!(load_complete_output(&output, Some(workspace.path())).await.is_none());
    }

    #[tokio::test]
    async fn test_renderer_records_mcp_event_for_mcp_tool() {
        let mut renderer = AnsiRenderer::stdout();

        // Note: tests involving `apply_turn_outcome` live in `turn/turn_loop.rs` and can be added there
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::new(32, true); // enabled

        let output_json = serde_json::json!({"exit_code":0});
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: output_json.clone(),
            stdout: Some("ok".to_string()),
            modified_files: vec![],
            command_success: true,
        });

        let handle = dummy_handle();
        let mut output_ctx = OutcomeContext {
            workspace_root: None,
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };
        let (_mod_files, _touched, _last_stdout) =
            process_outcome_common(&mut output_ctx, "mcp_example", &serde_json::json!({}), &outcome)
                .await
                .expect("render should succeed")
                .into_full_tuple();

        // Ensure mcp panel recorded an event
        assert!(mcp.event_count() > 0);
    }

    #[tokio::test]
    async fn spooled_exec_output_keeps_transcript_at_reference_only() {
        let mut renderer = AnsiRenderer::stdout();
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::default();
        let handle = dummy_handle();

        transcript::clear();

        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "output": "preview text that should stay out of transcript persistence",
                "spool_path": ".vtcode/context/tool_outputs/exec_command_1.txt",
                "exit_code": 0,
                "is_exited": true
            }),
            stdout: Some("preview text that should stay out of transcript persistence".to_string()),
            modified_files: vec![],
            command_success: true,
        });

        let mut output_ctx = OutcomeContext {
            workspace_root: None,
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };

        process_outcome_common(
            &mut output_ctx,
            tools::UNIFIED_EXEC,
            &serde_json::json!({
                "action": "run",
                "command": "cargo check -p vtcode-core"
            }),
            &outcome,
        )
        .await
        .expect("render should succeed");

        let transcript_lines = transcript::snapshot();
        let transcript_text = transcript_lines.join("\n");
        let stripped_text = vtcode_core::utils::ansi_parser::strip_ansi(&transcript_text);
        assert!(stripped_text.contains("Large output was spooled to"), "Transcript: {stripped_text:?}");
        assert!(!stripped_text.contains("preview text that should stay out of transcript persistence"));

        transcript::clear();
    }

    #[tokio::test]
    async fn inline_tool_output_viewer_retains_complete_spooled_capture() {
        let workspace = TempDir::new().expect("workspace temp dir");
        let spool_path = workspace.path().join(".vtcode/context/tool_outputs/exec_command_1.txt");
        tokio::fs::create_dir_all(spool_path.parent().expect("spool parent"))
            .await
            .expect("create spool parent");
        let complete_output = "first complete line\nsecond complete line\n";
        tokio::fs::write(&spool_path, complete_output).await.expect("write spool");

        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::default();
        transcript::clear();
        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "output": "preview line",
                "spool_path": ".vtcode/context/tool_outputs/exec_command_1.txt",
                "spooled_bytes": complete_output.len(),
                "spool_sha256": vtcode_commons::utils::calculate_sha256(complete_output.as_bytes()),
                "spool_state": "completed",
                "exit_code": 0,
                "is_exited": true
            }),
            stdout: Some("preview line".to_string()),
            modified_files: vec![],
            command_success: true,
        });
        let mut output_ctx = OutcomeContext {
            workspace_root: Some(workspace.path()),
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };

        process_outcome_common(
            &mut output_ctx,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "cargo check"}),
            &outcome,
        )
        .await
        .expect("render should succeed");

        let mut recorded = None;
        let mut commands = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            if let InlineCommand::RecordToolOutput { lines, .. } = &command {
                recorded = Some(lines.clone());
            }
            commands.push(command);
        }
        let lines = recorded.expect("the complete output should be recorded for the viewer");
        assert_eq!(lines[0], "• Ran cargo check");
        assert!(lines.iter().any(|line| line == "  └ first complete line"));
        assert!(lines.iter().any(|line| line == "    second complete line"));
        assert!(!lines.iter().any(|line| line.contains("preview line")));
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, InlineCommand::CollapsePtyBlock(_))),
            "received {} inline commands",
            commands.len()
        );

        let transcript_text = transcript::snapshot().join("\n");
        assert!(!transcript_text.contains("first complete line"));
        assert!(!transcript_text.contains("second complete line"));
        transcript::clear();
    }

    #[tokio::test]
    async fn unavailable_spool_capture_remains_visible_and_does_not_collapse_pty() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "printf preview"}),
            &serde_json::json!({"spool_path": null, "output": "preview output"}),
            true,
            None,
            None,
        )
        .await
        .expect("unavailable spool result should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendLine { segments, .. }
                if segments.iter().any(|segment| segment.text.contains("capture unavailable")))
        }));
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, InlineCommand::CollapsePtyBlock(_)))
        );
    }

    #[tokio::test]
    async fn compact_pty_completion_emits_grouped_activity_without_live_preview() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "printf first"}),
            &serde_json::json!({"stdout": "first\nsecond"}),
            true,
            None,
            None,
        )
        .await
        .expect("compact PTY output should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, InlineCommand::CollapsePtyBlock(_)))
        );
        assert!(
            !commands.iter().any(|command| {
                matches!(
                    command,
                    InlineCommand::AppendLine { .. } | InlineCommand::Inline { .. } | InlineCommand::ReplaceLast { .. }
                )
            }),
            "compact PTY completion must not flash a live output block"
        );
    }

    #[tokio::test]
    async fn compact_pty_attention_keeps_command_summary_and_stderr_visible() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "printf diagnostic"}),
            &serde_json::json!({"stdout": "normal output", "stderr": "diagnostic output"}),
            true,
            None,
            None,
        )
        .await
        .expect("compact PTY diagnostics should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendToolOutputLine { segments, .. }
                if segments.iter().map(|segment| segment.text.as_str()).collect::<String>().contains("• Ran printf diagnostic"))
        }));
        assert!(commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendLine { segments, .. }
                if segments.iter().map(|segment| segment.text.as_str()).collect::<String>().contains("stderr: diagnostic output"))
        }));
    }

    #[test]
    fn compact_pty_failure_keeps_command_summary_without_live_preview() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::default();
        let mut output_ctx = OutcomeContext {
            workspace_root: None,
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };
        let status = ToolExecutionStatus::Failure {
            error: ToolExecutionError::from_anyhow(
                tools::RUN_PTY_CMD,
                &anyhow::anyhow!("command failed"),
                0,
                false,
                false,
                Some("test"),
            ),
        };

        handle_non_success_common(
            &mut output_ctx,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "false"}),
            &status,
        )
        .expect("compact PTY failure should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendToolOutputLine { segments, .. }
                if segments.iter().map(|segment| segment.text.as_str()).collect::<String>().contains("• Ran false"))
        }));
    }

    #[tokio::test]
    async fn compact_command_output_emits_group_metadata_and_complete_capture() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({"stdout": "first\nsecond"});

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf first"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("compact command output should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let capture_id = commands.iter().find_map(|command| match command {
            InlineCommand::RecordToolOutput { id, .. } => Some(*id),
            _ => None,
        });
        let activity = commands.iter().find_map(|command| match command {
            InlineCommand::AppendCompactActivity(activity) => Some(activity),
            _ => None,
        });

        let capture_id = capture_id.expect("complete command capture should be retained");
        let activity = activity.expect("compact command activity should be emitted");
        assert_eq!(activity.review_anchor, Some(capture_id));
        assert_eq!(activity.hidden_line_count, 2);
        assert_eq!(activity.display_text(), "• Ran printf first · … +2 lines");
        assert!(
            commands
                .iter()
                .all(|command| { !matches!(command, InlineCommand::AppendLine { .. } | InlineCommand::Inline { .. }) })
        );
    }

    #[tokio::test]
    async fn compact_command_capture_keeps_follow_up_guidance() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({
            "stdout": "command output",
            "next_action": "Review the result before continuing."
        });

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf guidance"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("guidance-bearing command output should render");

        let capture = std::iter::from_fn(|| receiver.try_recv().ok()).find_map(|command| match command {
            InlineCommand::RecordToolOutput { lines, .. } => Some(lines),
            _ => None,
        });
        let capture = capture.expect("complete command capture should be retained");
        assert!(capture.iter().any(|line| line.contains("Review the result before continuing.")));
    }

    #[tokio::test]
    async fn compact_command_capture_keeps_structured_result_metadata() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({
            "stdout": "command output",
            "generated_files": {
                "count": 1,
                "files": ["src/generated.rs"],
                "summary": "Generated one file"
            },
            "metadata_flag": false,
            "metadata_count": 0,
            "fallback_tool": tools::CODE_SEARCH,
            "fallback_tool_args": {"query": "generated"},
            "stderr_preview": "no stderr was emitted"
        });

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "generate"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("structured command output should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let capture = commands.iter().find_map(|command| match command {
            InlineCommand::RecordToolOutput { lines, .. } => Some(lines.join("\n")),
            _ => None,
        });
        let capture = capture.expect("complete command capture should be retained");
        assert!(capture.contains("structured output"));
        assert!(capture.contains("generated_files"));
        assert!(capture.contains("src/generated.rs"));
        assert!(capture.contains("metadata_flag"));
        assert!(capture.contains("metadata_count"));
        assert!(capture.contains("fallback_tool"));
        assert!(capture.contains("fallback_tool_args"));
        assert!(capture.contains("stderr_preview"));

        let visible_text = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.iter().map(|segment| segment.text.as_str()).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible_text.contains("generated_files"));
        assert!(visible_text.contains("src/generated.rs"));
    }

    #[tokio::test]
    async fn expanded_command_summary_carries_capture_identity() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        renderer.set_tool_display_mode(ToolDisplayMode::Expanded);

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf identity"}),
            &serde_json::json!({"stdout": "captured output"}),
            true,
            None,
            None,
        )
        .await
        .expect("expanded command output should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let capture_id = commands.iter().find_map(|command| match command {
            InlineCommand::RecordToolOutput { id, .. } => Some(*id),
            _ => None,
        });
        let summary_id = commands.iter().find_map(|command| match command {
            InlineCommand::AppendToolOutputLine { id, .. } => Some(*id),
            _ => None,
        });

        assert_eq!(summary_id, capture_id);
    }

    #[tokio::test]
    async fn compact_warning_remains_visible_and_flushes_command_group() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        renderer
            .render_compact_command_activity("printf first", 0, None, None)
            .expect("seed compact command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf warning"}),
            &serde_json::json!({"warning": "no results"}),
            true,
            None,
            None,
        )
        .await
        .expect("warning command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf second"}),
            &serde_json::json!({"stdout": "second output"}),
            true,
            None,
            None,
        )
        .await
        .expect("following command should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let activities = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendCompactActivity(activity) | InlineCommand::ReplaceCompactActivity(activity) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let visible_text = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.iter().map(|segment| segment.text.as_str()).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(activities.len(), 2);
        assert!(activities.iter().all(|activity| activity.command_count == 1));
        assert!(visible_text.contains("no results"));
    }

    #[tokio::test]
    async fn compact_warning_flushes_non_pty_command_alias_group() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        renderer
            .render_compact_command_activity("printf first", 0, None, None)
            .expect("seed compact command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            "bash",
            &serde_json::json!({"command": "printf warning"}),
            &serde_json::json!({"warning": "no results"}),
            true,
            None,
            None,
        )
        .await
        .expect("non-PTY warning command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            "bash",
            &serde_json::json!({"command": "printf second"}),
            &serde_json::json!({"stdout": "second output"}),
            true,
            None,
            None,
        )
        .await
        .expect("following command should render");

        let activities = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendCompactActivity(activity) | InlineCommand::ReplaceCompactActivity(activity) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activities.len(), 2);
        assert!(activities.iter().all(|activity| activity.command_count == 1));
    }

    #[tokio::test]
    async fn compact_file_diff_is_a_glance_boundary_between_command_groups() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf first"}),
            &serde_json::json!({"stdout": "first output"}),
            true,
            None,
            None,
        )
        .await
        .expect("first command should render");

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EDIT_FILE,
            &serde_json::json!({"path": "src/lib.rs"}),
            &serde_json::json!({
                "success": true,
                "diff": [{
                    "path": "src/lib.rs",
                    "operation": "updated",
                    "content": "@@ -1 +1 @@\n-before\n+after\n",
                    "additions": 1,
                    "deletions": 1,
                    "truncated": false,
                    "skipped": false
                }]
            }),
            true,
            None,
            None,
        )
        .await
        .expect("file diff should render");

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf second"}),
            &serde_json::json!({"stdout": "second output"}),
            true,
            None,
            None,
        )
        .await
        .expect("second command should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let activities = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendCompactActivity(activity) | InlineCommand::ReplaceCompactActivity(activity) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let visible_text = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.iter().map(|segment| segment.text.as_str()).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(activities.len(), 2);
        assert!(activities.iter().all(|activity| activity.command_count == 1));
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, InlineCommand::ReplaceCompactActivity(_)))
        );
        assert!(!visible_text.contains("Edit file"));
        assert!(visible_text.contains("• Edited src/lib.rs (+1 -1)"));
        assert!(visible_text.contains("-    1 │ before"));
        assert!(visible_text.contains("+    1 │ after"));
    }

    #[tokio::test]
    async fn compact_command_artifacts_start_a_fresh_group() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        renderer
            .render_compact_command_activity("printf first", 1, None, None)
            .expect("seed compact command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf second"}),
            &serde_json::json!({"stdout": "normal output", "stderr": "diagnostic output"}),
            true,
            None,
            None,
        )
        .await
        .expect("artifact-bearing command should render");

        let activities = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendCompactActivity(activity) | InlineCommand::ReplaceCompactActivity(activity) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activities.len(), 2);
        assert!(activities.iter().all(|activity| activity.command_count == 1));
    }

    #[tokio::test]
    async fn compact_pty_artifacts_flush_a_preceding_command_group() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        renderer
            .render_compact_command_activity("printf first", 0, None, None)
            .expect("seed compact command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "printf diagnostic"}),
            &serde_json::json!({"output": "normal output", "stderr": "diagnostic output"}),
            true,
            None,
            None,
        )
        .await
        .expect("artifact-bearing PTY command should render");
        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf second"}),
            &serde_json::json!({"stdout": "normal output"}),
            true,
            None,
            None,
        )
        .await
        .expect("following command should render");

        let activities = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendCompactActivity(activity) | InlineCommand::ReplaceCompactActivity(activity) => {
                    Some(activity)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(activities.len(), 2);
        assert!(activities.iter().all(|activity| activity.command_count == 1));
    }

    #[tokio::test]
    async fn compact_command_output_keeps_distinct_stderr_visible() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({
            "stdout": "normal output",
            "stderr": "diagnostic output"
        });

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf test"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("stderr-bearing command output should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let visible_text = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.iter().map(|segment| segment.text.as_str()).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            commands
                .iter()
                .any(|command| matches!(command, InlineCommand::AppendCompactActivity(_)))
        );
        assert!(visible_text.contains("diagnostic output"));
        assert!(!visible_text.contains("normal output"));
    }

    #[tokio::test]
    async fn compact_command_output_preserves_identical_stdout_and_stderr() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({
            "stdout": "same output",
            "stderr": "same output"
        });

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::EXECUTE_CODE,
            &serde_json::json!({"command": "printf test"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("identical named streams should remain visible");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        let visible_text = commands
            .iter()
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.iter().map(|segment| segment.text.as_str()).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible_text.contains("same output"));
        let capture_text = commands
            .iter()
            .find_map(|command| match command {
                InlineCommand::RecordToolOutput { lines, .. } => Some(lines.join("\n")),
                _ => None,
            })
            .expect("complete command capture should be retained");
        assert!(capture_text.contains("stdout"));
        assert!(capture_text.contains("stderr"));
        assert_eq!(capture_text.matches("same output").count(), 2);
    }

    #[tokio::test]
    async fn pty_input_forwarding_does_not_collapse_as_a_command() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::SEND_PTY_INPUT,
            &serde_json::json!({"session_id": "pty-1", "chars": ""}),
            &serde_json::json!({"output": "polled output"}),
            true,
            None,
            None,
        )
        .await
        .expect("PTY input forwarding should render");

        let commands = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert!(!commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendCompactActivity(_) | InlineCommand::CollapsePtyBlock(_))
        }));
        assert!(!commands.iter().any(|command| {
            matches!(command, InlineCommand::AppendLine { segments, .. }
                if segments.iter().any(|segment| segment.text.contains("• Ran send_pty_input")))
        }));
    }

    #[tokio::test]
    async fn compact_pty_output_keeps_distinct_stderr_visible() {
        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let output = serde_json::json!({
            "output": "terminal output",
            "stderr": "pty diagnostic",
            "is_exited": true,
            "exit_code": 0
        });

        render_tool_output_common(
            &mut renderer,
            &handle,
            tools::RUN_PTY_CMD,
            &serde_json::json!({"command": "printf test"}),
            &output,
            true,
            None,
            None,
        )
        .await
        .expect("PTY stderr should render");

        let visible_text = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|command| match command {
                InlineCommand::AppendLine { segments, .. } => {
                    Some(segments.into_iter().map(|segment| segment.text).collect::<String>())
                }
                InlineCommand::Inline { segment, .. } => Some(segment.text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(visible_text.contains("stderr: pty diagnostic"), "visible text: {visible_text:?}");
        assert!(!visible_text.contains("• Ran printf test ·"));
    }

    #[tokio::test]
    async fn test_handle_pipeline_output_collects_modified_files_and_records_stats() {
        if !stdin().is_terminal() {
            eprintln!("Skipping TUI-dependent test in non-interactive environment");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let mut registry = ToolRegistry::new(workspace.clone()).await;
        let permission_cache_arc = Arc::new(RwLock::new(ToolPermissionCache::new()));
        let permissions_state = Arc::new(RwLock::new(vtcode_core::config::PermissionsConfig::default()));

        let mut session = spawn_session_with_options(
            inline_theme_from_core_styles(&theme::active_styles()),
            SessionOptions {
                inline_rows: 10,
                workspace_root: Some(workspace.clone()),
                ..SessionOptions::default()
            },
        )
        .unwrap();
        let handle = session.clone_inline_handle();
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        let cache = Arc::new(RwLock::new(ToolResultCache::new(8)));
        let key = ToolCacheKey::new("read_file", "{}", "/tmp/foo.txt");
        {
            let mut c = cache.write().await;
            c.insert_arc(key.clone(), Arc::new("{}".to_string()));
            assert!(c.get(&key).is_some());
        }

        let decision_ledger = Arc::new(RwLock::new(DecisionTracker::new()));
        let mut session_stats = SessionStats::default();
        let mut plan_session =
            crate::agent::runloop::unified::planning_workflow_state::PlanningWorkflowSessionState::default();
        let mut mcp_panel = McpPanelState::new(10, true);
        let approval_recorder = ApprovalRecorder::new(workspace.clone());
        let traj = TrajectoryLogger::new(&workspace);
        let tools = Arc::new(RwLock::new(Vec::new()));

        let mut harness_state = build_harness_state();
        let mut ctx = RunLoopContext::new(
            &mut renderer,
            &handle,
            &mut registry,
            &tools,
            &cache,
            &permission_cache_arc,
            &permissions_state,
            &decision_ledger,
            &mut session_stats,
            &mut plan_session,
            &mut mcp_panel,
            &approval_recorder,
            &mut session,
            None,
            &traj,
            &mut harness_state,
            None,
        );

        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"ok": true}),
            stdout: None,
            modified_files: vec!["/tmp/foo.txt".to_string()],
            command_success: true,
        });

        let (mod_files, _last_stdout) =
            handle_pipeline_output(&mut ctx, "read_file", &serde_json::json!({}), &outcome, None::<&VTCodeConfig>)
                .await
                .expect("handle should succeed");

        assert_eq!(mod_files, vec![PathBuf::from("/tmp/foo.txt")]);

        // Cache invalidation is handled in execution side-effects, not output rendering.
        {
            let c = cache.write().await;
            assert!(c.get(&key).is_some());
        }

        // Ensure session stats were updated
        let rec = session_stats.sorted_tools();
        assert!(rec.contains(&"read_file".to_string()));
    }

    #[tokio::test]
    async fn task_tracker_updates_replace_previous_inline_block() {
        transcript::clear();

        let (sender, mut receiver) = unbounded_channel();
        let handle = InlineHandle::new_for_tests(sender);
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
        let mut stats = SessionStats::default();
        let mut mcp = McpPanelState::default();

        let first = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "status": "updated",
                "view": {
                    "title": "Respond to user greeting and assess next steps",
                    "lines": [
                        {"display": "├ ✔ Greet user and summarize current workspace state"},
                        {"display": "├ > Ask what task they'd like to tackle"},
                        {"display": "└ • Offer to provide workspace tour if needed"}
                    ]
                },
                "checklist": {
                    "title": "Respond to user greeting and assess next steps",
                    "total": 3,
                    "completed": 1,
                    "in_progress": 1,
                    "pending": 1,
                    "blocked": 0,
                    "progress_percent": 33,
                    "items": [
                        {"index": 1, "description": "Greet user and summarize current workspace state", "status": "completed"},
                        {"index": 2, "description": "Ask what task they'd like to tackle", "status": "in_progress"},
                        {"index": 3, "description": "Offer to provide workspace tour if needed", "status": "pending"}
                    ]
                },
                "message": "Item 2 status changed: pending → in_progress"
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });
        let second = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({
                "status": "updated",
                "view": {
                    "title": "Respond to user greeting and assess next steps",
                    "lines": [
                        {"display": "├ ✔ Greet user and summarize current workspace state"},
                        {"display": "├ ✔ Ask what task they'd like to tackle"},
                        {"display": "└ • Offer to provide workspace tour if needed"}
                    ]
                },
                "checklist": {
                    "title": "Respond to user greeting and assess next steps",
                    "total": 3,
                    "completed": 2,
                    "in_progress": 0,
                    "pending": 1,
                    "blocked": 0,
                    "progress_percent": 67,
                    "items": [
                        {"index": 1, "description": "Greet user and summarize current workspace state", "status": "completed"},
                        {"index": 2, "description": "Ask what task they'd like to tackle", "status": "completed"},
                        {"index": 3, "description": "Offer to provide workspace tour if needed", "status": "pending"}
                    ]
                },
                "message": "Item 2 status changed: in_progress → completed"
            }),
            stdout: None,
            modified_files: vec![],
            command_success: true,
        });

        let args = serde_json::json!({"action": "update", "index": 2, "status": "in_progress"});
        let mut output_ctx = OutcomeContext {
            workspace_root: None,
            session_stats: &mut stats,
            renderer: &mut renderer,
            handle: &handle,
            mcp_panel_state: &mut mcp,
            vt_config: None::<&VTCodeConfig>,
        };

        process_outcome_common(&mut output_ctx, tools::TASK_TRACKER, &args, &first)
            .await
            .expect("first tracker render should succeed");

        let args = serde_json::json!({"action": "update", "index": 2, "status": "completed"});
        process_outcome_common(&mut output_ctx, tools::TASK_TRACKER, &args, &second)
            .await
            .expect("second tracker render should succeed");

        let mut saw_task_panel_update = false;
        while let Ok(command) = receiver.try_recv() {
            if matches!(command, InlineCommand::ShowTransient { .. }) {
                saw_task_panel_update = true;
            }
        }

        assert!(saw_task_panel_update, "expected tracker updates to refresh the dedicated task panel");
    }

    #[tokio::test]
    async fn test_handle_pipeline_output_mcp_events() {
        if !stdin().is_terminal() {
            eprintln!("Skipping TUI-dependent test in non-interactive environment");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();

        let mut registry = ToolRegistry::new(workspace.clone()).await;
        let permission_cache_arc = Arc::new(RwLock::new(ToolPermissionCache::new()));
        let permissions_state = Arc::new(RwLock::new(vtcode_core::config::PermissionsConfig::default()));

        let mut session = spawn_session_with_options(
            inline_theme_from_core_styles(&theme::active_styles()),
            SessionOptions {
                inline_rows: 10,
                workspace_root: Some(workspace.clone()),
                ..SessionOptions::default()
            },
        )
        .unwrap();
        let handle = session.clone_inline_handle();
        let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());

        let cache = Arc::new(RwLock::new(ToolResultCache::new(8)));
        let decision_ledger = Arc::new(RwLock::new(DecisionTracker::new()));
        let mut session_stats = SessionStats::default();
        let mut plan_session =
            crate::agent::runloop::unified::planning_workflow_state::PlanningWorkflowSessionState::default();
        let mut mcp_panel = McpPanelState::new(10, true);
        let approval_recorder = ApprovalRecorder::new(workspace.clone());
        let traj = TrajectoryLogger::new(&workspace);
        let tools = Arc::new(RwLock::new(Vec::new()));

        let mut harness_state = build_harness_state();
        let mut ctx = RunLoopContext::new(
            &mut renderer,
            &handle,
            &mut registry,
            &tools,
            &cache,
            &permission_cache_arc,
            &permissions_state,
            &decision_ledger,
            &mut session_stats,
            &mut plan_session,
            &mut mcp_panel,
            &approval_recorder,
            &mut session,
            None,
            &traj,
            &mut harness_state,
            None,
        );

        let outcome = ToolPipelineOutcome::from_status(ToolExecutionStatus::Success {
            output: serde_json::json!({"exit_code": 0}),
            stdout: Some("ok".to_string()),
            modified_files: vec![],
            command_success: true,
        });

        let (_mod_files, _last_stdout) =
            handle_pipeline_output(&mut ctx, "mcp_example", &serde_json::json!({}), &outcome, None::<&VTCodeConfig>)
                .await
                .expect("handle should succeed");

        assert!(ctx.mcp_panel_state.event_count() > 0);
    }
}
