use std::collections::{HashMap, HashSet};

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use ratatui_cheese::input::{Input, InputState};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{Session, ToolOutputBlock};
use crate::tui::config::constants::ui;
use crate::tui::core_tui::session::action::Action;
use crate::tui::core_tui::session::list_panel::input_styles_from_theme;
use crate::tui::core_tui::session::text_utils::strip_ansi_codes;
use crate::tui::core_tui::style::{ratatui_color_from_ansi, ratatui_style_from_inline};
use crate::tui::core_tui::types::InlineMessageKind;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TranscriptRenderMode {
    #[default]
    Rich,
    Raw,
}

impl TranscriptRenderMode {
    fn label(self) -> &'static str {
        match self {
            Self::Rich => "rich",
            Self::Raw => "raw",
        }
    }

    fn toggle(self) -> Self {
        match self {
            Self::Rich => Self::Raw,
            Self::Raw => Self::Rich,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ToolOutputSearchState {
    active: bool,
    pending_query: String,
    query: String,
    matches: Vec<usize>,
    current_match: Option<usize>,
    restore_scroll_top: usize,
    restore_query: String,
    restore_match: Option<usize>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ReviewBlockKey {
    Core(usize),
    Tool(u64),
    OrphanTool(usize),
}

impl Default for ReviewBlockKey {
    fn default() -> Self {
        Self::Core(0)
    }
}

#[derive(Clone, Copy, Debug)]
enum ReviewSourceKind {
    Core(usize),
    Tool(usize),
}

#[derive(Clone, Copy, Debug)]
struct ReviewSource {
    key: ReviewBlockKey,
    revision: u64,
    kind: ReviewSourceKind,
}

#[derive(Clone, Debug, Default)]
struct CachedToolOutputBlock {
    key: ReviewBlockKey,
    revision: u64,
    /// ANSI-free lines used for search, copying, editor handoff, and raw export.
    lines: Vec<String>,
    /// Width-aware styled lines used by the default rich review mode.
    rich_lines: Vec<Line<'static>>,
    lowered_lines: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReviewRevision {
    transcript: u64,
    tool_output: u64,
    presentation: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ToolOutputViewerState {
    width: u16,
    height: u16,
    source_revision: ReviewRevision,
    messages: Vec<CachedToolOutputBlock>,
    row_offsets: Vec<usize>,
    total_lines: usize,
    cached_export_text: Option<String>,
    scroll_top: usize,
    search: ToolOutputSearchState,
    mode: TranscriptRenderMode,
    focus_target: Option<u64>,
    viewer_area: Rect,
    content_area: Rect,
    title_mode_hit_region: Option<Rect>,
    hovered_mode_control: bool,
    title_close_hit_region: Option<Rect>,
    hovered_close_control: bool,
}

impl ToolOutputViewerState {
    pub(crate) fn open(session: &Session, width: u16, height: u16) -> Self {
        Self::open_focused(session, width, height, None)
    }

    pub(crate) fn open_focused(session: &Session, width: u16, height: u16, focus_target: Option<u64>) -> Self {
        let mut state = Self { focus_target, ..Self::default() };
        state.refresh(session, width, height);
        if focus_target.is_none() {
            state.scroll_to_bottom(height);
        }
        state
    }

    /// Tool capture ids currently held in this open viewer. Used to pin those
    /// blocks against FIFO eviction so an open review does not lose content.
    ///
    /// Anchored captures key as `Tool(id)`; unanchored ones key as
    /// `OrphanTool(index)` but still carry `block.id` in `revision`.
    pub(crate) fn retained_tool_ids(&self) -> HashSet<u64> {
        self.messages
            .iter()
            .filter_map(|message| match message.key {
                ReviewBlockKey::Tool(_) | ReviewBlockKey::OrphanTool(_) => Some(message.revision),
                ReviewBlockKey::Core(_) => None,
            })
            .collect()
    }

    pub(crate) fn refresh(&mut self, session: &Session, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        let revision = ReviewRevision {
            transcript: session.core.current_transcript_revision(),
            tool_output: session.tool_output_revision,
            presentation: session.core.transcript_presentation_revision,
        };
        if self.width == width && self.height == height && self.source_revision == revision {
            self.focus_pending_target(height);
            self.clamp_scroll(height);
            return;
        }

        let was_at_bottom = self.is_at_bottom(self.height);
        let width_changed = self.width != width;
        let presentation_changed = self.source_revision.presentation != revision.presentation;
        self.refresh_messages(session, width, width_changed || presentation_changed);
        self.width = width;
        self.height = height;
        self.source_revision = revision;
        self.recompute_matches();

        let focused = self.focus_pending_target(height);
        if focused {
            return;
        }
        if was_at_bottom {
            self.scroll_to_bottom(height);
        } else {
            self.clamp_scroll(height);
        }
    }

    pub(crate) fn toggle_render_mode(&mut self) {
        self.mode = self.mode.toggle();
        for message in &mut self.messages {
            // Search rows are derived from the active render mode. Invalidate
            // the lowercase cache when rich wrapping and raw lines switch.
            message.lowered_lines = None;
        }
        self.update_row_offsets();
        self.recompute_matches();
        self.clamp_scroll(self.height);
    }

    fn focus_pending_target(&mut self, height: u16) -> bool {
        let Some(target) = self.focus_target else {
            return false;
        };
        let Some(message_index) = self
            .messages
            .iter()
            .position(|message| message.key == ReviewBlockKey::Tool(target))
        else {
            return false;
        };
        self.scroll_top = self.row_offsets.get(message_index).copied().unwrap_or_default();
        self.scroll_top = self.scroll_top.min(self.max_scroll(height));
        self.focus_target = None;
        true
    }

    pub(crate) fn set_viewer_area(&mut self, area: Rect) {
        self.viewer_area = area;
    }

    pub(crate) fn viewer_contains(&self, column: u16, row: u16) -> bool {
        (self.viewer_area.width == 0 || self.viewer_area.height == 0)
            || self.viewer_area.contains(Position { x: column, y: row })
    }

    pub(crate) fn body_contains(&self, column: u16, row: u16) -> bool {
        (self.content_area.width == 0 || self.content_area.height == 0)
            || self.content_area.contains(Position { x: column, y: row })
    }

    pub(crate) fn content_height_or(&self, fallback: u16) -> u16 {
        if self.content_area.height == 0 {
            fallback.max(1)
        } else {
            self.content_area.height
        }
    }

    pub(crate) fn mode_control_contains(&self, column: u16, row: u16) -> bool {
        self.title_mode_hit_region
            .is_some_and(|area| area.contains(Position { x: column, y: row }))
    }

    pub(crate) fn close_control_contains(&self, column: u16, row: u16) -> bool {
        self.title_close_hit_region
            .is_some_and(|area| area.contains(Position { x: column, y: row }))
    }

    pub(crate) fn update_mode_hover(&mut self, column: u16, row: u16) -> bool {
        let hovered = self.mode_control_contains(column, row);
        if self.hovered_mode_control == hovered {
            return false;
        }
        self.hovered_mode_control = hovered;
        true
    }

    pub(crate) fn update_close_hover(&mut self, column: u16, row: u16) -> bool {
        let hovered = self.close_control_contains(column, row);
        if self.hovered_close_control == hovered {
            return false;
        }
        self.hovered_close_control = hovered;
        true
    }

    #[cfg(test)]
    pub(crate) fn render_mode(&self) -> TranscriptRenderMode {
        self.mode
    }

    fn line_count(&self) -> usize {
        self.total_lines.max(1)
    }

    pub(crate) fn export_text(&mut self) -> String {
        if let Some(text) = &self.cached_export_text {
            return text.clone();
        }

        let mut export = String::new();
        let mut wrote_line = false;
        for message in &self.messages {
            for line in &message.lines {
                if wrote_line {
                    export.push('\n');
                }
                export.push_str(line);
                wrote_line = true;
            }
        }

        self.cached_export_text = Some(export.clone());
        export
    }

    fn visible_lines(&self, height: usize) -> Vec<Line<'static>> {
        let height = height.max(1);
        let end = self.scroll_top.saturating_add(height).min(self.total_lines);
        let current_match_line = self.current_match_line();
        let mut visible = Vec::with_capacity(height);

        for row in self.scroll_top..end {
            let mut line = self.line_for_mode_at(row).unwrap_or_default();
            if current_match_line == Some(row) {
                let style = line.style.add_modifier(Modifier::REVERSED);
                line = line.style(style);
            }
            visible.push(line);
        }

        while visible.len() < height {
            visible.push(Line::default());
        }

        visible
    }

    pub(crate) fn scroll_line_up(&mut self, height: u16) {
        self.scroll_top = self.scroll_top.saturating_sub(1);
        self.clamp_scroll(height);
    }

    pub(crate) fn scroll_line_down(&mut self, height: u16) {
        self.scroll_top = self.scroll_top.saturating_add(1).min(self.max_scroll(height));
    }

    pub(crate) fn scroll_half_page_up(&mut self, height: u16) {
        self.scroll_top = self.scroll_top.saturating_sub(Self::page_step(height).max(1) / 2);
        self.clamp_scroll(height);
    }

    pub(crate) fn scroll_half_page_down(&mut self, height: u16) {
        self.scroll_top = self
            .scroll_top
            .saturating_add(Self::page_step(height).max(1) / 2)
            .min(self.max_scroll(height));
    }

    pub(crate) fn scroll_full_page_up(&mut self, height: u16) {
        self.scroll_top = self.scroll_top.saturating_sub(Self::page_step(height));
        self.clamp_scroll(height);
    }

    pub(crate) fn scroll_full_page_down(&mut self, height: u16) {
        self.scroll_top = self
            .scroll_top
            .saturating_add(Self::page_step(height))
            .min(self.max_scroll(height));
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
    }

    pub(crate) fn scroll_to_bottom(&mut self, height: u16) {
        self.scroll_top = self.max_scroll(height);
    }

    pub(crate) fn start_search(&mut self) {
        if self.search.active {
            return;
        }
        self.search.active = true;
        self.search.pending_query = self.search.query.clone();
        self.search.restore_scroll_top = self.scroll_top;
        self.search.restore_query = self.search.query.clone();
        self.search.restore_match = self.search.current_match;
    }

    pub(crate) fn search_active(&self) -> bool {
        self.search.active
    }

    fn search_query(&self) -> &str {
        if self.search.active {
            &self.search.pending_query
        } else {
            &self.search.query
        }
    }

    pub(crate) fn insert_search_text(&mut self, text: &str) {
        self.search.pending_query.push_str(text);
    }

    pub(crate) fn backspace_search(&mut self) {
        self.search.pending_query.pop();
    }

    pub(crate) fn cancel_search(&mut self) {
        self.search.active = false;
        self.scroll_top = self.search.restore_scroll_top;
        self.search.query = self.search.restore_query.clone();
        self.search.current_match = self.search.restore_match;
        self.search.pending_query.clear();
        self.recompute_matches();
    }

    pub(crate) fn commit_search(&mut self, height: u16) {
        self.search.active = false;
        self.search.query = std::mem::take(&mut self.search.pending_query);
        self.recompute_matches();
        if !self.search.matches.is_empty() {
            self.search.current_match = Some(0);
            self.jump_to_current_match(height);
        } else {
            self.search.current_match = None;
        }
    }

    pub(crate) fn jump_next_match(&mut self, height: u16) {
        if self.search.matches.is_empty() {
            return;
        }
        let next = match self.search.current_match {
            Some(current) => (current + 1) % self.search.matches.len(),
            None => 0,
        };
        self.search.current_match = Some(next);
        self.jump_to_current_match(height);
    }

    pub(crate) fn jump_previous_match(&mut self, height: u16) {
        if self.search.matches.is_empty() {
            return;
        }
        let next = match self.search.current_match {
            Some(0) | None => self.search.matches.len().saturating_sub(1),
            Some(current) => current.saturating_sub(1),
        };
        self.search.current_match = Some(next);
        self.jump_to_current_match(height);
    }

    pub(crate) fn status_label(&self) -> String {
        let total = self.line_count();
        let line = (self.scroll_top + 1).min(total);
        let match_status = if self.search.query.is_empty() {
            "search off".to_string()
        } else if self.search.matches.is_empty() {
            format!("search '{}' (0 matches)", self.search.query)
        } else {
            let current = self.search.current_match.unwrap_or(0) + 1;
            format!("search '{}' ({}/{})", self.search.query, current, self.search.matches.len())
        };
        format!("line {line}/{total} • {} • {match_status}", self.mode.label())
    }

    fn refresh_messages(&mut self, session: &Session, width: u16, width_changed: bool) {
        let previous = self
            .messages
            .drain(..)
            .map(|message| (message.key, message))
            .collect::<HashMap<_, _>>();
        let mut previous = previous;
        let sources = collect_review_sources(session);
        let mut messages = Vec::with_capacity(sources.len());

        for source in sources {
            let cached = previous
                .remove(&source.key)
                .filter(|message| !width_changed && message.revision == source.revision);
            messages.push(cached.unwrap_or_else(|| build_cached_block(session, source, width)));
        }

        self.messages = messages;
        self.cached_export_text = None;
        self.update_row_offsets();
    }

    fn update_row_offsets(&mut self) {
        self.row_offsets.clear();
        self.row_offsets.reserve(self.messages.len());

        let mut current_offset = 0;
        for message in &self.messages {
            self.row_offsets.push(current_offset);
            current_offset += self.message_line_count(message);
        }

        self.total_lines = current_offset;
    }

    fn message_line_count(&self, message: &CachedToolOutputBlock) -> usize {
        let count = match self.mode {
            TranscriptRenderMode::Rich => message.rich_lines.len(),
            TranscriptRenderMode::Raw => message.lines.len(),
        };
        count.max(1)
    }

    fn message_index_at(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total_lines || self.row_offsets.is_empty() {
            return None;
        }

        let message_index = self.row_offsets.partition_point(|offset| *offset <= row).saturating_sub(1);
        let local_index = row.saturating_sub(self.row_offsets[message_index]);
        Some((message_index, local_index))
    }

    fn line_text_at(&self, row: usize) -> Option<&str> {
        let (message_index, local_index) = self.message_index_at(row)?;
        self.messages.get(message_index)?.lines.get(local_index).map(String::as_str)
    }

    fn line_for_mode_at(&self, row: usize) -> Option<Line<'static>> {
        let (message_index, local_index) = self.message_index_at(row)?;
        let message = self.messages.get(message_index)?;
        match self.mode {
            TranscriptRenderMode::Rich => message.rich_lines.get(local_index).cloned(),
            TranscriptRenderMode::Raw => message.lines.get(local_index).map(|line| Line::raw(line.clone())),
        }
    }

    fn current_match_line(&self) -> Option<usize> {
        self.search
            .current_match
            .and_then(|index| self.search.matches.get(index).copied())
    }

    fn jump_to_current_match(&mut self, height: u16) {
        let Some(line) = self.current_match_line() else {
            return;
        };
        self.scroll_top = line.min(self.max_scroll(height));
    }

    fn recompute_matches(&mut self) {
        self.search.matches.clear();
        if self.search.query.is_empty() {
            self.search.current_match = None;
            return;
        }

        let needle = self.search.query.to_ascii_lowercase();
        let mode = self.mode;
        let mut row_index = 0usize;
        for message in &mut self.messages {
            let lowered_lines = message.lowered_lines.get_or_insert_with(|| match mode {
                TranscriptRenderMode::Rich => message
                    .rich_lines
                    .iter()
                    .map(line_text)
                    .map(|line| line.to_ascii_lowercase())
                    .collect(),
                TranscriptRenderMode::Raw => message.lines.iter().map(|line| line.to_ascii_lowercase()).collect(),
            });
            for line in lowered_lines {
                if line.contains(&needle) {
                    self.search.matches.push(row_index);
                }
                row_index += 1;
            }
        }

        if let Some(current) = self.search.current_match
            && current < self.search.matches.len()
        {
            return;
        }

        self.search.current_match = (!self.search.matches.is_empty()).then_some(0);
    }

    fn clamp_scroll(&mut self, height: u16) {
        self.scroll_top = self.scroll_top.min(self.max_scroll(height));
    }

    fn max_scroll(&self, height: u16) -> usize {
        self.total_lines.saturating_sub(usize::from(height.max(1)))
    }

    fn is_at_bottom(&self, height: u16) -> bool {
        self.scroll_top >= self.max_scroll(height)
    }

    fn page_step(height: u16) -> usize {
        usize::from(height.max(2)).saturating_sub(1)
    }
}

/// Shared tail of the compact-activity hint, rendered after the review
/// keybinding in both plain-text and segmented form.
const COMPACT_ACTIVITY_HINT_TAIL: &str = "transcript · click to expand";

pub(super) fn compact_activity_hint_text(session: &Session) -> Option<String> {
    if !session.core.transcript_review_hints_visible() {
        return None;
    }
    session
        .core
        .primary_binding_label(Action::OpenTranscriptReview)
        .map(|binding| format!("{binding} {COMPACT_ACTIVITY_HINT_TAIL}"))
}

pub(super) fn compact_activity_segments(
    session: &Session,
    metadata: &vtcode_commons::ui_protocol::CompactActivityMetadata,
) -> Vec<crate::tui::core_tui::types::InlineSegment> {
    let styles = crate::tui::ui::shell_syntax::ShellLineStyles::from_session(session);
    let mut segments = crate::tui::ui::shell_syntax::line_to_compact_segments(metadata, &styles);

    if session.core.transcript_review_hints_visible()
        && let Some(binding) = session.core.primary_binding_label(Action::OpenTranscriptReview)
    {
        let separator_style = session.core.styles.default_inline_style().dim();
        segments.push(crate::tui::core_tui::types::InlineSegment {
            text: " · ".to_string(),
            style: std::sync::Arc::new(separator_style),
        });
        let binding_style = session.core.styles.accent_inline_style().underline().bold();
        segments.push(crate::tui::core_tui::types::InlineSegment {
            text: binding.to_string(),
            style: std::sync::Arc::new(binding_style),
        });
        // Keep the explanatory words dimmed but underline the click affordance
        // itself so it reads as clickable. Hit regions are derived from
        // underlined spans, so both the binding and this action open review.
        let rest_style = session.core.styles.default_inline_style().dim();
        segments.push(crate::tui::core_tui::types::InlineSegment {
            text: " transcript · ".to_string(),
            style: std::sync::Arc::new(rest_style),
        });
        let action_style = session.core.styles.accent_inline_style().underline();
        segments.push(crate::tui::core_tui::types::InlineSegment {
            text: "click to expand".to_string(),
            style: std::sync::Arc::new(action_style),
        });
    }

    segments
}

fn collect_review_sources(session: &Session) -> Vec<ReviewSource> {
    let core_lines = session
        .core
        .lines
        .iter()
        .enumerate()
        .map(|(index, _)| rendered_message_text(session, index))
        .collect::<Vec<_>>();
    let review_revisions = session.core.review_message_revisions();
    let mut anchored_blocks = HashMap::<usize, Vec<usize>>::new();
    let mut positioned_orphans = Vec::<(usize, usize)>::new();

    for (block_index, block) in session.tool_output_blocks.iter().enumerate() {
        let Some(anchor) = block.anchor_line else {
            if let Some(recorded_at_line) = block.recorded_at_line {
                positioned_orphans.push((recorded_at_line, block_index));
            }
            continue;
        };
        // The app session sets this line from a per-call identity marker (or
        // from the live PTY header). Never reverse-match the rendered command
        // text here: identical commands are valid consecutive calls, and rich
        // wrapping can legitimately change their visible text.
        anchored_blocks.entry(anchor).or_default().push(block_index);
    }
    positioned_orphans.sort_unstable();

    let mut used_blocks = HashSet::new();
    let mut sources = Vec::with_capacity(core_lines.len() + session.tool_output_blocks.len());
    let mut index = 0usize;
    let mut next_positioned_orphan = 0usize;
    while index < core_lines.len() {
        while let Some(&(recorded_at_line, block_index)) = positioned_orphans.get(next_positioned_orphan)
            && recorded_at_line <= index
        {
            let block = &session.tool_output_blocks[block_index];
            sources.push(ReviewSource {
                key: ReviewBlockKey::OrphanTool(block_index),
                revision: block.id,
                kind: ReviewSourceKind::Tool(block_index),
            });
            used_blocks.insert(block_index);
            next_positioned_orphan += 1;
        }

        if let Some(block_indices) = anchored_blocks.get(&index) {
            for &block_index in block_indices {
                let block = &session.tool_output_blocks[block_index];
                sources.push(ReviewSource {
                    key: ReviewBlockKey::Tool(block.id),
                    revision: block.id,
                    kind: ReviewSourceKind::Tool(block_index),
                });
                used_blocks.insert(block_index);
            }
            index = tool_output_body_end(session, &core_lines, index, &anchored_blocks);
        } else {
            sources.push(ReviewSource {
                key: ReviewBlockKey::Core(index),
                revision: review_revisions[index],
                kind: ReviewSourceKind::Core(index),
            });
            index += 1;
        }
    }

    for &(_, block_index) in positioned_orphans.iter().skip(next_positioned_orphan) {
        let block = &session.tool_output_blocks[block_index];
        sources.push(ReviewSource {
            key: ReviewBlockKey::OrphanTool(block_index),
            revision: block.id,
            kind: ReviewSourceKind::Tool(block_index),
        });
        used_blocks.insert(block_index);
    }

    for (block_index, block) in session.tool_output_blocks.iter().enumerate() {
        if used_blocks.contains(&block_index) {
            continue;
        }
        sources.push(ReviewSource {
            key: ReviewBlockKey::OrphanTool(block_index),
            revision: block.id,
            kind: ReviewSourceKind::Tool(block_index),
        });
    }

    sources
}

fn rendered_message_text(session: &Session, index: usize) -> String {
    let Some(line) = session.core.lines.get(index) else {
        return String::new();
    };
    if let Some(activity) = session.compact_activity_for_line(index) {
        return activity.display_text();
    }
    session
        .core
        .render_message_spans_for_line(line)
        .into_iter()
        .map(|span| strip_ansi_codes(span.content.as_ref()).into_owned())
        .collect()
}

fn tool_output_body_end(
    session: &Session,
    core_lines: &[String],
    anchor: usize,
    anchored_blocks: &HashMap<usize, Vec<usize>>,
) -> usize {
    let Some(anchor_line) = session.core.lines.get(anchor) else {
        return anchor.saturating_add(1);
    };
    let anchor_kind = anchor_line.kind;
    let mut end = anchor.saturating_add(1);
    while let Some(line) = session.core.lines.get(end) {
        // A following PTY/Tool line may be the next command's live output,
        // not detail belonging to this summary. Identity anchors are the
        // unambiguous boundary; text and message kind alone are not.
        if anchored_blocks.contains_key(&end) {
            break;
        }
        let text = core_lines.get(end).map(String::as_str).unwrap_or_default();
        let is_detail = line.kind == InlineMessageKind::Info && (text.starts_with("  ") || text.starts_with("    "));
        let belongs_to_tool = match anchor_kind {
            InlineMessageKind::Pty => line.kind == InlineMessageKind::Pty,
            InlineMessageKind::Tool => matches!(line.kind, InlineMessageKind::Tool | InlineMessageKind::Pty),
            InlineMessageKind::Info => {
                is_detail || matches!(line.kind, InlineMessageKind::Tool | InlineMessageKind::Pty)
            }
            _ => false,
        };
        if !belongs_to_tool {
            break;
        }
        end += 1;
    }
    end
}

fn build_cached_block(session: &Session, source: ReviewSource, width: u16) -> CachedToolOutputBlock {
    match source.kind {
        ReviewSourceKind::Core(index) => {
            if let Some(activity) = session.compact_activity_for_line(index) {
                let lines = wrap_output_line(&activity.display_text(), usize::from(width.max(1)));
                let style = ratatui_style_from_inline(
                    &session.core.styles.accent_inline_style().bold(),
                    session.core.theme.foreground,
                );
                let rich_lines = lines.iter().map(|line| Line::styled(line.clone(), style)).collect::<Vec<_>>();
                return CachedToolOutputBlock {
                    key: source.key,
                    revision: source.revision,
                    lines,
                    rich_lines,
                    lowered_lines: None,
                };
            }
            let mut rich_lines = session
                .core
                .reflow_message_lines_for_review(index, width)
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>();
            if rich_lines.is_empty() {
                rich_lines.push(Line::default());
            }
            let lines = rich_lines.iter().map(line_text).collect::<Vec<_>>();
            CachedToolOutputBlock {
                key: source.key,
                revision: source.revision,
                lines,
                rich_lines,
                lowered_lines: None,
            }
        }
        ReviewSourceKind::Tool(index) => {
            let block = &session.tool_output_blocks[index];
            let lines = collect_tool_output_lines(block, width);
            let rich_lines = lines
                .iter()
                .map(|line| Line::styled(line.clone(), tool_output_line_style(session, line)))
                .collect();
            CachedToolOutputBlock {
                key: source.key,
                revision: source.revision,
                lines,
                rich_lines,
                lowered_lines: None,
            }
        }
    }
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| strip_ansi_codes(span.content.as_ref()).into_owned())
        .collect()
}

fn tool_output_line_style(session: &Session, line: &str) -> Style {
    let trimmed = line.trim_start();
    if trimmed.starts_with("• ") {
        return session.core.styles.accent_style().add_modifier(Modifier::BOLD);
    }

    let lowercase = trimmed.to_ascii_lowercase();
    let kind = if lowercase.contains("run error") || lowercase.contains("exit code") {
        InlineMessageKind::Error
    } else if lowercase.contains("warning") {
        InlineMessageKind::Warning
    } else {
        InlineMessageKind::Pty
    };
    let mut style = session.core.styles.default_style();
    if let Some(color) = session.core.text_fallback(kind) {
        style = style.fg(ratatui_color_from_ansi(color));
    }
    if kind == InlineMessageKind::Pty {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn collect_tool_output_lines(block: &ToolOutputBlock, width: u16) -> Vec<String> {
    let max_width = usize::from(width.max(1));
    let mut lines = block
        .lines
        .iter()
        .flat_map(|line| wrap_output_line(strip_ansi_codes(line).as_ref(), max_width))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn wrap_output_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut wrapped = Vec::new();
    let mut current = String::new();
    let mut current_width: usize = 0;
    for ch in line.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !current.is_empty() && current_width.saturating_add(char_width) > width {
            wrapped.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(char_width);
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
    wrapped
}

pub(crate) fn render_tool_output_viewer(
    session: &Session,
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut ToolOutputViewerState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title_prefix = " Transcript Review ";
    let status = format!(" {}", state.status_label());
    let mode_label = format!(" [{}] ", state.mode.label());
    let mode_x = area
        .x
        .saturating_add(1)
        .saturating_add(UnicodeWidthStr::width(title_prefix) as u16)
        .saturating_add(UnicodeWidthStr::width(status.as_str()) as u16);
    let mode_width = UnicodeWidthStr::width(mode_label.as_str()) as u16;
    state.title_mode_hit_region = (mode_width > 0 && mode_x < area.right())
        .then(|| Rect::new(mode_x, area.y, mode_width.min(area.right().saturating_sub(mode_x)), 1));
    let close_label = if session.core.transcript_review_close_button_visible() {
        " [close] "
    } else {
        ""
    };
    let close_x = mode_x.saturating_add(mode_width);
    let close_width = UnicodeWidthStr::width(close_label) as u16;
    state.title_close_hit_region = (close_width > 0 && close_x < area.right())
        .then(|| Rect::new(close_x, area.y, close_width.min(area.right().saturating_sub(close_x)), 1));
    state.set_viewer_area(area);

    let mode_style = session.core.header_secondary_style().add_modifier(Modifier::BOLD).add_modifier(
        if state.hovered_mode_control {
            Modifier::REVERSED | Modifier::UNDERLINED
        } else {
            Modifier::empty()
        },
    );
    let close_style = session.core.header_secondary_style().add_modifier(Modifier::BOLD).add_modifier(
        if state.hovered_close_control {
            Modifier::REVERSED | Modifier::UNDERLINED
        } else {
            Modifier::empty()
        },
    );
    let title = Line::from(vec![
        Span::styled(title_prefix, session.core.section_title_style().add_modifier(Modifier::BOLD)),
        Span::styled(status, session.core.header_secondary_style()),
        Span::styled(mode_label, mode_style),
        Span::styled(close_label, close_style),
    ]);
    let block = Block::default().borders(Borders::ALL).title(title);
    frame.render_widget(Clear, area);
    frame.render_widget(block.clone(), area);
    let inner = block.inner(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let show_search = state.search_active();
    let show_footer =
        session.core.transcript_review_shortcut_guide_visible() && inner.height >= if show_search { 3 } else { 2 };
    let mut constraints = vec![Constraint::Min(1)];
    if show_search {
        constraints.push(Constraint::Length(2));
    }
    if show_footer {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::vertical(constraints).split(inner);
    let content_height = chunks[0].height;
    state.content_area = chunks[0];
    let lines = state.visible_lines(usize::from(content_height));
    frame.render_widget(Paragraph::new(lines).style(session.core.styles.default_style()), chunks[0]);

    if show_search && chunks.len() > 1 {
        let input_styles = input_styles_from_theme(&session.core.theme);
        let input_widget = Input::new("Search")
            .placeholder("type to search...")
            .prompt("/")
            .styles(input_styles);

        let mut input_state = InputState::new();
        let query = state.search_query().to_string();
        input_state.set_value(query.clone());
        input_state.set_focused(true);
        for _ in 0..query.chars().count() {
            input_state.move_right();
        }

        frame.render_stateful_widget(&input_widget, chunks[1], &mut input_state);
    }

    if show_footer {
        if let Some(hint) = transcript_review_shortcut_hint(session) {
            let footer_index = chunks.len().saturating_sub(1);
            frame.render_widget(
                Paragraph::new(Line::styled(hint, session.core.styles.default_style().dim())),
                chunks[footer_index],
            );
        }
    }
}

fn transcript_review_shortcut_hint(session: &Session) -> Option<String> {
    if !session.core.transcript_review_shortcut_guide_visible() {
        return None;
    }

    let mut hints = Vec::with_capacity(5);
    if let Some(binding) = session.core.primary_binding_label(Action::OpenTranscriptReview) {
        hints.push(format!("{binding} open/close"));
    }
    if let Some(binding) = session.core.primary_binding_label(Action::ToggleTranscriptRenderMode) {
        hints.push(format!("{binding} rich/raw"));
    }
    hints.extend(["q/Esc close", "/ search", "↑/↓ scroll"].map(str::to_string));
    Some(hints.join(" · "))
}

pub(crate) fn viewer_content_width(area: Rect) -> u16 {
    area.width.saturating_sub(2).min(ui::TUI_MAX_VIEWPORT_WIDTH)
}

pub(crate) fn viewer_content_height(session: &Session, state: &ToolOutputViewerState, area: Rect) -> u16 {
    let inner_height = area.height.saturating_sub(2);
    let show_search = state.search_active();
    let show_footer =
        session.core.transcript_review_shortcut_guide_visible() && inner_height >= if show_search { 3 } else { 2 };
    let reserved_rows = usize::from(show_search) * 2 + usize::from(show_footer);
    inner_height.saturating_sub(reserved_rows as u16).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::core_tui::app::session::AppSession;
    use crate::tui::core_tui::app::types::InlineCommand;
    use crate::tui::core_tui::session::config::AppearanceConfig;
    use crate::tui::core_tui::types::{InlineSegment, InlineTextStyle, InlineTheme};
    use std::sync::Arc;

    fn test_session() -> AppSession {
        AppSession::new(InlineTheme::default(), None, 24)
    }

    #[test]
    fn theme_change_rebuilds_unchanged_capture_styles() {
        let mut session = test_session();
        add_block(&mut session, &["captured output"]);
        let mut viewer = ToolOutputViewerState::open(&session, 40, 10);
        let before = viewer.messages[0].rich_lines.clone();
        session.handle_command(InlineCommand::SetTheme {
            theme: InlineTheme {
                foreground: Some(anstyle::Color::Rgb(anstyle::RgbColor(12, 34, 56))),
                ..Default::default()
            },
        });
        viewer.refresh(&session, 40, 10);
        let fresh = ToolOutputViewerState::open(&session, 40, 10);
        assert_ne!(before, viewer.messages[0].rich_lines);
        assert_eq!(viewer.messages[0].rich_lines, fresh.messages[0].rich_lines);
    }

    #[test]
    fn shrinking_viewer_keeps_bottom_follow() {
        let mut session = test_session();
        for _ in 0..20 {
            add_block(&mut session, &["output"]);
        }
        let mut viewer = ToolOutputViewerState::open(&session, 40, 10);
        assert!(viewer.is_at_bottom(10));
        viewer.refresh(&session, 40, 5);
        assert!(viewer.is_at_bottom(5));
    }

    fn text_segment(text: impl Into<String>) -> InlineSegment {
        InlineSegment {
            text: text.into(),
            style: Arc::new(InlineTextStyle::default()),
        }
    }

    fn add_block(session: &mut AppSession, lines: &[&str]) {
        session.tool_output_blocks.push(ToolOutputBlock {
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
            ..Default::default()
        });
        session.tool_output_revision += 1;
    }

    #[test]
    fn unanchored_capture_stays_at_its_recorded_transcript_position() {
        let mut session = test_session();
        session.core.push_line(InlineMessageKind::Agent, vec![text_segment("before")]);
        session.handle_command(InlineCommand::RecordToolOutput { id: 91, lines: vec!["captured output".to_string()] });
        session.core.push_line(InlineMessageKind::Agent, vec![text_segment("after")]);

        let mut viewer = ToolOutputViewerState::open(&session, 60, 8);
        let export = viewer.export_text();
        assert!(export.find("before").unwrap() < export.find("captured output").unwrap());
        assert!(export.find("captured output").unwrap() < export.find("after").unwrap());
    }

    #[test]
    fn refresh_appends_without_rebuilding_unchanged_blocks() {
        let mut session = test_session();
        add_block(&mut session, &["• Ran first", "  └ alpha"]);
        add_block(&mut session, &["• Ran second", "  └ beta"]);

        let mut viewer = ToolOutputViewerState::open(&session, 40, 10);
        let original_first = viewer.messages[0].revision;

        add_block(&mut session, &["• Ran third", "  └ gamma"]);
        viewer.refresh(&session, 40, 10);

        assert_eq!(viewer.messages[0].revision, original_first);
        assert_eq!(viewer.messages.len(), 3);
        assert!(viewer.export_text().contains("gamma"));
    }

    #[test]
    fn refresh_rebuilds_grouped_info_head_after_detail_append() {
        let mut session = test_session();
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Info,
            segments: vec![text_segment("first diagnostic detail")],
        });
        let mut viewer = ToolOutputViewerState::open(&session, 60, 10);
        assert!(
            viewer.messages[0]
                .lines
                .iter()
                .any(|line| line.contains("first diagnostic detail"))
        );

        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Info,
            segments: vec![text_segment("second diagnostic detail")],
        });
        viewer.refresh(&session, 60, 10);

        assert!(
            viewer.messages[0]
                .lines
                .iter()
                .any(|line| line.contains("second diagnostic detail"))
        );
        assert!(
            viewer.messages[0]
                .rich_lines
                .iter()
                .map(line_text)
                .any(|line| line.contains("second diagnostic detail"))
        );
    }

    #[test]
    fn grouped_review_revision_aggregates_only_the_group_head() {
        let mut session = test_session();
        for detail in ["first detail", "second detail", "third detail"] {
            session.handle_command(InlineCommand::AppendLine {
                kind: InlineMessageKind::Info,
                segments: vec![text_segment(detail)],
            });
        }

        let sources = collect_review_sources(&session);
        let revisions = sources
            .into_iter()
            .filter_map(|source| match source.kind {
                ReviewSourceKind::Core(index) if index < 3 => Some(source.revision),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(revisions, vec![3, 2, 3]);
    }

    #[test]
    fn refresh_reflows_blocks_when_width_changes() {
        let mut session = test_session();
        add_block(&mut session, &["• Ran a command with a long output line"]);

        let mut viewer = ToolOutputViewerState::open(&session, 80, 10);
        let wide_lines = viewer.messages[0].lines.len();
        viewer.refresh(&session, 12, 10);

        assert!(viewer.messages[0].lines.len() > wide_lines);
    }

    #[test]
    fn search_uses_cached_lowercase_lines() {
        let mut session = test_session();
        add_block(&mut session, &["• Ran Alpha"]);
        add_block(&mut session, &["  └ beta alpha"]);

        let mut viewer = ToolOutputViewerState::open(&session, 40, 10);
        viewer.search.query = "alpha".to_string();
        viewer.recompute_matches();
        let lowered = viewer.messages[0].lowered_lines.as_ref().expect("lowered lines cached")[0].clone();

        viewer.jump_next_match(10);
        viewer.recompute_matches();

        assert!(lowered.contains("alpha"));
        assert_eq!(viewer.search.matches, vec![0, 1]);
    }

    #[test]
    fn export_text_is_cached_until_a_new_block_arrives() {
        let mut session = test_session();
        add_block(&mut session, &["• Ran alpha"]);

        let mut viewer = ToolOutputViewerState::open(&session, 40, 10);
        let exported = viewer.export_text();
        assert!(exported.contains("alpha"));
        assert_eq!(viewer.cached_export_text.as_deref(), Some(exported.as_str()));

        add_block(&mut session, &["• Ran beta"]);
        viewer.refresh(&session, 40, 10);

        assert_eq!(viewer.cached_export_text, None);
        let refreshed = viewer.export_text();
        assert!(refreshed.contains("alpha"));
        assert!(refreshed.contains("beta"));
    }

    #[test]
    fn viewer_keeps_complete_output_for_each_tool_call() {
        let mut session = test_session();
        add_block(
            &mut session,
            &[
                "• Ran cargo check",
                "  └ first complete line",
                "    final complete line",
            ],
        );
        add_block(&mut session, &["• Ran cargo fmt", "  └ fmt complete"]);

        let viewer = ToolOutputViewerState::open(&session, 80, 10);
        let export = viewer.clone().export_text();

        assert!(export.contains("first complete line"));
        assert!(export.contains("final complete line"));
        assert!(export.contains("• Ran cargo fmt"));
        assert!(!export.contains("Ran 2 commands"));
    }

    #[test]
    fn raw_export_strips_ansi_from_complete_captures() {
        let mut session = test_session();
        add_block(&mut session, &["\u{1b}[31m• Ran colored\u{1b}[0m", "\u{1b}[32mcomplete\u{1b}[0m"]);

        let mut viewer = ToolOutputViewerState::open(&session, 80, 10);
        let export = viewer.export_text();

        assert_eq!(export, "• Ran colored\ncomplete");
        assert!(!export.contains('\u{1b}'));
    }

    #[test]
    fn whole_conversation_export_preserves_order_and_complete_tool_output() {
        let mut session = test_session();
        session.handle_command(InlineCommand::AppendPastedMessage {
            kind: InlineMessageKind::User,
            text: "user request".to_string(),
            line_count: 1,
        });
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![text_segment("assistant before tool")],
        });
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Policy,
            segments: vec![text_segment("reasoning")],
        });
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 0,
            lines: vec![
                "• Ran cargo check".to_string(),
                "  └ complete stdout".to_string(),
                "    complete stderr".to_string(),
            ],
        });
        session.handle_command(InlineCommand::AppendToolOutputLine {
            id: 0,
            kind: InlineMessageKind::Info,
            segments: vec![text_segment("• Ran cargo check")],
        });
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Warning,
            segments: vec![text_segment("warning after tool")],
        });
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Error,
            segments: vec![text_segment("error after tool")],
        });
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![text_segment("assistant after tool")],
        });

        let mut viewer = ToolOutputViewerState::open(&session, 100, 20);
        let export = viewer.export_text();
        let ordered = [
            "user request",
            "assistant before tool",
            "reasoning",
            "• Ran cargo check",
            "complete stdout",
            "complete stderr",
            "warning after tool",
            "error after tool",
            "assistant after tool",
        ];
        let positions = ordered
            .iter()
            .map(|needle| export.find(needle).expect("conversation entry in export"))
            .collect::<Vec<_>>();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(
            viewer
                .messages
                .iter()
                .filter(|message| message.key == ReviewBlockKey::Tool(0))
                .count(),
            1
        );

        let rich_mode = viewer.render_mode();
        viewer.toggle_render_mode();
        assert_eq!(rich_mode, TranscriptRenderMode::Rich);
        assert_eq!(viewer.render_mode(), TranscriptRenderMode::Raw);
        assert_eq!(viewer.export_text(), export);
    }

    #[test]
    fn whole_review_stops_before_following_anchored_pty_call() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 1,
            lines: vec!["• Ran first".to_string(), "  └ first output".to_string()],
        });
        session.handle_command(InlineCommand::AppendCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 1,
                command_count: 1,
                command: Some("first".to_string().into()),
                hidden_line_count: 1,
                suffix: None,
                review_anchor: Some(1),
                review_anchors: vec![1],
            },
        ));
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Pty,
            segments: vec![text_segment("• Ran second")],
        });
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 2,
            lines: vec!["• Ran second".to_string(), "  └ second output".to_string()],
        });

        let mut viewer = ToolOutputViewerState::open(&session, 100, 20);
        let export = viewer.export_text();
        assert!(export.find("first output").unwrap() < export.find("second output").unwrap());
        assert_eq!(export.matches("• Ran second").count(), 1);
    }

    #[test]
    fn whole_conversation_export_keeps_follow_up_guidance() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 7,
            lines: vec![
                "• Ran cargo check".to_string(),
                "  └ complete output".to_string(),
                "    Review the result before continuing.".to_string(),
            ],
        });
        session.handle_command(InlineCommand::AppendToolOutputLine {
            id: 7,
            kind: InlineMessageKind::Info,
            segments: vec![text_segment("• Ran cargo check")],
        });

        let mut viewer = ToolOutputViewerState::open(&session, 100, 20);
        assert!(viewer.export_text().contains("Review the result before continuing."));
    }

    #[test]
    fn active_transcript_updates_refresh_without_reopening() {
        let mut session = test_session();
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![text_segment("initial response")],
        });
        let mut viewer = ToolOutputViewerState::open(&session, 80, 10);
        assert!(viewer.export_text().contains("initial response"));

        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![text_segment("streamed continuation")],
        });
        viewer.refresh(&session, 80, 10);
        let export = viewer.export_text();
        assert!(export.contains("streamed continuation"));
    }

    #[test]
    fn focused_review_jumps_to_the_requested_capture_in_a_group() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 11,
            lines: vec!["• Ran first".to_string(), "  └ first output".to_string()],
        });
        session.handle_command(InlineCommand::AppendCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 1,
                command_count: 1,
                command: Some("first".to_string().into()),
                hidden_line_count: 1,
                suffix: None,
                review_anchor: Some(11),
                review_anchors: vec![11],
            },
        ));
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 12,
            lines: vec!["• Ran second".to_string(), "  └ second output".to_string()],
        });
        session.handle_command(InlineCommand::ReplaceCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 1,
                command_count: 2,
                command: None,
                hidden_line_count: 2,
                suffix: None,
                review_anchor: Some(11),
                review_anchors: vec![11, 12],
            },
        ));

        let viewer = ToolOutputViewerState::open_focused(&session, 80, 2, Some(12));

        assert_eq!(
            viewer.messages.iter().map(|message| message.key).collect::<Vec<_>>(),
            vec![ReviewBlockKey::Tool(11), ReviewBlockKey::Tool(12)]
        );
        assert_eq!(viewer.scroll_top, 2);
        assert_eq!(viewer.focus_target, None);
    }

    #[test]
    fn compact_activity_hint_uses_the_primary_review_binding() {
        let session = test_session();
        let hint = compact_activity_hint_text(&session).expect("default review binding should have a hint");
        assert_eq!(hint, "Ctrl+T transcript · click to expand");
    }

    #[test]
    fn compact_activity_hint_underlines_click_affordance() {
        let session = test_session();
        let metadata = vtcode_commons::ui_protocol::CompactActivityMetadata {
            group_id: 7,
            command_count: 2,
            command: None,
            hidden_line_count: 0,
            suffix: None,
            review_anchor: Some(7),
            review_anchors: vec![7],
        };
        let segments = compact_activity_segments(&session, &metadata);
        let text: String = segments.iter().map(|segment| segment.text.as_str()).collect();
        assert!(text.contains("2 commands"));
        assert!(text.contains("click to expand"));
        let action = segments
            .iter()
            .find(|segment| segment.text == "click to expand")
            .expect("click affordance");
        assert!(action.style.effects.contains(anstyle::Effects::UNDERLINE));
    }

    #[test]
    fn compact_activity_hint_refreshes_when_review_binding_changes() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 12,
            lines: vec!["• Ran printf hint".to_string(), "  └ output".to_string()],
        });
        session.handle_command(InlineCommand::AppendCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 12,
                command_count: 1,
                command: Some("printf hint".to_string().into()),
                hidden_line_count: 1,
                suffix: None,
                review_anchor: Some(12),
                review_anchors: vec![12],
            },
        ));

        let initial = session.core.lines[0]
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(initial.contains("Ctrl+T"));

        let mut bindings = hashbrown::HashMap::new();
        bindings.insert("open_transcript_review".to_string(), vec!["ctrl+x".to_string()]);
        session.handle_command(InlineCommand::SetKeyBindings { bindings });

        let refreshed = session.core.lines[0]
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert!(refreshed.contains("Ctrl+X"));
        assert!(!refreshed.contains("Ctrl+T"));
    }

    #[test]
    fn transcript_review_controls_follow_appearance_configuration() {
        let appearance = AppearanceConfig {
            show_transcript_review_hints: false,
            show_transcript_review_shortcut_guide: false,
            show_transcript_review_close_button: false,
            ..AppearanceConfig::default()
        };
        let session = AppSession::new_with_logs(
            InlineTheme::default(),
            None,
            24,
            true,
            Some(appearance),
            Vec::new(),
            "Agent TUI".to_string(),
        );

        assert!(compact_activity_hint_text(&session).is_none());
        assert!(transcript_review_shortcut_hint(&session).is_none());
        assert!(!session.core.transcript_review_close_button_visible());
        let metadata = vtcode_commons::ui_protocol::CompactActivityMetadata {
            group_id: 1,
            command_count: 1,
            command: Some("printf configured".to_string().into()),
            hidden_line_count: 1,
            suffix: None,
            review_anchor: Some(1),
            review_anchors: vec![1],
        };
        let segs = compact_activity_segments(&session, &metadata);
        // Single-command now tokenized: •, Ran, command tokens + hidden count.
        assert!(segs.len() > 1);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("printf configured"));
        assert!(!text.contains("Ctrl+T"));
    }

    #[test]
    fn compact_activity_hint_updates_when_appearance_is_reloaded() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 48,
            lines: vec!["• Ran printf reload".to_string(), "  └ complete output".to_string()],
        });
        session.handle_command(InlineCommand::AppendCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 48,
                command_count: 1,
                command: Some("printf reload".to_string().into()),
                hidden_line_count: 1,
                suffix: None,
                review_anchor: Some(48),
                review_anchors: vec![48],
            },
        ));
        // With shell syntax: •, Ran, command tokens, hidden count, plus 3 hint segments (separator, binding, rest).
        assert!(session.core.lines[0].segments.len() > 3);
        let hint_text: String = session.core.lines[0].segments.iter().map(|s| s.text.as_str()).collect();
        assert!(hint_text.contains("Ctrl+T"));

        let mut appearance = session.core.appearance.clone();
        appearance.show_transcript_review_hints = false;
        session.handle_command(InlineCommand::SetAppearance { appearance });

        // Without hint, still tokenized command segments remain.
        assert!(session.core.lines[0].segments.len() > 1);
        let plain_text: String = session.core.lines[0].segments.iter().map(|s| s.text.as_str()).collect();
        assert!(!plain_text.contains("click to expand"));
    }

    #[test]
    fn repeated_command_captures_keep_their_identity_and_order() {
        let mut session = test_session();
        for (id, output) in [(1, "first capture"), (2, "second capture")] {
            session.handle_command(InlineCommand::RecordToolOutput {
                id,
                lines: vec![format!("capture block {id}"), format!("  └ {output}")],
            });
            session.handle_command(InlineCommand::AppendToolOutputLine {
                id,
                kind: InlineMessageKind::Info,
                segments: vec![text_segment("• Ran cargo check")],
            });
        }

        let viewer = ToolOutputViewerState::open(&session, 100, 20);
        let export = viewer.clone().export_text();
        assert!(
            export.find("first capture").expect("first capture")
                < export.find("second capture").expect("second capture")
        );
    }

    #[test]
    fn unanchored_failed_capture_stays_before_following_compact_activity() {
        let mut session = test_session();
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 21,
            lines: vec![
                "• Ran failed command".to_string(),
                "    failed: command exited with status 1".to_string(),
            ],
        });
        session.handle_command(InlineCommand::RecordToolOutput {
            id: 22,
            lines: vec!["• Ran successful command".to_string(), "  └ success output".to_string()],
        });
        session.handle_command(InlineCommand::AppendCompactActivity(
            vtcode_commons::ui_protocol::CompactActivityMetadata {
                group_id: 22,
                command_count: 1,
                command: Some("successful command".to_string().into()),
                hidden_line_count: 1,
                suffix: None,
                review_anchor: Some(22),
                review_anchors: vec![22],
            },
        ));

        let mut viewer = ToolOutputViewerState::open(&session, 100, 20);
        let export = viewer.export_text();
        assert!(
            export.find("failed: command exited").expect("failed capture")
                < export.find("success output").expect("successful capture")
        );
    }

    #[test]
    fn wrapping_preserves_blank_output_lines() {
        assert_eq!(wrap_output_line("", 20), vec![String::new()]);
        assert_eq!(wrap_output_line("abcdef", 3), vec!["abc", "def"]);
    }
}
