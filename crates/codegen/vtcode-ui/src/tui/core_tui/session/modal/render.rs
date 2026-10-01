use crate::tui::config::constants::ui;
use crate::tui::ui::markdown::render_markdown;
use crate::tui::ui::tui::session::inline_list::{
    InlineListRow, list_cursor, row_height, selection_padding, selection_padding_width,
};
use crate::tui::ui::tui::session::list_panel::{
    SharedListPanelSections, SharedListPanelStyles, SharedListWidgetModel, render_shared_list_panel,
};
use crate::tui::ui::tui::types::{InlineItemKind, InlineListSelection, InlineStatus, InlineTone, SecurePromptConfig};
use ratatui::{
    prelude::*,
    widgets::{Paragraph, Tabs, Wrap},
};
use ratatui_cheese::input::{Input, InputState, InputStyles};
use unicode_width::UnicodeWidthStr;

use super::layout::{ModalBodyContext, ModalRenderStyles, ModalSection};
use super::state::{ModalListState, ModalSearchState, WizardModalState, WizardStepState};
use crate::tui::core_tui::session::transcript_links::{TranscriptFileLinkTarget, decorate_detected_link_lines};
use crate::tui::core_tui::style::ratatui_style_from_inline;
use crate::tui::ui::shell_syntax::{ShellLineStyles, shell_syntax_segments};
use crate::tui::ui::tui::session::wrapping;
use ratatui::style::Color as RatatuiColor;
use std::mem;
use std::path::Path;

fn modal_text_area_aligned_with_list(area: Rect) -> Rect {
    let gutter = selection_padding_width().min(area.width as usize) as u16;
    if gutter == 0 || area.width <= gutter {
        area
    } else {
        Rect {
            x: area.x.saturating_add(gutter),
            width: area.width.saturating_sub(gutter),
            ..area
        }
    }
}

fn markdown_to_plain_lines(text: &str) -> Vec<String> {
    let mut lines = render_markdown(text)
        .into_iter()
        .map(|line| line.segments.into_iter().map(|segment| segment.text).collect::<String>())
        .collect::<Vec<_>>();

    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }

    if lines.is_empty() { vec![String::new()] } else { lines }
}

fn wrap_line_to_width(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_owned()];
    }

    if line.is_empty() {
        return vec![String::new()];
    }

    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in line.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if current_width + ch_width > width && !current.is_empty() {
            rows.push(mem::take(&mut current));
            current_width = 0;
            if ch.is_whitespace() {
                continue;
            }
        }

        current.push(ch);
        current_width += ch_width;
    }

    if !current.is_empty() {
        rows.push(current);
    }

    if rows.is_empty() { vec![String::new()] } else { rows }
}

fn markdown_lines_for_modal(text: &str, style: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in markdown_to_plain_lines(text) {
        lines.push(Line::from(Span::styled(line, style)));
    }

    if lines.is_empty() { vec![Line::default()] } else { lines }
}

#[cfg(test)]
fn render_markdown_lines_for_modal(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    wrapping::wrap_lines_preserving_urls(markdown_lines_for_modal(text, style), width)
}

#[derive(Clone, Debug)]
pub struct ModalInlineEditor {
    pub(crate) item_index: usize,
    pub(crate) label: String,
    pub(crate) text: String,
    pub(crate) placeholder: Option<String>,
    pub(crate) active: bool,
}

#[derive(Default)]
pub(crate) struct ModalRenderOutcome {
    pub(crate) list_area: Option<Rect>,
    pub(crate) text_areas: Vec<Rect>,
    pub(crate) link_targets: Vec<TranscriptFileLinkTarget>,
}

impl ModalRenderOutcome {
    pub(crate) fn push_text_area(&mut self, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.text_areas.push(area);
    }
}

fn render_modal_text_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    workspace_root: Option<&Path>,
    last_mouse_position: Option<(u16, u16)>,
    link_style: Style,
    hovered_link_style: Style,
    outcome: &mut ModalRenderOutcome,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (decorated, targets) =
        decorate_detected_link_lines(lines, area, workspace_root, last_mouse_position, link_style, hovered_link_style);
    frame.render_widget(Paragraph::new(decorated).wrap(Wrap { trim: false }), area);
    outcome.push_text_area(area);
    outcome.link_targets.extend(targets);
}

struct ModalListPanelModel<'a> {
    list: &'a mut ModalListState,
    styles: &'a ModalRenderStyles,
    inline_editor: Option<&'a ModalInlineEditor>,
    show_numbers: bool,
}

impl SharedListWidgetModel for ModalListPanelModel<'_> {
    fn rows(&self, width: u16) -> Vec<(InlineListRow, u16)> {
        if self.list.visible_indices.is_empty() {
            return vec![(
                InlineListRow::single(
                    Line::from(Span::styled(ui::MODAL_LIST_NO_RESULTS_MESSAGE.to_owned(), self.styles.detail)),
                    self.styles.detail,
                ),
                1_u16,
            )];
        }

        let selection_gutter = selection_padding_width() as u16;
        let content_width = width.saturating_sub(selection_gutter) as usize;
        let selected_visible = self.list.list_state.selected();
        // Gate once: per-row numbers stay cheap so crowded lists do not
        // pay a quadratic scan for badges they will not show.
        let numbered = self.show_numbers && self.list.numbered_shortcuts();
        self.list
            .visible_indices
            .iter()
            .enumerate()
            .map(|(visible_index, &item_index)| {
                let is_selected = selected_visible == Some(visible_index)
                    && self.list.items.get(item_index).is_some_and(|i| i.selection.is_some());
                let shortcut_number = numbered.then(|| self.list.shortcut_number(visible_index)).flatten();
                let lines = modal_list_item_lines(
                    self.list,
                    visible_index,
                    item_index,
                    self.styles,
                    content_width,
                    self.inline_editor,
                    is_selected,
                    shortcut_number,
                );
                (
                    InlineListRow {
                        lines: lines.clone(),
                        style: self.styles.selectable,
                    },
                    row_height(&lines),
                )
            })
            .collect()
    }

    fn selected(&self) -> Option<usize> {
        self.list.list_state.selected()
    }

    fn set_selected(&mut self, selected: Option<usize>) {
        self.list.list_state.select(selected);
    }

    fn set_scroll_offset(&mut self, offset: usize) {
        let max_offset = self.list.max_scroll_offset();
        *self.list.list_state.offset_mut() = offset.min(max_offset);
    }

    fn set_viewport_rows(&mut self, rows: u16) {
        self.list.set_viewport_rows(rows);
        self.list.ensure_visible(rows);
    }
}

pub fn render_modal_list(
    frame: &mut Frame<'_>,
    area: Rect,
    list: &mut ModalListState,
    styles: &ModalRenderStyles,
    footer_hint: Option<&str>,
    inline_editor: Option<&ModalInlineEditor>,
    show_numbers: bool,
    status: Option<&InlineStatus>,
) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }

    let mut info = Vec::new();
    // Status strip sits above the keyboard/filter hint so the last action is
    // still readable while the user keeps navigating.
    if let Some(status) = status {
        let tone = tone_style(status.tone, styles);
        info.push(Line::from(vec![Span::styled("• ", tone), Span::styled(status.message.clone(), tone)]));
    }
    info.extend(modal_list_summary_line(list, styles, footer_hint));
    let mut panel_model = ModalListPanelModel { list, styles, inline_editor, show_numbers };
    let sections = SharedListPanelSections { header: Vec::new(), info, search: None };
    render_shared_list_panel(
        frame,
        area,
        sections,
        SharedListPanelStyles {
            base_style: styles.background,
            selected_style: Some(styles.highlight),
            text_style: styles.detail,
            divider_style: Some(styles.border),
            input_styles: InputStyles::default(),
            show_divider: false,
        },
        &mut panel_model,
    );

    area
}

/// Render wizard tabs header showing steps with completion status
pub fn render_wizard_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    steps: &[WizardStepState],
    current_step: usize,
    styles: &ModalRenderStyles,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    if steps.len() <= 1 {
        let label = steps
            .first()
            .map(|step| {
                if step.completed {
                    format!("✔ {}", step.title)
                } else {
                    step.title.clone()
                }
            })
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(label, styles.highlight))).wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let titles: Vec<Line<'static>> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let icon = if step.completed { "✔" } else { "☐" };
            let text = format!("{} {}", icon, step.title);
            if i == current_step {
                Line::from(text).style(styles.highlight)
            } else if step.completed {
                Line::from(text).style(styles.selectable)
            } else {
                Line::from(text).style(styles.detail)
            }
        })
        .collect();

    let tabs = Tabs::new(titles)
        .select(Some(current_step))
        .divider(" │ ")
        .padding("", "")
        .highlight_style(styles.highlight);

    frame.render_widget(tabs, area);
}

pub(crate) fn inline_editor_for_step(step: &WizardStepState) -> Option<ModalInlineEditor> {
    let selected_visible = step.list.list_state.selected()?;
    let item_index = *step.list.visible_indices.get(selected_visible)?;
    let item = step.list.items.get(item_index)?;

    match item.selection.as_ref() {
        Some(InlineListSelection::RequestUserInputAnswer { selected, other, .. })
            if selected.is_empty() && other.is_some() =>
        {
            Some(ModalInlineEditor {
                item_index,
                label: step.freeform_label.clone().unwrap_or_else(|| "Custom note".to_string()),
                text: step.notes.clone(),
                placeholder: step.freeform_placeholder.clone(),
                active: step.notes_active,
            })
        }
        _ => None,
    }
}

/// Render wizard modal body including tabs, question, and list
pub(crate) fn render_wizard_modal_body(
    frame: &mut Frame<'_>,
    area: Rect,
    wizard: &mut WizardModalState,
    styles: &ModalRenderStyles,
    input_styles: &InputStyles,
    workspace_root: Option<&Path>,
    last_mouse_position: Option<(u16, u16)>,
    link_style: Style,
    hovered_link_style: Style,
) -> ModalRenderOutcome {
    let mut outcome = ModalRenderOutcome::default();
    if area.width == 0 || area.height == 0 {
        return outcome;
    }

    let is_multistep = wizard.mode == crate::tui::ui::tui::types::WizardModalMode::MultiStep;
    let text_alignment_fn: fn(Rect) -> Rect = if is_multistep {
        |rect| rect
    } else {
        modal_text_area_aligned_with_list
    };
    let content_width = text_alignment_fn(area).width.max(1) as usize;
    let current_step_state = wizard.steps.get(wizard.current_step);
    let inline_editor = current_step_state.and_then(inline_editor_for_step);
    let has_notes =
        current_step_state.is_some_and(|s| s.notes_active || !s.notes.is_empty()) && inline_editor.is_none();
    let instruction_lines = wizard.instruction_lines();
    let header_lines = if is_multistep {
        markdown_lines_for_modal(wizard.question_header().as_str(), styles.header)
    } else {
        Vec::new()
    };
    let question_lines = wizard
        .steps
        .get(wizard.current_step)
        .map(|step| markdown_lines_for_modal(step.question.as_str(), styles.header))
        .unwrap_or_else(|| vec![Line::default()]);

    let mut info_lines = question_lines;

    info_lines.extend(
        instruction_lines
            .into_iter()
            .map(|line| Line::from(Span::styled(line, styles.hint))),
    );
    let header_row_count = wrapping::wrap_lines_preserving_urls(header_lines.clone(), content_width)
        .len()
        .max(1);
    let info_row_count = wrapping::wrap_lines_preserving_urls(info_lines.clone(), content_width)
        .len()
        .max(1);

    // Layout: [Header] [Info] [Input?] [Search?] [Divider?] [List]
    let mut constraints = Vec::new();
    if is_multistep {
        constraints.push(Constraint::Length(header_row_count.min(u16::MAX as usize) as u16));
    } else {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(info_row_count.min(u16::MAX as usize) as u16));
    if has_notes {
        constraints.push(Constraint::Length(1));
    }
    if wizard.search.is_some() {
        constraints.push(Constraint::Length(1));
    }
    let show_list_divider = wizard.search.is_some();
    if show_list_divider {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(3));

    let chunks = Layout::vertical(constraints).split(area);

    let mut idx = 0;
    if is_multistep {
        let header_area = text_alignment_fn(chunks[idx]);
        render_modal_text_lines(
            frame,
            header_area,
            header_lines,
            workspace_root,
            last_mouse_position,
            link_style,
            hovered_link_style,
            &mut outcome,
        );
    } else {
        let tabs_area = text_alignment_fn(chunks[idx]);
        render_wizard_tabs(frame, tabs_area, &wizard.steps, wizard.current_step, styles);
        outcome.push_text_area(tabs_area);
    }
    idx += 1;

    render_modal_text_lines(
        frame,
        text_alignment_fn(chunks[idx]),
        info_lines,
        workspace_root,
        last_mouse_position,
        link_style,
        hovered_link_style,
        &mut outcome,
    );
    idx += 1;

    if has_notes
        && let Some(step) = wizard.steps.get(wizard.current_step)
        && idx < chunks.len()
    {
        let label_text = step.freeform_label.as_deref().unwrap_or("Custom note");
        let input_widget = Input::new(label_text)
            .placeholder(step.freeform_placeholder.as_deref().unwrap_or("Type here..."))
            .palette(&ratatui_cheese::theme::Palette::dark());
        let mut input_state = InputState::new();
        input_state.set_value(step.notes.clone());
        if step.notes_active {
            input_state.set_focused(true);
            // Position cursor at end
            for _ in 0..step.notes.chars().count() {
                input_state.move_right();
            }
        }
        let input_area = text_alignment_fn(chunks[idx]);
        frame.render_stateful_widget(&input_widget, input_area, &mut input_state);
        idx += 1;
    }

    if let Some(search) = wizard.search.as_ref()
        && idx < chunks.len()
    {
        render_modal_search(frame, text_alignment_fn(chunks[idx]), search, input_styles);
        idx += 1;
    }

    if show_list_divider && idx < chunks.len() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                ui::INLINE_BLOCK_HORIZONTAL.repeat(chunks[idx].width as usize),
                styles.border,
            )))
            .wrap(Wrap { trim: false }),
            chunks[idx],
        );
        idx += 1;
    }

    let wizard_numbers = wizard.numbered_shortcuts();
    if let Some(step) = wizard.steps.get_mut(wizard.current_step)
        && idx < chunks.len()
    {
        outcome.list_area = Some(render_modal_list(
            frame,
            chunks[idx],
            &mut step.list,
            styles,
            None,
            inline_editor.as_ref(),
            wizard_numbers,
            None,
        ));
    }

    outcome
}

fn modal_list_summary_line(
    list: &ModalListState,
    styles: &ModalRenderStyles,
    footer_hint: Option<&str>,
) -> Vec<Line<'static>> {
    if !list.filter_active() {
        return match list.non_filter_summary_text(footer_hint) {
            Some(message) => vec![Line::from(Span::styled(message, styles.hint))],
            None => Vec::new(),
        };
    }

    let mut spans = Vec::new();
    let matches = list.visible_selectable_count();
    let total = list.total_selectable();
    if matches == 0 {
        spans.push(Span::styled(ui::MODAL_LIST_SUMMARY_NO_MATCHES.to_owned(), styles.search_match));
        if !ui::MODAL_LIST_SUMMARY_RESET_HINT.is_empty() {
            spans.push(Span::styled(
                format!("{}{}", ui::MODAL_LIST_SUMMARY_SEPARATOR, ui::MODAL_LIST_SUMMARY_RESET_HINT),
                styles.hint,
            ));
        }
    } else {
        spans.push(Span::styled(format!("{matches} / {total}"), styles.detail));
    }

    if spans.is_empty() {
        Vec::new()
    } else {
        vec![Line::from(spans)]
    }
}

pub(crate) fn render_modal_body(
    frame: &mut Frame<'_>,
    area: Rect,
    context: ModalBodyContext<'_, '_>,
    workspace_root: Option<&Path>,
    last_mouse_position: Option<(u16, u16)>,
    link_style: Style,
    hovered_link_style: Style,
) -> ModalRenderOutcome {
    let mut outcome = ModalRenderOutcome::default();
    if area.width == 0 || area.height == 0 {
        return outcome;
    }

    let mut sections = Vec::new();
    let has_instructions = context.instructions.iter().any(|line| !line.trim().is_empty());
    let has_secure_prompt = context.secure_prompt.is_some();
    let has_search = context.search.is_some();
    let has_list = context.list.is_some();
    // Eight wrapped visual rows: prompt + full-text summary (up to four
    // wrapped rows) + steps + overflow. Matches `MAX_INLINE_INSTRUCTION_ROWS`
    // in `modal_renderer.rs` so the body viewport and the claimed modal
    // height agree and no blank gap or clipped summary appears.
    let instruction_row_limit = if has_secure_prompt || has_search || has_list {
        8
    } else {
        area.height.max(1) as usize
    };
    let instruction_lines = if has_instructions {
        modal_instruction_lines(area, context.instructions, context.styles)
    } else {
        Vec::new()
    };
    let instruction_row_count =
        wrapping::wrap_lines_preserving_urls(instruction_lines.clone(), area.width.max(1) as usize)
            .len()
            .clamp(1, instruction_row_limit);
    if has_instructions {
        sections.push(ModalSection::Instructions);
    }
    if has_secure_prompt {
        sections.push(ModalSection::Prompt);
    }
    if has_search {
        sections.push(ModalSection::Search);
    }
    if has_list {
        sections.push(ModalSection::List);
    }

    if sections.is_empty() {
        return outcome;
    }

    let mut constraints = Vec::new();
    for section in &sections {
        match section {
            ModalSection::Search => {
                let rows = context
                    .search
                    .map(|s| if s.label.is_empty() { 1 } else { 2 })
                    .unwrap_or(2)
                    .min(area.height);
                constraints.push(Constraint::Length(rows));
            }
            ModalSection::Instructions => {
                let visible_rows = instruction_row_count as u16;
                constraints.push(Constraint::Length(visible_rows.min(area.height)));
            }
            ModalSection::Prompt => constraints.push(Constraint::Length(2.min(area.height))),
            ModalSection::List => constraints.push(Constraint::Min(1)),
        }
    }
    let show_list_divider = context.list.is_some();
    if show_list_divider {
        // The divider is spliced directly before the List chunk, so List must
        // be the final section for the `chunk_idx` bookkeeping below to hold.
        debug_assert!(
            matches!(sections.last(), Some(ModalSection::List)),
            "list divider assumes List is the final modal section"
        );
        let insert_at = constraints.len().saturating_sub(1);
        constraints.insert(insert_at, Constraint::Length(1));
    }

    let chunks = Layout::vertical(constraints).split(area);
    let mut list_state = context.list;

    let mut chunk_idx = 0usize;
    for section in sections {
        let chunk = chunks[chunk_idx];
        match section {
            ModalSection::Instructions => {
                if chunk.height > 0 && !instruction_lines.is_empty() {
                    render_modal_text_lines(
                        frame,
                        chunk,
                        instruction_lines.clone(),
                        workspace_root,
                        last_mouse_position,
                        link_style,
                        hovered_link_style,
                        &mut outcome,
                    );
                }
            }
            ModalSection::Prompt => {
                if let Some(config) = context.secure_prompt {
                    render_secure_prompt(frame, chunk, config, context.input, context.cursor, context.input_styles);
                }
            }
            ModalSection::Search => {
                if let Some(config) = context.search {
                    render_modal_search(frame, chunk, config, context.input_styles);
                }
            }
            ModalSection::List => {
                if show_list_divider && chunk_idx > 0 {
                    let divider_chunk = chunks[chunk_idx];
                    frame.render_widget(
                        Paragraph::new(Line::from(Span::styled(
                            ui::INLINE_BLOCK_HORIZONTAL.repeat(divider_chunk.width as usize),
                            context.styles.border,
                        )))
                        .wrap(Wrap { trim: false }),
                        divider_chunk,
                    );
                    chunk_idx += 1;
                }
                if let Some(list_state) = list_state.as_deref_mut() {
                    outcome.list_area = Some(render_modal_list(
                        frame,
                        chunks[chunk_idx],
                        list_state,
                        context.styles,
                        context.footer_hint,
                        None,
                        context.search.is_none(),
                        context.status,
                    ));
                }
            }
        }
        chunk_idx += 1;
    }

    outcome
}

enum DiffLineKind {
    Addition,
    Deletion,
    HunkHeader,
}

fn classify_diff_line(line: &str) -> Option<DiffLineKind> {
    let trimmed = line.trim();
    if trimmed.starts_with("@@ ") {
        Some(DiffLineKind::HunkHeader)
    } else if trimmed.starts_with('+') {
        Some(DiffLineKind::Addition)
    } else if trimmed.starts_with('-') {
        Some(DiffLineKind::Deletion)
    } else {
        None
    }
}

fn diff_line_style(kind: &DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Addition => Style::default().fg(RatatuiColor::LightGreen),
        DiffLineKind::Deletion => Style::default().fg(RatatuiColor::LightRed),
        DiffLineKind::HunkHeader => Style::default().add_modifier(Modifier::DIM),
    }
}

/// Plan-approval header rows already carry their own markers (`1. …`, `… and
/// N more`), so prepending the generic `•` bullet would double-mark them.
/// Detect those rows and render them as plain body text without a bullet.
fn is_numbered_step_row(trimmed: &str) -> bool {
    let Some((number, rest)) = trimmed.split_once('.') else {
        return false;
    };
    !number.is_empty() && number.chars().all(|character| character.is_ascii_digit()) && !rest.trim().is_empty()
}

fn is_plan_overflow_row(trimmed: &str) -> bool {
    trimmed.starts_with('…') || trimmed.starts_with("...") || trimmed.starts_with("·")
}

/// Split `Label: value` metadata rows (`Risk`, `Source`, the permission-popup
/// agent goal, the approval sandbox posture, …) so the label can render
/// muted and the value in body style. Returns the trimmed label and value;
/// `Tool:` stays a header and never matches here.
fn split_context_row(trimmed: &str) -> Option<(&str, &str)> {
    const CONTEXT_LABELS: &[&str] = &[
        "Reason",
        "Risk",
        "Expected",
        "Suggestion",
        "Impact",
        "Fix",
        "Source",
        "Summary",
        "Plan",
        "Environment",
        "What the agent is trying to do",
        "Requested from",
    ];
    let (label, value) = trimmed.split_once(':')?;
    let label = label.trim();
    let value = value.trim();
    if !CONTEXT_LABELS.contains(&label) || value.is_empty() {
        return None;
    }
    Some((label, value))
}

fn modal_instruction_lines(area: Rect, instructions: &[String], styles: &ModalRenderStyles) -> Vec<Line<'static>> {
    fn parse_instruction_highlight_markup(text: &str) -> (String, bool) {
        let trimmed = text.trim();
        match trimmed
            .strip_prefix("**")
            .and_then(|value| value.strip_suffix("**"))
            .map(str::trim)
        {
            Some(value) if !value.is_empty() => (value.to_string(), true),
            _ => (trimmed.to_string(), false),
        }
    }

    fn wrap_instruction_lines(text: &str, width: usize) -> Vec<String> {
        if width == 0 {
            return vec![text.to_owned()];
        }

        let mut lines = Vec::new();
        let mut current = String::new();

        for word in text.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if current.is_empty() {
                current.push_str(word);
                continue;
            }

            let current_width = UnicodeWidthStr::width(current.as_str());
            let candidate_width = current_width.saturating_add(1).saturating_add(word_width);
            if candidate_width > width {
                lines.push(current);
                current = word.to_owned();
            } else {
                current.push(' ');
                current.push_str(word);
            }
        }

        if !current.is_empty() {
            lines.push(current);
        }

        if lines.is_empty() { vec![text.to_owned()] } else { lines }
    }

    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut items: Vec<Vec<Line<'static>>> = Vec::new();
    let mut first_content_rendered = false;
    let mut first_code_row_seen = false;
    let content_width = area.width.saturating_sub(2) as usize;
    let bullet_prefix = format!("{} ", ui::MODAL_INSTRUCTIONS_BULLET);
    let bullet_indent = " ".repeat(UnicodeWidthStr::width(bullet_prefix.as_str()));
    let shell_styles = ShellLineStyles::new();
    // Full syntax highlighting for the approval command block: command,
    // options, strings, variables, and separators keep distinct token
    // colors so the reviewable invocation stays scannable. Indented (not
    // `•`-bulleted) to read as one code block without the old `│` gutter.
    let code_gutter = "    ";

    for line in instructions {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            items.push(vec![Line::default()]);
            continue;
        }

        if let Some(header) = trimmed.strip_prefix("## ") {
            // Blank row before each new section gives header-led sections
            // clear visual separation without extra chrome.
            if first_content_rendered {
                items.push(vec![Line::default()]);
            }
            first_content_rendered = true;
            items.push(vec![Line::from(Span::styled(
                header.to_uppercase(),
                styles.header.add_modifier(Modifier::BOLD),
            ))]);
            continue;
        }

        if let Some(code) = trimmed.strip_prefix('`').and_then(|value| value.strip_suffix('`')) {
            let command = code.trim();
            first_content_rendered = true;
            let mut spans = vec![Span::styled(code_gutter.to_string(), styles.divider)];
            // The approval command block opens with a `$ ` shell marker on
            // its first row. Paint it dimmed and keep it out of the syntax
            // tokenizer, where a lone `$` would highlight as a variable.
            // Only the first code row is eligible, so a continuation line
            // that literally starts with `$ ` keeps its token colors.
            // Generic path: any modal's first `$ `-prefixed code fence gets
            // this treatment; today only the approval preview emits one.
            let (marker, body) = if !first_code_row_seen {
                first_code_row_seen = true;
                command.strip_prefix("$ ").map(|rest| ("$ ", rest)).unwrap_or(("", command))
            } else {
                ("", command)
            };
            if !marker.is_empty() {
                spans.push(Span::styled(marker.to_string(), styles.detail));
            }
            for segment in shell_syntax_segments(body, &shell_styles, true) {
                spans.push(Span::styled(segment.text, ratatui_style_from_inline(&segment.style, None)));
            }
            items.push(vec![Line::from(spans)]);
            continue;
        }

        let (display_text, is_highlighted) = parse_instruction_highlight_markup(trimmed);
        let wrapped = wrap_instruction_lines(&display_text, content_width);
        if wrapped.is_empty() {
            items.push(vec![Line::default()]);
            continue;
        }

        if let Some(diff_kind) = classify_diff_line(trimmed) {
            first_content_rendered = true;
            let style = diff_line_style(&diff_kind);
            items.push(vec![Line::from(vec![
                Span::styled(bullet_indent.clone(), Style::default()),
                Span::styled(display_text, style),
            ])]);
        } else if let Some((label, value)) = split_context_row(trimmed) {
            // `Reason:` / `Risk:` / `Source:` rows render as dim label +
            // body value with a hanging indent — no bullet — so metadata
            // reads as subordinate to the COMMAND code block above.
            // `Summary:` / `Plan:` use the same treatment so the plan-approval
            // header reads as an overview section instead of a bulleted list.
            first_content_rendered = true;
            let wrapped_value = wrap_instruction_lines(value, content_width.saturating_sub(2).max(1));
            let mut lines = Vec::new();
            for (index, segment) in wrapped_value.into_iter().enumerate() {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("  ".to_string(), Style::default()),
                        Span::styled(format!("{label}:"), styles.detail),
                        Span::raw(" ".to_string()),
                        Span::styled(segment, styles.instruction_body),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("    ".to_string(), Style::default()),
                        Span::styled(segment, styles.instruction_body),
                    ]));
                }
            }
            if lines.is_empty() {
                lines.push(Line::default());
            }
            items.push(lines);
        } else if !first_content_rendered {
            let mut lines = Vec::new();
            for (index, segment) in wrapped.into_iter().enumerate() {
                let style = if is_highlighted {
                    styles.highlight.add_modifier(Modifier::BOLD)
                } else if index == 0 {
                    styles.header
                } else {
                    styles.instruction_body
                };
                lines.push(Line::from(Span::styled(segment, style)));
            }
            items.push(lines);
            first_content_rendered = true;
        } else if is_plan_overflow_row(trimmed) {
            // `… and N more plan steps` is meta-evidence, not content: dim it
            // and skip the bullet so the header stays scannable.
            first_content_rendered = true;
            let mut lines = Vec::new();
            for segment in wrapped {
                lines.push(Line::from(vec![
                    Span::styled(bullet_indent.clone(), Style::default()),
                    Span::styled(segment, styles.hint),
                ]));
            }
            items.push(lines);
        } else if is_numbered_step_row(trimmed) {
            // Numbered plan steps (`1. …`) already carry a list marker.
            // Render them indented but bullet-free to avoid `• 1. …`.
            first_content_rendered = true;
            let mut lines = Vec::new();
            for (index, segment) in wrapped.into_iter().enumerate() {
                let body_style = if is_highlighted {
                    styles.highlight.add_modifier(Modifier::BOLD)
                } else {
                    styles.instruction_body
                };
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(bullet_indent.clone(), Style::default()),
                        Span::styled(segment, body_style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(bullet_indent.clone(), Style::default()),
                        Span::styled(format!("  {segment}"), body_style),
                    ]));
                }
            }
            items.push(lines);
        } else {
            let mut lines = Vec::new();
            for (index, segment) in wrapped.into_iter().enumerate() {
                let body_style = if is_highlighted {
                    styles.highlight.add_modifier(Modifier::BOLD)
                } else {
                    styles.instruction_body
                };
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(bullet_prefix.clone(), styles.instruction_bullet),
                        Span::styled(segment, body_style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(bullet_indent.clone(), styles.instruction_bullet),
                        Span::styled(segment, body_style),
                    ]));
                }
            }
            items.push(lines);
        }
    }

    if items.is_empty() {
        items.push(vec![Line::default()]);
    }

    let mut rendered_lines = Vec::new();

    for lines in items {
        rendered_lines.extend(lines);
    }

    rendered_lines
}

fn render_modal_search(frame: &mut Frame<'_>, area: Rect, search: &ModalSearchState, input_styles: &InputStyles) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let input_widget = Input::new(search.label.as_str())
        .placeholder(search.placeholder.as_deref().unwrap_or("Type to filter..."))
        .prompt(">")
        .styles(input_styles.clone());

    let mut input_state = InputState::new();
    input_state.set_value(search.query.clone());
    input_state.set_focused(true);
    for _ in 0..search.query.chars().count() {
        input_state.move_right();
    }

    frame.render_stateful_widget(&input_widget, area, &mut input_state);
}

fn render_secure_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    config: &SecurePromptConfig,
    input: &str,
    cursor: usize,
    input_styles: &InputStyles,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let input_widget = Input::new(config.label.as_str())
        .placeholder(config.placeholder.as_deref().unwrap_or("Enter value..."))
        .password_mode(config.mask_input)
        .password_char('\u{2022}')
        .styles(input_styles.clone());

    let mut input_state = InputState::new();
    input_state.set_value(input.to_owned());
    input_state.set_focused(true);
    let char_count = input.chars().count();
    let target = cursor.min(char_count);
    for _ in 0..target {
        input_state.move_right();
    }

    frame.render_stateful_widget(&input_widget, area, &mut input_state);
}

pub(super) fn highlight_segments(
    text: &str,
    normal_style: Style,
    highlight_style: Style,
    terms: &[String],
) -> Vec<Span<'static>> {
    if text.is_empty() {
        return vec![Span::styled(String::new(), normal_style)];
    }

    if terms.is_empty() {
        return vec![Span::styled(text.to_owned(), normal_style)];
    }

    let lower = text.to_ascii_lowercase();
    let mut char_offsets: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
    char_offsets.push(text.len());
    let char_count = char_offsets.len().saturating_sub(1);
    if char_count == 0 {
        return vec![Span::styled(text.to_owned(), normal_style)];
    }

    let mut highlight_flags = vec![false; char_count];
    for term in terms {
        let needle = term.as_str();
        if needle.is_empty() {
            continue;
        }

        let mut search_start = 0usize;
        while search_start < lower.len() {
            let Some(pos) = lower[search_start..].find(needle) else {
                break;
            };
            let byte_start = search_start + pos;
            let byte_end = byte_start + needle.len();
            let start_index = char_offsets.partition_point(|offset| *offset < byte_start);
            let end_index = char_offsets.partition_point(|offset| *offset < byte_end);
            for flag in highlight_flags.iter_mut().take(end_index.min(char_count)).skip(start_index) {
                *flag = true;
            }
            search_start = byte_end;
        }
    }

    let mut segments = Vec::new();
    let mut current = String::new();
    let mut current_highlight = highlight_flags.first().copied().unwrap_or(false);
    for (idx, ch) in text.chars().enumerate() {
        let highlight = highlight_flags.get(idx).copied().unwrap_or(false);
        if idx == 0 {
            current_highlight = highlight;
        } else if highlight != current_highlight {
            let style = if current_highlight {
                highlight_style
            } else {
                normal_style
            };
            segments.push(Span::styled(mem::take(&mut current), style));
            current_highlight = highlight;
        }
        current.push(ch);
    }

    if !current.is_empty() {
        let style = if current_highlight {
            highlight_style
        } else {
            normal_style
        };
        segments.push(Span::styled(current, style));
    }

    if segments.is_empty() {
        segments.push(Span::styled(String::new(), normal_style));
    }

    segments
}

pub fn modal_list_item_lines(
    list: &ModalListState,
    visible_index: usize,
    item_index: usize,
    styles: &ModalRenderStyles,
    content_width: usize,
    inline_editor: Option<&ModalInlineEditor>,
    is_selected: bool,
    shortcut_number: Option<usize>,
) -> Vec<Line<'static>> {
    let item = match list.items.get(item_index) {
        Some(i) => i,
        None => {
            tracing::warn!("modal list item index {item_index} out of bounds");
            return vec![Line::default()];
        }
    };
    if item.is_divider {
        // Untitled dividers span the list content width so option groups
        // (approve vs deny) read as clearly separated sections.
        let divider = if item.title.is_empty() {
            ui::INLINE_BLOCK_HORIZONTAL.repeat(content_width.max(8))
        } else {
            item.title.clone()
        };
        let selection_pad = selection_padding();
        let mut spans = Vec::new();
        if !selection_pad.is_empty() {
            spans.push(Span::raw(selection_pad));
        }
        spans.push(Span::styled(divider, styles.divider));
        return vec![Line::from(spans)];
    }

    let indent = "  ".repeat(item.indent as usize);
    let gutter_width = selection_padding_width();
    let blank_gutter = selection_padding();
    let cursor_indicator = list_cursor(is_selected);

    let cursor_style = if is_selected {
        styles.highlight
    } else {
        styles.selectable
    };
    let mut primary_spans = Vec::new();
    if gutter_width > 0 {
        primary_spans.push(Span::styled(cursor_indicator, cursor_style));
    }

    // Numbered shortcut badge (`1.`–`9.`) for search-less modals, mirroring
    // the digit keys that jump to each option. Non-selectable rows (dividers,
    // separators) carry no number.
    if let Some(number) = shortcut_number {
        primary_spans.push(Span::styled(format!("{number}."), styles.detail));
        primary_spans.push(Span::raw(" "));
    }

    if !indent.is_empty() {
        primary_spans.push(Span::raw(indent.clone()));
    }

    if let Some(badge) = &item.badge {
        let badge_label = format!("[{badge}]");
        primary_spans.push(Span::styled(badge_label, modal_badge_style(badge.as_str(), item.badge_tone, styles)));
        primary_spans.push(Span::raw(" "));
    }

    let title_style = if item.is_hint() {
        styles.detail
    } else if is_selected && item.selection.is_some() {
        styles.highlight
    } else if item.selection.is_some() {
        styles.selectable
    } else if item.is_header() {
        styles.header
    } else {
        styles.detail
    };

    let title_spans = highlight_segments(item.title.as_str(), title_style, styles.search_match, list.highlight_terms());
    primary_spans.extend(title_spans);

    // Live value for setting rows: trailing column after a dimmed separator.
    // Tone follows `badge_tone` (On → success, Off/unset → dimmed, else accent).
    if let Some(value) = &item.value {
        // Pad short titles so values line up as a column when possible.
        let title_width: usize = item.title.chars().count();
        let target = crate::design::constants::VALUE_COL;
        if title_width < target {
            primary_spans.push(Span::raw(" ".repeat(target - title_width)));
        } else {
            primary_spans.push(Span::raw("  "));
        }
        primary_spans.push(Span::styled("·  ", styles.detail));
        let value_style = if is_selected {
            styles.highlight
        } else if item.badge_tone == InlineTone::Neutral {
            styles.detail
        } else {
            tone_style(item.badge_tone, styles)
        };
        primary_spans.extend(highlight_segments(
            value.as_str(),
            value_style,
            styles.search_match,
            list.highlight_terms(),
        ));
    }

    // Shared rhythm: every selectable row keeps one blank separator row
    // after it so dense subtitle lists (settings, model picker, permission
    // groups) stay scannable. Dividers keep a single full-width rule.
    let mut lines = Vec::new();
    if item.is_header() {
        if visible_index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(primary_spans));
        lines.push(Line::default());
    } else {
        lines.push(Line::from(primary_spans));
    }

    if let Some(subtitle) = &item.subtitle {
        let indent_width = item.indent as usize * 2;
        let wrapped_width = content_width.saturating_sub(indent_width).max(1);
        let wrapped_lines = wrap_line_to_width(subtitle.as_str(), wrapped_width);

        for wrapped in wrapped_lines {
            let mut secondary_spans = Vec::new();
            if !blank_gutter.is_empty() {
                secondary_spans.push(Span::raw(blank_gutter.clone()));
            }
            if !indent.is_empty() {
                secondary_spans.push(Span::raw(indent.clone()));
            }
            let subtitle_spans =
                highlight_segments(wrapped.as_str(), styles.detail, styles.search_match, list.highlight_terms());
            secondary_spans.extend(subtitle_spans);
            lines.push(Line::from(secondary_spans));
        }
    }

    if let Some(editor) = inline_editor
        && editor.item_index == item_index
    {
        let mut editor_spans = Vec::new();
        if !blank_gutter.is_empty() {
            editor_spans.push(Span::raw(blank_gutter));
        }
        if !indent.is_empty() {
            editor_spans.push(Span::raw(indent.clone()));
        }

        editor_spans.push(Span::styled(format!("{} ", editor.label), styles.header));
        if editor.text.is_empty() {
            if let Some(placeholder) = editor.placeholder.as_ref() {
                editor_spans.push(Span::styled(placeholder.clone(), styles.detail));
            }
        } else {
            editor_spans.push(Span::styled(editor.text.clone(), styles.selectable));
        }

        if editor.active {
            editor_spans.push(Span::styled("▌", styles.highlight));
        }

        lines.push(Line::from(editor_spans));
    }

    if item.selection.is_some() {
        lines.push(Line::default());
    }
    lines
}

fn tone_style(tone: InlineTone, styles: &ModalRenderStyles) -> Style {
    match tone {
        InlineTone::Neutral => styles.badge,
        InlineTone::Accent => styles.accent,
        InlineTone::Success => styles.success,
        InlineTone::Warning => styles.warning,
        InlineTone::Danger => styles.danger,
        InlineTone::Current => styles.accent.add_modifier(Modifier::BOLD),
    }
}

fn modal_badge_style(badge: &str, tone: InlineTone, styles: &ModalRenderStyles) -> Style {
    if tone != InlineTone::Neutral {
        return tone_style(tone, styles);
    }
    // Fallback for callers that set only a badge label (no tone).
    match badge {
        "Active" | "Action" | "Current" => styles.header.add_modifier(Modifier::BOLD),
        "Read-only" => styles.detail.add_modifier(Modifier::ITALIC),
        _ => styles.badge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ui::tui::InlineListItem;
    use ratatui::{Terminal, backend::TestBackend};

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.clone().into_owned())
            .collect::<String>()
    }

    fn modal_render_styles() -> ModalRenderStyles {
        ModalRenderStyles {
            border: Style::default(),
            highlight: Style::default(),
            badge: Style::default(),
            header: Style::default(),
            selectable: Style::default(),
            detail: Style::default(),
            search_match: Style::default(),
            title: Style::default(),
            divider: Style::default(),
            background: Style::default(),
            instruction_border: Style::default(),
            instruction_title: Style::default(),
            instruction_bullet: Style::default(),
            instruction_body: Style::default(),
            hint: Style::default(),
            success: Style::default(),
            warning: Style::default(),
            danger: Style::default(),
            accent: Style::default(),
        }
    }

    #[test]
    fn modal_instruction_lines_marks_sections_and_highlights_commands() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 6),
            &[
                "Tool: shell — Run command".to_string(),
                "## Command".to_string(),
                "`cargo nextest run`".to_string(),
                "## Context".to_string(),
                "Reason: build check".to_string(),
            ],
            &styles,
        );

        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("COMMAND"));
        assert!(rendered.contains("CONTEXT"));
        assert!(rendered.contains("cargo nextest run"));
        assert!(lines.iter().any(|line| line.spans.len() > 1));
    }

    #[test]
    fn modal_instruction_lines_separate_sections_with_blank_row() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 10),
            &[
                "Tool: exec_command".to_string(),
                "## Command".to_string(),
                "`cargo test`".to_string(),
                "## Why".to_string(),
                "Reason: verify build".to_string(),
            ],
            &styles,
        );

        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        let why_idx = texts.iter().position(|text| text == "WHY").expect("WHY header");
        assert_eq!(texts[why_idx - 1], "", "section header needs a blank separator row");
    }

    #[test]
    fn modal_instruction_command_uses_indent_without_pipe_or_bullet() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 6),
            &[
                "The agent wants to run a shell command and needs your approval.".to_string(),
                "`cargo test`".to_string(),
            ],
            &styles,
        );

        let command_line = lines
            .iter()
            .find(|line| line_text(line).contains("cargo"))
            .expect("command row");
        let text = line_text(command_line);
        assert!(!text.contains('│'), "command row must not use pipe gutter, got: {text}");
        assert!(!text.contains('•'), "command row must not use prose bullet, got: {text}");
    }

    #[test]
    fn modal_instruction_command_keeps_syntax_token_colors() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 6),
            &[
                "The agent wants to run a shell command and needs your approval.".to_string(),
                "`cargo test --locked`".to_string(),
            ],
            &styles,
        );

        let command_line = lines
            .iter()
            .find(|line| line_text(line).contains("cargo"))
            .expect("command row");
        // Gutter + at least command/args/option segments.
        assert!(command_line.spans.len() > 2, "command should be tokenized, got: {command_line:?}");
        let token_styles: std::collections::HashSet<String> = command_line
            .spans
            .iter()
            .skip(1)
            .map(|span| format!("{:?}", span.style))
            .collect();
        assert!(token_styles.len() > 1, "command tokens should keep distinct syntax colors, got: {token_styles:?}");
    }

    #[test]
    fn modal_instruction_command_renders_shell_marker_as_own_span() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(Rect::new(0, 0, 80, 6), &["`$ cargo test --locked`".to_string()], &styles);

        let command_line = lines
            .iter()
            .find(|line| line_text(line).contains("cargo"))
            .expect("command row");
        // Gutter + marker + at least command/option token segments.
        assert!(command_line.spans.len() > 3, "marker must not swallow body tokens, got: {command_line:?}");
        assert_eq!(command_line.spans[1].content.as_ref(), "$ ");
        assert_eq!(line_text(command_line).trim_start(), "$ cargo test --locked");
    }

    #[test]
    fn modal_instruction_shell_marker_applies_only_to_first_code_row() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 8),
            &["`$ echo hi`".to_string(), "`$ echo bye`".to_string()],
            &styles,
        );

        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(texts.len(), 2);
        let first = lines.iter().find(|line| line_text(line).contains("hi")).expect("first row");
        assert_eq!(first.spans[1].content.as_ref(), "$ ");
        // A continuation line that literally starts with `$ ` keeps its
        // tokenizer output instead of gaining a marker span.
        let second = lines.iter().find(|line| line_text(line).contains("bye")).expect("second row");
        assert_ne!(second.spans[1].content.as_ref(), "$ ", "got: {second:?}");
    }

    fn numbered_option(title: &str) -> InlineListItem {
        InlineListItem {
            title: title.to_string(),
            subtitle: None,
            badge: None,
            indent: 0,
            selection: Some(InlineListSelection::SlashCommand(title.to_string())),
            search_value: None,
            ..Default::default()
        }
    }

    fn separator_option() -> InlineListItem {
        InlineListItem {
            title: String::new(),
            subtitle: None,
            badge: None,
            indent: 0,
            selection: None,
            search_value: None,
            ..Default::default()
        }
    }

    #[test]
    fn modal_list_item_numbers_skip_non_selectable_rows() {
        let styles = modal_render_styles();
        let list = ModalListState::new(
            vec![
                numbered_option("Approve once"),
                numbered_option("Allow for session"),
                separator_option(),
                numbered_option("Deny once"),
            ],
            None,
        );

        let first_lines: Vec<String> = (0..4)
            .map(|visible_index| {
                let item_index = list.visible_indices[visible_index];
                let number = list.shortcut_number(visible_index);
                let rows = modal_list_item_lines(&list, visible_index, item_index, &styles, 60, None, false, number);
                line_text(&rows[0])
            })
            .collect();
        assert!(
            first_lines[0].contains("1.") && first_lines[0].contains("Approve once"),
            "got: {:?}",
            first_lines[0]
        );
        assert_eq!(
            line_text(
                &modal_list_item_lines(
                    &list,
                    0,
                    list.visible_indices[0],
                    &styles,
                    60,
                    None,
                    false,
                    list.shortcut_number(0)
                )[0]
            )
            .trim_start(),
            "1. Approve once"
        );
        assert!(
            first_lines[1].contains("2.") && first_lines[1].contains("Allow for session"),
            "got: {:?}",
            first_lines[1]
        );
        assert!(!first_lines[2].contains("3."), "separator must carry no number, got: {:?}", first_lines[2]);
        assert!(first_lines[3].contains("3.") && first_lines[3].contains("Deny once"), "got: {:?}", first_lines[3]);
    }

    #[test]
    fn modal_list_item_numbers_hidden_when_disabled() {
        let styles = modal_render_styles();
        let list = ModalListState::new(vec![numbered_option("Approve once"), numbered_option("Deny once")], None);

        for visible_index in 0..2 {
            let item_index = list.visible_indices[visible_index];
            let rows = modal_list_item_lines(&list, visible_index, item_index, &styles, 60, None, false, None);
            let text = line_text(&rows[0]);
            assert!(!text.contains("1.") && !text.contains("2."), "numbers must stay hidden, got: {text}");
        }
    }

    #[test]
    fn modal_list_item_numbers_hidden_on_crowded_lists() {
        // Ten selectables fail the shared gate: no row may show a number,
        // or digits would advertise shortcuts that routing swallows.
        let styles = modal_render_styles();
        let items: Vec<InlineListItem> = (1..=10).map(|number| numbered_option(&format!("Option {number}"))).collect();
        let list = ModalListState::new(items, None);
        assert!(!list.numbered_shortcuts());

        for visible_index in 0..10 {
            let item_index = list.visible_indices[visible_index];
            let number = list.shortcut_number(visible_index);
            assert_eq!(number, None, "crowded row {visible_index} must have no number");
            let rows = modal_list_item_lines(&list, visible_index, item_index, &styles, 60, None, false, number);
            let text = line_text(&rows[0]);
            assert!(!text.contains("1."), "dead number leaked on crowded row, got: {text}");
        }
    }

    #[test]
    fn narrow_modal_keeps_command_tail_and_omission_evidence() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 32, 16),
            &[
                "Tool: exec_command".to_string(),
                "## Command".to_string(),
                "`python3 -c 'print(1)' … --output ../../critical.txt`".to_string(),
                "… +5 more lines (full command runs on approval)".to_string(),
                "`rm -- target/last-line`".to_string(),
            ],
            &styles,
        );

        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("../../critical.txt"), "trailing destination must remain visible: {rendered}");
        assert!(rendered.contains("+5 more lines"), "omission count must remain visible: {rendered}");
        assert!(rendered.contains("target/last-line"), "multiline tail must remain visible: {rendered}");
    }

    #[test]
    fn modal_instruction_context_row_splits_label_and_value() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 6),
            &[
                "The agent wants to run a shell command and needs your approval.".to_string(),
                "Risk: High".to_string(),
            ],
            &styles,
        );

        let risk_line = lines.iter().find(|line| line_text(line).contains("High")).expect("risk row");
        let text = line_text(risk_line);
        assert!(!text.contains('•'), "context row must not use bullet, got: {text}");
        assert!(risk_line.spans.len() > 1, "label and value should be separate spans");
    }

    #[test]
    fn modal_instruction_environment_row_splits_label_and_value() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 6),
            &["Environment: default policy + extra grants".to_string()],
            &styles,
        );

        let env_line = lines
            .iter()
            .find(|line| line_text(line).contains("extra grants"))
            .expect("env row");
        let text = line_text(env_line);
        assert!(!text.contains('•'), "env row must not use bullet, got: {text}");
        assert!(env_line.spans.len() > 1, "label and value should be separate spans");
        assert!(env_line.spans.iter().any(|span| span.content.as_ref() == "Environment:"));
    }

    #[test]
    fn modal_instruction_permission_labels_render_without_bullets() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 8),
            &[
                "The agent wants to run a shell command and needs your approval.".to_string(),
                "`cargo test`".to_string(),
                "What the agent is trying to do: verify build".to_string(),
                "Requested from: agent-1".to_string(),
            ],
            &styles,
        );

        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        let goal = texts.iter().find(|text| text.contains("verify build")).expect("goal row");
        assert!(!goal.contains('•'), "goal row must not use bullet, got: {goal}");
        let source = texts.iter().find(|text| text.contains("agent-1")).expect("source row");
        assert!(!source.contains('•'), "source row must not use bullet, got: {source}");
        assert!(!texts.iter().any(|text| text.contains('│')), "no pipe gutter expected, got: {texts:?}");
    }

    #[test]
    fn plan_approval_header_renders_without_bullets() {
        let styles = modal_render_styles();
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 80, 10),
            &[
                "A plan is ready to execute. Would you like to proceed?".to_string(),
                "Summary: Fix vtcode analyze so it runs non-interactively".to_string(),
                "1. Add tools::LIST_FILES to AUTO_ALLOW_TOOLS".to_string(),
                "2. Fix step parsing in analyze.rs".to_string(),
                "… and 2 more plan steps".to_string(),
            ],
            &styles,
        );

        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(texts.iter().all(|text| !text.contains('•')), "plan header must not use bullets, got: {texts:?}");
        assert!(texts.iter().any(|text| text.contains("Summary:")), "summary overview must remain");
        assert!(texts.iter().any(|text| text.contains("1. Add")), "numbered steps must remain");
        assert!(texts.iter().any(|text| text.contains("more plan steps")), "overflow evidence must remain");
    }

    #[test]
    fn plan_approval_long_header_wraps_without_bullets_or_elision() {
        let styles = modal_render_styles();
        let summary = "Summary: Fix vtcode analyze so it runs non-interactively with auto-allowed tools, correct step parsing, and bounded verification";
        let step = "1. Make TurnDiffTracker bounded and deterministic: sort paths for stable output ordering";
        let lines = modal_instruction_lines(
            Rect::new(0, 0, 40, 10),
            &[
                "A plan is ready to execute. Would you like to proceed?".to_string(),
                summary.to_string(),
                step.to_string(),
                "… and 2 more plan steps".to_string(),
            ],
            &styles,
        );

        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert!(
            texts.iter().all(|text| !text.contains('•')),
            "wrapped plan header must not use bullets, got: {texts:?}"
        );
        let normalized = texts.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains("non-interactively"), "full summary text must survive wrapping: {normalized}");
        assert!(
            normalized.contains("stable output ordering"),
            "full step text must survive wrapping without elision: {normalized}"
        );
        assert!(
            texts
                .iter()
                .filter(|text| text.trim_end().ends_with('…'))
                .all(|text| text.contains("more plan steps")),
            "only the overflow row may end with an ellipsis, got: {texts:?}"
        );
    }

    fn render_modal_lines(search: ModalSearchState) -> Vec<String> {
        let styles = modal_render_styles();
        let mut list = ModalListState::new(
            vec![InlineListItem {
                title: "Alpha".to_string(),
                subtitle: Some("First item".to_string()),
                badge: Some("OpenAI".to_string()),
                indent: 0,
                selection: Some(InlineListSelection::Model(0)),
                search_value: Some("alpha".to_string()),
                ..Default::default()
            }],
            None,
        );
        let instructions = vec!["Choose a model".to_string()];
        let backend = TestBackend::new(80, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                render_modal_body(
                    frame,
                    Rect::new(0, 0, 80, 8),
                    ModalBodyContext {
                        instructions: &instructions,
                        status: None,
                        footer_hint: None,
                        list: Some(&mut list),
                        styles: &styles,
                        secure_prompt: None,
                        search: Some(&search),
                        input: "",
                        cursor: 0,
                        input_styles: &InputStyles::default(),
                    },
                    None,
                    None,
                    Style::default(),
                    Style::default(),
                );
            })
            .expect("modal render should succeed");

        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn render_markdown_lines_for_modal_wraps_long_questions() {
        let lines = render_markdown_lines_for_modal(
            "What user-visible outcome should this change deliver, and what constraints or non-goals must remain unchanged?",
            40,
            Style::default(),
        );

        assert!(lines.len() > 1, "long question should wrap across lines");
        for line in &lines {
            let text = line_text(line);
            assert!(UnicodeWidthStr::width(text.as_str()) <= 40, "line exceeded modal width: {text}");
        }
    }

    #[test]
    fn render_markdown_lines_for_modal_renders_markdown_headings() {
        let lines = render_markdown_lines_for_modal("### Goal\n- Reduce prompt size", 80, Style::default());

        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("Goal"));
        assert!(!rendered.contains("### Goal"));
        assert!(rendered.contains("Reduce prompt size"));
    }

    #[test]
    fn config_list_summary_uses_navigation_hint_instead_of_density() {
        let list = ModalListState::new(
            vec![InlineListItem {
                title: "Permission default".to_string(),
                subtitle: Some("permissions.default = ask".to_string()),
                badge: Some("Toggle".to_string()),
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("permissions.default:cycle".to_string())),
                search_value: None,
                ..Default::default()
            }],
            None,
        );

        let styles = ModalRenderStyles {
            border: Style::default(),
            highlight: Style::default(),
            badge: Style::default(),
            header: Style::default(),
            selectable: Style::default(),
            detail: Style::default(),
            search_match: Style::default(),
            title: Style::default(),
            divider: Style::default(),
            background: Style::default(),
            instruction_border: Style::default(),
            instruction_title: Style::default(),
            instruction_bullet: Style::default(),
            instruction_body: Style::default(),
            hint: Style::default(),
            success: Style::default(),
            warning: Style::default(),
            danger: Style::default(),
            accent: Style::default(),
        };

        let summary = modal_list_summary_line(&list, &styles, None)
            .into_iter()
            .next()
            .expect("expected summary line for config list");
        let text = line_text(&summary);
        assert!(text.contains("↑↓ select"), "hint: {text}");
        assert!(!text.contains("Alt+D"));
        assert!(!text.contains("Density:"));
    }

    #[test]
    fn config_list_summary_ignores_explicit_footer_hint() {
        // Regression: callers must not pass a footer to a list containing
        // `ConfigAction` items. Such lists are `FixedComfortable` and render
        // the shared navigation hint, so an explicit footer is silently
        // dropped. Pin that behavior so dead footer copy cannot be reintroduced.
        let list = ModalListState::new(
            vec![InlineListItem {
                title: "Permission default".to_string(),
                subtitle: Some("permissions.default = ask".to_string()),
                badge: Some("Toggle".to_string()),
                indent: 0,
                selection: Some(InlineListSelection::ConfigAction("permissions.default:cycle".to_string())),
                search_value: None,
                ..Default::default()
            }],
            None,
        );

        let styles = ModalRenderStyles {
            border: Style::default(),
            highlight: Style::default(),
            badge: Style::default(),
            header: Style::default(),
            selectable: Style::default(),
            detail: Style::default(),
            search_match: Style::default(),
            title: Style::default(),
            divider: Style::default(),
            background: Style::default(),
            instruction_border: Style::default(),
            instruction_title: Style::default(),
            instruction_bullet: Style::default(),
            instruction_body: Style::default(),
            hint: Style::default(),
            success: Style::default(),
            warning: Style::default(),
            danger: Style::default(),
            accent: Style::default(),
        };

        let summary = modal_list_summary_line(&list, &styles, Some("Esc to go back"))
            .into_iter()
            .next()
            .expect("summary line");
        let text = line_text(&summary);
        assert!(text.contains("↑↓ select"), "config lists render the shared navigation hint: {text}");
        assert!(!text.contains("Esc to go back"), "explicit footer must be dropped for config lists: {text}");
    }

    #[test]
    fn non_config_list_summary_omits_density_hint() {
        let list = ModalListState::new(
            vec![InlineListItem {
                title: "gpt-5".to_string(),
                subtitle: Some("General reasoning".to_string()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::Model(0)),
                search_value: Some("gpt-5".to_string()),
                ..Default::default()
            }],
            None,
        );

        let styles = ModalRenderStyles {
            border: Style::default(),
            highlight: Style::default(),
            badge: Style::default(),
            header: Style::default(),
            selectable: Style::default(),
            detail: Style::default(),
            search_match: Style::default(),
            title: Style::default(),
            divider: Style::default(),
            background: Style::default(),
            instruction_border: Style::default(),
            instruction_title: Style::default(),
            instruction_bullet: Style::default(),
            instruction_body: Style::default(),
            hint: Style::default(),
            success: Style::default(),
            warning: Style::default(),
            danger: Style::default(),
            accent: Style::default(),
        };

        let summary = modal_list_summary_line(&list, &styles, None);
        assert!(summary.is_empty(), "density summary should be hidden");
    }

    #[test]
    fn modal_text_area_alignment_reserves_selection_gutter() {
        let area = Rect::new(10, 3, 20, 4);
        let aligned = modal_text_area_aligned_with_list(area);
        let gutter = selection_padding_width() as u16;

        assert_eq!(aligned.x, area.x + gutter);
        assert_eq!(aligned.width, area.width - gutter);
        assert_eq!(aligned.y, area.y);
        assert_eq!(aligned.height, area.height);
    }

    #[test]
    fn modal_text_area_alignment_keeps_narrow_areas_unchanged() {
        let gutter = selection_padding_width() as u16;
        let area = Rect::new(2, 1, gutter, 2);
        let aligned = modal_text_area_aligned_with_list(area);
        assert_eq!(aligned, area);
    }

    #[test]
    fn modal_search_field_renders_placeholder_inside_brackets() {
        let lines = render_modal_lines(ModalSearchState {
            label: "Search models".to_string(),
            placeholder: Some("provider, name, id".to_string()),
            query: String::new(),
            fuzzy: false,
        });

        let has_title = lines.iter().any(|line| line.contains("Search models"));
        assert!(has_title, "search title should render");
        let has_placeholder = lines.iter().any(|line| line.contains("provider, name, id"));
        assert!(has_placeholder, "search placeholder should render");
    }

    #[test]
    fn modal_search_field_renders_query_above_list() {
        let lines = render_modal_lines(ModalSearchState {
            label: "Search models".to_string(),
            placeholder: Some("provider, name, id".to_string()),
            query: "openrouter".to_string(),
            fuzzy: false,
        });

        let search_index = lines
            .iter()
            .position(|line| line.contains("openrouter"))
            .expect("search query should render");
        let item_index = lines
            .iter()
            .position(|line| line.contains("Alpha"))
            .expect("list item should render");

        assert!(search_index < item_index);
    }

    #[test]
    fn modal_search_field_without_label_omits_title_row() {
        // An empty label collapses the search field to a single row (prompt +
        // placeholder), removing the redundant "Search …" title that restates
        // the placeholder.
        let lines = render_modal_lines(ModalSearchState {
            label: String::new(),
            placeholder: Some("provider, name, id".to_string()),
            query: String::new(),
            fuzzy: false,
        });

        let placeholder_row = lines
            .iter()
            .position(|line| line.contains("provider, name, id"))
            .expect("placeholder should render on the single search row");
        // The prompt indicator marks the search row.
        assert!(lines[placeholder_row].contains('>'), "search row should render the prompt indicator");
    }

    #[test]
    fn filtered_modal_summary_shows_matches_without_repeating_query() {
        let list = ModalListState::new(
            vec![InlineListItem {
                title: "gpt-5".to_string(),
                subtitle: Some("General reasoning".to_string()),
                badge: None,
                indent: 0,
                selection: Some(InlineListSelection::Model(0)),
                search_value: Some("gpt-5".to_string()),
                ..Default::default()
            }],
            None,
        );
        let styles = modal_render_styles();
        let mut list = list;
        list.apply_search("gpt", false);

        let summary = modal_list_summary_line(&list, &styles, None)
            .into_iter()
            .next()
            .expect("summary should exist");
        let text = line_text(&summary);

        assert!(text.contains("1 / 1"), "quiet match counter: {text}");
        assert!(!text.contains("gpt"));
        assert!(!text.contains("Filter:"));
    }

    #[test]
    fn instruction_highlight_markup_strips_bold_markers() {
        let styles = modal_render_styles();
        let mut list = ModalListState::new(Vec::new(), None);
        let instructions = vec!["Header".to_string(), "**ABCD-EFGH**".to_string()];
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|frame| {
                render_modal_body(
                    frame,
                    Rect::new(0, 0, 40, 8),
                    ModalBodyContext {
                        instructions: &instructions,
                        status: None,
                        footer_hint: None,
                        list: Some(&mut list),
                        styles: &styles,
                        secure_prompt: None,
                        search: None,
                        input: "",
                        cursor: 0,
                        input_styles: &InputStyles::default(),
                    },
                    None,
                    None,
                    Style::default(),
                    Style::default(),
                );
            })
            .expect("modal render should succeed");

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("ABCD-EFGH"));
        assert!(!rendered.contains("**ABCD-EFGH**"));
    }

    #[test]
    fn setting_row_renders_accent_value_and_dimmed_subtitle() {
        let styles = modal_render_styles();
        let list = ModalListState::new(
            vec![InlineListItem {
                title: "Fullscreen copy".to_string(),
                value: Some("On".to_string()),
                subtitle: Some("Copy selection to clipboard".to_string()),
                badge: Some("On".to_string()),
                selection: Some(InlineListSelection::ConfigAction("settings:set:x:toggle".to_string())),
                badge_tone: InlineTone::Success,
                kind: InlineItemKind::Setting,
                ..Default::default()
            }],
            None,
        );
        let lines = modal_list_item_lines(&list, 0, 0, &styles, 60, None, false, None);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(text.contains("Fullscreen copy"), "title rendered: {text}");
        assert!(text.contains("On"), "value rendered: {text}");
        assert!(text.contains("Copy selection to clipboard"), "subtitle rendered: {text}");
    }

    #[test]
    fn status_strip_uses_tone_and_sits_above_hint() {
        let styles = modal_render_styles();
        let mut list = ModalListState::new(
            vec![InlineListItem {
                title: "Item".to_string(),
                selection: Some(InlineListSelection::ConfigAction("x".to_string())),
                ..Default::default()
            }],
            None,
        );
        let status = InlineStatus::success("Enabled IDE context");
        let area = Rect::new(0, 0, 60, 6);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).expect("terminal");
        terminal
            .draw(|frame| {
                render_modal_list(frame, area, &mut list, &styles, Some("Esc close"), None, false, Some(&status));
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Enabled IDE context"), "status visible: {rendered}");
        assert!(rendered.contains("•"), "status bullet: {rendered}");
    }

    #[test]
    fn badge_tone_maps_current_to_accent_bold() {
        let styles = modal_render_styles();
        let current = modal_badge_style("Current", InlineTone::Current, &styles);
        let danger = modal_badge_style("Destructive", InlineTone::Danger, &styles);
        assert_ne!(current, danger, "tones must be distinguishable");
        assert_eq!(current, styles.accent.add_modifier(Modifier::BOLD));
        assert_eq!(danger, styles.danger);
    }

    #[test]
    fn group_headers_add_spacing_above_and_below() {
        let styles = modal_render_styles();
        let list = ModalListState::new(
            vec![
                InlineListItem::group_header("Anthropic"),
                InlineListItem {
                    title: "Claude".to_string(),
                    subtitle: Some("desc".to_string()),
                    selection: Some(InlineListSelection::Model(0)),
                    ..Default::default()
                },
                InlineListItem::group_header("OpenAI"),
                InlineListItem {
                    title: "GPT".to_string(),
                    subtitle: Some("desc".to_string()),
                    selection: Some(InlineListSelection::Model(1)),
                    ..Default::default()
                },
            ],
            None,
        );
        // First header: no leading blank, but trailing blank before items.
        let h0 = modal_list_item_lines(&list, 0, 0, &styles, 60, None, false, None);
        assert_eq!(h0.len(), 2, "first header is title + trailing gap: {h0:?}");
        // Later header: blank above and below.
        let h1 = modal_list_item_lines(&list, 2, 2, &styles, 60, None, false, None);
        assert_eq!(h1.len(), 3, "later header is gap + title + gap: {h1:?}");
        // Item rows are title + subtitle + blank separator gap.
        let item = modal_list_item_lines(&list, 1, 1, &styles, 60, None, false, None);
        assert_eq!(item.len(), 3, "title + subtitle + gap: {item:?}");
        assert!(item.last().is_some_and(|line| line.spans.is_empty()), "last row is the blank gap: {item:?}");
    }

    #[test]
    fn all_subtitle_lists_keep_comfortable_spacing_between_items() {
        // Shared rhythm across modal subclasses: config rows and plain model
        // rows alike end with a blank separator gap.
        let styles = modal_render_styles();
        let list = ModalListState::new(
            vec![
                InlineListItem {
                    title: "Temperature".to_string(),
                    value: Some("0.7".to_string()),
                    subtitle: Some("Sampling temperature (0.0 precise to 1.0 creative).".to_string()),
                    selection: Some(InlineListSelection::ConfigAction(
                        "settings:set:agent.temperature:inc".to_string(),
                    )),
                    ..Default::default()
                },
                InlineListItem {
                    title: "Claude".to_string(),
                    subtitle: Some("desc".to_string()),
                    selection: Some(InlineListSelection::Model(0)),
                    ..Default::default()
                },
            ],
            None,
        );
        for (visible_index, item_index) in [0usize, 1usize].into_iter().enumerate() {
            let item = modal_list_item_lines(&list, visible_index, item_index, &styles, 60, None, false, None);
            assert_eq!(item.len(), 3, "title + subtitle + gap: {item:?}");
            assert!(item.last().is_some_and(|line| line.spans.is_empty()), "last row is the blank gap: {item:?}");
        }
    }
}
