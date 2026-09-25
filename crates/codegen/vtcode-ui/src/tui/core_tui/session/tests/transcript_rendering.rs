#![allow(
    missing_docs,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
use super::super::*;
use super::helpers::*;
use crate::tui::core_tui::style::{ratatui_color_from_ansi, ratatui_style_from_inline};

// ---------------------------------------------------------------------------
// Common test helpers extracted from repeated patterns
// ---------------------------------------------------------------------------

fn make_pty_segment(text: &str) -> InlineSegment {
    InlineSegment {
        text: text.to_string(),
        style: Arc::new(InlineTextStyle::default()),
    }
}

fn push_pty_line(session: &mut Session, text: &str) {
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment(text)]);
}

fn make_styled_line(session: &Session, text: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_string(),
        ratatui_style_from_inline(&session.default_style(), None),
    )])
}

fn agent_append_line_command(text: &str) -> InlineCommand {
    InlineCommand::AppendLine {
        kind: InlineMessageKind::Agent,
        segments: vec![InlineSegment {
            text: text.to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn streaming_new_lines_preserves_scrolled_view() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    for index in 1..=LINE_COUNT {
        let label = format!("{LABEL_PREFIX}-{index}");
        session.push_line(InlineMessageKind::Agent, vec![make_segment(label.as_str())]);
    }

    session.scroll_page_up();
    let before = visible_transcript(&mut session);
    let before_offset = session.scroll_offset();

    session.append_inline(InlineMessageKind::Agent, make_segment(EXTRA_SEGMENT));

    let after = visible_transcript(&mut session);
    assert_eq!(before.len(), after.len());
    assert_eq!(session.scroll_offset(), before_offset, "streaming should preserve manual scroll offset");
}

#[test]
fn streaming_segments_render_incrementally() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.push_line(InlineMessageKind::Agent, vec![make_segment("")]);

    session.append_inline(InlineMessageKind::Agent, make_segment("Hello"));
    let first = visible_transcript(&mut session);
    assert!(first.iter().any(|line| line.contains("Hello")));

    session.append_inline(InlineMessageKind::Agent, make_segment(" world"));
    let second = visible_transcript(&mut session);
    assert!(second.iter().any(|line| line.contains("Hello world")));
}

#[test]
fn appended_info_lines_refresh_the_cached_info_block() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.push_line(InlineMessageKind::Info, vec![make_segment("Active WebMCP bridge started.")]);
    let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    session.push_line(InlineMessageKind::Info, vec![make_segment("WebSocket: ws://127.0.0.1:57759/webmcp")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("Pairing code: 1AF23A43F9C4")]);

    let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("WebSocket: ws://127.0.0.1:57759/webmcp")),
        "appended WebSocket line should be visible in the refreshed info block: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("Pairing code: 1AF23A43F9C4")),
        "appended pairing line should be visible in the refreshed info block: {rendered:?}"
    );
}

#[test]
fn info_box_after_tool_summary_invalidates_its_own_cached_head() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.push_line(InlineMessageKind::Info, vec![make_segment("• Ran cargo check")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("First info line")]);
    let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    session.push_line(InlineMessageKind::Info, vec![make_segment("Second info line")]);

    let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);
    assert!(
        rendered.iter().any(|line| line.contains("Second info line")),
        "the info box after a tool summary should reflow from its own head: {rendered:?}"
    );
}

#[test]
fn page_up_reveals_prior_lines_until_buffer_start() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    for index in 1..=LINE_COUNT {
        let label = format!("{LABEL_PREFIX}-{index}");
        session.push_line(InlineMessageKind::Agent, vec![make_segment(label.as_str())]);
    }

    let bottom_view = visible_transcript(&mut session);
    let start_offset = session.scroll_offset();
    for _ in 0..(LINE_COUNT * 2) {
        session.scroll_page_up();
        if session.scroll_offset() > start_offset {
            break;
        }
    }
    let scrolled_view = visible_transcript(&mut session);

    assert!(session.scroll_offset() > start_offset);
    assert_ne!(bottom_view, scrolled_view);
}

#[test]
fn resizing_viewport_clamps_scroll_offset() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    for index in 1..=(LINE_COUNT * 5) {
        let label = format!("{LABEL_PREFIX}-{index}");
        session.push_line(InlineMessageKind::Agent, vec![make_segment(label.as_str())]);
    }

    visible_transcript(&mut session);
    for _ in 0..(LINE_COUNT * 2) {
        session.scroll_page_up();
        if session.scroll_offset() > 0 {
            break;
        }
    }
    assert!(session.scroll_offset() > 0);
    let scrolled_offset = session.scroll_offset();

    session
        .force_view_rows((LINE_COUNT as u16) + ui::INLINE_HEADER_HEIGHT + Session::input_block_height_for_lines(1) + 2);

    let max_offset = session.current_max_scroll_offset();
    assert!(session.scroll_offset() <= scrolled_offset);
    assert!(session.scroll_offset() <= max_offset);
}

#[test]
fn scroll_end_displays_full_final_paragraph() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let total = LINE_COUNT * 5;

    for index in 1..=total {
        let label = format!("{LABEL_PREFIX}-{index}");
        let text = format!("{label}\n{label}-continued");
        session.push_line(InlineMessageKind::Agent, vec![make_segment(text.as_str())]);
    }

    // Prime layout to ensure transcript dimensions are measured.
    visible_transcript(&mut session);

    for _ in 0..total {
        session.scroll_page_up();
        if session.scroll_offset() == session.current_max_scroll_offset() {
            break;
        }
    }
    assert!(session.scroll_offset() > 0);

    for _ in 0..total {
        session.scroll_page_down();
        if session.scroll_offset() == 0 {
            break;
        }
    }

    assert_eq!(session.scroll_offset(), 0);

    let view = visible_transcript(&mut session);
    let expected_tail = format!("{LABEL_PREFIX}-{total}-continued");
    assert!(
        view.iter().any(|line| line.contains(&expected_tail)),
        "expected final paragraph tail `{expected_tail}` to appear, got {view:?}"
    );
}

#[test]
fn user_messages_render_with_dividers() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::User, vec![make_segment("Hi")]);

    let width = 10;
    let lines = session.reflow_transcript_lines(width);
    assert!(
        lines.iter().any(|line| line_text(line).contains("Hi")),
        "expected user message to remain visible in transcript"
    );
}

#[test]
fn agent_messages_use_zero_indent_prose() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Agent,
        vec![make_segment(
            "Hello, here is the information you requested. This is an example of a standard agent message.",
        )],
    );

    let lines = session.reflow_transcript_lines(32);
    let content_lines: Vec<String> = lines.iter().map(line_text).filter(|text| !text.trim().is_empty()).collect();
    assert!(content_lines.len() >= 2, "expected wrapped agent lines to be visible");
    let first_line = &content_lines[0];
    let second_line = &content_lines[1];

    assert!(
        !first_line.starts_with(' ') && !first_line.starts_with('•'),
        "agent prose should start at column 0 with no bullet gap, got: {first_line:?}",
    );
    assert!(
        !second_line.starts_with(' '),
        "agent message continuation should start at column 0, got: {second_line:?}",
    );
    assert!(!first_line.contains('│'), "agent message should not render a left border",);
}

#[test]
fn agent_prose_has_no_bullet_gap() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("Response")]);

    let index = session.lines.len().checked_sub(1).expect("agent message should be available");
    let spans = session.render_message_spans(index);
    assert!(
        !spans.iter().any(|span| span.content.as_ref().contains('•')),
        "agent prose must not render a bullet gap, got {spans:?}"
    );
}

#[test]
fn wrap_line_splits_double_width_graphemes() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "你好世界");

    let wrapped = session.wrap_line(line, 4);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["你好".to_string(), "世界".to_string()]);
}

#[test]
fn wrap_line_keeps_explicit_blank_rows() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "top\n\nbottom");

    let wrapped = session.wrap_line(line, 40);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["top".to_string(), String::new(), "bottom".to_string()]);
}

#[test]
fn wrap_line_prefers_word_boundaries_for_plain_text() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "alpha beta gamma");

    let wrapped = session.wrap_line(line, 7);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]);
}

#[test]
fn wrap_line_keeps_words_intact_across_same_style_stream_chunks() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let style = ratatui_style_from_inline(&session.default_style(), None);
    let line = Line::from(vec![
        Span::styled("alpha be".to_string(), style),
        Span::styled("ta gamma".to_string(), style),
    ]);

    let wrapped = session.wrap_line(line, 7);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]);
}

#[test]
fn wrap_line_keeps_list_continuation_aligned() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "• alpha beta gamma");

    let wrapped = session.wrap_line(line, 8);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["• alpha".to_string(), "  beta".to_string(), "  gamma".to_string()]);
}

#[test]
fn wrap_line_preserves_characters_wider_than_viewport() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "你");

    let wrapped = session.wrap_line(line, 1);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["你".to_string()]);
}

#[test]
fn wrap_line_discards_carriage_return_before_newline() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let line = make_styled_line(&session, "foo\r\nbar");

    let wrapped = session.wrap_line(line, 80);
    let rendered: Vec<String> = wrapped.iter().map(line_text).collect();

    assert_eq!(rendered, vec!["foo".to_string(), "bar".to_string()]);
}

#[test]
fn tool_code_fence_markers_are_skipped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.append_inline(
        InlineMessageKind::Tool,
        InlineSegment {
            text: "```rust\nfn demo() {}\n```".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        },
    );

    let tool_lines: Vec<&MessageLine> = session
        .lines
        .iter()
        .filter(|line| line.kind == InlineMessageKind::Tool)
        .collect();

    assert_eq!(tool_lines.len(), 1);
    let Some(first_line) = tool_lines.first() else {
        panic!("Expected at least one tool line");
    };
    assert_eq!(first_line.segments.len(), 1);
    let Some(first_segment) = first_line.segments.first() else {
        panic!("Expected at least one segment");
    };
    assert_eq!(first_segment.text.as_str(), "```rust\nfn demo() {}\n```");
    assert!(!session.in_tool_code_fence);
}

#[test]
fn pty_block_omits_placeholder_when_empty() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Pty, Vec::new());

    let lines = session.reflow_pty_lines(0, 80);
    assert!(lines.is_empty());
}

#[test]
fn pty_block_hides_until_output_available() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Pty, Vec::new());

    assert!(session.reflow_pty_lines(0, 80).is_empty());

    push_pty_line(&mut session, "first output");

    assert!(session.reflow_pty_lines(0, 80).is_empty(), "placeholder PTY line should remain hidden",);

    let rendered = session.reflow_pty_lines(1, 80);
    assert!(rendered.iter().any(|line| !line.line.spans.is_empty()));
}

#[test]
fn pty_block_skips_status_only_sequence() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Pty, Vec::new());
    session.push_line(InlineMessageKind::Pty, Vec::new());

    assert!(session.reflow_pty_lines(0, 80).is_empty());
    assert!(session.reflow_pty_lines(1, 80).is_empty());
}

#[test]
fn pty_tool_block_has_top_and_bottom_spacing() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    push_pty_line(&mut session, "first output");
    push_pty_line(&mut session, "second output");

    let first = session.reflow_pty_lines(0, 80);
    let last = session.reflow_pty_lines(1, 80);

    assert!(first.first().is_some_and(|line| line.line.spans.is_empty()));
    assert!(last.last().is_some_and(|line| line.line.spans.is_empty()));
}

#[test]
fn tool_block_has_top_and_bottom_spacing() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("tool output")]);

    let rendered = session.reflow_message_lines(0, 80, false);

    assert!(rendered.first().is_some_and(|line| line.line.spans.is_empty()));
    assert!(rendered.last().is_some_and(|line| line.line.spans.is_empty()));
}

#[test]
fn cached_reflow_refreshes_tool_and_pty_block_boundaries() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("I will run the check.")]);
    let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);
    let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);
    let agent = rendered
        .iter()
        .position(|line| line.contains("I will run the check."))
        .expect("agent line");
    let tool = rendered
        .iter()
        .position(|line| line.contains("Ran cargo check"))
        .expect("tool header");
    assert_eq!(tool, agent + 2, "agent-to-tool boundary needs exactly one blank row: {rendered:?}");
    assert!(rendered[agent + 1].trim().is_empty(), "tool gap must be blank: {rendered:?}");

    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::User, vec![make_segment("Run the check.")]);
    let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);
    let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);
    let user = rendered
        .iter()
        .position(|line| line.contains("Run the check."))
        .expect("user line");
    let tool = rendered
        .iter()
        .position(|line| line.contains("Ran cargo check"))
        .expect("tool header");
    assert_eq!(tool, user + 2, "user-to-tool boundary needs exactly one blank row: {rendered:?}");
    assert!(rendered[user + 1].trim().is_empty(), "tool gap must be blank: {rendered:?}");

    for kind in [InlineMessageKind::Tool, InlineMessageKind::Pty] {
        let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
        session.push_line(kind, vec![make_segment("• Ran first-command")]);
        let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

        session.push_line(kind, vec![make_segment("  └ second output")]);
        let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);
        let first = rendered
            .iter()
            .position(|line| line.contains("first-command"))
            .expect("first command");
        let second = rendered
            .iter()
            .position(|line| line.contains("second output"))
            .expect("second output");
        assert_eq!(
            second,
            first + 1,
            "{kind:?} block must not keep stale trailing spacing after an appended line: {rendered:?}"
        );
    }
}

#[test]
fn cached_reflow_drops_replaced_info_group_details() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Info, vec![make_segment("First info detail")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("Removed info detail")]);
    let _ = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    session.replace_last(1, InlineMessageKind::Agent, vec![vec![make_segment("Replacement answer")]], None);
    let rendered = rendered_transcript_widget_lines(&mut session, VIEW_WIDTH, VIEW_ROWS);

    assert!(
        rendered.iter().any(|line| line.contains("First info detail")),
        "remaining detail is missing: {rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains("Replacement answer")),
        "replacement is missing: {rendered:?}"
    );
    assert!(
        rendered.iter().all(|line| !line.contains("Removed info detail")),
        "cached info-box head retained a replaced detail: {rendered:?}"
    );
}

#[test]
fn agent_followed_by_user_has_single_blank_before_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("the answer")]);
    session.push_line(InlineMessageKind::User, vec![make_segment("follow-up")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let answer = texts.iter().position(|text| text.contains("the answer")).expect("agent answer");
    let divider = texts
        .iter()
        .position(|text| !text.is_empty() && text.chars().all(|ch| ch == '─'))
        .expect("user divider");
    assert!(divider > answer, "divider should follow the answer, got {texts:?}");
    let gap = &texts[answer + 1..divider];
    assert_eq!(gap.len(), 1, "expected exactly one blank row before the divider, got {texts:?}");
    assert!(gap[0].trim().is_empty(), "gap row should be blank, got {texts:?}");
}

#[test]
fn agent_trailing_blank_lines_do_not_stack_with_turn_gap() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("the answer\n\n")]);
    session.push_line(InlineMessageKind::User, vec![make_segment("follow-up")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let answer = texts.iter().position(|text| text.contains("the answer")).expect("agent answer");
    let divider = texts
        .iter()
        .position(|text| !text.is_empty() && text.chars().all(|ch| ch == '─'))
        .expect("user divider");
    let gap = &texts[answer + 1..divider];
    assert_eq!(gap.len(), 1, "content trailing blanks must not stack with the turn gap, got {texts:?}");
    assert!(gap[0].trim().is_empty(), "gap row should be blank, got {texts:?}");
}

#[test]
fn agent_trailing_blanks_do_not_stack_with_tool_gap() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("lead-in prose\n\n")]);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let prose = texts
        .iter()
        .position(|text| text.contains("lead-in prose"))
        .expect("agent prose");
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    let gap = &texts[prose + 1..tool];
    assert_eq!(gap.len(), 1, "content trailing blanks must not stack with the tool gap, got {texts:?}");
    assert!(gap[0].trim().is_empty(), "gap row should be blank, got {texts:?}");
}

#[test]
fn user_followed_by_tool_has_single_blank() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::User, vec![make_segment("run the check")]);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let user = texts.iter().position(|text| text.contains("run the check")).expect("user text");
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    assert!(tool > user, "tool header should follow the user text, got {texts:?}");
    let gap = &texts[user + 1..tool];
    assert_eq!(gap.len(), 1, "expected exactly one blank row, got {texts:?}");
    assert!(gap[0].trim().is_empty(), "gap row should be blank, got {texts:?}");
}

#[test]
fn policy_run_before_tool_has_single_blank() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Policy, vec![make_segment("thinking aloud")]);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let policy = texts
        .iter()
        .position(|text| text.contains("thinking aloud"))
        .expect("policy text");
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    assert!(tool > policy, "tool header should follow the policy text, got {texts:?}");
    let gap = &texts[policy + 1..tool];
    assert_eq!(gap.len(), 1, "expected exactly one blank row, got {texts:?}");
    assert!(gap[0].trim().is_empty(), "gap row should be blank, got {texts:?}");
}

#[test]
fn spacing_zero_keeps_minimum_gap_before_divider_and_tool_blocks() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.appearance.message_block_spacing = 0;
    session.push_line(InlineMessageKind::Agent, vec![make_segment("the answer")]);
    session.push_line(InlineMessageKind::User, vec![make_segment("follow-up")]);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let answer = texts.iter().position(|text| text.contains("the answer")).expect("agent answer");
    let divider = texts
        .iter()
        .position(|text| !text.is_empty() && text.chars().all(|ch| ch == '─'))
        .expect("user divider");
    let gap = &texts[answer + 1..divider];
    assert_eq!(gap.len(), 1, "agent gap must keep its min-1 floor, got {texts:?}");

    let user = texts.iter().position(|text| text.contains("follow-up")).expect("user text");
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    let tool_gap = &texts[user + 1..tool];
    assert_eq!(tool_gap.len(), 1, "tool top must keep its min-1 floor, got {texts:?}");
}

#[test]
fn spacing_two_does_not_stack_beyond_config() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.appearance.message_block_spacing = 2;
    session.push_line(InlineMessageKind::Agent, vec![make_segment("the answer\n\n")]);
    session.push_line(InlineMessageKind::User, vec![make_segment("follow-up")]);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let answer = texts.iter().position(|text| text.contains("the answer")).expect("agent answer");
    let divider = texts
        .iter()
        .position(|text| !text.is_empty() && text.chars().all(|ch| ch == '─'))
        .expect("user divider");
    let gap = &texts[answer + 1..divider];
    assert_eq!(gap.len(), 2, "content blanks must not stack on top of spacing 2, got {texts:?}");

    let user = texts.iter().position(|text| text.contains("follow-up")).expect("user text");
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    let tool_gap = &texts[user + 1..tool];
    assert_eq!(tool_gap.len(), 2, "tool top follows config without doubling, got {texts:?}");
}

#[test]
fn pty_wrapped_lines_keep_hanging_left_padding() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    push_pty_line(&mut session, "  └ this PTY output line wraps on narrow widths");

    let rendered = session.reflow_pty_lines(0, 18);
    let content_lines: Vec<String> = rendered
        .iter()
        .map(|line| line_text(&line.line))
        .filter(|text| !text.is_empty())
        .collect();
    assert!(content_lines.len() >= 2, "expected wrapped PTY output, got {} content line(s)", content_lines.len());

    let first = &content_lines[0];
    let second = &content_lines[1];

    // No left gutter – PTY body is flush, tree marker provides its own indent.
    assert!(first.starts_with("  └ "), "first line was: {first:?}");
    assert!(second.starts_with("    "), "wrapped line should keep hanging indent, got: {second:?}");
}

#[test]
fn pty_wrapped_lines_do_not_exceed_viewport_width() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    push_pty_line(&mut session, "  └ this PTY output line wraps on narrow widths");

    let width = 18usize;
    let rendered = session.reflow_pty_lines(0, width as u16);
    for line in rendered {
        let line_width: usize = line.line.spans.iter().map(|span| span.width()).sum();
        assert!(line_width <= width, "wrapped PTY line exceeded viewport width: {line_width} > {width}",);
    }
}

#[test]
fn tool_diff_numbered_lines_keep_hanging_indent_when_wrapped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Tool,
        vec![InlineSegment {
            text: "459 + let digits_len = digits.chars().take_while(|c| c.is_ascii_digit()).count();".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );

    let rendered = session.reflow_transcript_lines(40);
    let content_lines: Vec<String> = rendered.iter().map(line_text).filter(|text| !text.is_empty()).collect();
    assert!(
        content_lines.len() >= 2,
        "expected wrapped tool diff output, got {} content line(s)",
        content_lines.len()
    );

    let first = &content_lines[0];
    let second = &content_lines[1];

    assert!(first.contains("459 + "), "first line should include diff gutter: {first:?}");
    assert!(
        second.starts_with("          "),
        "wrapped line should keep hanging indent after tool prefix, got: {second:?}"
    );
}

#[test]
fn compact_tinted_diff_rows_keep_hanging_indent_when_wrapped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    // Compact layout: one leading marker cell, no +/- gutter or line numbers.
    let body = format!(" | {} | {} |", "What can go wrong", "How VT Code responds ".repeat(6));
    let style = InlineTextStyle {
        color: Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack)),
        bg_color: Some(anstyle::Color::Ansi(anstyle::AnsiColor::Green)),
        ..InlineTextStyle::default()
    };
    session.push_line(InlineMessageKind::Tool, vec![InlineSegment { text: body.clone(), style: Arc::new(style) }]);

    let rendered = session.reflow_transcript_lines(40);
    let content_lines: Vec<String> = rendered.iter().map(line_text).filter(|text| !text.trim().is_empty()).collect();
    assert!(content_lines.len() >= 2, "expected wrapped compact diff row: {content_lines:?}");
    assert!(content_lines[0].contains("What can go wrong"));
    // Tool blocks already pad continuations; require hang depth for the
    // compact marker cell plus tool prefix, not a vacuous single space.
    assert!(
        content_lines[1].starts_with("  ") && !content_lines[1].starts_with("   |"),
        "compact continuation should hang under the marker/tool prefix, got: {:?}",
        content_lines[1]
    );
}

#[test]
fn agent_numbered_code_lines_keep_hanging_indent_when_wrapped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Agent,
        vec![
            InlineSegment {
                text: " 12  ".to_string(),
                style: Arc::new(InlineTextStyle {
                    effects: anstyle::Effects::DIMMED,
                    ..InlineTextStyle::default()
                }),
            },
            make_segment("fn wrapped_diff_continuation_prefix(line_text: &str) -> Option<String> {"),
        ],
    );

    let rendered = session.reflow_transcript_lines(36);
    let content_lines: Vec<String> = rendered.iter().map(line_text).filter(|text| !text.trim().is_empty()).collect();
    assert!(content_lines.len() >= 2, "expected wrapped code line, got: {content_lines:?}");

    let first = &content_lines[0];
    let second = &content_lines[1];
    // Zero-indent: only the code gutter hangs on continuation rows.
    let expected_prefix = " ".repeat(" 12  ".chars().count());

    assert!(first.contains("12  fn wrapped_diff"), "first line was: {first:?}");
    assert!(
        second.starts_with(&expected_prefix),
        "wrapped code continuation should keep gutter indent, got: {second:?}"
    );
}

#[test]
fn agent_omitted_code_lines_keep_hanging_indent_when_wrapped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Agent,
        vec![InlineSegment {
            text: "21-421  … [+400 lines omitted; use read_file with offset/limit (1-indexed line numbers) for full content]".to_string(),
            style: Arc::new(InlineTextStyle {
                effects: anstyle::Effects::DIMMED,
                ..InlineTextStyle::default()
            }),
        }],
    );

    let rendered = session.reflow_transcript_lines(52);
    let content_lines: Vec<String> = rendered.iter().map(line_text).filter(|text| !text.trim().is_empty()).collect();
    assert!(content_lines.len() >= 2, "expected wrapped omitted line, got: {content_lines:?}");

    let first = &content_lines[0];
    let second = &content_lines[1];
    // Zero-indent: only the code gutter hangs on continuation rows.
    let expected_prefix = " ".repeat("21-421  ".chars().count());

    assert!(first.contains("21-421"), "first line was: {first:?}");
    assert!(first.contains("…"), "first line was: {first:?}");
    assert!(first.contains("[+400"), "first line was: {first:?}");
    assert!(
        second.starts_with(&expected_prefix),
        "wrapped omitted-line continuation should keep gutter indent, got: {second:?} expected: {expected_prefix:?}"
    );
}

#[test]
fn pty_command_header_verb_uses_primary_color_bullet_uses_theme_foreground() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Pty,
        vec![
            InlineSegment {
                text: "• Ran".to_string(),
                style: Arc::new(InlineTextStyle::default()),
            },
            InlineSegment {
                text: " cat file".to_string(),
                style: Arc::new(InlineTextStyle::default()),
            },
        ],
    );

    let rendered = session.reflow_pty_lines(0, 80);
    let spans: Vec<_> = rendered.iter().flat_map(|line| line.line.spans.iter()).collect();

    // Bullet "• " → theme foreground, no bold
    let bullet_span = spans.iter().find(|s| s.content.as_ref() == "• ").expect("expected • span");
    assert!(
        !bullet_span.style.add_modifier.contains(Modifier::BOLD),
        "• bullet should NOT be bold, got modifiers: {:?}",
        bullet_span.style.add_modifier,
    );

    let theme_fg = InlineTheme::default().foreground.map(ratatui_color_from_ansi);
    assert_eq!(bullet_span.style.fg, theme_fg, "bullet fg should be theme foreground");

    // Verb "Ran" → primary/neutral header color + bold
    let verb_span = spans.iter().find(|s| s.content.as_ref() == "Ran").expect("expected verb span");
    assert!(
        verb_span.style.add_modifier.contains(Modifier::BOLD),
        "verb should be bold, got modifiers: {:?}",
        verb_span.style.add_modifier,
    );
    let theme_primary = InlineTheme::default()
        .primary
        .or(InlineTheme::default().foreground)
        .map(ratatui_color_from_ansi);
    assert_eq!(verb_span.style.fg, theme_primary, "Ran verb should use the header primary color");
}

#[test]
fn pty_command_header_removes_dim_from_all_header_spans() {
    let foreground = AnsiColorEnum::Rgb(RgbColor(0xCC, 0xCC, 0xCC));
    let subdued = AnsiColorEnum::Rgb(RgbColor(0x66, 0x66, 0x66));
    let dimmed = Arc::new(InlineTextStyle {
        color: Some(subdued),
        effects: anstyle::Effects::DIMMED,
        ..InlineTextStyle::default()
    });
    let mut session = Session::new(
        InlineTheme {
            foreground: Some(foreground),
            pty_body: Some(subdued),
            tool_body: Some(subdued),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![
            InlineSegment {
                text: "• ".to_string(), style: Arc::clone(&dimmed)
            },
            InlineSegment {
                text: "Ran".to_string(),
                style: Arc::clone(&dimmed),
            },
            InlineSegment { text: " sed -n 1,260p".to_string(), style: dimmed },
        ],
    );

    let rendered = session.reflow_pty_lines(0, 80);
    let command_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("sed"))
        .expect("expected command header span");

    assert!(!command_span.style.add_modifier.contains(Modifier::DIM));
    assert_eq!(command_span.style.fg, Some(Color::Rgb(0xCC, 0xCC, 0xCC)));
}

#[test]
fn pty_command_headers_remain_opaque_after_prior_output() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "• Ran first-command".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "    first output".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "• Ran second-command".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );

    let rendered = session.reflow_pty_lines(2, 80);
    let second_header = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("second-command"))
        .expect("expected second command header");

    assert!(!second_header.style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn pty_command_header_preserves_status_color_on_bullet() {
    let foreground = AnsiColorEnum::Rgb(RgbColor(0xCC, 0xCC, 0xCC));
    let mut session = Session::new(
        InlineTheme {
            foreground: Some(foreground),
            tool_body: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Green)),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "• Ran find src/agent -type f".to_string(),
            style: Arc::new(InlineTextStyle {
                color: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Red)),
                ..InlineTextStyle::default()
            }),
        }],
    );

    let rendered = session.reflow_pty_lines(0, 80);
    let bullet_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.as_ref() == "• ")
        .expect("expected • span");
    let command_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("find"))
        .expect("expected command span");

    assert_eq!(bullet_span.style.fg, Some(Color::Red));
    assert_eq!(command_span.style.fg, Some(Color::Rgb(0xCC, 0xCC, 0xCC)));
}

#[test]
fn pty_multiline_command_header_body_matches_single_line_tool_header() {
    // Screenshot 2026-09-18: a wrapped `• Ran ...` header with `│`
    // continuations rendered shell token colors (bold `cargo`/`echo`) while
    // single-line Tool headers stay uniform. Both header lines must render
    // as uniform non-bold foreground text: status bullet, bold verb only.
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    let bold_command = Arc::new(InlineTextStyle {
        color: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Green)),
        effects: anstyle::Effects::BOLD,
        ..InlineTextStyle::default()
    });
    let option = Arc::new(InlineTextStyle {
        color: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Red)),
        ..InlineTextStyle::default()
    });
    let plain = Arc::new(InlineTextStyle::default());
    // Shell-highlighted multi-segment header, as produced by live PTY
    // segment splitting: bullet, verb, body with bold command + option.
    session.push_line(
        InlineMessageKind::Pty,
        vec![
            InlineSegment {
                text: "• ".to_string(), style: Arc::clone(&plain)
            },
            InlineSegment { text: "Ran".to_string(), style: Arc::clone(&plain) },
            InlineSegment { text: " ".to_string(), style: Arc::clone(&plain) },
            InlineSegment {
                text: "cargo".to_string(),
                style: Arc::clone(&bold_command),
            },
            InlineSegment {
                text: " nextest run".to_string(),
                style: Arc::clone(&plain),
            },
            InlineSegment {
                text: " -p".to_string(),
                style: Arc::clone(&option),
            },
        ],
    );
    // Wrapped `│` continuation carrying shell token styles, including bold.
    session.push_line(
        InlineMessageKind::Pty,
        vec![
            InlineSegment { text: "  ".to_string(), style: Arc::clone(&plain) },
            InlineSegment { text: "│".to_string(), style: Arc::clone(&plain) },
            InlineSegment { text: " ".to_string(), style: Arc::clone(&plain) },
            InlineSegment {
                text: "/tmp/theme6.log".to_string(),
                style: Arc::clone(&plain),
            },
            InlineSegment { text: " ".to_string(), style: Arc::clone(&plain) },
            InlineSegment {
                text: "echo".to_string(),
                style: Arc::clone(&bold_command),
            },
        ],
    );

    let theme_fg = InlineTheme::default().foreground.map(ratatui_color_from_ansi);
    let first = session.reflow_pty_lines(0, 80);
    let first_spans: Vec<_> = first.iter().flat_map(|line| line.line.spans.iter()).collect();
    let first_text: String = first_spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(first_text.contains("• Ran cargo nextest run -p"), "header text preserved, got: {first_text:?}");
    for span in &first_spans {
        let content: &str = span.content.as_ref();
        if content.is_empty() {
            continue;
        }
        if content == "Ran" {
            assert!(span.style.add_modifier.contains(Modifier::BOLD), "verb stays bold");
        } else if content == "• " {
            assert!(
                !span.style.add_modifier.contains(Modifier::BOLD),
                "bullet not bold, got {:?}",
                span.style.add_modifier
            );
        } else {
            assert!(
                !span.style.add_modifier.contains(Modifier::BOLD),
                "header body not bold, got {:?} for {content:?}",
                span.style.add_modifier
            );
            assert_eq!(span.style.fg, theme_fg, "header body uses foreground, got {:?} for {content:?}", span.style.fg);
        }
    }

    let second = session.reflow_pty_lines(1, 80);
    let second_spans: Vec<_> = second.iter().flat_map(|line| line.line.spans.iter()).collect();
    let second_text: String = second_spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(second_text.contains("/tmp/theme6.log"), "continuation path kept, got: {second_text:?}");
    assert!(second_text.contains("echo"), "continuation command kept, got: {second_text:?}");
    for span in &second_spans {
        let content: &str = span.content.as_ref();
        if content.trim().is_empty() {
            continue;
        }
        assert!(
            !span.style.add_modifier.contains(Modifier::BOLD),
            "continuation not bold, got {:?} for {content:?}",
            span.style.add_modifier
        );
        assert_eq!(span.style.fg, theme_fg, "continuation uses foreground, got {:?} for {content:?}", span.style.fg);
    }
}

#[test]
fn pty_pipe_prefixed_output_after_output_keeps_pty_style() {
    // A `│`-prefixed row that follows output (not a header) must keep PTY
    // body styling instead of chaining onto the uniform header treatment.
    let foreground = AnsiColorEnum::Rgb(RgbColor(0xCC, 0xCC, 0xCC));
    let pty_body = AnsiColorEnum::Rgb(RgbColor(0x66, 0x66, 0x66));
    let mut session = Session::new(
        InlineTheme {
            foreground: Some(foreground),
            pty_body: Some(pty_body),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment("  └ some output")]);
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment("  │ tree output")]);

    let rendered = session.reflow_pty_lines(1, 80);
    let tree_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("tree"))
        .expect("expected tree span");
    assert_eq!(tree_span.style.fg, Some(Color::Rgb(0x66, 0x66, 0x66)));
}

#[test]
fn pty_pipe_prefixed_output_after_wrapped_header_keeps_pty_style() {
    let foreground = AnsiColorEnum::Rgb(RgbColor(0xCC, 0xCC, 0xCC));
    let pty_body = AnsiColorEnum::Rgb(RgbColor(0x66, 0x66, 0x66));
    let mut session = Session::new(
        InlineTheme {
            foreground: Some(foreground),
            pty_body: Some(pty_body),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment("• Ran long command")]);
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment("  │ wrapped argument")]);
    // The four-space output gutter is distinct from the two-space header
    // continuation prefix, even when the program output starts with `│`.
    session.push_line(InlineMessageKind::Pty, vec![make_pty_segment("    │ program output")]);

    let rendered = session.reflow_pty_lines(2, 80);
    let output_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("program output"))
        .expect("expected pipe-prefixed output span");
    assert_eq!(output_span.style.fg, Some(Color::Rgb(0x66, 0x66, 0x66)));
}

#[test]
fn tool_command_header_does_not_use_accent_tool_body_as_fallback() {
    let foreground = AnsiColorEnum::Rgb(RgbColor(0xCC, 0xCC, 0xCC));
    let mut session = Session::new(
        InlineTheme {
            foreground: Some(foreground),
            tool_body: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Green)),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(
        InlineMessageKind::Tool,
        vec![InlineSegment {
            text: "• Ran find src/agent -type f".to_string(),
            style: Arc::new(InlineTextStyle {
                color: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Green)),
                ..InlineTextStyle::default()
            }),
        }],
    );

    let rendered = session.reflow_transcript_lines(80);
    let verb_span = rendered
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Ran")
        .expect("expected Ran span");
    let command_span = rendered
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("find"))
        .expect("expected command span");

    assert_eq!(verb_span.style.fg, Some(Color::Rgb(0xCC, 0xCC, 0xCC)));
    assert_eq!(command_span.style.fg, Some(Color::Rgb(0xCC, 0xCC, 0xCC)));
    assert!(!verb_span.style.add_modifier.contains(Modifier::DIM));
    assert!(!command_span.style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn tool_output_is_not_dimmed_and_tool_header_is_opaque() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Tool,
        vec![InlineSegment {
            text: "• Ran sed -n 1,260p\n    output line".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );

    let rendered = session.reflow_transcript_lines(80);
    let header = rendered
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == "Ran")
        .expect("expected tool header");
    let output = rendered
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.contains("output line"))
        .expect("expected tool output");

    assert!(!header.style.add_modifier.contains(Modifier::DIM));
    assert!(!output.style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn exit_status_row_uses_success_and_error_accents() {
    let theme = InlineTheme {
        foreground: Some(anstyle::AnsiColor::White.into()),
        background: Some(anstyle::AnsiColor::Black.into()),
        primary: Some(anstyle::AnsiColor::Green.into()),
        error: Some(anstyle::AnsiColor::Red.into()),
        ..InlineTheme::default()
    };

    let badge_foreground = |text: &str| {
        let mut session = Session::new(theme.clone(), None, VIEW_ROWS);
        session.push_line(
            InlineMessageKind::Tool,
            vec![InlineSegment {
                text: text.to_string(),
                style: Arc::new(InlineTextStyle::default()),
            }],
        );
        let rendered = session.reflow_transcript_lines(80);
        let badge = rendered
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains(text))
            .unwrap_or_else(|| panic!("expected exit badge span for {text}"));
        assert!(!badge.style.add_modifier.contains(Modifier::DIM), "exit badge must not be dimmed: {text}");
        badge.style.fg
    };

    // Clean exit resolves to the success accent; any non-zero code to the error
    // accent. Both stay at normal intensity and keep the `✓ exit N` text.
    assert_eq!(badge_foreground("✓ exit 0"), Some(ratatui_color_from_ansi(anstyle::AnsiColor::Green.into())));
    assert_eq!(badge_foreground("✓ exit 1"), Some(ratatui_color_from_ansi(anstyle::AnsiColor::Red.into())));
}

#[test]
fn pty_lines_use_subdued_foreground() {
    let theme = InlineTheme {
        foreground: Some(AnsiColorEnum::Rgb(RgbColor(0xEE, 0xEE, 0xEE))),
        background: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Black)),
        pty_body: Some(AnsiColorEnum::Rgb(RgbColor(0x7A, 0x7A, 0x7A))),
        ..InlineTheme::default()
    };
    let mut session = Session::new(theme, None, VIEW_ROWS);
    push_pty_line(&mut session, "plain pty output");

    let rendered = session.reflow_pty_lines(0, 80);
    let body_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("plain pty output"))
        .expect("expected PTY body span");
    assert_eq!(
        body_span.style.fg,
        Some(Color::Rgb(0x7A, 0x7A, 0x7A)),
        "PTY body span should use the subdued pty_body foreground"
    );
    assert!(!body_span.style.add_modifier.contains(Modifier::DIM), "PTY body should render at normal intensity");
}

#[test]
fn assistant_text_is_brighter_than_pty_output() {
    let agent_fg = Color::Rgb(0xEE, 0xEE, 0xEE);
    let pty_fg = Color::Rgb(0x7A, 0x7A, 0x7A);
    let theme = InlineTheme {
        foreground: Some(AnsiColorEnum::Rgb(RgbColor(0xEE, 0xEE, 0xEE))),
        pty_body: Some(AnsiColorEnum::Rgb(RgbColor(0x7A, 0x7A, 0x7A))),
        ..Default::default()
    };

    let mut session = Session::new(theme, None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Agent,
        vec![InlineSegment {
            text: "assistant reply".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "pty output".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    );

    let agent_spans = session.render_message_spans(0);
    let agent_body = agent_spans
        .iter()
        .find(|span| span.content.contains("assistant reply"))
        .expect("expected assistant body span");
    assert_eq!(agent_body.style.fg, Some(agent_fg));

    let pty_rendered = session.reflow_pty_lines(1, 80);
    let pty_body = pty_rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("pty output"))
        .expect("expected PTY body span");
    assert_eq!(pty_body.style.fg, Some(pty_fg));
    assert!(!pty_body.style.add_modifier.contains(Modifier::DIM));
    assert_ne!(agent_body.style.fg, pty_body.style.fg);
}

#[test]
fn pty_ansi_detail_colors_keep_full_intensity() {
    let mut session = Session::new(
        InlineTheme {
            background: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::Black)),
            pty_body: Some(AnsiColorEnum::Rgb(RgbColor(0x7A, 0x7A, 0x7A))),
            ..Default::default()
        },
        None,
        VIEW_ROWS,
    );
    session.push_line(
        InlineMessageKind::Pty,
        vec![InlineSegment {
            text: "SUCCESS: Code formatting is correct!".to_string(),
            style: Arc::new(InlineTextStyle {
                color: Some(AnsiColorEnum::Ansi(anstyle::AnsiColor::BrightGreen)),
                ..InlineTextStyle::default()
            }),
        }],
    );

    let rendered = session.reflow_pty_lines(0, 80);
    let body_span = rendered
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .find(|span| span.content.contains("SUCCESS"))
        .expect("expected ANSI-colored PTY detail span");

    assert!(body_span.style.fg.is_some());
    assert_ne!(
        body_span.style.fg,
        Some(Color::Rgb(55, 165, 55)),
        "explicit PTY detail colors must no longer be attenuated toward the background"
    );
    assert!(!body_span.style.add_modifier.contains(Modifier::DIM));
}

#[test]
fn pty_scroll_preserves_order() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    for index in 0..200 {
        let label = format!("{LABEL_PREFIX}-{index}");
        push_pty_line(&mut session, &label);
    }

    let bottom_view = visible_transcript(&mut session);
    assert!(
        bottom_view.iter().any(|line| line.contains(&format!("{LABEL_PREFIX}-199"))),
        "bottom view should include latest PTY line"
    );

    for _ in 0..200 {
        session.scroll_page_up();
        if session.scroll_manager.offset() == session.current_max_scroll_offset() {
            break;
        }
    }

    let top_view = visible_transcript(&mut session);
    assert!(
        (0..=5).any(|index| top_view.iter().any(|line| line.contains(&format!("{LABEL_PREFIX}-{index}")))),
        "top view should include earliest PTY lines"
    );
    assert!(
        top_view.iter().all(|line| !line.contains(&format!("{LABEL_PREFIX}-199"))),
        "top view should not include latest PTY line"
    );
}

#[test]
fn streaming_state_starts_false() {
    let session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    assert!(!session.is_streaming_final_answer);
}

#[test]
fn streaming_state_set_on_agent_append_line() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    assert!(!session.is_streaming_final_answer);

    session.handle_command(agent_append_line_command("Hello"));

    assert!(session.is_streaming_final_answer);
}

#[test]
fn streaming_state_set_on_agent_inline() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    assert!(!session.is_streaming_final_answer);

    session.handle_command(InlineCommand::Inline {
        kind: InlineMessageKind::Agent,
        segment: InlineSegment {
            text: "Hello".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        },
    });

    assert!(session.is_streaming_final_answer);
}

#[test]
fn streaming_state_cleared_on_turn_completion() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.handle_command(agent_append_line_command("Hello"));
    assert!(session.is_streaming_final_answer);

    session.handle_command(InlineCommand::SetInputStatus { left: None, right: None });

    assert!(!session.is_streaming_final_answer);
}

#[test]
fn streaming_state_not_cleared_on_status_update_with_content() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.handle_command(agent_append_line_command("Hello"));
    assert!(session.is_streaming_final_answer);

    session.handle_command(InlineCommand::SetInputStatus { left: Some("Working...".to_string()), right: None });

    assert!(session.is_streaming_final_answer);
}

#[test]
fn non_agent_messages_dont_trigger_streaming_state() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.handle_command(InlineCommand::AppendLine {
        kind: InlineMessageKind::User,
        segments: vec![InlineSegment {
            text: "Hello".to_string(),
            style: Arc::new(InlineTextStyle::default()),
        }],
    });

    assert!(!session.is_streaming_final_answer);
}

#[test]
fn empty_agent_segments_dont_trigger_streaming_state() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    session.handle_command(InlineCommand::AppendLine { kind: InlineMessageKind::Agent, segments: vec![] });

    assert!(!session.is_streaming_final_answer);
}

/// Regression for tool-summary detail grouping: a header (`• Search code`)
/// with its `  └ ` details must render as a tight block – only one blank
/// line before the header and one after the last detail, no gaps between
/// header/details. This prevents the “too blank” double-gap reported in
/// the screenshot (header → details and detail → detail should be tight).
#[test]
fn tool_summary_details_are_tightly_grouped() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Info, vec![make_segment("• Search code for core agent loop")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("  └ File types: rs")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("  └ Max results: 25")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("  └ Path: crates/codegen/vtcode-core")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("  └ Result types: definition, path")]);
    let width = 80u16;
    let lines = session.reflow_transcript_lines(width);
    // Header + 4 details should be tight: only top/bottom blanks, no gaps inside.
    assert_eq!(lines.len(), 7);
    let texts: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(texts[0].trim().is_empty()); // top
    assert_eq!(texts[1], "• Search code for core agent loop");
    assert_eq!(texts[2], "  └ File types: rs");
    assert_eq!(texts[3], "  └ Max results: 25");
    assert_eq!(texts[4], "  └ Path: crates/codegen/vtcode-core");
    assert_eq!(texts[5], "  └ Result types: definition, path");
    assert!(texts[6].trim().is_empty()); // bottom
}

/// Agent pre-announcement → tool block keeps exactly one blank line so the
/// message stays visually distinct from its work log (single ownership: the
/// agent suppresses its trailing gap and the tool block owns the top gap).
#[test]
fn agent_to_tool_has_single_blank() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(
        InlineMessageKind::Agent,
        vec![make_segment(
            "Got the doc overview. Now let me see the actual loop implementation.",
        )],
    );
    session.push_line(InlineMessageKind::Info, vec![make_segment("• Ran 2 commands")]);
    let width = 80u16;
    let lines = session.reflow_transcript_lines(width);
    let texts: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    // Find the Agent line and the following Ran line.
    let agent_idx = texts.iter().position(|t| t.contains("Got the doc")).expect("agent line");
    let ran_idx = texts.iter().position(|t| t.contains("Ran 2 commands")).expect("ran line");
    // One blank line between them so prose stays distinct from its work log.
    assert_eq!(ran_idx, agent_idx + 2, "expected one blank row, got texts: {texts:?}");
    assert!(texts[agent_idx + 1].trim().is_empty(), "gap row should be blank, got texts: {texts:?}");
    // Also verify the pure policy: Agent -> Tool adds the tool top gap.
    use crate::tui::core_tui::session::message::MessageLine;
    use crate::tui::core_tui::session::reflow::should_add_tool_block_top_spacing_for_kinds;
    use crate::tui::core_tui::types::InlineMessageKind as Kind;
    let agent_line = MessageLine {
        kind: Kind::Agent,
        segments: vec![make_segment("hello")],
        link_ranges: vec![],
        revision: 0,
    };
    let tool_line = MessageLine {
        kind: Kind::Info,
        segments: vec![make_segment("• Search code")],
        link_ranges: vec![],
        revision: 0,
    };
    assert!(should_add_tool_block_top_spacing_for_kinds(&agent_line, &tool_line));
}

// ---------------------------------------------------------------------------
// Agent section dividers (Tool/Pty -> Agent prose breaks)
// ---------------------------------------------------------------------------

fn divider_positions(texts: &[String]) -> Vec<usize> {
    texts
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.is_empty() && text.chars().all(|ch| ch == '─'))
        .map(|(idx, _)| idx)
        .collect()
}

#[test]
fn tool_followed_by_agent_has_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("synthesis of the check")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let tool = texts
        .iter()
        .position(|text| text.contains("Ran cargo check"))
        .expect("tool header");
    let answer = texts.iter().position(|text| text.contains("synthesis")).expect("agent answer");
    let dividers = divider_positions(&texts);
    assert_eq!(dividers.len(), 1, "expected one section divider, got {texts:?}");
    let divider = dividers[0];
    assert!(divider > tool && divider < answer, "divider should sit between tool and agent, got {texts:?}");
    let gap = &texts[tool + 1..divider];
    assert_eq!(gap.len(), 1, "expected exactly one blank row before the divider, got {texts:?}");
    assert!(gap[0].trim().is_empty());
}

#[test]
fn pty_followed_by_agent_has_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    push_pty_line(&mut session, "• Ran git status");
    push_pty_line(&mut session, "M src/main.rs");
    session.push_line(InlineMessageKind::Agent, vec![make_segment("working tree is dirty")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let dividers = divider_positions(&texts);
    assert_eq!(dividers.len(), 1, "pty burst should close with one divider, got {texts:?}");
    let answer = texts.iter().position(|text| text.contains("dirty")).expect("agent answer");
    assert!(dividers[0] < answer);
}

#[test]
fn info_summary_followed_by_agent_has_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Search code")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("  └ Path: src/")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("found the loop")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let dividers = divider_positions(&texts);
    assert_eq!(dividers.len(), 1, "tool detail tail should still break the section, got {texts:?}");
}

#[test]
fn agent_separates_from_tool_and_pty_blocks() {
    for kind in [InlineMessageKind::Tool, InlineMessageKind::Pty] {
        let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
        session.push_line(InlineMessageKind::Agent, vec![make_segment("lead-in prose")]);
        let header = if kind == InlineMessageKind::Tool {
            "• Ran cargo check"
        } else {
            "• Ran git status"
        };
        if kind == InlineMessageKind::Tool {
            session.push_line(kind, vec![make_segment(header)]);
        } else {
            push_pty_line(&mut session, header);
        }

        let rendered = session.reflow_transcript_lines(80);
        let texts: Vec<String> = rendered.iter().map(line_text).collect();
        let prose = texts
            .iter()
            .position(|text| text.contains("lead-in prose"))
            .expect("agent prose");
        let block = texts.iter().position(|text| text.contains("Ran")).expect("tool header");
        assert_eq!(block, prose + 2, "prose must keep one blank row before {kind:?} block, got {texts:?}");
        assert!(texts[prose + 1].trim().is_empty(), "gap row should be blank, got {texts:?}");
    }
}

#[test]
fn agent_prose_separates_from_work_log_with_rule_section_break() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("Links all resolve.")]);
    session.push_line(InlineMessageKind::Info, vec![make_segment("• Ran 5 commands")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("The provider list is richer.")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    let prose = texts
        .iter()
        .position(|text| text.contains("Links all resolve."))
        .expect("agent prose");
    let hint = texts
        .iter()
        .position(|text| text.contains("Ran 5 commands"))
        .expect("work-log hint");
    assert_eq!(hint, prose + 2, "prose must keep one blank row before its work log, got {texts:?}");
    assert!(texts[prose + 1].trim().is_empty(), "gap row should be blank, got {texts:?}");
    let dividers = divider_positions(&texts);
    assert_eq!(dividers.len(), 1, "one rule separates the sections, got {texts:?}");
    let follow = texts
        .iter()
        .position(|text| text.contains("provider list"))
        .expect("next prose");
    assert_eq!(follow, dividers[0] + 1, "next prose must hug the rule, got {texts:?}");
}

#[test]
fn agent_followed_by_agent_has_no_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("first paragraph")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("second paragraph")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    assert!(divider_positions(&texts).is_empty(), "consecutive agent lines must not divide, got {texts:?}");
}

#[test]
fn empty_agent_after_tool_has_no_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("   ")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    assert!(divider_positions(&texts).is_empty(), "empty agent rows must not add chrome, got {texts:?}");
}

#[test]
fn policy_followed_by_agent_has_no_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Policy, vec![make_segment("thinking aloud")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("the answer")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    assert!(divider_positions(&texts).is_empty(), "reasoning -> content flow must stay undivided, got {texts:?}");
}

#[test]
fn user_followed_by_agent_has_no_section_divider() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.push_line(InlineMessageKind::User, vec![make_segment("run the check")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("on it")]);

    let rendered = session.reflow_transcript_lines(80);
    let texts: Vec<String> = rendered.iter().map(line_text).collect();
    // The single divider belongs to the User turn itself; no extra section
    // divider may appear between User and the following Agent.
    let dividers = divider_positions(&texts);
    assert_eq!(dividers.len(), 1, "only the user turn divider should exist, got {texts:?}");
    let user = texts.iter().position(|text| text.contains("run the check")).expect("user text");
    let agent = texts.iter().position(|text| text.contains("on it")).expect("agent text");
    assert!(dividers[0] < user, "user divider should precede the user text, got {texts:?}");
    assert!(
        !dividers.iter().any(|divider| *divider > user && *divider < agent),
        "no section divider between user and agent, got {texts:?}"
    );
}

#[test]
fn section_divider_uses_muted_dim_style() {
    use anstyle::{Color as AnsiColorEnum, RgbColor};

    let muted = AnsiColorEnum::Rgb(RgbColor(0x77, 0x99, 0xAA));
    let theme = InlineTheme {
        secondary: Some(muted),
        primary: Some(AnsiColorEnum::Rgb(RgbColor(0x12, 0x34, 0x56))),
        ..Default::default()
    };
    let mut session = Session::new(theme, None, VIEW_ROWS);
    session.push_line(InlineMessageKind::Tool, vec![make_segment("• Ran cargo check")]);
    session.push_line(InlineMessageKind::Agent, vec![make_segment("synthesis")]);

    let agent_index = 1;
    let rows = session.reflow_message_lines(agent_index, 40, false);
    let divider = rows
        .iter()
        .find(|row| {
            let text: String = row.line.spans.iter().map(|span| span.content.as_ref()).collect();
            !text.is_empty() && text.chars().all(|ch| ch == '─')
        })
        .expect("section divider row");
    let expected_fg = Some(ratatui_color_from_ansi(muted));
    assert_eq!(divider.line.style.fg, expected_fg);
    assert!(divider.line.style.add_modifier.contains(Modifier::DIM));
    assert!(!divider.line.style.add_modifier.contains(Modifier::BOLD));
}

// ---------------------------------------------------------------------------
// Transcript eviction (bounded snapshot)
// ---------------------------------------------------------------------------

fn collapsed_json_payload() -> String {
    let mut json = String::from("{\n");
    let line_total = ui::INLINE_JSON_COLLAPSE_LINE_THRESHOLD + 5;
    for idx in 0..line_total {
        json.push_str(&format!("  \"key{idx}\": \"value{idx}\",\n"));
    }
    json.push_str("  \"end\": true\n}");
    json
}

#[test]
fn eviction_drops_pastes_inside_evicted_prefix_without_panicking() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    // Register a collapsed paste near the transcript front so the first
    // eviction chunk (the 1000 oldest lines) covers its line index.
    let json = collapsed_json_payload();
    let line_count = json.lines().count();
    session.append_pasted_message(InlineMessageKind::Tool, json, line_count);
    assert_eq!(session.collapsed_pastes.len(), 1);
    assert!(session.collapsed_pastes[0].line_index < ui::TUI_TRANSCRIPT_EVICT_CHUNK);

    for idx in 0..ui::TUI_TRANSCRIPT_MAX_MSGS {
        session.push_line(InlineMessageKind::Info, vec![make_segment(&format!("filler-{idx}"))]);
    }

    assert_eq!(session.lines.len(), ui::TUI_TRANSCRIPT_MAX_MSGS + 1 - ui::TUI_TRANSCRIPT_EVICT_CHUNK);
    // The placeholder line was evicted, so its paste record must be gone too.
    assert!(session.collapsed_pastes.is_empty());
}

#[test]
fn eviction_shifts_surviving_paste_indices() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);

    let lines_before_paste = 1500usize;
    for idx in 0..lines_before_paste {
        session.push_line(InlineMessageKind::Info, vec![make_segment(&format!("filler-{idx}"))]);
    }

    let json = collapsed_json_payload();
    let line_count = json.lines().count();
    session.append_pasted_message(InlineMessageKind::Tool, json, line_count);
    let original_index = session.collapsed_pastes[0].line_index;
    assert_eq!(original_index, lines_before_paste);

    let remaining = ui::TUI_TRANSCRIPT_MAX_MSGS + 1 - session.lines.len();
    for idx in 0..remaining {
        session.push_line(InlineMessageKind::Info, vec![make_segment(&format!("tail-{idx}"))]);
    }

    let shifted_index = session.collapsed_pastes[0].line_index;
    assert_eq!(shifted_index, original_index - ui::TUI_TRANSCRIPT_EVICT_CHUNK);
    let preview_line = session.lines.get(shifted_index).expect("paste placeholder survives");
    let text: String = preview_line.segments.iter().map(|segment| segment.text.as_str()).collect();
    assert!(text.contains("showing last"));
}

#[test]
fn agent_prose_links_stay_aligned_when_rows_justify() {
    let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
    session.set_workspace_root(Some(vtcode_tui_workspace_root()));
    // Plain single-style prose: rows stay single-span so mid-length rows are
    // justification candidates. The inline path must keep exact link bounds
    // even when its row is padded out to full width.
    let path = transcript_file_fixture_relative_path();
    let text = format!(
        "Verified the implementation across all of the modules and then confirmed the behavior in {path}, and continued checking the remaining pieces of the transcript afterwards today"
    );
    session.push_line(InlineMessageKind::Agent, vec![make_segment(&text)]);

    let rows = session.reflow_message_lines(0, 80, false);
    assert!(rows.len() > 1, "expected the prose to wrap");

    let mut found = 0;
    for row in &rows {
        let row_text: String = row.line.spans.iter().map(|span| span.content.as_ref()).collect();
        for link in &row.explicit_links {
            let slice = row_text.get(link.start..link.end).unwrap_or_default();
            assert_eq!(slice, path, "link underline misaligned after reflow: {slice:?} in {row_text:?}");
            found += 1;
        }
    }
    assert_eq!(found, 1, "expected the inline path to stay linked");
}

#[test]
fn tool_command_header_wraps_in_full_without_truncation() {
    // Screenshot 2026-09-24 16:37: a piped `• Ran grep ... | grep -v ...`
    // header must wrap across lines with every segment intact and no `…`.
    // Wrapping may only insert whitespace (hanging indent); comparing with
    // whitespace stripped proves no characters are lost or truncated.
    // Fixture uses the exact screenshot bytes: `||` inside the quoted pattern
    // and the backslash-escaped `\.backup` must both survive wrapping.
    let command = "grep -rn \"@vinhnx/vtcode|npm install -g||npx @vinhnx\" docs | grep -v node_modules | grep -v package-lock | grep -v \"\\.backup\"";
    let expected_flat: String = format!("• Ran {command}").chars().filter(|c| !c.is_whitespace()).collect();
    for width in [80u16, 50, 40] {
        let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
        session.push_line(InlineMessageKind::Tool, vec![make_segment(&format!("• Ran {command}"))]);
        let rows = session.reflow_transcript_lines(width);
        let joined: String = rows
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(!joined.contains('…'), "width {width}: header must not truncate, got: {joined:?}");
        let flat: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(flat, expected_flat, "width {width}: wrapped header lost content, got: {joined:?}");
        // Proper line wrapping shape: the header must actually wrap at narrow
        // widths, the first content row keeps the bullet+verb, continuations
        // hang under it (gutter + hanging indent, never a mid-token restart),
        // and no row overflows the viewport. Spacing blanks are skipped.
        let content_rows: Vec<String> = rows
            .iter()
            .map(|row| row.spans.iter().map(|span| span.content.as_ref()).collect())
            .filter(|text: &String| !text.trim().is_empty())
            .collect();
        assert!(content_rows.len() > 1, "width {width}: long header must wrap, got: {joined:?}");
        assert!(
            content_rows[0].starts_with("• Ran "),
            "width {width}: first row keeps bullet+verb, got: {:?}",
            content_rows[0]
        );
        for text in content_rows.iter().skip(1) {
            assert!(text.starts_with("  "), "width {width}: continuation hangs under the header, got: {text:?}");
            assert!(text.chars().count() <= usize::from(width), "width {width}: row overflows viewport, got: {text:?}");
        }
    }
}

#[test]
fn pty_command_header_wraps_in_full_without_truncation() {
    // Same screenshot command through the live PTY path: `• Ran` plus its
    // `  │ ` continuations must keep every pipe segment with no `…`. Exact
    // screenshot bytes (`||`, `\.backup`); the `│` stream glyphs are content
    // and must survive reflow rather than being stripped or truncated.
    let header = "• Ran grep -rn \"@vinhnx/vtcode|npm install -g||npx @vinhnx\" docs |";
    let continuation = "  │ grep -v node_modules | grep -v package-lock | grep -v \"\\.backup\"";
    let expected_flat: String = format!("{header}{continuation}")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for width in [80u16, 50, 40] {
        let mut session = Session::new(InlineTheme::default(), None, VIEW_ROWS);
        push_pty_line(&mut session, header);
        push_pty_line(&mut session, continuation);
        let first = session.reflow_pty_lines(0, width);
        let second = session.reflow_pty_lines(1, width);
        let joined: String = first
            .iter()
            .chain(second.iter())
            .flat_map(|line| line.line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(!joined.contains('…'), "width {width}: PTY header must not truncate, got: {joined:?}");
        let flat: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(flat, expected_flat, "width {width}: PTY header lost content, got: {joined:?}");
        // Stream `│` glyphs survive reflow and no row overflows the viewport.
        assert!(joined.contains('│'), "width {width}: continuation glyph lost, got: {joined:?}");
        for line in first.iter().chain(second.iter()) {
            let text: String = line.line.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(text.chars().count() <= usize::from(width), "width {width}: row overflows viewport, got: {text:?}");
        }
    }
}
