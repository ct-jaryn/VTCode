use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Widget, Wrap},
};

use crate::tui::config::constants::ui;
use crate::tui::ui::tui::session::{Session, TranscriptLine, pulse_spinner_frame_for_phase};
use vtcode_config::constants::tools;

/// Widget for rendering the transcript area with conversation history
///
/// This widget handles:
/// - Scroll viewport management
/// - Content caching and optimization
/// - Text wrapping and overflow
/// - Queue overlay rendering
///
/// # Example
/// ```ignore
/// TranscriptWidget::new(session)
///     .show_scrollbar(true)
///     .custom_style(style)
///     .render(area, buf);
/// ```
pub struct TranscriptWidget<'a> {
    session: &'a mut Session,
    show_scrollbar: bool,
    custom_style: Option<Style>,
}

impl<'a> TranscriptWidget<'a> {
    /// Create a new TranscriptWidget with required parameters
    pub fn new(session: &'a mut Session) -> Self {
        Self { session, show_scrollbar: false, custom_style: None }
    }

    /// Enable or disable scrollbar rendering
    #[must_use]
    pub fn show_scrollbar(mut self, show: bool) -> Self {
        self.show_scrollbar = show;
        self
    }

    /// Set a custom style for the transcript
    #[must_use]
    pub fn custom_style(mut self, style: Style) -> Self {
        self.custom_style = Some(style);
        self
    }
}

impl<'a> Widget for TranscriptWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            self.session.set_transcript_area(None);
            self.session.clear_transcript_file_link_targets();
            return;
        }

        // No left gutter – transcript content is flush with the terminal edge.
        // Previously a `Block` with a border added a 1-column inset on all sides,
        // which produced the left-blank before bullets/warnings seen in the
        // screenshot. Rendering without a border makes `inner == area`.
        let inner = area;

        if inner.height == 0 || inner.width == 0 {
            self.session.set_transcript_area(None);
            self.session.clear_transcript_file_link_targets();
            return;
        }
        self.session.set_transcript_area(Some(inner));

        // Clamp effective dimensions to prevent pathological CPU usage with huge terminals
        // See: https://github.com/anthropics/claude-code/issues/21567
        let effective_height = inner.height.min(ui::TUI_MAX_VIEWPORT_HEIGHT);
        let effective_width = inner.width.min(ui::TUI_MAX_VIEWPORT_WIDTH);

        self.session.apply_transcript_rows(effective_height);

        let content_width = effective_width;
        if content_width == 0 {
            self.session.clear_transcript_file_link_targets();
            return;
        }
        self.session.apply_transcript_width(content_width);

        let viewport_rows = effective_height as usize;
        let effective_padding = ui::effective_transcript_bottom_padding(viewport_rows);
        let total_rows = self.session.total_transcript_rows(content_width) + effective_padding;
        let (top_offset, _clamped_total_rows) = self.session.prepare_transcript_scroll(total_rows, viewport_rows);
        let vertical_offset = top_offset.min(self.session.scroll_manager.max_offset());
        self.session.transcript_view_top = vertical_offset;

        let visible_start = vertical_offset;
        let scroll_area = inner;

        // Use cached visible lines to avoid rebuilding on every frame
        let cached_lines = self
            .session
            .collect_transcript_window_cached(content_width, visible_start, viewport_rows);

        // Check if we need to mutate the lines (fill empty space or add overlays)
        let fill_count = viewport_rows.saturating_sub(cached_lines.len());
        let needs_mutation = fill_count > 0 || !self.session.queued_inputs.is_empty();

        let mut visible_lines = if needs_mutation {
            // Need to mutate, so clone and modify
            let mut lines = cached_lines.to_vec();
            if fill_count > 0 {
                let target_len = lines.len() + fill_count;
                lines.resize_with(target_len, TranscriptLine::default);
            }
            self.session.overlay_queue_lines(&mut lines, content_width);
            self.session.decorate_visible_cached_transcript_links(lines, scroll_area)
        } else {
            self.session
                .decorate_borrowed_cached_transcript_links(cached_lines.as_slice(), scroll_area)
        };
        apply_active_file_operation_spinner(self.session, &mut visible_lines);

        // Only clear if content actually changed, not on viewport-only scroll
        // This is a significant optimization: avoids expensive Clear operation on most scrolls
        if self.session.transcript_clear_required {
            Clear.render(scroll_area, buf);
            self.session.transcript_clear_required = false;
        }
        // Paint full-width line tints AFTER Paragraph. Paragraph::render
        // first fills the whole area with `default_style` (terminal bg),
        // which would wipe a pre-painted band on cells past the line text.
        let default_bg = self.session.styles.default_style().bg;
        let paragraph = Paragraph::new(visible_lines.clone())
            .style(self.session.styles.default_style())
            .wrap(Wrap { trim: false });
        paragraph.render(scroll_area, buf);
        apply_full_width_line_backgrounds(buf, scroll_area, &visible_lines, default_bg);
    }
}

const FILE_OPERATION_STATUS_TOOLS: &[&str] = &[
    tools::WRITE_FILE,
    tools::CREATE_FILE,
    tools::EDIT_FILE,
    tools::APPLY_PATCH,
    tools::SEARCH_REPLACE,
    tools::DELETE_FILE,
    tools::UNIFIED_FILE,
];

const FILE_OPERATION_INDICATORS: &[&str] = &[
    "❋ Writing ",
    "❋ Editing ",
    "❋ Applying patch to ",
    "❋ Search/replace in ",
    "❋ Deleting ",
];

fn apply_active_file_operation_spinner(session: &Session, lines: &mut [Line<'static>]) {
    let Some(frame) = active_file_operation_spinner_frame(session) else {
        return;
    };

    for line in lines.iter_mut().rev() {
        if is_file_operation_indicator_line(line) && replace_indicator_icon(line, frame) {
            break;
        }
    }
}

fn active_file_operation_spinner_frame(session: &Session) -> Option<&'static str> {
    if !session.appearance.should_animate_progress_status() {
        return None;
    }

    let left = session.input_status_left.as_deref()?.to_ascii_lowercase();
    let tool_name = left.strip_prefix("running tool: ")?;
    let is_active_file_tool = FILE_OPERATION_STATUS_TOOLS.contains(&tool_name);

    is_active_file_tool.then(|| pulse_spinner_frame_for_phase(session.shimmer_state.phase()))
}

fn is_file_operation_indicator_line(line: &Line<'_>) -> bool {
    let text = line.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
    FILE_OPERATION_INDICATORS.iter().any(|pattern| text.contains(pattern))
}

fn replace_indicator_icon(line: &mut Line<'static>, frame: &str) -> bool {
    let mut replaced = false;
    let mut new_spans = Vec::with_capacity(line.spans.len() + 2);

    for span in std::mem::take(&mut line.spans) {
        if replaced {
            new_spans.push(span);
            continue;
        }

        let style = span.style;
        let text = span.content.into_owned();
        let Some(icon_index) = text.find('❋') else {
            new_spans.push(Span::styled(text, style));
            continue;
        };
        let icon_end = icon_index + '❋'.len_utf8();
        if icon_index > 0 {
            new_spans.push(Span::styled(text[..icon_index].to_string(), style));
        }
        new_spans.push(Span::styled(frame.to_string(), style));
        if icon_end < text.len() {
            new_spans.push(Span::styled(text[icon_end..].to_string(), style));
        }
        replaced = true;
    }

    line.spans = new_spans;
    replaced
}

/// Full-row tint for a diff line.
///
/// Uses the marker/gutter background rather than a changed-word chip so a row
/// with several syntax spans does not extend the stronger chip across padding.
///
/// Returns `None` for side-by-side rows — those mix coloured panes with an
/// uncoloured sibling (or two different colours), and full-width fill would
/// paint the empty pane with the other side's tint.
fn line_background(line: &Line<'_>) -> Option<Color> {
    let mut first_background = None;
    let mut marker_background = None;
    let mut has_uncolored_divider = false;
    for span in &line.spans {
        match span.style.bg {
            Some(Color::Reset) if span.content.trim() == "│" => has_uncolored_divider = true,
            Some(Color::Reset) => {}
            Some(bg) => {
                first_background.get_or_insert(bg);
                if marker_background.is_none() && matches!(span.content.chars().next(), Some('+' | '-')) {
                    marker_background = Some(bg);
                }
            }
            None if span.content.trim() == "│" => has_uncolored_divider = true,
            None => {}
        }
    }
    // Side-by-side rows carry an uncoloured divider. Full-width fill would
    // bleed one pane's tint into the other. Do not inspect every later span
    // for a marker: a changed word can legitimately begin with `+` or `-`
    // (for example `--last`) without turning a unified row into two panes.
    if has_uncolored_divider {
        return None;
    }
    marker_background.or(first_background)
}

/// Fill untinted cells on a diff row with the line tint.
///
/// Only cells still on the terminal default background are painted. Word-chip
/// cells (stronger red/green) must keep their colour so the two-level band
/// survives the full-width fill.
fn apply_full_width_line_backgrounds(buf: &mut Buffer, area: Rect, lines: &[Line<'_>], default_bg: Option<Color>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let max_rows = usize::from(area.height).min(lines.len());
    for (row, line) in lines.iter().take(max_rows).enumerate() {
        let Some(bg) = line_background(line) else {
            continue;
        };
        let y = area.y + row as u16;
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            // Reset / terminal default → line tint. Any explicit paint
            // (word chip, already-tinted span, etc.) is left alone.
            if cell.bg == Color::Reset || Some(cell.bg) == default_bg {
                cell.bg = bg;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::core_tui::types::{InlineMessageKind, InlineSegment, InlineTextStyle, InlineTheme};
    use std::sync::Arc;

    fn segment(text: &str) -> InlineSegment {
        InlineSegment {
            text: text.to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }
    }

    #[test]
    fn full_width_diff_tint_survives_paragraph_default_bg() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        // Simulate Paragraph painting the whole area with the terminal default.
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        let line = RatLine::from(vec![RatSpan::styled("+ hi", RatStyle::default().bg(Color::Rgb(20, 58, 45)))]);
        let lines = vec![line];
        // Correct order: Paragraph (already simulated) then full-width tint.
        apply_full_width_line_backgrounds(&mut buf, area, &lines, Some(Color::Black));

        let bg = Color::Rgb(20, 58, 45);
        assert_eq!(buf[(0, 0)].bg, bg, "left edge must keep the tint");
        assert_eq!(buf[(19, 0)].bg, bg, "right edge must be full-width tinted");
    }

    #[test]
    fn full_width_fill_preserves_word_chip_backgrounds() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let line_bg = Color::Rgb(20, 58, 45);
        let chip_bg = Color::Rgb(36, 100, 70);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        // Simulate Paragraph writing: line tint on part, chip on "words".
        buf[(3, 0)].bg = chip_bg;
        buf[(4, 0)].bg = chip_bg;
        buf[(5, 0)].bg = line_bg;

        let line = RatLine::from(vec![RatSpan::styled("ab", RatStyle::default().bg(line_bg))]);
        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(3, 0)].bg, chip_bg, "word chip must not be overwritten");
        assert_eq!(buf[(4, 0)].bg, chip_bg, "word chip must not be overwritten");
        assert_eq!(buf[(19, 0)].bg, line_bg, "empty cells fill with line tint");
        assert_eq!(buf[(0, 0)].bg, line_bg);
    }

    #[test]
    fn full_width_fill_uses_the_row_tint_when_word_chips_are_present() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let line_bg = Color::Rgb(20, 58, 45);
        let chip_bg = Color::Rgb(36, 100, 70);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        for x in 1..8 {
            buf[(x, 0)].bg = chip_bg;
        }
        let line = RatLine::from(vec![
            RatSpan::styled("+", RatStyle::default().bg(line_bg)),
            RatSpan::styled("changed", RatStyle::default().bg(chip_bg)),
            RatSpan::styled(" ", RatStyle::default().bg(line_bg)),
        ]);

        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(1, 0)].bg, chip_bg, "word chip must remain stronger");
        assert_eq!(buf[(19, 0)].bg, line_bg, "row tint must fill after word chip");
    }

    #[test]
    fn full_width_fill_uses_the_marker_tint_when_syntax_chips_outnumber_it() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let line_bg = Color::Rgb(20, 58, 45);
        let chip_bg = Color::Rgb(36, 100, 70);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        let line = RatLine::from(vec![
            RatSpan::styled("+", RatStyle::default().bg(line_bg)),
            RatSpan::styled("changed", RatStyle::default().bg(chip_bg)),
            RatSpan::styled(" syntax", RatStyle::default().bg(chip_bg)),
            RatSpan::styled(" tokens", RatStyle::default().bg(chip_bg)),
        ]);

        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(19, 0)].bg, line_bg, "padding must use the marker tint");
    }

    #[test]
    fn full_width_fill_does_not_misclassify_dash_prefixed_word_chip() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 36, 1);
        let mut buf = Buffer::empty(area);
        let line_bg = Color::Rgb(70, 38, 42);
        let chip_bg = Color::Rgb(140, 52, 58);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        let line = RatLine::from(vec![
            RatSpan::styled("-", RatStyle::default().bg(line_bg)),
            RatSpan::styled(" 256 │ vtcode trajectory ", RatStyle::default().bg(line_bg)),
            RatSpan::styled("--last", RatStyle::default().bg(chip_bg)),
        ]);
        for x in 25..31 {
            buf[(x, 0)].bg = chip_bg;
        }

        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(26, 0)].bg, chip_bg, "intraline chip must remain stronger");
        assert_eq!(buf[(35, 0)].bg, line_bg, "a dash-prefixed chip must not create a tint gap");
    }

    #[test]
    fn full_width_fill_survives_an_uncolored_indent_before_the_marker() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let line_bg = Color::Rgb(20, 58, 45);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        let line = RatLine::from(vec![
            RatSpan::raw("    "),
            RatSpan::styled("+", RatStyle::default().bg(line_bg)),
            RatSpan::styled("new", RatStyle::default().bg(line_bg)),
        ]);

        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(19, 0)].bg, line_bg, "indentation must not disable row tinting");
    }

    #[test]
    fn full_width_fill_skips_side_by_side_dividers() {
        use ratatui::style::Style as RatStyle;
        use ratatui::text::{Line as RatLine, Span as RatSpan};

        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let left_bg = Color::Rgb(70, 38, 42);
        let right_bg = Color::Rgb(20, 58, 45);
        buf.set_style(area, RatStyle::default().bg(Color::Black));
        let line = RatLine::from(vec![
            RatSpan::styled("-", RatStyle::default().bg(left_bg)),
            RatSpan::raw("old       "),
            RatSpan::raw("│"),
            RatSpan::styled("+", RatStyle::default().bg(right_bg)),
            RatSpan::styled("new", RatStyle::default().bg(right_bg)),
        ]);

        apply_full_width_line_backgrounds(&mut buf, area, &[line], Some(Color::Black));

        assert_eq!(buf[(19, 0)].bg, Color::Black, "pane tint must not bleed across the row");
    }

    fn row_text(buf: &Buffer, area: Rect, row: u16) -> String {
        (area.left()..area.right()).map(|x| buf[(x, row)].symbol()).collect::<String>()
    }

    #[test]
    fn scroll_metric_invalidation_does_not_request_transcript_clear() {
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.transcript_clear_required = false;

        session.invalidate_scroll_metrics();

        assert!(!session.transcript_clear_required);
    }

    #[test]
    fn render_clears_stale_wrapped_rows_when_requested() {
        let area = Rect::new(0, 0, 14, 6);
        let inner = area;
        let mut buf = Buffer::empty(area);
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.push_line(InlineMessageKind::Agent, vec![segment("this line wraps across several rows")]);

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        let revision = session.next_revision();
        session.lines[0].segments = vec![segment("short")];
        session.lines[0].revision = revision;
        session.mark_line_dirty(0);
        session.invalidate_transcript_cache();
        for row in inner.y + 1..inner.bottom() {
            for x in inner.left()..inner.right() {
                buf[(x, row)].set_symbol("X");
            }
        }

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        assert!((inner.y + 1..inner.bottom()).all(|row| row_text(&buf, inner, row).trim().is_empty()));
    }

    #[test]
    fn render_preserves_queue_overlay_lines() {
        let area = Rect::new(0, 0, 20, 6);
        let inner = area;
        let mut buf = Buffer::empty(area);
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.push_line(InlineMessageKind::Agent, vec![segment("alpha")]);
        session.push_queued_input("queued follow-up".to_string());

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        let has_queued = (inner.y..inner.bottom()).any(|row| row_text(&buf, inner, row).contains("queued"));
        assert!(has_queued, "queued overlay should be visible");
    }

    #[test]
    fn render_queue_overlay_orders_items_fifo_oldest_on_top() {
        let area = Rect::new(0, 0, 30, 8);
        let inner = area;
        let mut buf = Buffer::empty(area);
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.push_line(InlineMessageKind::Agent, vec![segment("alpha")]);
        session.push_queued_input("first".to_string());
        session.push_queued_input("second".to_string());
        session.push_queued_input("third".to_string());

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        // The queue is strict FIFO, so the overlay shows the oldest message at
        // the top and the newest directly above the input field.
        let overlay_rows: Vec<String> = (inner.y..inner.bottom())
            .map(|row| row_text(&buf, inner, row))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .collect();
        let first_row = overlay_rows.iter().position(|row| row.contains("first"));
        let second_row = overlay_rows.iter().position(|row| row.contains("second"));
        let third_row = overlay_rows.iter().position(|row| row.contains("third"));
        assert!(
            first_row.is_some() && second_row.is_some() && third_row.is_some(),
            "all three queued items must be visible in the overlay, got: {overlay_rows:?}"
        );
        assert!(
            first_row.unwrap() < second_row.unwrap() && second_row.unwrap() < third_row.unwrap(),
            "expected FIFO order first < second < third, got: {overlay_rows:?}"
        );
    }

    #[test]
    fn render_queue_overlay_flattens_multiline_entries() {
        let area = Rect::new(0, 0, 40, 8);
        let inner = area;
        let mut buf = Buffer::empty(area);
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.push_line(InlineMessageKind::Agent, vec![segment("alpha")]);
        session.push_queued_input("line one\nline two".to_string());

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        let overlay_text: String = (inner.y..inner.bottom()).map(|row| row_text(&buf, inner, row)).collect();
        assert!(
            overlay_text.contains("line one") && overlay_text.contains("line two"),
            "both lines of the queued entry must be visible, got: {overlay_text:?}"
        );
        assert!(
            overlay_text.contains("⏎"),
            "newlines must be flattened into a visible separator, got: {overlay_text:?}"
        );
    }

    #[test]
    fn render_clears_stale_queue_overlay_rows_when_queue_is_removed() {
        let area = Rect::new(0, 0, 20, 6);
        let inner = area;
        let mut buf = Buffer::empty(area);
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.push_line(InlineMessageKind::Agent, vec![segment("alpha")]);
        session.push_queued_input("queued follow-up".to_string());

        TranscriptWidget::new(&mut session).render(area, &mut buf);
        let has_queued_before = (inner.y..inner.bottom()).any(|row| row_text(&buf, inner, row).contains("queued"));
        assert!(has_queued_before, "queued should be visible before pop");

        let _ = session.pop_latest_queued_input();

        TranscriptWidget::new(&mut session).render(area, &mut buf);

        let has_queued_after = (inner.y..inner.bottom()).any(|row| row_text(&buf, inner, row).contains("queued"));
        assert!(!has_queued_after, "queued should be cleared after pop");
    }

    #[test]
    fn resize_larger_keeps_existing_transcript_lines_visible() {
        let small_area = Rect::new(0, 0, 20, 4);
        let large_area = Rect::new(0, 0, 20, 10);
        let small_inner = small_area;
        let large_inner = large_area;
        let mut small_buf = Buffer::empty(small_area);
        let mut large_buf = Buffer::empty(large_area);
        let mut session = Session::new(InlineTheme::default(), None, 12);

        for index in 0..6 {
            session.push_line(InlineMessageKind::Agent, vec![segment(&format!("line {index}"))]);
        }

        TranscriptWidget::new(&mut session).render(small_area, &mut small_buf);
        let small_rendered: Vec<String> = (small_inner.y..small_inner.bottom())
            .map(|row| row_text(&small_buf, small_inner, row).trim().to_string())
            .filter(|row| !row.is_empty())
            .collect();
        TranscriptWidget::new(&mut session).render(large_area, &mut large_buf);

        let rendered: Vec<String> = (large_inner.y..large_inner.bottom())
            .map(|row| row_text(&large_buf, large_inner, row).trim().to_string())
            .filter(|row| !row.is_empty())
            .collect();

        assert!(rendered.len() > small_rendered.len());
        assert!(rendered.iter().any(|row| row == "line 1"));
        assert!(rendered.iter().any(|row| row == "line 5"));
    }

    #[test]
    fn width_resize_keeps_transcript_visible() {
        let wide_area = Rect::new(0, 0, 28, 8);
        let narrow_area = Rect::new(0, 0, 16, 8);
        let narrow_inner = narrow_area;
        let mut wide_buf = Buffer::empty(wide_area);
        let mut narrow_buf = Buffer::empty(narrow_area);
        let mut session = Session::new(InlineTheme::default(), None, 12);

        for index in 0..4 {
            session.push_line(InlineMessageKind::Agent, vec![segment(&format!("line {index}"))]);
        }

        TranscriptWidget::new(&mut session).render(wide_area, &mut wide_buf);
        TranscriptWidget::new(&mut session).render(narrow_area, &mut narrow_buf);

        let rendered: Vec<String> = (narrow_inner.y..narrow_inner.bottom())
            .map(|row| row_text(&narrow_buf, narrow_inner, row).trim().to_string())
            .filter(|row| !row.is_empty())
            .collect();

        assert!(!rendered.is_empty());
        assert!(rendered.iter().any(|row| row == "line 1"));
        assert!(rendered.iter().any(|row| row == "line 3"));
    }
}
