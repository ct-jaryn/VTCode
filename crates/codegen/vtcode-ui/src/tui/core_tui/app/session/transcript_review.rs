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
    /// Evidence target for each wrapped tool row, shared across its continuations.
    evidence_links: Vec<Option<std::sync::Arc<str>>>,
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
    /// Uses the same body rectangle, wrapping, and scroll position as rendering.
    pub(crate) fn evidence_at(&self, column: u16, row: u16) -> Option<&str> {
        if !self.body_contains(column, row) || self.content_area.height == 0 {
            return None;
        }
        let logical_row = self
            .scroll_top
            .saturating_add(usize::from(row.saturating_sub(self.content_area.y)));
        let (message, local_row) = self.block_at(logical_row)?;
        message.evidence_links.get(local_row)?.as_deref()
    }

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
        self.recompute_matches_after_refresh();

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
        rect_contains_or_empty(self.viewer_area, column, row)
    }

    pub(crate) fn body_contains(&self, column: u16, row: u16) -> bool {
        rect_contains_or_empty(self.content_area, column, row)
    }

    pub(crate) fn content_height_or(&self, fallback: u16) -> u16 {
        if self.content_area.height == 0 {
            fallback.max(1)
        } else {
            self.content_area.height
        }
    }

    pub(crate) fn mode_control_contains(&self, column: u16, row: u16) -> bool {
        opt_rect_contains(self.title_mode_hit_region, column, row)
    }

    pub(crate) fn close_control_contains(&self, column: u16, row: u16) -> bool {
        opt_rect_contains(self.title_close_hit_region, column, row)
    }

    pub(crate) fn update_mode_hover(&mut self, column: u16, row: u16) -> bool {
        let hovered = self.mode_control_contains(column, row);
        set_hover(&mut self.hovered_mode_control, hovered)
    }

    pub(crate) fn update_close_hover(&mut self, column: u16, row: u16) -> bool {
        let hovered = self.close_control_contains(column, row);
        set_hover(&mut self.hovered_close_control, hovered)
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
        self.scroll_by(-1, height);
    }

    pub(crate) fn scroll_line_down(&mut self, height: u16) {
        self.scroll_by(1, height);
    }

    pub(crate) fn scroll_half_page_up(&mut self, height: u16) {
        let half = (Self::page_step(height).max(1) / 2) as isize;
        self.scroll_by(-half, height);
    }

    pub(crate) fn scroll_half_page_down(&mut self, height: u16) {
        let half = (Self::page_step(height).max(1) / 2) as isize;
        self.scroll_by(half, height);
    }

    pub(crate) fn scroll_full_page_up(&mut self, height: u16) {
        self.scroll_by(-(Self::page_step(height) as isize), height);
    }

    pub(crate) fn scroll_full_page_down(&mut self, height: u16) {
        self.scroll_by(Self::page_step(height) as isize, height);
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
        // No rescan: typing only touched `pending_query`, so the committed
        // query is unchanged and `matches` is already current (streaming
        // refreshes kept it updated incrementally). The next `refresh`
        // reconciles any transcript change that landed after the last frame.
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
        if let Some(next) = next_match_index(self.search.current_match, self.search.matches.len(), true) {
            self.search.current_match = Some(next);
            self.jump_to_current_match(height);
        }
    }

    pub(crate) fn jump_previous_match(&mut self, height: u16) {
        if let Some(next) = next_match_index(self.search.current_match, self.search.matches.len(), false) {
            self.search.current_match = Some(next);
            self.jump_to_current_match(height);
        }
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
        let mut previous = HashMap::with_capacity(self.messages.len());
        for message in self.messages.drain(..) {
            previous.insert(message.key, message);
        }
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

    fn block_at(&self, row: usize) -> Option<(&CachedToolOutputBlock, usize)> {
        if row >= self.total_lines || self.row_offsets.is_empty() {
            return None;
        }

        let message_index = self.row_offsets.partition_point(|offset| *offset <= row).saturating_sub(1);
        let local_index = row.saturating_sub(self.row_offsets[message_index]);
        self.messages.get(message_index).map(|message| (message, local_index))
    }

    fn line_for_mode_at(&self, row: usize) -> Option<Line<'static>> {
        let (message, local_index) = self.block_at(row)?;
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

    /// Incremental rescan after a transcript refresh. The committed query is
    /// unchanged on this path (only `commit_search` changes it, via a full
    /// rescan), so prefix matches stay valid: blocks reused from the previous
    /// frame keep their cached lowercase lines, and only the suffix from the
    /// first rebuilt block onward needs re-scanning. This keeps streaming
    /// appends O(tail) instead of O(transcript) while search is active.
    fn recompute_matches_after_refresh(&mut self) {
        if self.search.query.is_empty() {
            self.search.matches.clear();
            self.search.current_match = None;
            return;
        }

        let first_changed = self.messages.iter().position(|message| message.lowered_lines.is_none());
        let Some(first_changed) = first_changed else {
            // Nothing rebuilt (e.g. revision bump with identical sources).
            return;
        };

        let base_row = self.row_offsets.get(first_changed).copied().unwrap_or(0);
        // `matches` is built in row order, so a sorted truncate keeps the prefix.
        let keep = self.search.matches.partition_point(|&row| row < base_row);
        self.search.matches.truncate(keep);

        let needle = self.search.query.to_ascii_lowercase();
        let mode = self.mode;
        let mut row_index = base_row;
        for message in self.messages.iter_mut().skip(first_changed) {
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

    fn scroll_by(&mut self, delta: isize, height: u16) {
        if delta < 0 {
            self.scroll_top = self.scroll_top.saturating_sub(delta.unsigned_abs());
            self.clamp_scroll(height);
        } else {
            self.scroll_top = self.scroll_top.saturating_add(delta as usize).min(self.max_scroll(height));
        }
    }

    fn page_step(height: u16) -> usize {
        usize::from(height.max(2)).saturating_sub(1)
    }
}

/// Shared tail of the compact-activity hint, rendered after the review
/// keybinding in both plain-text and segmented form.
const COMPACT_ACTIVITY_HINT_TAIL: &str = "transcript · click to expand";

fn rect_contains_or_empty(area: Rect, column: u16, row: u16) -> bool {
    (area.width == 0 || area.height == 0) || area.contains(Position { x: column, y: row })
}

fn opt_rect_contains(region: Option<Rect>, column: u16, row: u16) -> bool {
    region.is_some_and(|area| area.contains(Position { x: column, y: row }))
}

fn set_hover(current: &mut bool, hovered: bool) -> bool {
    if *current == hovered {
        return false;
    }
    *current = hovered;
    true
}

fn next_match_index(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if forward {
        Some(match current {
            Some(index) => (index + 1) % len,
            None => 0,
        })
    } else {
        Some(match current {
            Some(0) | None => len.saturating_sub(1),
            Some(index) => index.saturating_sub(1),
        })
    }
}

fn review_hint_binding(session: &Session) -> Option<&str> {
    if !session.core.transcript_review_hints_visible() {
        return None;
    }
    session.core.primary_binding_label(Action::OpenTranscriptReview)
}

pub(super) fn compact_activity_hint_text(session: &Session) -> Option<String> {
    review_hint_binding(session).map(|binding| format!("{binding} {COMPACT_ACTIVITY_HINT_TAIL}"))
}

pub(super) fn compact_activity_segments(
    session: &Session,
    metadata: &vtcode_commons::ui_protocol::CompactActivityMetadata,
) -> Vec<crate::tui::core_tui::types::InlineSegment> {
    let styles = crate::tui::ui::shell_syntax::ShellLineStyles::from_session(session);
    let mut segments = crate::tui::ui::shell_syntax::line_to_compact_segments(metadata, &styles);

    if let Some(binding) = review_hint_binding(session) {
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
    let core_len = session.core.lines.len();
    let review_revisions = session.core.review_message_revisions();
    let mut anchored_blocks = HashMap::<usize, Vec<usize>>::with_capacity(session.tool_output_blocks.len());
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

    let mut used_blocks = HashSet::with_capacity(session.tool_output_blocks.len());
    let mut sources = Vec::with_capacity(core_len + session.tool_output_blocks.len());
    let mut index = 0usize;
    let mut next_positioned_orphan = 0usize;
    while index < core_len {
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
            index = tool_output_body_end(session, index, &anchored_blocks);
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

fn tool_output_body_end(session: &Session, anchor: usize, anchored_blocks: &HashMap<usize, Vec<usize>>) -> usize {
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
        let belongs_to_tool = match anchor_kind {
            InlineMessageKind::Pty => line.kind == InlineMessageKind::Pty,
            InlineMessageKind::Tool => matches!(line.kind, InlineMessageKind::Tool | InlineMessageKind::Pty),
            InlineMessageKind::Info => {
                if !matches!(line.kind, InlineMessageKind::Tool | InlineMessageKind::Pty) {
                    // Detail text is only needed for Info-followed-by-Info;
                    // render lazily so a full-transcript pass stays O(n)
                    // without per-line ANSI stripping.
                    if line.kind != InlineMessageKind::Info {
                        break;
                    }
                    let text = rendered_message_text(session, end);
                    if !(text.starts_with("  ") || text.starts_with("    ")) {
                        break;
                    }
                }
                true
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
                    evidence_links: Vec::new(),
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
                evidence_links: Vec::new(),
                lowered_lines: None,
            }
        }
        ReviewSourceKind::Tool(index) => {
            let block = &session.tool_output_blocks[index];
            let rows = collect_tool_output_rows(block, width);
            let lines = rows.lines;
            let rich_lines = lines
                .iter()
                .map(|line| Line::styled(line.clone(), tool_output_line_style(session, line)))
                .collect();
            CachedToolOutputBlock {
                key: source.key,
                revision: source.revision,
                lines,
                rich_lines,
                evidence_links: rows.evidence_links,
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

struct ToolOutputRows {
    lines: Vec<String>,
    evidence_links: Vec<Option<std::sync::Arc<str>>>,
}

fn evidence_target(line: &str) -> Option<std::sync::Arc<str>> {
    let (_, suffix) = line.split_once("[evidence](vtcode-evidence:")?;
    let (reference, _) = suffix.split_once(')')?;
    let mut parts = reference.split(':');
    let session = parts.next()?;
    if session.is_empty()
        || session.len() > 256
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    parts.next()?.parse::<u64>().ok()?;
    let digest = parts.next()?;
    if parts.next().is_some() || digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("vtcode-evidence:{reference}").into())
}

fn collect_tool_output_rows(block: &ToolOutputBlock, width: u16) -> ToolOutputRows {
    let max_width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut evidence_links = Vec::new();
    for line in &block.lines {
        let clean = strip_ansi_codes(line);
        let target = evidence_target(&clean);
        let wrapped = wrap_output_line(&clean, max_width);
        evidence_links.extend(std::iter::repeat_n(target, wrapped.len()));
        lines.extend(wrapped);
    }
    if lines.is_empty() {
        lines.push(String::new());
        evidence_links.push(None);
    }
    ToolOutputRows { lines, evidence_links }
}

fn wrap_output_line(line: &str, width: usize) -> Vec<String> {
    // Plain-text hard wrap for ANSI-free review/export lines. Kept separate
    // from `text_utils::wrap_line` (styled, word-boundary wrapping) on purpose:
    // unifying them would change wrapping behavior for tool output.
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
    let mut constraints = Vec::with_capacity(3);
    constraints.push(Constraint::Min(1));
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
        let cursor_steps = query.chars().count();
        input_state.set_value(query);
        input_state.set_focused(true);
        for _ in 0..cursor_steps {
            input_state.move_right();
        }

        frame.render_stateful_widget(&input_widget, chunks[1], &mut input_state);
    }

    if show_footer {
        if let Some(hint) = transcript_review_shortcut_hint(session, show_search) {
            let footer_index = chunks.len().saturating_sub(1);
            frame.render_widget(
                Paragraph::new(Line::styled(hint, session.core.styles.default_style().dim())),
                chunks[footer_index],
            );
        }
    }
}

fn transcript_review_shortcut_hint(session: &Session, searching: bool) -> Option<String> {
    if !session.core.transcript_review_shortcut_guide_visible() {
        return None;
    }

    if searching {
        // While the search input owns keys, `q`/scroll hints don't apply:
        // typing goes to the query, Esc cancels, Enter finds.
        return Some("Esc cancel · Enter find".to_string());
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
#[path = "transcript_review/tests.rs"]
mod tests;
