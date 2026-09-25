//! Agent Legibility:
//! - Entrypoint: `maybe_force_planning_workflow_interview` is the sole orchestrator; it decides via `gating` whether an interview is ready/needed and, if so, injects a static clarifying-question interview via `interview_forcing`.
//! - Common changes:
//!   - Pure readiness/need-state predicates (no I/O) live in `gating.rs` — extend those for new interview-skip conditions.
//!   - Static fallback question shaping lives in `interview_payload.rs` (`build_fallback_question`); plan draft / open-decision detection helpers live in `interview_context.rs`.
//! - Constraints: This file is intentionally kept to orchestration + shared constants only. Put new logic in `gating.rs` (pure predicates) or `interview_payload.rs` (question shaping) — do not grow this root file's function bodies.
//! - Verify: `cargo check -p vtcode && cargo test -p vtcode --bin vtcode inline_events::tests`

use crate::agent::runloop::unified::planning_workflow_state::PlanningWorkflowSessionState;

#[path = "planning_workflow/gating.rs"]
mod gating;
#[path = "planning_workflow/interview_context.rs"]
mod interview_context;
#[path = "planning_workflow/interview_forcing.rs"]
mod interview_forcing;
#[path = "planning_workflow/interview_payload.rs"]
mod interview_payload;

use crate::agent::runloop::unified::turn::context::TurnProcessingResult;
use crate::agent::runloop::unified::turn::turn_processing::response_processing::extract_interview_questions;
pub(crate) use gating::planning_workflow_interview_ready;
use gating::{InterviewGate, evaluate_interview_gate};
use interview_forcing::{
    InterviewToolCallFilter, InterviewToolCallFilterPolicy, filter_interview_tool_calls,
    inject_planning_workflow_interview, maybe_append_planning_workflow_reminder, strip_assistant_text,
};

#[cfg(test)]
use super::response_processing::prepare_tool_calls;

#[cfg(test)]
use vtcode_core::llm::provider as uni;

const MIN_PLANNING_WORKFLOW_TURNS_BEFORE_INTERVIEW: usize = 1;
const PLANNING_WORKFLOW_REMINDER: &str = vtcode_core::prompts::system::PLANNING_WORKFLOW_IMPLEMENT_REMINDER;

pub(crate) fn maybe_force_planning_workflow_interview(
    processing_result: TurnProcessingResult,
    response_text: Option<&str>,
    session_stats: &crate::agent::runloop::unified::state::SessionStats,
    plan_session: &mut PlanningWorkflowSessionState,
    conversation_len: usize,
) -> TurnProcessingResult {
    if !plan_session.interview_forcing_allowed() || matches!(processing_result, TurnProcessingResult::Refusal { .. }) {
        return processing_result;
    }
    let response_has_plan =
        matches!(&processing_result, TurnProcessingResult::TextResponse { proposed_plan: Some(_), .. })
            || response_text.is_some_and(|text| text.contains("<proposed_plan>") || text.contains("<plan>"));
    let gate = evaluate_interview_gate(response_text, response_has_plan, session_stats, plan_session);
    if gate.response_has_plan {
        return handle_plan_response(processing_result, gate, plan_session, conversation_len);
    }

    handle_non_plan_response(processing_result, response_text, gate, plan_session, conversation_len)
}

fn handle_plan_response(
    processing_result: TurnProcessingResult,
    gate: InterviewGate,
    plan_session: &mut PlanningWorkflowSessionState,
    conversation_len: usize,
) -> TurnProcessingResult {
    let filtered = filter_interview_tool_calls(
        processing_result,
        InterviewToolCallFilterPolicy {
            allow_interview: gate.allow_interview,
            response_has_plan: gate.response_has_plan,
        },
    )
    .processing_result;

    if gate.allow_interview && gate.needs_interview {
        return inject_planning_workflow_interview(strip_assistant_text(filtered), plan_session, conversation_len);
    }

    maybe_append_planning_workflow_reminder(filtered)
}

fn handle_non_plan_response(
    processing_result: TurnProcessingResult,
    response_text: Option<&str>,
    gate: InterviewGate,
    plan_session: &mut PlanningWorkflowSessionState,
    conversation_len: usize,
) -> TurnProcessingResult {
    let filter = filter_interview_tool_calls(
        processing_result,
        InterviewToolCallFilterPolicy {
            allow_interview: gate.allow_interview,
            response_has_plan: gate.response_has_plan,
        },
    );
    let InterviewToolCallFilter {
        processing_result,
        had_interview_tool_calls,
        had_non_interview_tool_calls,
    } = filter;

    if plan_session.interview_pending() {
        return handle_pending_interview(
            had_interview_tool_calls,
            had_non_interview_tool_calls,
            gate,
            processing_result,
            plan_session,
            conversation_len,
        );
    }

    let explicit_questions = response_text
        .map(|text| !extract_interview_questions(text).is_empty())
        .unwrap_or(false);
    if explicit_questions {
        if gate.allow_interview {
            plan_session.mark_interview_shown();
        }
        return processing_result;
    }

    if had_interview_tool_calls {
        if gate.allow_interview {
            plan_session.mark_interview_shown();
        } else {
            plan_session.mark_interview_pending();
        }
        return processing_result;
    }

    if had_non_interview_tool_calls {
        if gate.needs_interview {
            plan_session.mark_interview_pending();
        }
        return processing_result;
    }

    if !gate.allow_interview || !gate.needs_interview {
        return processing_result;
    }

    inject_planning_workflow_interview(processing_result, plan_session, conversation_len)
}

fn handle_pending_interview(
    had_interview_tool_calls: bool,
    had_non_interview_tool_calls: bool,
    gate: InterviewGate,
    processing_result: TurnProcessingResult,
    plan_session: &mut PlanningWorkflowSessionState,
    conversation_len: usize,
) -> TurnProcessingResult {
    if !gate.needs_interview {
        plan_session.clear_interview_pending();
        return processing_result;
    }

    if had_interview_tool_calls && gate.allow_interview {
        plan_session.mark_interview_shown();
        return processing_result;
    }

    if had_non_interview_tool_calls || !gate.allow_interview {
        return processing_result;
    }

    inject_planning_workflow_interview(processing_result, plan_session, conversation_len)
}

#[cfg(test)]
mod tests;
