//! Append-only request context persisted in canonical history.
//!
//! Context derived at request time (few-shot examples selected for the latest
//! user query, the IDE editor snapshot) used to be spliced into every request
//! at a moving position: few-shot after the newest message, editor context at
//! `messages[0]`. On routes that bind replayed thinking to the exact prior
//! prefix (Claude Opus 5.5, Claude Fable 5.1) and on every prompt cache, a
//! block that moves, changes, or disappears between requests invalidates
//! everything after it; a changed `messages[0]` invalidates the whole history.
//!
//! Instead, each block is written into canonical history at the first request
//! of a user turn, and later requests replay it unchanged at the same
//! position, so each request only appends to the previous one:
//! - few-shot context goes directly after the user message, once per turn, as
//!   a typed turn-scoped system message;
//! - editor context goes directly before the user message, and only when the
//!   snapshot differs from the one the model last saw.
//!
//! Canonical history keeps both as system-role messages so user-turn logic
//! (session titles, rewind points, intent extraction) never mistakes them for
//! user input. [`translate_request_context_for_wire`] shapes them per route:
//! - editor context is always sent as a user-role message: it precedes the
//!   user message, where a mid-conversation system message is not accepted;
//! - on routes with turn-scoped system messages, few-shot context keeps
//!   `role: "system"` and `clear_at: "next_user_message"`, so the provider
//!   stops applying it once the next user turn arrives while the prefix stays
//!   byte-identical;
//! - other routes receive few-shot context as a user-role message. Their
//!   adapters fold mid-history system messages into the top-level system
//!   prompt, which would rewrite the cached system prefix whenever the
//!   selection changes.

use vtcode_core::EDITOR_CONTEXT_PROMPT_HEADER;
use vtcode_core::llm::provider as uni;
use vtcode_core::prompts::FEW_SHOT_SECTION_HEADER;

fn is_few_shot_context_message(message: &uni::Message) -> bool {
    message.role == uni::MessageRole::System && message.content.as_text().starts_with(FEW_SHOT_SECTION_HEADER)
}

fn is_editor_context_message(message: &uni::Message) -> bool {
    message.role == uni::MessageRole::System && message.content.as_text().starts_with(EDITOR_CONTEXT_PROMPT_HEADER)
}

/// Sent once when editor context was shared earlier but no longer is (no
/// active file, IDE context disabled), so the model stops relying on the
/// last persisted snapshot.
fn editor_context_unavailable_block() -> String {
    format!("{EDITOR_CONTEXT_PROMPT_HEADER}\n- No active editor file is shared now; earlier editor context is stale.")
}

/// Index of the latest user message when no assistant or tool message follows
/// it, i.e. when the provider has not produced any output for this turn yet.
/// Context may only be placed around such a message without editing a prefix
/// an earlier request already sent.
fn unanswered_user_turn_index(history: &[uni::Message]) -> Option<usize> {
    let user_index = history.iter().rposition(|message| message.role == uni::MessageRole::User)?;
    let answered = history[user_index + 1..]
        .iter()
        .any(|message| matches!(message.role, uni::MessageRole::Assistant | uni::MessageRole::Tool));
    (!answered).then_some(user_index)
}

/// Persist the few-shot block selected for the current user turn.
///
/// Runs on every request but only writes while the turn is unanswered: the
/// block is inserted right after the user message, or refreshed in place when
/// a retry of the same unanswered turn selects different examples. Once the
/// provider has answered, the persisted block is left untouched so every
/// later request replays the same prefix.
pub(super) fn persist_turn_few_shot_context(history: &mut Vec<uni::Message>, few_shot_context: Option<String>) {
    let Some(few_shot_context) = few_shot_context.filter(|text| !text.trim().is_empty()) else {
        return;
    };
    let Some(user_index) = unanswered_user_turn_index(history) else {
        return;
    };

    if let Some(existing) = history[user_index + 1..]
        .iter_mut()
        .find(|message| is_few_shot_context_message(message))
    {
        if existing.content.as_text().as_ref() != few_shot_context {
            *existing = uni::Message::turn_scoped_system(few_shot_context);
        }
        return;
    }

    history.insert(user_index + 1, uni::Message::turn_scoped_system(few_shot_context));
}

/// Persist the editor context for the current user turn when it changed.
///
/// The block goes directly before the unanswered user message, so the model
/// reads it together with that message and every later request replays it at
/// the same position. An unchanged snapshot writes nothing; the earlier block
/// stays authoritative. A block already sitting in front of the unanswered
/// message belongs to this turn and is refreshed in place (or dropped when it
/// no longer differs from the one before it) because no answer depends on it.
pub(super) fn persist_turn_editor_context(history: &mut Vec<uni::Message>, editor_context: Option<String>) {
    let Some(user_index) = unanswered_user_turn_index(history) else {
        return;
    };
    let pending_slot = user_index
        .checked_sub(1)
        .filter(|&index| is_editor_context_message(&history[index]));
    let previous = history[..pending_slot.unwrap_or(user_index)]
        .iter()
        .rev()
        .find(|message| is_editor_context_message(message))
        .map(|message| message.content.as_text().into_owned());

    let desired = editor_context
        .filter(|block| !block.trim().is_empty())
        .or_else(|| {
            previous
                .as_deref()
                .filter(|block| *block != editor_context_unavailable_block())
                .map(|_| editor_context_unavailable_block())
        })
        .filter(|block| previous.as_deref() != Some(block.as_str()));

    match (pending_slot, desired) {
        (Some(slot), Some(block)) => {
            if history[slot].content.as_text().as_ref() != block {
                history[slot] = uni::Message::system(block);
            }
        }
        (Some(slot), None) => {
            history.remove(slot);
        }
        (None, Some(block)) => history.insert(user_index, uni::Message::system(block)),
        (None, None) => {}
    }
}

pub(super) fn request_context_needs_wire_translation(
    messages: &[uni::Message],
    turn_scoped_system_messages: bool,
) -> bool {
    messages.iter().any(|message| {
        is_editor_context_message(message)
            || (!turn_scoped_system_messages && (message.clear_at.is_some() || is_few_shot_context_message(message)))
    })
}

/// Shape persisted request context for the active route. Canonical history is
/// never modified; callers pass a request-only copy.
pub(super) fn translate_request_context_for_wire(messages: &mut [uni::Message], turn_scoped_system_messages: bool) {
    for message in messages {
        if is_editor_context_message(message) {
            message.role = uni::MessageRole::User;
            message.clear_at = None;
            continue;
        }
        if turn_scoped_system_messages {
            continue;
        }
        if is_few_shot_context_message(message) {
            // Keep the block where it was persisted; a user-role message is
            // never folded into the top-level system prompt.
            message.role = uni::MessageRole::User;
        }
        // Translate the provider-specific lifecycle field of typed turn-scoped
        // markers into an ordinary directive for routes without native
        // support.
        message.clear_at = None;
    }
}

#[cfg(test)]
mod tests {
    use vtcode_core::llm::provider as uni;

    use super::*;

    fn few_shot(text: &str) -> String {
        format!("{FEW_SHOT_SECTION_HEADER}\n{text}")
    }

    #[test]
    fn few_shot_is_inserted_after_the_unanswered_user_message() {
        let mut history = vec![
            uni::Message::user("first".to_string()),
            uni::Message::assistant("done".to_string()),
            uni::Message::user("second".to_string()),
        ];

        persist_turn_few_shot_context(&mut history, Some(few_shot("example")));

        assert_eq!(history.len(), 4);
        assert_eq!(history[2], uni::Message::user("second".to_string()));
        assert_eq!(history[3], uni::Message::turn_scoped_system(few_shot("example")));
    }

    #[test]
    fn few_shot_is_not_moved_or_duplicated_once_the_turn_is_answered() {
        let mut history = vec![uni::Message::user("task".to_string())];
        persist_turn_few_shot_context(&mut history, Some(few_shot("example")));
        history.push(uni::Message::assistant_with_tools(
            String::new(),
            vec![uni::ToolCall::function(
                "call_1".to_string(),
                "read_file".to_string(),
                "{}".to_string(),
            )],
        ));
        history.push(uni::Message::tool_response("call_1".to_string(), "contents".to_string()));
        let answered = history.clone();

        persist_turn_few_shot_context(&mut history, Some(few_shot("example")));
        persist_turn_few_shot_context(&mut history, Some(few_shot("different")));

        assert_eq!(history, answered, "an answered turn's prefix must stay byte-identical");
    }

    #[test]
    fn retry_of_unanswered_turn_refreshes_few_shot_in_place() {
        let mut history = vec![uni::Message::user("task".to_string())];
        persist_turn_few_shot_context(&mut history, Some(few_shot("old")));
        persist_turn_few_shot_context(&mut history, Some(few_shot("old")));
        assert_eq!(history.len(), 2);

        persist_turn_few_shot_context(&mut history, Some(few_shot("new")));
        assert_eq!(history.len(), 2);
        assert_eq!(history[1], uni::Message::turn_scoped_system(few_shot("new")));
    }

    #[test]
    fn earlier_turn_few_shot_is_kept_when_a_new_turn_starts() {
        let mut history = vec![uni::Message::user("first".to_string())];
        persist_turn_few_shot_context(&mut history, Some(few_shot("one")));
        history.push(uni::Message::assistant("done".to_string()));
        history.push(uni::Message::user("second".to_string()));
        persist_turn_few_shot_context(&mut history, Some(few_shot("two")));

        assert_eq!(
            history,
            vec![
                uni::Message::user("first".to_string()),
                uni::Message::turn_scoped_system(few_shot("one")),
                uni::Message::assistant("done".to_string()),
                uni::Message::user("second".to_string()),
                uni::Message::turn_scoped_system(few_shot("two")),
            ]
        );
    }

    fn editor(text: &str) -> String {
        format!("{EDITOR_CONTEXT_PROMPT_HEADER}\n- Active file: {text}")
    }

    #[test]
    fn editor_context_is_inserted_before_the_unanswered_user_message() {
        let mut history = vec![
            uni::Message::user("first".to_string()),
            uni::Message::assistant("done".to_string()),
            uni::Message::user("second".to_string()),
        ];

        persist_turn_editor_context(&mut history, Some(editor("src/main.rs")));

        assert_eq!(history[2], uni::Message::system(editor("src/main.rs")));
        assert_eq!(history[3], uni::Message::user("second".to_string()));
    }

    #[test]
    fn unchanged_editor_context_is_not_repeated_on_later_turns() {
        let mut history = vec![uni::Message::user("first".to_string())];
        persist_turn_editor_context(&mut history, Some(editor("src/main.rs")));
        persist_turn_editor_context(&mut history, Some(editor("src/main.rs")));
        history.push(uni::Message::assistant("done".to_string()));
        history.push(uni::Message::user("second".to_string()));
        persist_turn_editor_context(&mut history, Some(editor("src/main.rs")));

        assert_eq!(
            history,
            vec![
                uni::Message::system(editor("src/main.rs")),
                uni::Message::user("first".to_string()),
                uni::Message::assistant("done".to_string()),
                uni::Message::user("second".to_string()),
            ]
        );
    }

    #[test]
    fn changed_editor_context_is_appended_for_the_new_turn_only() {
        let mut history = vec![uni::Message::user("first".to_string())];
        persist_turn_editor_context(&mut history, Some(editor("src/main.rs")));
        history.push(uni::Message::assistant("done".to_string()));
        let answered_prefix = history.clone();

        // Mid-turn snapshot changes never touch an answered turn.
        persist_turn_editor_context(&mut history, Some(editor("src/lib.rs")));
        assert_eq!(history, answered_prefix);

        history.push(uni::Message::user("second".to_string()));
        persist_turn_editor_context(&mut history, Some(editor("src/lib.rs")));

        assert_eq!(&history[..answered_prefix.len()], answered_prefix.as_slice());
        assert_eq!(history[answered_prefix.len()], uni::Message::system(editor("src/lib.rs")));
        assert_eq!(history[answered_prefix.len() + 1], uni::Message::user("second".to_string()));
    }

    #[test]
    fn pending_editor_context_is_refreshed_or_dropped_in_place() {
        let mut history = vec![uni::Message::user("first".to_string())];
        persist_turn_editor_context(&mut history, Some(editor("a.rs")));
        history.push(uni::Message::assistant("done".to_string()));
        history.push(uni::Message::user("second".to_string()));

        persist_turn_editor_context(&mut history, Some(editor("b.rs")));
        persist_turn_editor_context(&mut history, Some(editor("c.rs")));
        assert_eq!(history.len(), 5);
        assert_eq!(history[3], uni::Message::system(editor("c.rs")));

        // Back to the snapshot the model already saw: the pending copy goes.
        persist_turn_editor_context(&mut history, Some(editor("a.rs")));
        assert_eq!(history.len(), 4);
        assert_eq!(history[3], uni::Message::user("second".to_string()));
    }

    #[test]
    fn cleared_editor_context_is_announced_once() {
        let mut history = vec![uni::Message::user("first".to_string())];
        persist_turn_editor_context(&mut history, None);
        assert_eq!(history.len(), 1, "no editor context was ever shared");

        persist_turn_editor_context(&mut history, Some(editor("a.rs")));
        history.push(uni::Message::assistant("done".to_string()));
        history.push(uni::Message::user("second".to_string()));
        persist_turn_editor_context(&mut history, None);
        assert_eq!(history[3], uni::Message::system(editor_context_unavailable_block()));

        history.push(uni::Message::assistant("ok".to_string()));
        history.push(uni::Message::user("third".to_string()));
        let before = history.clone();
        persist_turn_editor_context(&mut history, None);
        assert_eq!(history, before);
    }

    #[test]
    fn editor_context_is_sent_as_user_context_on_every_route() {
        for turn_scoped in [true, false] {
            let mut messages = vec![
                uni::Message::system(editor("src/main.rs")),
                uni::Message::user("task".to_string()),
            ];
            assert!(request_context_needs_wire_translation(&messages, turn_scoped));
            translate_request_context_for_wire(&mut messages, turn_scoped);
            assert_eq!(messages[0], uni::Message::user(editor("src/main.rs")));
        }
    }

    #[test]
    fn wire_translation_keeps_turn_scoped_system_on_supported_routes() {
        let mut messages = vec![
            uni::Message::user("task".to_string()),
            uni::Message::turn_scoped_system(few_shot("example")),
        ];
        let original = messages.clone();

        assert!(!request_context_needs_wire_translation(&messages, true));
        translate_request_context_for_wire(&mut messages, true);

        assert_eq!(messages, original);
    }

    #[test]
    fn wire_translation_sends_few_shot_as_user_context_elsewhere() {
        let notice = uni::Message::turn_scoped_system("notice".to_string());
        let mut messages = vec![
            uni::Message::user("task".to_string()),
            uni::Message::turn_scoped_system(few_shot("example")),
            notice,
        ];

        assert!(request_context_needs_wire_translation(&messages, false));
        translate_request_context_for_wire(&mut messages, false);

        assert_eq!(messages[1], uni::Message::user(few_shot("example")));
        assert_eq!(messages[2], uni::Message::system("notice".to_string()));
    }
}
