use super::*;
use ratatui::{buffer::Buffer, style::Modifier, widgets::Paragraph};
use vtcode_commons::ui_protocol::{ProgressOperation, ProgressPhase, ProgressUpdate};

#[derive(Clone, Copy)]
struct ActiveProgress {
    operation: ProgressOperation,
    phase: ProgressPhase,
    feedback_observed: bool,
}

#[derive(Default)]
pub(crate) struct TransientProgress {
    latest_id: u64,
    active: Option<ActiveProgress>,
    elapsed_secs: u64,
}

impl TransientProgress {
    pub(crate) fn apply(&mut self, update: ProgressUpdate) -> bool {
        match update {
            ProgressUpdate::Begin { operation, phase } if operation.id() > self.latest_id => {
                self.latest_id = operation.id();
                self.active = Some(ActiveProgress { operation, phase, feedback_observed: false });
                self.elapsed_secs = operation.started_at().elapsed().as_secs();
                true
            }
            ProgressUpdate::Phase { operation, phase }
                if self
                    .active
                    .is_some_and(|current| current.operation == operation && current.phase != phase) =>
            {
                if let Some(current) = self.active.as_mut() {
                    current.phase = phase;
                }
                self.elapsed_secs = operation.started_at().elapsed().as_secs();
                true
            }
            ProgressUpdate::Finish { operation }
                if self.active.is_some_and(|current| current.operation == operation) =>
            {
                self.active = None;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn tick(&mut self) -> bool {
        let Some(ActiveProgress { operation, phase, .. }) = self.active else {
            return false;
        };
        if !phase.is_animated() {
            return false;
        }
        let elapsed_secs = operation.started_at().elapsed().as_secs();
        if elapsed_secs == self.elapsed_secs {
            return false;
        }
        self.elapsed_secs = elapsed_secs;
        true
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn is_animated(&self) -> bool {
        self.active.is_some_and(|current| current.phase.is_animated())
    }

    pub(crate) fn text(&self) -> Option<String> {
        self.active.map(|current| current.phase.format(self.elapsed_secs))
    }
}

impl Session {
    pub(crate) fn render_progress(&mut self, area: Rect, buf: &mut Buffer) {
        if let Some(current) = self.progress.active.as_mut()
            && !current.feedback_observed
        {
            current.feedback_observed = true;
            tracing::debug!(target: "vtcode.response_latency", operation_id = current.operation.id(),
                accepted_to_feedback_ms = current.operation.started_at().elapsed().as_secs_f64() * 1000.0,
                "progress frame rendered");
        }
        let Some(text) = self.progress.text() else { return };
        let style = self.styles.default_style().add_modifier(Modifier::DIM);
        let spans = if self.progress.is_animated() && self.appearance.should_animate_progress_status() {
            tui_shimmer::shimmer_spans_with_style_at_phase(&text, style, self.shimmer_state.phase())
        } else {
            vec![Span::styled(text, style)]
        };
        Clear.render(area, buf);
        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}

#[cfg(test)]
mod tests;
