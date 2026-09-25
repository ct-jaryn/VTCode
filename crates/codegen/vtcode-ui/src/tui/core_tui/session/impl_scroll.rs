use std::time::{Duration, Instant};

use super::*;

impl Session {
    pub(crate) fn scroll_offset(&self) -> usize {
        self.scroll_manager.offset()
    }

    /// Tracks pointer edges during a transcript drag so `step_drag_auto_scroll`
    /// can keep revealing content while the mouse rests at the top/bottom edge.
    pub(crate) fn update_drag_auto_scroll(&mut self, column: u16, row: u16) {
        let Some(area) = self.transcript_area() else {
            self.drag_auto_scroll = None;
            return;
        };
        if area.height == 0 {
            self.drag_auto_scroll = None;
            return;
        }
        let last_row = area.y.saturating_add(area.height.saturating_sub(1));
        // With a single visible row both edges coincide; only a pointer strictly
        // outside the area is meaningful, so it cannot always collapse to "Up".
        let direction = if row < area.y {
            Some(DragAutoScrollDirection::Up)
        } else if row > last_row {
            Some(DragAutoScrollDirection::Down)
        } else if area.height > 1 && row <= area.y {
            Some(DragAutoScrollDirection::Up)
        } else if area.height > 1 && row >= last_row {
            Some(DragAutoScrollDirection::Down)
        } else {
            None
        };

        let Some(direction) = direction else {
            if self.drag_auto_scroll.take().is_some() {
                tracing::debug!(target: "vtcode_ui::scroll", row, "drag auto-scroll disarmed: pointer left edge");
            }
            return;
        };

        let clamped_row = row.clamp(area.y, last_row);
        let clamped_column = column.clamp(area.x, area.x.saturating_add(area.width.saturating_sub(1)));
        // Keep the original step timer when the edge stays the same, otherwise
        // slow drags along the edge would reset the interval and never step.
        self.drag_auto_scroll =
            if let Some(existing) = self.drag_auto_scroll.filter(|scroll| scroll.direction == direction) {
                Some(DragAutoScroll {
                    direction,
                    column: clamped_column,
                    row: clamped_row,
                    last_step: existing.last_step,
                })
            } else {
                tracing::debug!(
                    target: "vtcode_ui::scroll",
                    ?direction,
                    pointer_row = row,
                    area_top = area.y,
                    area_last_row = last_row,
                    "drag auto-scroll armed"
                );
                Some(DragAutoScroll {
                    direction,
                    column: clamped_column,
                    row: clamped_row,
                    last_step: Instant::now(),
                })
            };
    }
    pub(crate) fn cancel_drag_auto_scroll(&mut self) {
        self.drag_auto_scroll = None;
    }

    /// Reveals one batch of rows toward the dragged edge and extends the active
    /// selection onto the newly visible content. Called from the tick handler so
    /// scrolling continues while the pointer holds still at an edge.
    pub(crate) fn step_drag_auto_scroll(&mut self) {
        let Some(scroll) = self.drag_auto_scroll else {
            return;
        };
        // A zero-height transcript cannot scroll; drop the pending auto-scroll
        // state (mirrors the guard in update_drag_auto_scroll).
        if self.transcript_area().is_some_and(|area| area.height == 0) {
            self.drag_auto_scroll = None;
            return;
        }
        if scroll.last_step.elapsed() < Duration::from_millis(ui::DRAG_AUTO_SCROLL_INTERVAL_MS) {
            return;
        }
        self.ensure_scroll_metrics();
        let previous_offset = self.scroll_manager.offset();
        match scroll.direction {
            DragAutoScrollDirection::Down => self.scroll_manager.scroll_up(ui::DRAG_AUTO_SCROLL_STEP_LINES),
            DragAutoScrollDirection::Up => self.scroll_manager.scroll_down(ui::DRAG_AUTO_SCROLL_STEP_LINES),
        }
        let offset_delta = self.scroll_manager.offset() as i64 - previous_offset as i64;

        let Some(scroll) = self.drag_auto_scroll.as_mut() else {
            return;
        };
        scroll.last_step = Instant::now();
        if offset_delta == 0 {
            // The dragged edge has no more content to reveal; stop nudging until
            // the pointer moves to a different edge.
            let direction = scroll.direction;
            tracing::debug!(target: "vtcode_ui::scroll", ?direction, "drag auto-scroll disarmed: no more content");
            self.drag_auto_scroll = None;
            return;
        }

        let direction = scroll.direction;
        tracing::debug!(
            target: "vtcode_ui::scroll",
            ?direction,
            offset_delta,
            "drag auto-scroll stepped"
        );
        self.mouse_selection.adjust_for_scroll(offset_delta as i32);
        self.mouse_selection.update_selection(scroll.column, scroll.row);
        self.user_scrolled = true;
        self.mark_dirty();
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.mark_scrolling();
        self.ensure_scroll_metrics();
        let previous_offset = self.scroll_manager.offset();
        // Inverted model: max offset = top of content
        self.scroll_manager.scroll_to_bottom();
        let offset_delta = self.scroll_manager.offset() as i64 - previous_offset as i64;
        self.mouse_selection.adjust_for_scroll(offset_delta as i32);
        self.user_scrolled = true;
        self.mark_dirty();
    }

    pub(crate) fn scroll_to_bottom(&mut self) {
        self.mark_scrolling();
        self.ensure_scroll_metrics();
        let previous_offset = self.scroll_manager.offset();
        // Inverted model: offset 0 = bottom of content
        self.scroll_manager.scroll_to_top();
        let offset_delta = self.scroll_manager.offset() as i64 - previous_offset as i64;
        self.mouse_selection.adjust_for_scroll(offset_delta as i32);
        self.user_scrolled = false;
        self.mark_dirty();
    }
}
