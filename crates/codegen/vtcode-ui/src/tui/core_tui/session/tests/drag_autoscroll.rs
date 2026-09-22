#![cfg(unix)]
#![allow(
    missing_docs,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
use std::time::{Duration, Instant};

use super::super::*;
use super::helpers::*;

const STEP_BACKDATE: Duration = Duration::from_millis(200);

fn content_session() -> Session {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    for i in 0..80 {
        session.push_line(InlineMessageKind::Agent, vec![make_segment(&format!("line {i}"))]);
    }
    session
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> CrosstermEvent {
    CrosstermEvent::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })
}

#[test]
fn drag_at_bottom_edge_reveals_newer_content_and_disarms_at_end() {
    let mut session = content_session();
    let (transcript_area, _rendered) = rendered_transcript_lines(&mut session, VIEW_ROWS);
    let last_row = transcript_area.y + transcript_area.height.saturating_sub(1);
    let mid_row = transcript_area.y + 3;
    let (tx, _rx) = mpsc::unbounded_channel();

    session.handle_event(
        mouse_event(MouseEventKind::Down(MouseButton::Left), transcript_area.x + 2, mid_row),
        &tx,
        None,
    );

    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, mid_row),
        &tx,
        None,
    );
    assert!(session.drag_auto_scroll.is_none(), "dragging mid-transcript must not arm edge auto-scroll");

    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, last_row),
        &tx,
        None,
    );
    assert_eq!(session.drag_auto_scroll.map(|scroll| scroll.direction), Some(DragAutoScrollDirection::Down));

    // Scroll away from the newest content so there is room to reveal downward.
    session.scroll_to_top();
    let before = session.scroll_manager.offset();
    assert!(before > 0, "test needs scrollback above the viewport");

    // Within the interval a tick must not step.
    session.step_drag_auto_scroll();
    assert_eq!(session.scroll_manager.offset(), before);

    session.drag_auto_scroll.as_mut().expect("auto-scroll armed").last_step =
        Instant::now().checked_sub(STEP_BACKDATE).expect("test timestamp before epoch");
    session.step_drag_auto_scroll();
    assert!(session.scroll_manager.offset() < before, "bottom-edge auto-scroll must reveal newer content");

    // Exhaust the remaining scrollback: the edge state must disarm itself.
    while session.drag_auto_scroll.is_some() {
        session.drag_auto_scroll.as_mut().expect("armed").last_step =
            Instant::now().checked_sub(STEP_BACKDATE).expect("test timestamp before epoch");
        session.step_drag_auto_scroll();
    }
    assert_eq!(session.scroll_manager.offset(), 0);
}

#[test]
fn drag_at_top_edge_reveals_older_content_and_release_cancels() {
    let mut session = content_session();
    let (transcript_area, _rendered) = rendered_transcript_lines(&mut session, VIEW_ROWS);
    let (tx, _rx) = mpsc::unbounded_channel();

    session.handle_event(
        mouse_event(MouseEventKind::Down(MouseButton::Left), transcript_area.x + 2, transcript_area.y + 5),
        &tx,
        None,
    );
    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, transcript_area.y),
        &tx,
        None,
    );
    assert_eq!(session.drag_auto_scroll.map(|scroll| scroll.direction), Some(DragAutoScrollDirection::Up));

    session.scroll_to_bottom();
    let before = session.scroll_manager.offset();

    session.drag_auto_scroll.as_mut().expect("auto-scroll armed").last_step =
        Instant::now().checked_sub(STEP_BACKDATE).expect("test timestamp before epoch");
    session.step_drag_auto_scroll();
    assert!(session.scroll_manager.offset() > before, "top-edge auto-scroll must reveal older content");

    session.handle_event(
        mouse_event(MouseEventKind::Up(MouseButton::Left), transcript_area.x + 2, transcript_area.y),
        &tx,
        None,
    );
    assert!(session.drag_auto_scroll.is_none());
}

#[test]
fn dragging_back_into_the_middle_disarms_edge_auto_scroll() {
    let mut session = content_session();
    let (transcript_area, _rendered) = rendered_transcript_lines(&mut session, VIEW_ROWS);
    let last_row = transcript_area.y + transcript_area.height.saturating_sub(1);
    let mid_row = transcript_area.y + 3;
    let (tx, _rx) = mpsc::unbounded_channel();

    session.handle_event(
        mouse_event(MouseEventKind::Down(MouseButton::Left), transcript_area.x + 2, mid_row),
        &tx,
        None,
    );
    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, last_row),
        &tx,
        None,
    );
    assert!(session.drag_auto_scroll.is_some());

    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, mid_row),
        &tx,
        None,
    );
    assert!(session.drag_auto_scroll.is_none());
}

#[test]
fn zero_height_transcript_area_never_arms_auto_scroll() {
    let mut session = content_session();
    session.set_transcript_area(Some(Rect::new(0, 0, 80, 0)));

    session.update_drag_auto_scroll(10, 0);
    assert!(session.drag_auto_scroll.is_none(), "zero-height area must not arm auto-scroll");
}

#[test]
fn escape_mid_drag_dismisses_selection_and_disarms_auto_scroll() {
    let mut session = content_session();
    let (transcript_area, _rendered) = rendered_transcript_lines(&mut session, VIEW_ROWS);
    let last_row = transcript_area.y + transcript_area.height.saturating_sub(1);
    let (tx, _rx) = mpsc::unbounded_channel();

    session.handle_event(
        mouse_event(MouseEventKind::Down(MouseButton::Left), transcript_area.x + 2, transcript_area.y + 1),
        &tx,
        None,
    );
    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, last_row),
        &tx,
        None,
    );
    assert!(session.mouse_selection.has_selection, "drag must have produced a selection");
    assert!(session.drag_auto_scroll.is_some(), "dragging to the bottom edge must arm auto-scroll before Esc");

    let event = session.process_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(event.is_none(), "Esc must be consumed by the dismissal, got {event:?}");

    assert!(!session.mouse_selection.has_selection, "Esc must clear the highlight");
    assert!(
        session.drag_auto_scroll.is_none(),
        "dismissal must disarm edge auto-scroll so the transcript stops scrolling"
    );
    assert!(matches!(session.mouse_drag_target, MouseDragTarget::None), "dismissal must release the drag target");

    // A later drag event must not resurrect the cleared interaction.
    session.scroll_to_top();
    let before = session.scroll_manager.offset();
    session.handle_event(
        mouse_event(MouseEventKind::Drag(MouseButton::Left), transcript_area.x + 2, last_row),
        &tx,
        None,
    );
    assert!(session.drag_auto_scroll.is_none(), "a released drag must not re-arm auto-scroll");
    assert!(!session.mouse_selection.has_selection);
    assert_eq!(session.scroll_manager.offset(), before);
}
