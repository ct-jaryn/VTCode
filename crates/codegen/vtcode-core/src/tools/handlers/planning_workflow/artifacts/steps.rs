//! Quote-aware step metadata and bracket-list parsing.

use super::sections::{strip_ascii_case_insensitive_prefix, strip_leading_emphasis, strip_list_marker};

pub(super) const PLAN_TARGET_LABELS: &[&str] =
    &["files/symbols", "files", "symbols", "target", "behavior", "behaviour"];

pub(super) fn marker_value<'a>(line: &'a str, labels: &[&str]) -> Option<&'a str> {
    let line = strip_list_marker(line);
    // Tolerate emphasis around the label (`**Files/symbols:** value`) — models
    // routinely bold marker labels, and the `**` otherwise breaks both the
    // label prefix and the trailing `:` match (checkpoint turn_912).
    let line = strip_leading_emphasis(line);
    labels.iter().find_map(|label| {
        strip_ascii_case_insensitive_prefix(line, &format!("{label}:"))
            .or_else(|| {
                // Emphasis between label and colon: `**Files/symbols:**`.
                strip_ascii_case_insensitive_prefix(line, label)
                    .and_then(|rest| strip_leading_emphasis(rest).strip_prefix(':').map(str::trim_start))
            })
            .map(|value| strip_leading_emphasis(value).trim_start())
    })
}

/// Explicit metadata markers delimit canonical steps; arrows in prose or
/// verification commands remain content. Retain bare-arrow legacy steps
/// when no target marker is present. Validation and tracker generation share
/// these boundaries so a valid plan cannot lose its action when persisted.
pub(super) fn step_action_segments(action: &str) -> Vec<String> {
    struct ArrowBoundary {
        start_byte: usize,
        end_byte: usize,
    }
    let mut boundaries = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut bracket_depth = 0usize;
    let mut previous = None;
    let mut previous_non_whitespace = None;
    for (index, character) in action.char_indices() {
        let preceding = previous;
        let preceding_syntax = previous_non_whitespace;
        previous = Some(character);
        if !character.is_whitespace() {
            previous_non_whitespace = Some(character);
        }
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
            continue;
        }
        let lifetime_apostrophe = character == '\''
            && (preceding_syntax.is_some_and(|character| matches!(character, '&' | '<'))
                || (preceding_syntax.is_some_and(|character| matches!(character, ':' | '+'))
                    && action[index + 1..].find('\'').is_none_or(|offset| {
                        action[index + offset + 2..]
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_alphanumeric() || character == '_')
                    }))
                || (preceding_syntax == Some(',')
                    && action[index + 1..]
                        .trim_start_matches(|character: char| character.is_alphanumeric() || character == '_')
                        .trim_start()
                        .starts_with([',', '>', ':', '+'])));
        if matches!(character, '\'' | '"' | '`')
            && !lifetime_apostrophe
            && !(character == '\'' && preceding.is_some_and(char::is_alphanumeric))
        {
            quote = Some(character);
            continue;
        }
        match character {
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {}
        }
        if bracket_depth != 0 {
            continue;
        }
        let width = if character == '→' {
            character.len_utf8()
        } else if action[index..].starts_with("->") {
            2
        } else {
            continue;
        };
        boundaries.push(ArrowBoundary { start_byte: index, end_byte: index + width });
    }
    let has_target_marker = boundaries
        .iter()
        .any(|boundary| marker_value(action[boundary.end_byte..].trim_start(), PLAN_TARGET_LABELS).is_some());
    let mut segments = Vec::new();
    let mut start = 0;
    for boundary in boundaries {
        let remainder = action[boundary.end_byte..].trim_start();
        if has_target_marker
            && marker_value(remainder, PLAN_TARGET_LABELS).is_none()
            && marker_value(remainder, &["verify", "verification", "outcome"]).is_none()
        {
            continue;
        }
        segments.push(action[start..boundary.start_byte].trim().to_string());
        start = boundary.end_byte;
    }
    segments.push(action[start..].trim().to_string());
    segments
}

pub(super) fn parse_bracket_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    split_bracket_items(trimmed)
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Split a bracket-list body on commas that sit outside single/double quotes.
///
/// Shell verifications routinely embed commas inside quoted `sed` address
/// ranges (`sed -n '/^## A/,/^## B/p'`) or `rg` alternations. A naive split
/// turns one concrete command into two fragments, and the first fragment
/// then fails verification as `verification item 1 must be a concrete
/// command or check` (session `session-vtcode-20260914T031505Z_075199-09813`,
/// step 4). Quote tracking keeps such commands intact.
pub fn split_bracket_items(body: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::with_capacity(body.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(ch);
            }
            // Backslash escapes the next char outside single quotes (shell:
            // literal inside `'…'`, escape inside `"…"` and unquoted so an
            // escaped comma/quote never splits or toggles).
            '\\' if !in_single => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ',' if !in_single && !in_double => {
                items.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    items.push(current);
    items
}
