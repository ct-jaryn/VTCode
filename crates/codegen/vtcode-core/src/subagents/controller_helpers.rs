#![allow(
    unused_imports,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use futures::future::select_all;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, RwLock};

use crate::config::VTCodeConfig;
use crate::config::types::ReasoningEffortLevel;
use crate::core::agent::runner::{AgentRunner, RunnerSettings};
use crate::core::agent::task::Task;
use crate::core::threads::{ThreadBootstrap, ThreadId, ThreadRuntimeHandle, ThreadSnapshot};
use crate::hooks::{LifecycleHookEngine, SessionStartTrigger};
use crate::llm::provider::Message;
use crate::tools::exec_session::ExecSessionManager;
use crate::tools::pty::{PtyManager, PtySize};
use crate::utils::session_archive::{SessionArchive, find_session_by_identifier};
use vtcode_config::SubagentSpec;
use vtcode_config::auth::OpenAIChatGptAuthHandle;

use self::background::*;
use self::config::*;
use self::constants::*;
use self::discovery::discover_controller_subagents;
use self::model::*;
use vtcode_config::subagents::SUBAGENT_HARD_CONCURRENCY_LIMIT;

#[allow(
    unused_imports,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]
use super::*;

pub(super) fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

pub(super) async fn load_session_listing(
    path: &std::path::Path,
) -> Result<crate::utils::session_archive::SessionListing> {
    use anyhow::Context;
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read session archive {}", path.display()))?;
    let snapshot: crate::utils::session_archive::SessionSnapshot =
        serde_json::from_str(&raw).with_context(|| format!("Failed to parse session archive {}", path.display()))?;
    Ok(crate::utils::session_archive::SessionListing { path: path.to_path_buf(), snapshot })
}

pub(super) async fn checkpoint_subagent_archive_start(archive: &SessionArchive, messages: &[Message]) -> Result<()> {
    use crate::utils::session_archive::SessionMessage;
    let recent_messages: Vec<SessionMessage> = messages.iter().map(SessionMessage::from).collect::<Vec<_>>();
    archive
        .persist_progress_async(crate::utils::session_archive::SessionProgressArgs {
            total_messages: recent_messages.len(),
            distinct_tools: Vec::new(),
            messages: recent_messages.clone(),
            recent_messages,
            turn_number: 1,
            token_usage: None,
            max_context_tokens: None,
            loaded_skills: Some(Vec::new()),
            turn_diagnostics: None,
        })
        .await?;
    Ok(())
}

pub(super) async fn persist_child_archive(
    archive: &SessionArchive,
    messages: &[Message],
    agent_name: &str,
) -> Result<Option<PathBuf>> {
    use crate::utils::session_archive::SessionMessage;
    let transcript = messages
        .iter()
        .filter_map(transcript_line_from_message)
        .take(SUBAGENT_TRANSCRIPT_LINE_LIMIT)
        .collect::<Vec<_>>();
    let stored_messages = messages.iter().map(SessionMessage::from).collect::<Vec<_>>();
    let path = archive.finalize(transcript, stored_messages.len(), vec![agent_name.to_string()], stored_messages)?;
    Ok(Some(path))
}

fn transcript_line_from_message(message: &Message) -> Option<String> {
    let role = message.role.to_string();
    let content = message.content.trim();
    if content.is_empty() {
        return None;
    }
    Some(format!("{role}: {content}"))
}

/// Parse an explicit `Decision: APPROVED` / `Decision: REJECTED` line from a
/// verifier summary. Markdown emphasis around the label is ignored, and the
/// last decision line wins. Returns `None` only when no decision line is
/// present, so the caller can fall back to keyword heuristics. A decision line
/// whose value is not a clean approval (for example `NOT APPROVED`, `pending`,
/// or `approved, but ...` with a negation) fails closed as `Some(false)`.
pub(super) fn parse_verifier_decision(summary: &str) -> Option<bool> {
    summary.lines().rev().find_map(|line| {
        let lower = line.trim().trim_start_matches(['-', '*', '#', ' ']).to_ascii_lowercase();
        let rest = lower.strip_prefix("decision")?;
        let value = rest.trim_start_matches(['*', ' ']).strip_prefix(':')?;
        Some(decision_value_approves(value))
    })
}

/// Words that turn an approval-looking decision value into a non-approval.
const DECISION_NEGATIONS: &[&str] = &["not", "no", "never", "cannot", "cant", "dont", "isnt", "wont"];

/// A decision value approves only when its first word is `approve`/`approved`
/// and no word negates it. Everything else, including `reject`, rejects.
fn decision_value_approves(value: &str) -> bool {
    let words = value
        .split(|c: char| !c.is_ascii_alphabetic() && c != '\'')
        .map(|word| word.replace('\'', ""))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let Some(first) = words.first() else {
        return false;
    };
    matches!(first.as_str(), "approve" | "approved")
        && !words.iter().any(|word| DECISION_NEGATIONS.contains(&word.as_str()))
}

/// Keyword fallback for verifier summaries without a `Decision:` line.
/// Rejection keywords (including negated approvals such as "not approved")
/// win over approval keywords, and any structured issue blocks approval.
pub(super) fn heuristic_verifier_approval(summary: &str, issues: &[String]) -> bool {
    let lower = summary.to_lowercase();
    let explicitly_rejected = lower.contains("reject")
        || lower.contains("not approve")
        || lower.contains("disapprove")
        || lower.contains("unapproved")
        || lower.contains("denied")
        || lower.contains("unsafe")
        || lower.contains("blocked")
        || lower.contains("dangerous")
        || lower.contains("malicious")
        || lower.contains("vulnerability");
    if explicitly_rejected {
        return false;
    }
    let explicitly_approved = lower.contains("approved")
        || lower.contains("safe to merge")
        || lower.contains("no issues found")
        || lower.contains("looks correct")
        || lower.contains("verification passed");
    explicitly_approved && issues.is_empty()
}

/// Extract issue descriptions from a verifier sub-agent's summary text.
///
/// Only structured issue lines count: after an optional list marker (`-`,
/// `*`, `+`, `1.`, `1)`) and optional Markdown emphasis, the line must start
/// with `ISSUE:` (case-insensitive), the one prefix the verifier response
/// format defines. Prose that merely mentions "error:" or quoted compiler
/// output is ignored. Returns the text after the marker, e.g.
/// `ISSUE: src/lib.rs:3 missing check`.
pub(super) fn extract_issues_from_summary(summary: &str) -> Vec<String> {
    summary.lines().filter_map(structured_issue_line).collect()
}

fn structured_issue_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let unlisted = strip_list_marker(trimmed);
    let body = unlisted.trim_start_matches(['*', '_', '`']);
    let label = body.get(..5)?;
    if !label.eq_ignore_ascii_case("issue") {
        return None;
    }
    let after_label = body[5..].trim_start_matches(['*', '_', '`']);
    let description = after_label.strip_prefix(':')?.trim_start_matches(['*', '_', '`']).trim();
    if description.is_empty() {
        return None;
    }
    Some(format!("ISSUE: {description}"))
}

fn strip_list_marker(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix(['-', '*', '+'])
        && rest.starts_with(char::is_whitespace)
    {
        return rest.trim_start();
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0
        && let Some(rest) = line[digits..].strip_prefix(['.', ')'])
    {
        return rest.trim_start();
    }
    line
}
