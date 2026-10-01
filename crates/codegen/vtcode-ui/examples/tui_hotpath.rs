//! Hotpath profiling harness for TUI render/input hot paths.
//!
//! Build and run (prints a hotpath timing+alloc report on exit):
//! ```bash
//! cargo run -p vtcode-ui --example tui_hotpath --features profiling
//! ```
//!
//! Workload mirrors the heavy cases from the FPS work: large transcript,
//! streaming appends, scroll, and keystrokes.

use std::sync::Arc;
use std::time::Instant;

use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers},
};
use tokio::sync::mpsc;
use vtcode_ui::tui::app::{InlineCommand, InlineEvent, InlineMessageKind, InlineSegment, InlineTextStyle, InlineTheme};
use vtcode_ui::tui::core_tui::app::AppSession;

fn segment(text: impl Into<String>) -> InlineSegment {
    InlineSegment {
        text: text.into(),
        style: Arc::new(InlineTextStyle::default()),
    }
}

#[hotpath::measure]
fn fill_transcript(session: &mut AppSession, count: usize) {
    for index in 0..count {
        let text = format!("line {index}: the quick brown fox jumps over the lazy dog while streaming tool output");
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![segment(text)],
        });
    }
}

#[hotpath::measure]
fn stream_chunks(session: &mut AppSession, chunks: usize) {
    session.handle_command(InlineCommand::AppendLine {
        kind: InlineMessageKind::Agent,
        segments: vec![segment("stream-start")],
    });
    for index in 0..chunks {
        session.handle_command(InlineCommand::AppendLine {
            kind: InlineMessageKind::Agent,
            segments: vec![segment(format!(" chunk-{index}"))],
        });
    }
}

#[hotpath::measure]
fn render_frames(session: &mut AppSession, terminal: &mut Terminal<TestBackend>, frames: usize) {
    for _ in 0..frames {
        let _ = terminal.draw(|frame| session.render(frame));
    }
}

#[hotpath::measure]
fn input_burst(session: &mut AppSession, events: usize) {
    let (tx, mut rx) = mpsc::unbounded_channel::<InlineEvent>();
    for index in 0..events {
        let key = if index % 7 == 0 {
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
        } else if index % 7 == 1 {
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        } else {
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        };
        session.handle_event(CrosstermEvent::Key(key), &tx, None);
    }
    while rx.try_recv().is_ok() {}
}

#[hotpath::measure]
fn main_workload() {
    let mut session = AppSession::new(InlineTheme::default(), None, 30);
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");

    fill_transcript(&mut session, 2_000);
    render_frames(&mut session, &mut terminal, 120);
    stream_chunks(&mut session, 400);
    render_frames(&mut session, &mut terminal, 80);
    input_burst(&mut session, 300);
    render_frames(&mut session, &mut terminal, 60);

    // Eviction path: push past the transcript cap.
    fill_transcript(&mut session, 4_000);
    render_frames(&mut session, &mut terminal, 40);
}

#[hotpath::main]
fn main() {
    let started = Instant::now();
    main_workload();
    eprintln!("[tui_hotpath] workload finished in {:?}", started.elapsed());
}
