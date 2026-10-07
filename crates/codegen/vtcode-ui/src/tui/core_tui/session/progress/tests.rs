use super::*;
use crate::tui::core_tui::types::InlineSegment;
use crate::tui::core_tui::widgets::TranscriptWidget;
use ratatui::{Terminal, backend::TestBackend};

fn rendered_text(buf: &Buffer) -> String {
    buf.content.iter().map(|cell| cell.symbol()).collect()
}

#[test]
fn accepted_operation_is_visible_without_changing_input_authority() {
    let mut session = Session::new(InlineTheme::default(), None, 20);
    session.set_input("hello");
    assert!(matches!(
        session.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InlineEvent::Submit(_))
    ));
    assert!(!session.progress.is_active(), "a submission is not yet an accepted runtime operation");
    let operation = ProgressOperation::start();
    session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
        operation,
        phase: ProgressPhase::PreparingContext,
    }));
    assert!(session.progress.is_active());
    assert_eq!(session.activity_state, ActivityState::Idle);
    assert!(!session.is_running_activity());
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal.draw(|frame| session.render(frame)).unwrap();
    assert!(rendered_text(terminal.backend().buffer()).contains("Preparing context"));
    assert!(session.lines.is_empty());
    assert!(session.transcript_export_text().is_empty());
    session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Finish { operation }));
    assert!(!session.progress.is_active());
}

#[test]
fn another_submission_during_preparation_does_not_replace_active_progress() {
    let mut session = Session::new(InlineTheme::default(), None, 20);
    let operation = ProgressOperation::start();
    session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
        operation,
        phase: ProgressPhase::SavingCheckpoint,
    }));
    session.set_input("next message");
    assert!(matches!(
        session.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(InlineEvent::Submit(_))
    ));
    assert_eq!(session.progress.active.unwrap().operation, operation);
    assert_eq!(session.progress.text().as_deref(), Some("Saving checkpoint · 0s"));
    session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Finish { operation }));
    assert!(!session.progress.is_active());
}

#[test]
fn copy_outcome_keeps_static_footer_style_during_animated_progress() {
    let mut session = Session::new(InlineTheme::default(), None, 20);
    session.show_copy_notification(5);
    let expected = session.render_input_status_line(120).unwrap();
    session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
        operation: ProgressOperation::start(),
        phase: ProgressPhase::WaitingForModel,
    }));
    assert_eq!(session.render_input_status_line(120).unwrap(), expected);
}

#[test]
fn progress_reserves_one_row_and_cleans_up_at_wide_and_narrow_widths() {
    for width in [120, 48, 12] {
        let mut session = Session::new(InlineTheme::default(), None, 12);
        for index in 0..20 {
            session.push_line(
                InlineMessageKind::Agent,
                vec![InlineSegment {
                    text: format!("row {index}"),
                    style: Arc::new(InlineTextStyle::default()),
                }],
            );
        }
        let export = session.transcript_export_text();
        let operation = ProgressOperation::start();
        session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
            operation,
            phase: ProgressPhase::SavingCheckpoint,
        }));
        let area = Rect::new(0, 0, width, 8);
        let mut buf = Buffer::empty(area);
        TranscriptWidget::new(&mut session).render(area, &mut buf);
        let body = session.transcript_area().unwrap();
        assert_eq!(body.height, 7);
        assert_eq!(body.bottom(), 7, "progress must be outside selection and link geometry");
        let row: String = (0..width).map(|x| buf[(x, 7)].symbol()).collect();
        assert!(row.contains("Saving"));
        assert_eq!(session.transcript_export_text(), export);
        session
            .mouse_selection
            .set_selection((body.x, body.y), (area.right().saturating_sub(1), 7));
        let selected = session.mouse_selection.extract_text(&buf, body);
        assert!(!selected.contains("Saving"));
        assert!(selected.contains("row"));
        let revision = session.current_transcript_revision();
        session.handle_tick();
        assert_eq!(session.current_transcript_revision(), revision, "animation must not reflow history");
        session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Finish { operation }));
        let mut cleared = Buffer::empty(area);
        TranscriptWidget::new(&mut session).render(area, &mut cleared);
        assert_eq!(session.transcript_area().unwrap().height, 8);
        assert!(!rendered_text(&cleared).contains("Saving"));
    }
}

#[test]
fn progress_approval_and_accessibility_fallbacks_keep_static_labels() {
    for screen_reader in [false, true] {
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.appearance.reduce_motion_mode = !screen_reader;
        session.appearance.screen_reader_mode = screen_reader;
        let operation = ProgressOperation::start();
        session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
            operation,
            phase: ProgressPhase::WaitingForModel,
        }));
        let area = Rect::new(0, 0, 80, 1);
        let mut buf = Buffer::empty(area);
        session.render_progress(area, &mut buf);
        assert!(rendered_text(&buf).contains("Waiting for model"));
        let phase = session.shimmer_state.phase();
        session.handle_tick();
        assert_eq!(session.shimmer_state.phase().to_bits(), phase.to_bits());
        session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Phase {
            operation,
            phase: ProgressPhase::WaitingForApproval,
        }));
        session.progress.elapsed_secs = 7;
        assert!(!session.progress.tick());
        assert_eq!(session.progress.text().as_deref(), Some("Waiting for approval · 7s"));
        assert!(!session.progress.is_animated());
    }
}

#[test]
fn progress_rejects_stale_updates_and_late_restart() {
    let old = ProgressOperation::start();
    let new = ProgressOperation::start();
    let mut progress = TransientProgress::default();
    assert!(progress.apply(ProgressUpdate::Begin {
        operation: old,
        phase: ProgressPhase::PreparingContext
    }));
    assert!(progress.apply(ProgressUpdate::Begin {
        operation: new,
        phase: ProgressPhase::SavingCheckpoint
    }));
    assert!(!progress.apply(ProgressUpdate::Phase { operation: old, phase: ProgressPhase::RunningTools }));
    assert!(!progress.apply(ProgressUpdate::Finish { operation: old }));
    assert_eq!(progress.text().as_deref(), Some("Saving checkpoint · 0s"));
    assert!(progress.apply(ProgressUpdate::Finish { operation: new }));
    assert!(!progress.apply(ProgressUpdate::Begin { operation: old, phase: ProgressPhase::Retrying }));
    assert!(!progress.apply(ProgressUpdate::Phase { operation: new, phase: ProgressPhase::Processing }));
    assert!(!progress.is_active());
}

#[test]
fn model_phases_are_deduplicated_against_current_presentation() {
    let operation = ProgressOperation::start();
    let mut progress = TransientProgress::default();
    assert!(progress.apply(ProgressUpdate::Begin { operation, phase: ProgressPhase::ReceivingResponse }));
    assert!(!progress.apply(ProgressUpdate::Phase { operation, phase: ProgressPhase::ReceivingResponse }));
    for phase in [ProgressPhase::RunningTools, ProgressPhase::WaitingForApproval] {
        assert!(progress.apply(ProgressUpdate::Phase { operation, phase }));
        assert!(progress.apply(ProgressUpdate::Phase { operation, phase: ProgressPhase::ReceivingResponse }));
        assert!(!progress.apply(ProgressUpdate::Phase { operation, phase: ProgressPhase::ReceivingResponse }));
    }
}

#[test]
fn progress_only_transcript_excludes_drag_and_completed_selection() {
    for completed in [false, true] {
        let mut session = Session::new(InlineTheme::default(), None, 12);
        session.handle_command(InlineCommand::UpdateProgress(ProgressUpdate::Begin {
            operation: ProgressOperation::start(),
            phase: ProgressPhase::SavingCheckpoint,
        }));
        let area = Rect::new(0, 2, 80, 1);
        session.mouse_selection.start_selection(0, 2);
        session.mouse_selection.update_selection(20, 2);
        if completed {
            session.mouse_selection.finish_selection(20, 2);
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        terminal
            .draw(|frame| {
                TranscriptWidget::new(&mut session).render(area, frame.buffer_mut());
                assert!(session.transcript_area().is_none());
                frame.buffer_mut().set_style(
                    area,
                    ratatui::style::Style::new()
                        .fg(ratatui::style::Color::White)
                        .bg(ratatui::style::Color::Black),
                );
                let before = frame.buffer_mut().clone();
                let viewport = frame.area();
                session.finalize_mouse_selection(frame, viewport);
                assert_eq!(*frame.buffer_mut(), before, "progress must not be highlighted without a transcript body");
            })
            .unwrap();

        // Overlay selection keeps its own geometry even when the body is absent.
        session.mouse_selection.start_overlay_selection(0, 2);
        session.mouse_selection.finish_selection(7, 2);
        terminal
            .draw(|frame| {
                Paragraph::new("overlay text")
                    .style(
                        ratatui::style::Style::new()
                            .fg(ratatui::style::Color::White)
                            .bg(ratatui::style::Color::Black),
                    )
                    .render(area, frame.buffer_mut());
                let before = frame.buffer_mut()[(0, 2)].clone();
                let viewport = frame.area();
                session.finalize_mouse_selection(frame, viewport);
                assert_ne!(frame.buffer_mut()[(0, 2)], before);
                assert_eq!(session.mouse_selection.extract_text(frame.buffer_mut(), viewport), "overlay");
            })
            .unwrap();
    }
}
