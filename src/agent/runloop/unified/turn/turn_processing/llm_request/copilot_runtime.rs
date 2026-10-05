mod streaming;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anstyle::Color;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use vtcode_config::auth::CopilotAuthConfig;
use vtcode_config::core::permissions::AgentPermissionsConfig;
use vtcode_core::acp::{PermissionGrant, ToolPermissionCache};
use vtcode_core::config::PtyConfig;
use vtcode_core::copilot::{
    CopilotAcpCompatibilityState, CopilotObservedToolCall, CopilotObservedToolCallStatus, CopilotPermissionDecision,
    CopilotPermissionRequest, CopilotRuntimeRequest, CopilotTerminalCreateRequest, CopilotTerminalCreateResponse,
    CopilotTerminalExitStatus, CopilotTerminalOutputResponse, CopilotToolCallFailure, CopilotToolCallRequest,
    CopilotToolCallResponse, CopilotToolCallSuccess, PromptSession,
};
use vtcode_core::core::trajectory::TrajectoryLogger;
use vtcode_core::exec::events::ToolCallStatus;
use vtcode_core::exec_policy::AskForApproval;
use vtcode_core::llm::provider as uni;
use vtcode_core::llm::provider::ToolDefinition;
use vtcode_core::tools::registry::{ToolProgressCallback, ToolRegistry};
use vtcode_core::types::CompactStr;
use vtcode_core::utils::ansi::AnsiRenderer;
use vtcode_core::utils::style_helpers::ColorPalette;
use vtcode_ui::tui::app::{InlineHandle, InlineSession};

use super::request_builder::COLLAPSED_TOOL_OUTPUT_NOTICE;
use crate::agent::runloop::mcp_events::McpPanelState;
use crate::agent::runloop::tool_output::resolve_stdout_tail_limit;
use crate::agent::runloop::unified::async_mcp_manager::approval_policy_from_human_in_the_loop;
use crate::agent::runloop::unified::inline_events::harness::{
    HarnessEventEmitter, tool_invocation_completed_event, tool_output_completed_event, tool_output_started_event,
    tool_started_event, tool_updated_event,
};
use crate::agent::runloop::unified::planning_workflow_state::PlanningWorkflowSessionState;
use crate::agent::runloop::unified::progress::{ProgressReporter, ProgressUpdateGuard, spawn_elapsed_time_updater};
use crate::agent::runloop::unified::run_loop_context::{
    HarnessTurnState, RunLoopContext, SESSION_LIMIT_GRANT_DIRECTIVE, full_auto_loop_grants_enabled,
};
use crate::agent::runloop::unified::state::CtrlCState;
use crate::agent::runloop::unified::state::SessionStats;
use crate::agent::runloop::unified::tool_call_safety::{ToolCallSafetyValidator, invocation_id_from_call_id};
use crate::agent::runloop::unified::tool_output_handler::handle_pipeline_output;
use crate::agent::runloop::unified::tool_pipeline::{
    PtyStreamRuntime, ToolExecutionStatus, run_tool_call_with_args,
    validation::{SafetyValidationFailure, validate_tool_call_with_limit_prompt},
};
use crate::agent::runloop::unified::tool_routing::{
    HitlDecision, PreToolHookPhaseResult, ToolPermissionFlow, ToolPermissionsContext,
    ensure_tool_permission_with_call_id, prompt_external_tool_permission,
};
use crate::agent::runloop::unified::turn::tool_outcomes::error_handling::tool_denial_diagnostic;
use crate::agent::runloop::unified::turn::tool_outcomes::helpers::{
    LoopTracker, mutation_blocked_until_verification, update_repetition_tracker,
};
use crate::agent::runloop::unified::turn::tool_outcomes::{
    ToolFailureDiagnosis, bounded_diagnostic_field, bounded_error_evidence, bounded_output_evidence,
    deterministic_error_diagnosis, deterministic_output_diagnosis, escape_untrusted_evidence, render_diagnosis,
};
use crate::agent::runloop::unified::ui_interaction::PlaceholderSpinner;
use crate::agent::runloop::unified::ui_interaction_stream::CopilotRuntimeRequestHandler;

pub(super) struct CopilotRuntimeHost<'a> {
    tool_registry: &'a mut ToolRegistry,
    tool_result_cache: &'a Arc<RwLock<vtcode_core::tools::ToolResultCache>>,
    session: &'a mut InlineSession,
    session_stats: &'a mut SessionStats,
    plan_session: &'a mut PlanningWorkflowSessionState,
    mcp_panel_state: &'a mut McpPanelState,
    handle: &'a InlineHandle,
    ctrl_c_state: &'a Arc<CtrlCState>,
    ctrl_c_notify: &'a Arc<tokio::sync::Notify>,
    default_placeholder: Option<String>,
    approval_recorder: &'a vtcode_core::tools::ApprovalRecorder,
    decision_ledger: &'a Arc<RwLock<vtcode_core::core::decision_tracker::DecisionTracker>>,
    tool_permission_cache: &'a Arc<RwLock<ToolPermissionCache>>,
    permissions_state: &'a Arc<RwLock<vtcode_core::config::PermissionsConfig>>,
    active_agent_permissions: Option<&'a AgentPermissionsConfig>,
    safety_validator: &'a Arc<ToolCallSafetyValidator>,
    lifecycle_hooks: Option<&'a vtcode_core::hooks::LifecycleHookEngine>,
    approval_policy: AskForApproval,
    hitl_notification_bell: bool,
    skip_confirmations: bool,
    full_auto: bool,
    vt_cfg: Option<&'a vtcode_config::loader::VTCodeConfig>,
    traj: &'a TrajectoryLogger,
    harness_state: &'a mut HarnessTurnState,
    loop_tracker: Option<&'a mut LoopTracker>,
    suppress_output_signal: Option<Arc<AtomicBool>>,
    exposed_tools: Vec<ToolDefinition>,
    exposed_tool_names: BTreeSet<String>,
    harness_emitter: Option<&'a HarnessEventEmitter>,
    harness_item_prefix: String,
    agent_name: Option<String>,
    observed_tool_calls: HashMap<String, ObservedToolCallState>,
    local_terminal_sessions: HashMap<String, LocalTerminalSession>,
    compatibility_notice_shown: bool,
    pending_hook_rewritten_args: HashMap<CompactStr, Value>,
}

impl<'a> CopilotRuntimeHost<'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    pub(super) fn new(
        tool_registry: &'a mut ToolRegistry,
        tool_result_cache: &'a Arc<RwLock<vtcode_core::tools::ToolResultCache>>,
        session: &'a mut InlineSession,
        session_stats: &'a mut SessionStats,
        plan_session: &'a mut PlanningWorkflowSessionState,
        mcp_panel_state: &'a mut McpPanelState,
        handle: &'a InlineHandle,
        ctrl_c_state: &'a Arc<CtrlCState>,
        ctrl_c_notify: &'a Arc<tokio::sync::Notify>,
        default_placeholder: Option<String>,
        approval_recorder: &'a vtcode_core::tools::ApprovalRecorder,
        decision_ledger: &'a Arc<RwLock<vtcode_core::core::decision_tracker::DecisionTracker>>,
        tool_permission_cache: &'a Arc<RwLock<ToolPermissionCache>>,
        permissions_state: &'a Arc<RwLock<vtcode_core::config::PermissionsConfig>>,
        active_agent_permissions: Option<&'a AgentPermissionsConfig>,
        safety_validator: &'a Arc<ToolCallSafetyValidator>,
        lifecycle_hooks: Option<&'a vtcode_core::hooks::LifecycleHookEngine>,
        vt_cfg: Option<&'a vtcode_config::loader::VTCodeConfig>,
        traj: &'a TrajectoryLogger,
        harness_state: &'a mut HarnessTurnState,
        loop_tracker: Option<&'a mut LoopTracker>,
        suppress_output_signal: Option<Arc<AtomicBool>>,
        available_tools: Option<&Arc<Vec<ToolDefinition>>>,
        skip_confirmations: bool,
        full_auto: bool,
        harness_emitter: Option<&'a HarnessEventEmitter>,
        harness_item_prefix: String,
        agent_name: Option<String>,
    ) -> Self {
        let allowlist = vt_cfg
            .map(|cfg| cfg.auth.copilot.vtcode_tool_allowlist.clone())
            .unwrap_or_else(|| CopilotAuthConfig::default().vtcode_tool_allowlist);
        let exposed_tools = filter_copilot_tools(available_tools, &allowlist);
        let exposed_tool_names = exposed_tools
            .iter()
            .filter_map(tool_definition_name)
            .map(str::to_string)
            .collect();

        let (approval_policy, hitl_bell) = vt_cfg
            .map(|cfg| {
                (
                    approval_policy_from_human_in_the_loop(cfg.security.human_in_the_loop),
                    cfg.security.hitl_notification_bell,
                )
            })
            .unwrap_or((AskForApproval::OnRequest, true));

        Self {
            tool_registry,
            tool_result_cache,
            session,
            session_stats,
            plan_session,
            mcp_panel_state,
            handle,
            ctrl_c_state,
            ctrl_c_notify,
            default_placeholder,
            approval_recorder,
            decision_ledger,
            tool_permission_cache,
            permissions_state,
            active_agent_permissions,
            safety_validator,
            lifecycle_hooks,
            approval_policy,
            hitl_notification_bell: hitl_bell,
            skip_confirmations,
            full_auto,
            vt_cfg,
            traj,
            harness_state,
            loop_tracker,
            suppress_output_signal,
            exposed_tools,
            exposed_tool_names,
            harness_emitter,
            harness_item_prefix,
            agent_name,
            observed_tool_calls: HashMap::new(),
            local_terminal_sessions: HashMap::new(),
            compatibility_notice_shown: false,
            pending_hook_rewritten_args: HashMap::new(),
        }
    }

    pub(super) fn exposed_tools(&self) -> &[ToolDefinition] {
        &self.exposed_tools
    }

    fn session_limit_auto_grant(&self) -> bool {
        full_auto_loop_grants_enabled(self.full_auto, self.vt_cfg)
    }

    async fn handle_builtin_permission(
        &mut self,
        renderer: &mut AnsiRenderer,
        request: CopilotPermissionRequest,
    ) -> Result<CopilotPermissionDecision> {
        let Some(summary) = summarize_permission_request(&request) else {
            return Ok(CopilotPermissionDecision::DeniedNoApprovalRule);
        };

        if let Some(decision) = self.cached_permission_decision(&summary.cache_key).await {
            return Ok(decision);
        }

        if let Some((permission_decision, cache_for_session)) = auto_approve_builtin_permission(&request) {
            if cache_for_session {
                let mut cache = self.tool_permission_cache.write().await;
                cache.cache_grant(summary.cache_key, PermissionGrant::Session);
            }
            return Ok(permission_decision);
        }

        if self.approval_policy.rejects_request_permission_prompt() {
            return Ok(CopilotPermissionDecision::DeniedNoApprovalRule);
        }

        let decision = prompt_external_tool_permission(
            renderer,
            self.handle,
            self.session,
            self.ctrl_c_state,
            self.ctrl_c_notify,
            self.default_placeholder.clone(),
            &summary.tool_name,
            summary.tool_args.as_ref(),
            &summary.display_name,
            &summary.cache_key,
            &summary.learning_label,
            summary.reason.as_deref(),
            Some(self.approval_recorder),
            self.hitl_notification_bell,
            None,
        )
        .await?;

        // Record the user's decision for auto-accept pattern learning.
        let approved = matches!(
            decision,
            HitlDecision::Approved | HitlDecision::ApprovedSession | HitlDecision::ApprovedPermanent
        );
        if let Err(err) = self
            .approval_recorder
            .record_approval(&summary.cache_key, Some(&summary.learning_label), approved, None)
            .await
        {
            tracing::debug!(
                approval_key = %summary.cache_key,
                approved,
                error = %err,
                "Failed to record builtin permission decision for pattern learning"
            );
        }

        let (permission_decision, cache_for_session) =
            map_builtin_permission_prompt_decision(decision, summary.reason.clone());
        if cache_for_session {
            let mut cache = self.tool_permission_cache.write().await;
            cache.cache_grant(summary.cache_key, PermissionGrant::Session);
        }
        Ok(permission_decision)
    }

    async fn cached_permission_decision(&self, cache_key: &str) -> Option<CopilotPermissionDecision> {
        let mut cache = self.tool_permission_cache.write().await;
        match cache.get_permission(cache_key) {
            Some(PermissionGrant::Session | PermissionGrant::Permanent) => {
                Some(CopilotPermissionDecision::ApprovedAlways)
            }
            Some(PermissionGrant::Denied) => Some(CopilotPermissionDecision::DeniedByRules),
            _ => None,
        }
    }

    async fn handle_vtcode_tool_call(
        &mut self,
        renderer: &mut AnsiRenderer,
        request: CopilotToolCallRequest,
    ) -> Result<CopilotToolCallResponse> {
        if !self.exposed_tool_names.contains(request.tool_name.as_str()) {
            return Ok(tool_not_exposed_response(&request.tool_name));
        }

        let prepared = self
            .tool_registry
            .admit_public_tool_call(&request.tool_name, &request.arguments)
            .with_context(|| format!("copilot tool preflight for '{}'", request.tool_name))?;
        let canonical_tool_name = prepared.canonical_name;
        let effective_arguments = prepared.effective_args;

        if !self.exposed_tool_names.contains(canonical_tool_name.as_str()) {
            return Ok(tool_not_exposed_response(&canonical_tool_name));
        }

        if let Some(response) = self
            .prepare_vtcode_tool_execution(renderer, &request.tool_call_id, &canonical_tool_name, &effective_arguments)
            .await?
        {
            return Ok(response);
        }

        let effective_arguments = self
            .pending_hook_rewritten_args
            .remove(request.tool_call_id.as_str())
            .unwrap_or(effective_arguments);

        // The mutation guard runs after the hook phase so it evaluates the
        // arguments that will actually execute: a PreToolUse rewrite could
        // turn a read-only call into a mutating one (or vice versa).
        if self.loop_tracker.as_deref().is_some_and(|tracker| {
            mutation_blocked_until_verification(tracker, &canonical_tool_name, &effective_arguments)
        }) {
            return Ok(denied_tool_response(
                &canonical_tool_name,
                "mutating tool call blocked until verification succeeds",
            ));
        }

        self.record_tool_use(&canonical_tool_name);

        let tools = Arc::new(RwLock::new(self.exposed_tools.clone()));
        let turn_index = self.harness_state.tool_calls;
        let tool_item_id = harness_call_item_id(&self.harness_item_prefix, &request.tool_call_id, &canonical_tool_name);
        let (pipeline_outcome, last_stdout) = {
            let mut run_loop_ctx = RunLoopContext::new(
                renderer,
                self.handle,
                self.tool_registry,
                &tools,
                self.tool_result_cache,
                self.tool_permission_cache,
                self.permissions_state,
                self.decision_ledger,
                self.session_stats,
                self.plan_session,
                self.mcp_panel_state,
                self.approval_recorder,
                self.session,
                Some(self.safety_validator),
                self.traj,
                self.harness_state,
                self.harness_emitter,
            );

            let pipeline_outcome = run_tool_call_with_args(
                &mut run_loop_ctx,
                tool_item_id,
                &canonical_tool_name,
                &effective_arguments,
                self.ctrl_c_state,
                self.ctrl_c_notify,
                self.default_placeholder.clone(),
                self.lifecycle_hooks,
                self.skip_confirmations,
                self.vt_cfg,
                turn_index,
                true,
            )
            .await
            .with_context(|| format!("copilot tool execution for '{canonical_tool_name}'"))?;

            let (modified_files, last_stdout) = handle_pipeline_output(
                &mut run_loop_ctx,
                &canonical_tool_name,
                &effective_arguments,
                &pipeline_outcome,
                self.vt_cfg,
            )
            .await
            .with_context(|| format!("copilot tool output rendering for '{canonical_tool_name}'"))?;
            if !modified_files.is_empty() {
                run_loop_ctx
                    .session_stats
                    .record_touched_files(modified_files.iter().map(|path| path.display().to_string()));
            }

            if let Some(loop_tracker) = self.loop_tracker.as_deref_mut() {
                if update_repetition_tracker(
                    loop_tracker,
                    &pipeline_outcome,
                    &canonical_tool_name,
                    &effective_arguments,
                ) {
                    // A failed verifier grants fix-up edits; reset the text
                    // streak so the diagnostic summary does not immediately
                    // trip the pending-verification text cap. Reset through
                    // the run-loop context, which owns the `&mut`
                    // `harness_state` borrow for this block.
                    run_loop_ctx.harness_state.reset_assistant_text_response_streak();
                }
                run_loop_ctx
                    .session_stats
                    .set_verification_snapshot(loop_tracker.verification_snapshot());
                if let Some(signal) = self.suppress_output_signal.as_ref() {
                    signal.store(loop_tracker.verification_is_pending(), Ordering::Release);
                }
            }

            (pipeline_outcome, last_stdout)
        };

        if pipeline_outcome.status.is_failure_like() {
            self.harness_state.record_failed_tool_call();
        }

        match pipeline_outcome.status {
            ToolExecutionStatus::Success { output, command_success, .. } if command_success => {
                let text_result = last_stdout
                    .filter(|s: &String| !s.trim().is_empty())
                    .unwrap_or_else(|| serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string()));
                Ok(CopilotToolCallResponse::Success(CopilotToolCallSuccess {
                    text_result_for_llm: copilot_tool_result_text(&canonical_tool_name, text_result),
                }))
            }
            ToolExecutionStatus::Success { output, .. } => {
                let diagnosis = deterministic_output_diagnosis(&canonical_tool_name, &effective_arguments, &output);
                render_diagnosis(
                    renderer,
                    self.harness_emitter,
                    &self.harness_state.turn_id.0,
                    &canonical_tool_name,
                    &diagnosis,
                );
                let evidence = bounded_output_evidence(&canonical_tool_name, &effective_arguments, &output);
                Ok(copilot_failure_response_with_diagnosis(
                    &canonical_tool_name,
                    &evidence,
                    &format!(
                        "tool '{}' returned a non-zero exit code; {}",
                        canonical_tool_name, diagnosis.likely_cause
                    ),
                    &diagnosis,
                ))
            }
            ToolExecutionStatus::Failure { error } => {
                let diagnosis = deterministic_error_diagnosis(&error, "execution");
                render_diagnosis(
                    renderer,
                    self.harness_emitter,
                    &self.harness_state.turn_id.0,
                    &canonical_tool_name,
                    &diagnosis,
                );
                let evidence = bounded_error_evidence(&canonical_tool_name, &effective_arguments, &error, "execution");
                Ok(copilot_failure_response_with_diagnosis(
                    &canonical_tool_name,
                    &evidence,
                    &format!("tool '{canonical_tool_name}' failed: {}", error.message),
                    &diagnosis,
                ))
            }
            ToolExecutionStatus::Timeout { error } => {
                let diagnosis = deterministic_error_diagnosis(&error, "timeout");
                render_diagnosis(
                    renderer,
                    self.harness_emitter,
                    &self.harness_state.turn_id.0,
                    &canonical_tool_name,
                    &diagnosis,
                );
                let evidence = bounded_error_evidence(&canonical_tool_name, &effective_arguments, &error, "timeout");
                Ok(copilot_failure_response_with_diagnosis(
                    &canonical_tool_name,
                    &evidence,
                    &format!("tool '{canonical_tool_name}' timed out: {}", error.message),
                    &diagnosis,
                ))
            }
            ToolExecutionStatus::Cancelled => Ok(tool_cancelled_response(&canonical_tool_name)),
        }
    }

    async fn prepare_vtcode_tool_execution(
        &mut self,
        renderer: &mut AnsiRenderer,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Option<CopilotToolCallResponse>> {
        // PreToolUse hooks run before the safety gateway so rewritten
        // arguments are what every downstream check evaluates, mirroring the
        // non-prevalidated pipeline path.
        let hook_phase = match crate::agent::runloop::unified::tool_routing::pipeline_pre_tool_hooks(
            self.lifecycle_hooks,
            renderer,
            tool_name,
            arguments,
            tool_call_id,
        )
        .await
        {
            Ok(phase) => phase,
            Err(err) => {
                return Ok(Some(denied_tool_response(tool_name, &format!("pre-tool hook phase failed: {err}"))));
            }
        };
        let rewritten_arguments = match &hook_phase {
            Some(PreToolHookPhaseResult::Deny) => {
                return Ok(Some(denied_tool_response(tool_name, "Tool permission denied")));
            }
            Some(PreToolHookPhaseResult::Proceed { rewritten_args, .. }) => rewritten_args.clone(),
            None => None,
        };

        // Re-validate rewritten arguments against the tool schema: preflight
        // ran on the original payload, so a hook rewrite could otherwise
        // bypass schema checks.
        if let Some(rewritten) = rewritten_arguments.as_ref()
            && let Err(err) = self.tool_registry.preflight_validate_harness_call(tool_name, rewritten)
        {
            return Ok(Some(denied_tool_response(
                tool_name,
                &format!("PreToolUse hook produced invalid arguments: {err}"),
            )));
        }

        let invocation_id = invocation_id_from_call_id(tool_call_id);
        let safety_args = rewritten_arguments.as_ref().unwrap_or(arguments);
        let session_limit_auto_grant = self.session_limit_auto_grant();
        let safety_approval_justification = match validate_tool_call_with_limit_prompt(
            self.safety_validator,
            self.handle,
            self.session,
            self.ctrl_c_state,
            self.ctrl_c_notify,
            tool_name,
            safety_args,
            invocation_id,
            Some(self.harness_state),
            self.harness_emitter,
            self.agent_name.as_deref(),
            session_limit_auto_grant,
            self.traj,
            self.tool_registry.is_planning_active(),
        )
        .await
        {
            Ok(()) => None,
            Err(SafetyValidationFailure::SessionLimitNotIncreased) => {
                return Ok(Some(denied_tool_response(
                    tool_name,
                    "session tool limit reached and not increased by user",
                )));
            }
            Err(SafetyValidationFailure::SessionLimitPromptFailed(error)) => {
                return Ok(Some(denied_tool_response(
                    tool_name,
                    &format!("failed while requesting a session tool-limit increase: {error}"),
                )));
            }
            Err(SafetyValidationFailure::NeedsApproval(justification)) => Some(justification),
            Err(SafetyValidationFailure::Validation(error)) => {
                return Ok(Some(denied_tool_response(tool_name, &format!("safety validation failed: {error}"))));
            }
        };

        match ensure_tool_permission_with_call_id(
            self.tool_permissions_context_with_safety(renderer, safety_approval_justification.as_deref()),
            tool_name,
            Some(rewritten_arguments.as_ref().unwrap_or(arguments)),
            Some(tool_call_id),
            hook_phase,
        )
        .await?
        {
            ToolPermissionFlow::Approved { updated_args } => {
                let final_args = updated_args.or_else(|| rewritten_arguments.clone());
                if let Some(updated) = final_args {
                    // A PermissionRequest hook may supply its own rewrite via
                    // `updated_input`; validate the schema and re-run the
                    // safety gateway against the final arguments so the
                    // replacement does not execute under decisions made for
                    // earlier arguments.
                    if let Err(err) = self.tool_registry.preflight_validate_harness_call(tool_name, &updated) {
                        return Ok(Some(denied_tool_response(
                            tool_name,
                            &format!("PermissionRequest hook produced invalid arguments: {err}"),
                        )));
                    }
                    if safety_args != &updated {
                        let invocation_id = invocation_id_from_call_id(tool_call_id);
                        match validate_tool_call_with_limit_prompt(
                            self.safety_validator,
                            self.handle,
                            self.session,
                            self.ctrl_c_state,
                            self.ctrl_c_notify,
                            tool_name,
                            &updated,
                            invocation_id,
                            Some(self.harness_state),
                            self.harness_emitter,
                            self.agent_name.as_deref(),
                            session_limit_auto_grant,
                            self.traj,
                            self.tool_registry.is_planning_active(),
                        )
                        .await
                        {
                            Ok(()) => {}
                            Err(SafetyValidationFailure::SessionLimitNotIncreased) => {
                                return Ok(Some(denied_tool_response(
                                    tool_name,
                                    "session tool limit reached and not increased by user",
                                )));
                            }
                            Err(SafetyValidationFailure::SessionLimitPromptFailed(error)) => {
                                return Ok(Some(denied_tool_response(
                                    tool_name,
                                    &format!("failed while requesting a session tool-limit increase: {error}"),
                                )));
                            }
                            Err(SafetyValidationFailure::NeedsApproval(_)) => {
                                // The user already approved the final arguments
                                // in the PermissionRequest prompt; the gateway's
                                // NeedsApproval for the rewritten command is
                                // subsumed by that human approval.
                            }
                            Err(SafetyValidationFailure::Validation(error)) => {
                                return Ok(Some(denied_tool_response(
                                    tool_name,
                                    &format!("safety validation failed: {error}"),
                                )));
                            }
                        }
                    }
                    self.pending_hook_rewritten_args.insert(CompactStr::from(tool_call_id), updated);
                }
            }
            ToolPermissionFlow::Denied => {
                let diagnostic = tool_denial_diagnostic(tool_name);
                let text = if let Some(diag) = diagnostic.as_ref() {
                    let impact = diag["impact"].as_str().unwrap_or("");
                    let fix = diag["fix"]["action"].as_str().unwrap_or("");
                    format!("VT Code denied the tool `{tool_name}`. {impact} {fix}")
                } else {
                    format!("VT Code denied the tool `{tool_name}`.")
                };
                return Ok(Some(CopilotToolCallResponse::Failure(CopilotToolCallFailure {
                    text_result_for_llm: text,
                    error: format!("tool '{tool_name}' denied by user or policy"),
                })));
            }
            ToolPermissionFlow::Blocked { reason } => {
                return Ok(Some(denied_tool_response(tool_name, &reason)));
            }
            ToolPermissionFlow::Exit | ToolPermissionFlow::Interrupted => {
                return Ok(Some(denied_tool_response(tool_name, "permission request interrupted")));
            }
        }

        // Control-plane exec calls (wait/inspect) bypass the per-turn budget
        // exhaustion rejection so a long build stays observable.
        if !vtcode_core::tools::tool_intent::is_turn_budget_exempt_call(tool_name, safety_args)
            && let Some(exhaustion) = self.harness_state.tool_budget_exhaustion()
        {
            // Nothing will execute for this call id; drop the pending
            // rewrite so a later retry with the same id cannot inherit stale
            // arguments.
            self.pending_hook_rewritten_args.remove(tool_call_id);
            self.harness_state.record_tool_budget_rejection();
            return Ok(Some(tool_exceeded_budget_response(tool_name, exhaustion.max)));
        }

        self.harness_state.record_admitted_tool_call();
        // Control-plane exec calls (wait/inspect) do not consume the per-turn
        // tool-call budget; see `record_tool_call_budget_usage` for rationale.
        // The exemption is judged on `safety_args` — the same arguments the
        // safety gateway and permission flow evaluated — so a hook rewrite
        // that changes the action cannot swap the budget treatment after
        // admission.
        if !vtcode_core::tools::tool_intent::is_turn_budget_exempt_call(tool_name, safety_args)
            && let Some(warning) = self.harness_state.record_tool_call_with_default_warning()
        {
            warning.log_threshold_reached("Tool-call budget warning threshold reached in copilot ACP path");
        }

        Ok(None)
    }

    fn tool_permissions_context_with_safety<'b>(
        &'b mut self,
        renderer: &'b mut AnsiRenderer,
        safety_approval_justification: Option<&str>,
    ) -> ToolPermissionsContext<'b, InlineSession> {
        ToolPermissionsContext {
            tool_registry: self.tool_registry,
            renderer,
            handle: self.handle,
            session: self.session,
            active_thread_label: None,
            default_placeholder: self.default_placeholder.clone(),
            ctrl_c_state: self.ctrl_c_state,
            ctrl_c_notify: self.ctrl_c_notify,
            hooks: self.lifecycle_hooks,
            justification: None,
            approval_recorder: Some(self.approval_recorder),
            decision_ledger: Some(self.decision_ledger),
            tool_permission_cache: Some(self.tool_permission_cache),
            permissions_state: Some(self.permissions_state),
            active_agent_permissions: self.active_agent_permissions,
            hitl_notification_bell: self.hitl_notification_bell,
            approval_policy: self.approval_policy,
            skip_confirmations: self.skip_confirmations,
            permissions_config: self.vt_cfg.map(|cfg| &cfg.permissions),
            auto_permission_runtime: None,
            session_stats: Some(self.session_stats),
            safety_approval_justification: safety_approval_justification.map(String::from),
            harness_emitter: self.harness_emitter,
        }
    }

    fn record_tool_use(&mut self, tool_name: &str) {
        // Copilot executes VTCode tools inside the streaming request, so its
        // progress boundary is absent from `working_history`. Record it in the
        // authoritative turn state only after admission; denied calls never
        // reach this method and cannot erase the response streak.
        self.harness_state.record_out_of_band_tool_progress();
        self.session_stats.record_tool(tool_name);
    }

    fn record_out_of_band_tool_use(&mut self, tool_name: &str) {
        self.harness_state.record_out_of_band_tool_call();
        self.session_stats.record_tool(tool_name);
    }

    fn emit_tool_started_event(&self, tool_call_id: &str, tool_name: &str, arguments: &Value) {
        let Some(emitter) = self.harness_emitter else {
            return;
        };
        let item_id = harness_call_item_id(&self.harness_item_prefix, tool_call_id, tool_name);
        let raw_id = raw_tool_call_id(tool_call_id);
        let _ = emitter.emit(tool_started_event(item_id.clone(), tool_name, Some(arguments), raw_id));
        let _ = emitter.emit(tool_output_started_event(item_id, raw_id));
    }

    fn emit_tool_finished_event(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &Value,
        status: ToolCallStatus,
        output: Option<String>,
    ) {
        let Some(emitter) = self.harness_emitter else {
            return;
        };
        let item_id = harness_call_item_id(&self.harness_item_prefix, tool_call_id, tool_name);
        let raw_id = raw_tool_call_id(tool_call_id);
        let _ = emitter.emit(tool_invocation_completed_event(
            item_id.clone(),
            tool_name,
            Some(arguments),
            raw_id,
            status.clone(),
        ));
        let _ =
            emitter.emit(tool_output_completed_event(item_id, raw_id, status, None, None, output.unwrap_or_default()));
    }

    fn emit_tool_output_event(&self, tool_call_id: &str, tool_name: &str, output: &str) {
        let Some(emitter) = self.harness_emitter else {
            return;
        };
        let item_id = harness_call_item_id(&self.harness_item_prefix, tool_call_id, tool_name);
        let _ = emitter.emit(tool_updated_event(item_id, raw_tool_call_id(tool_call_id), output));
    }

    async fn handle_terminal_create(
        &mut self,
        request: CopilotTerminalCreateRequest,
    ) -> Result<CopilotTerminalCreateResponse> {
        let command_display = terminal_command_display(&request.command, &request.args);
        let response = self
            .tool_registry
            .execute_harness_command_session_terminal_run(terminal_run_args(&request))
            .await
            .context("copilot local terminal create")?;

        let terminal_id = response
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("copilot local terminal create missing session_id"))?;
        let initial_output = response.get("output").and_then(Value::as_str).map(str::to_string);
        let initial_exit_status = response
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(terminal_exit_status_from_code);
        let initial_session_completed = initial_exit_status.is_some();
        let released = Arc::new(AtomicBool::new(false));
        let exit_notify = Arc::new(tokio::sync::Notify::new());
        let state = Arc::new(Mutex::new(LocalTerminalSessionState::new(request.output_byte_limit)));
        {
            let mut session_state = lock_local_terminal_state(&state);
            if let Some(output) = initial_output.as_deref() {
                session_state.append_output(output);
            }
            session_state.exit_status = initial_exit_status.clone();
        }
        if initial_session_completed {
            exit_notify.notify_waiters();
        }

        let task = tokio::spawn(run_local_terminal_session(LocalTerminalTaskContext {
            tool_registry: self.tool_registry.clone(),
            exec_session_id: terminal_id.clone(),
            released: Arc::clone(&released),
            exit_notify: Arc::clone(&exit_notify),
            state: Arc::clone(&state),
            harness_emitter: self.harness_emitter.cloned(),
            harness_item_prefix: self.harness_item_prefix.clone(),
            handle: self.handle.clone(),
            tail_limit: resolve_stdout_tail_limit(self.vt_cfg),
            command_display,
            initial_output,
            pty_config: self.tool_registry.pty_config().clone(),
        }));

        self.local_terminal_sessions.insert(
            terminal_id.clone(),
            LocalTerminalSession {
                exec_session_id: terminal_id.clone(),
                released,
                exit_notify,
                state,
                task,
            },
        );

        Ok(CopilotTerminalCreateResponse { terminal_id })
    }

    async fn handle_terminal_output(&self, terminal_id: &str) -> Result<CopilotTerminalOutputResponse> {
        self.local_terminal_sessions
            .get(terminal_id)
            .map(|s| s.snapshot_output())
            .ok_or_else(|| anyhow!("copilot terminal '{terminal_id}' not found"))
    }

    async fn handle_terminal_release(&mut self, terminal_id: &str) -> Result<()> {
        let Some(session) = self.local_terminal_sessions.remove(terminal_id) else {
            return Ok(());
        };
        let exec_session_id = session.exec_session_id.clone();
        session.release();
        self.tool_registry.close_harness_exec_session(&exec_session_id).await?;
        Ok(())
    }

    async fn handle_terminal_kill(&self, terminal_id: &str) -> Result<()> {
        let session = self
            .local_terminal_sessions
            .get(terminal_id)
            .ok_or_else(|| anyhow!("copilot terminal '{terminal_id}' not found"))?;
        self.tool_registry
            .terminate_harness_exec_session(&session.exec_session_id)
            .await
            .with_context(|| format!("copilot terminal kill '{}'", session.exec_session_id))
    }

    async fn handle_terminal_wait_for_exit(&self, terminal_id: &str) -> Result<CopilotTerminalExitStatus> {
        let session = self
            .local_terminal_sessions
            .get(terminal_id)
            .ok_or_else(|| anyhow!("copilot terminal '{terminal_id}' not found"))?;
        Ok(session.wait_for_exit().await)
    }

    fn handle_observed_tool_call(&mut self, update: CopilotObservedToolCall) {
        if let Some(terminal_id) = update.terminal_id.as_deref()
            && let Some(session) = self.local_terminal_sessions.get(terminal_id)
        {
            let bind_result = session.bind_observed_tool_call(&update);
            if bind_result.emit_started {
                self.record_out_of_band_tool_use(&bind_result.association.tool_name);
                self.emit_tool_started_event(
                    &bind_result.association.tool_call_id,
                    &bind_result.association.tool_name,
                    &bind_result.association.arguments,
                );
            }
            if let Some(output) = bind_result.buffered_output.as_deref() {
                self.emit_tool_output_event(
                    &bind_result.association.tool_call_id,
                    &bind_result.association.tool_name,
                    output,
                );
            }
            if let Some(status) = bind_result.finish_status {
                if matches!(status, ToolCallStatus::Failed) {
                    self.harness_state.record_failed_tool_call();
                }
                self.emit_tool_finished_event(
                    &bind_result.association.tool_call_id,
                    &bind_result.association.tool_name,
                    &bind_result.association.arguments,
                    status,
                    bind_result.buffered_output,
                );
            }
            return;
        }

        let tool_call_id = update.tool_call_id.clone();
        let tail_limit = resolve_stdout_tail_limit(self.vt_cfg);

        let tool_update = {
            let state = self
                .observed_tool_calls
                .entry(tool_call_id.clone())
                .or_insert_with(|| ObservedToolCallState::new(update.tool_name.clone()));
            process_observed_tool_state(state, &update, tail_limit, self.handle, self.tool_registry)
        };

        if tool_update.started {
            let tool_name = self.observed_tool_calls[&tool_call_id].tool_name.clone();
            self.record_out_of_band_tool_use(&tool_name);
            self.emit_tool_started_event(&tool_call_id, &tool_name, update.arguments.as_ref().unwrap_or(&Value::Null));
        }

        if let Some(output) = tool_update.output_delta {
            let tool_name = self.observed_tool_calls[&tool_call_id].tool_name.clone();
            self.emit_tool_output_event(&tool_call_id, &tool_name, &output);
        }

        if tool_update.finished {
            let state = &self.observed_tool_calls[&tool_call_id];
            let status = match update.status {
                CopilotObservedToolCallStatus::Completed => ToolCallStatus::Completed,
                CopilotObservedToolCallStatus::Failed => ToolCallStatus::Failed,
                _ => ToolCallStatus::InProgress,
            };
            if matches!(status, ToolCallStatus::Failed) {
                self.harness_state.record_failed_tool_call();
            }
            self.emit_tool_finished_event(
                &tool_call_id,
                &state.tool_name,
                update.arguments.as_ref().unwrap_or(&Value::Null),
                status,
                update.output,
            );
        }
    }

    fn handle_compatibility_notice(
        &mut self,
        renderer: &mut AnsiRenderer,
        state: CopilotAcpCompatibilityState,
        message: String,
    ) -> Result<()> {
        if self.compatibility_notice_shown {
            return Ok(());
        }
        self.compatibility_notice_shown = true;
        tracing::warn!(
            target: "copilot.acp",
            ?state,
            message = %message,
            "GitHub Copilot ACP compatibility changed"
        );
        crate::agent::runloop::unified::turn::turn_helpers::display_status(renderer, &message)?;
        Ok(())
    }
}

#[async_trait]
impl CopilotRuntimeRequestHandler for CopilotRuntimeHost<'_> {
    async fn handle_runtime_request(
        &mut self,
        renderer: &mut AnsiRenderer,
        request: CopilotRuntimeRequest,
    ) -> Result<(), uni::LLMError> {
        match request {
            CopilotRuntimeRequest::Permission(request_event) => {
                let decision = self
                    .handle_builtin_permission(renderer, request_event.request.clone())
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond(decision).map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::ToolCall(request_event) => {
                self.harness_state.record_requested_tool_calls(1);
                let admitted_before = self.harness_state.admitted_tool_call_count();
                let response = self
                    .handle_vtcode_tool_call(renderer, request_event.request.clone())
                    .await
                    .map_err(|error| {
                        self.harness_state.record_failed_tool_call();
                        map_runtime_error(error)
                    })?;
                let response = append_session_limit_grant_guidance(
                    response,
                    self.harness_state.take_session_limit_grant_directive_pending(),
                );
                let budget_rejected = self.harness_state.take_tool_budget_rejection();
                if self.harness_state.admitted_tool_call_count() == admitted_before
                    && matches!(&response, CopilotToolCallResponse::Failure(_))
                    && !budget_rejected
                {
                    self.harness_state.record_denied_tool_call();
                }
                request_event.respond(response).map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::TerminalCreate(request_event) => {
                let response = self
                    .handle_terminal_create(request_event.request.clone())
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond(response).map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::TerminalOutput(request_event) => {
                let response = self
                    .handle_terminal_output(&request_event.request.terminal_id)
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond(response).map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::TerminalRelease(request_event) => {
                self.handle_terminal_release(&request_event.request.terminal_id)
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond().map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::TerminalKill(request_event) => {
                self.handle_terminal_kill(&request_event.request.terminal_id)
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond().map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::TerminalWaitForExit(request_event) => {
                let response = self
                    .handle_terminal_wait_for_exit(&request_event.request.terminal_id)
                    .await
                    .map_err(map_runtime_error)?;
                request_event.respond(response).map_err(map_runtime_error)?;
            }
            CopilotRuntimeRequest::ObservedToolCall(update) => {
                self.handle_observed_tool_call(update);
            }
            CopilotRuntimeRequest::CompatibilityNotice(notice) => {
                self.handle_compatibility_notice(renderer, notice.state, notice.message)
                    .map_err(map_runtime_error)?;
            }
        }
        Ok(())
    }
}

impl Drop for CopilotRuntimeHost<'_> {
    fn drop(&mut self) {
        for (_, session) in self.local_terminal_sessions.drain() {
            session.abort();
        }
    }
}

struct ObservedToolCallState {
    tool_name: String,
    started: bool,
    finished: bool,
    last_output: Option<String>,
    pty_stream: Option<ObservedToolPtyStream>,
}

impl ObservedToolCallState {
    fn new(tool_name: String) -> Self {
        Self {
            tool_name,
            started: false,
            finished: false,
            last_output: None,
            pty_stream: None,
        }
    }
}

struct ObservedToolUpdate {
    started: bool,
    output_delta: Option<String>,
    finished: bool,
}

fn process_observed_tool_state(
    state: &mut ObservedToolCallState,
    update: &CopilotObservedToolCall,
    tail_limit: usize,
    handle: &InlineHandle,
    tool_registry: &ToolRegistry,
) -> ObservedToolUpdate {
    if state.tool_name == "copilot_tool" && update.tool_name != "copilot_tool" {
        state.tool_name = update.tool_name.clone();
    }

    let started = if !state.started {
        state.started = true;
        true
    } else {
        false
    };

    if started
        && state.pty_stream.is_none()
        && let Some(cmd) = observed_tool_command_display(update)
    {
        state.pty_stream =
            Some(ObservedToolPtyStream::start(handle, tail_limit, cmd, tool_registry.pty_config().clone()));
    }

    let output_delta = if let Some(output) = update.output.as_deref().filter(|t| !t.trim().is_empty())
        && state.last_output.as_deref() != Some(output)
    {
        if let Some(delta) = observed_tool_output_delta(state.last_output.as_deref(), output)
            && !delta.is_empty()
            && let Some(stream) = state.pty_stream.as_ref()
        {
            stream.push_output(delta);
        }
        state.last_output = Some(output.to_string());
        Some(output.to_string())
    } else {
        None
    };

    let finished = !state.finished
        && matches!(update.status, CopilotObservedToolCallStatus::Completed | CopilotObservedToolCallStatus::Failed);
    if finished {
        state.finished = true;
        let _ = state.pty_stream.take().map(|s| s.finish(update.status));
    }

    ObservedToolUpdate { started, output_delta, finished }
}

struct ObservedToolPtyStream {
    _progress_reporter: ProgressReporter,
    _spinner: PlaceholderSpinner,
    _runtime: PtyStreamRuntime,
    callback: ToolProgressCallback,
}

impl ObservedToolPtyStream {
    fn start(handle: &InlineHandle, tail_limit: usize, command_display: String, pty_config: PtyConfig) -> Self {
        let progress_reporter = ProgressReporter::new();
        let spinner = PlaceholderSpinner::with_progress(
            handle,
            None,
            None,
            format!("Running command: {command_display}"),
            Some(&progress_reporter),
        );
        spinner.set_defer_restore(true);
        let (runtime, callback) = PtyStreamRuntime::start(
            handle.clone(),
            progress_reporter.clone(),
            tail_limit,
            Some(command_display),
            pty_config,
            None,
            true,
        );

        Self {
            _progress_reporter: progress_reporter,
            _spinner: spinner,
            _runtime: runtime,
            callback,
        }
    }

    fn push_output(&self, chunk: &str) {
        (self.callback)("exec_command", chunk);
    }

    fn finish(self, status: CopilotObservedToolCallStatus) {
        self._spinner.finish();
        let progress_reporter = self._progress_reporter.clone();
        let runtime = self._runtime;
        drop(self.callback);

        tokio::spawn(async move {
            progress_reporter.complete().await;
            runtime.shutdown(copilot_observed_status_color(status)).await;
        });
    }
}

#[derive(Clone)]
struct LocalTerminalAssociation {
    tool_call_id: String,
    tool_name: String,
    arguments: Value,
}

struct LocalTerminalSessionState {
    output: String,
    truncated: bool,
    output_byte_limit: Option<usize>,
    exit_status: Option<CopilotTerminalExitStatus>,
    association: Option<LocalTerminalAssociation>,
    tool_started: bool,
    tool_finished: bool,
}

impl LocalTerminalSessionState {
    fn new(output_byte_limit: Option<usize>) -> Self {
        Self {
            output: String::new(),
            truncated: false,
            output_byte_limit,
            exit_status: None,
            association: None,
            tool_started: false,
            tool_finished: false,
        }
    }

    fn append_output(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.output.push_str(chunk);
        if let Some(limit) = self.output_byte_limit
            && self.output.len() > limit
        {
            let mut drain_until = self.output.len() - limit;
            while drain_until < self.output.len() && !self.output.is_char_boundary(drain_until) {
                drain_until += 1;
            }
            if drain_until > 0 {
                self.output.drain(..drain_until);
                self.truncated = true;
            }
        }
    }
}

struct LocalTerminalSession {
    exec_session_id: String,
    released: Arc<AtomicBool>,
    exit_notify: Arc<tokio::sync::Notify>,
    state: Arc<Mutex<LocalTerminalSessionState>>,
    task: tokio::task::JoinHandle<()>,
}

struct LocalTerminalBindResult {
    association: LocalTerminalAssociation,
    emit_started: bool,
    buffered_output: Option<String>,
    finish_status: Option<ToolCallStatus>,
}

struct LocalTerminalTaskContext {
    tool_registry: ToolRegistry,
    exec_session_id: String,
    released: Arc<AtomicBool>,
    exit_notify: Arc<tokio::sync::Notify>,
    state: Arc<Mutex<LocalTerminalSessionState>>,
    harness_emitter: Option<HarnessEventEmitter>,
    harness_item_prefix: String,
    handle: InlineHandle,
    tail_limit: usize,
    command_display: String,
    initial_output: Option<String>,
    pty_config: PtyConfig,
}

impl LocalTerminalSession {
    fn bind_observed_tool_call(&self, update: &CopilotObservedToolCall) -> LocalTerminalBindResult {
        let mut state = lock_local_terminal_state(&self.state);
        let association = if let Some(association) = state.association.as_mut() {
            if association.tool_name == "copilot_tool" && update.tool_name != "copilot_tool" {
                association.tool_name = update.tool_name.clone();
            }
            if association.arguments.is_null()
                && let Some(arguments) = update.arguments.clone()
            {
                association.arguments = arguments;
            }
            association.clone()
        } else {
            let association = LocalTerminalAssociation {
                tool_call_id: update.tool_call_id.clone(),
                tool_name: update.tool_name.clone(),
                arguments: update.arguments.clone().unwrap_or(Value::Null),
            };
            state.association = Some(association.clone());
            association
        };

        let emit_started = if state.tool_started {
            false
        } else {
            state.tool_started = true;
            true
        };
        let buffered_output = emit_started
            .then(|| state.output.clone())
            .filter(|output| !output.trim().is_empty());
        let finish_status = if let Some(exit_status) = state.exit_status.clone() {
            if state.tool_finished {
                None
            } else {
                state.tool_finished = true;
                Some(tool_status_from_exit(&exit_status))
            }
        } else {
            None
        };

        LocalTerminalBindResult {
            association,
            emit_started,
            buffered_output,
            finish_status,
        }
    }

    fn snapshot_output(&self) -> CopilotTerminalOutputResponse {
        let state = lock_local_terminal_state(&self.state);
        CopilotTerminalOutputResponse {
            output: state.output.clone(),
            truncated: state.truncated,
            exit_status: state.exit_status.clone(),
        }
    }

    async fn wait_for_exit(&self) -> CopilotTerminalExitStatus {
        loop {
            if let Some(exit_status) = lock_local_terminal_state(&self.state).exit_status.clone() {
                return exit_status;
            }
            self.exit_notify.notified().await;
        }
    }

    fn release(self) {
        self.released.store(true, Ordering::Relaxed);
        self.exit_notify.notify_waiters();
        self.task.abort();
    }

    fn abort(self) {
        self.release();
    }
}

pub(super) fn prompt_session_to_stream(
    model: String,
    prompt_session: PromptSession,
) -> (uni::LLMStream, tokio::sync::mpsc::UnboundedReceiver<CopilotRuntimeRequest>) {
    streaming::prompt_session_to_stream(model, prompt_session)
}

async fn run_local_terminal_session(task: LocalTerminalTaskContext) {
    let LocalTerminalTaskContext {
        tool_registry,
        exec_session_id,
        released,
        exit_notify,
        state,
        harness_emitter,
        harness_item_prefix,
        handle,
        tail_limit,
        command_display,
        initial_output,
        pty_config,
    } = task;

    let (pty_stream, _elapsed_guard) = setup_terminal_stream(&handle, tail_limit, &command_display, pty_config).await;
    let mut final_status = None;

    if let Some(output) = initial_output.as_deref() {
        pty_stream.progress_callback("exec_command", output);
    }

    loop {
        if released.load(Ordering::Relaxed) {
            break;
        }

        match tool_registry.read_harness_exec_session_output(&exec_session_id, true).await {
            Ok(Some(chunk)) if !chunk.is_empty() => {
                pty_stream.progress_callback("exec_command", &chunk);
                if let Some((tool_call_id, tool_name, output)) = update_local_terminal_output(&state, &chunk) {
                    emit_terminal_output_event(
                        harness_emitter.as_ref(),
                        &harness_item_prefix,
                        &tool_call_id,
                        &tool_name,
                        &output,
                    );
                }
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    terminal_id = %exec_session_id,
                    error = %err,
                    "Failed to read Copilot local terminal output"
                );
                break;
            }
        }

        match tool_registry.harness_exec_session_completed(&exec_session_id).await {
            Ok(Some(code)) => {
                let exit_status = terminal_exit_status_from_code(i64::from(code));
                final_status = exit_status.as_ref().map(tool_status_from_exit);
                if let Some((tool_call_id, tool_name, arguments, output, status)) =
                    finalize_local_terminal_exit(&state, exit_status)
                {
                    emit_terminal_finished_event(
                        harness_emitter.as_ref(),
                        &harness_item_prefix,
                        &tool_call_id,
                        &tool_name,
                        &arguments,
                        status,
                        output,
                    );
                }
                exit_notify.notify_waiters();
                break;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    terminal_id = %exec_session_id,
                    error = %err,
                    "Failed to poll Copilot local terminal exit state"
                );
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    pty_stream.finalize(final_status).await;
}

struct TerminalStream {
    _progress_reporter: ProgressReporter,
    _spinner: PlaceholderSpinner,
    _runtime: PtyStreamRuntime,
    callback: ToolProgressCallback,
}

impl TerminalStream {
    fn progress_callback(&self, tool: &str, output: &str) {
        (self.callback)(tool, output);
    }

    async fn finalize(self, status: Option<ToolCallStatus>) {
        self._spinner.finish();
        let progress_reporter = self._progress_reporter.clone();
        let runtime = self._runtime;
        drop(self.callback);

        tokio::spawn(async move {
            progress_reporter.complete().await;
            // An incomplete local terminal session is not a success. Keep the
            // header yellow when cancellation or a monitor error prevents an
            // exit code from being observed.
            let status = status.unwrap_or(ToolCallStatus::InProgress);
            runtime.shutdown(tool_call_status_color(&status)).await;
        });
    }
}

async fn setup_terminal_stream(
    handle: &InlineHandle,
    tail_limit: usize,
    command_display: &str,
    pty_config: PtyConfig,
) -> (TerminalStream, ProgressUpdateGuard) {
    let progress_reporter = ProgressReporter::new();
    progress_reporter.set_total(100).await;
    progress_reporter.set_progress(40).await;
    progress_reporter
        .set_message(format!("Running command: {command_display}"))
        .await;

    let elapsed_guard = ProgressUpdateGuard::new(spawn_elapsed_time_updater(
        progress_reporter.clone(),
        format!("command: {command_display}"),
        500,
    ));

    let spinner = PlaceholderSpinner::with_progress(
        handle,
        None,
        None,
        format!("Running command: {command_display}"),
        Some(&progress_reporter),
    );
    spinner.set_defer_restore(true);

    let (runtime, callback) = PtyStreamRuntime::start(
        handle.clone(),
        progress_reporter.clone(),
        tail_limit,
        Some(command_display.to_string()),
        pty_config,
        None,
        true,
    );

    (
        TerminalStream {
            _progress_reporter: progress_reporter,
            _spinner: spinner,
            _runtime: runtime,
            callback,
        },
        elapsed_guard,
    )
}

fn terminal_run_args(request: &CopilotTerminalCreateRequest) -> Value {
    json!({
        "action": "run",
        "command": request.command,
        "args": request.args,
        "cwd": request.cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "tty": true,
        "yield_time_ms": 100,
        "env": request.env.iter().map(|e| json!({"name": e.name, "value": e.value})).collect::<Vec<_>>(),
    })
}

fn terminal_command_display(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        let parts: Vec<&str> = std::iter::once(command).chain(args.iter().map(|s| s.as_str())).collect();
        shell_words::join(&parts)
    }
}

fn extract_command_from_args(arguments: Option<&Value>) -> Option<String> {
    let arguments = arguments?;
    // Display-only extraction shared with tool summaries. The previous
    // per-key loop returned `None` via `?` when the `command` key was absent,
    // never reaching `cmd`/`raw_command`; the canonical helper scans every
    // key and also covers the legacy `bash_command` key.
    vtcode_core::tools::command_args::extract_command_text_with_key(arguments).map(|(text, _)| text)
}

fn terminal_exit_status_from_code(code: i64) -> Option<CopilotTerminalExitStatus> {
    u32::try_from(code)
        .ok()
        .map(|exit_code| CopilotTerminalExitStatus { exit_code: Some(exit_code), signal: None })
}

fn tool_status_from_exit(exit_status: &CopilotTerminalExitStatus) -> ToolCallStatus {
    match exit_status.exit_code {
        Some(0) => ToolCallStatus::Completed,
        Some(_) | None => ToolCallStatus::Failed,
    }
}

fn copilot_observed_status_color(status: CopilotObservedToolCallStatus) -> Color {
    let palette = ColorPalette::default();
    match status {
        CopilotObservedToolCallStatus::Completed => palette.success,
        CopilotObservedToolCallStatus::Failed => palette.error,
        CopilotObservedToolCallStatus::Pending | CopilotObservedToolCallStatus::InProgress => palette.warning,
    }
}

fn tool_call_status_color(status: &ToolCallStatus) -> Color {
    let palette = ColorPalette::default();
    match status {
        ToolCallStatus::Completed => palette.success,
        ToolCallStatus::Failed => palette.error,
        ToolCallStatus::InProgress => palette.warning,
    }
}

fn update_local_terminal_output(
    state: &Arc<Mutex<LocalTerminalSessionState>>,
    chunk: &str,
) -> Option<(String, String, String)> {
    let mut state = lock_local_terminal_state(state);
    state.append_output(chunk);
    let association = state.association.clone()?;
    if !state.tool_started || chunk.trim().is_empty() {
        return None;
    }
    Some((association.tool_call_id, association.tool_name, state.output.clone()))
}

fn finalize_local_terminal_exit(
    state: &Arc<Mutex<LocalTerminalSessionState>>,
    exit_status: Option<CopilotTerminalExitStatus>,
) -> Option<(String, String, Value, String, ToolCallStatus)> {
    let mut state = lock_local_terminal_state(state);
    state.exit_status = exit_status.clone();
    let association = state.association.clone()?;
    if state.tool_finished {
        return None;
    }
    let exit_status = state.exit_status.clone()?;
    state.tool_finished = true;
    Some((
        association.tool_call_id,
        association.tool_name,
        association.arguments,
        state.output.clone(),
        tool_status_from_exit(&exit_status),
    ))
}

fn emit_terminal_output_event(
    emitter: Option<&HarnessEventEmitter>,
    harness_item_prefix: &str,
    tool_call_id: &str,
    tool_name: &str,
    output: &str,
) {
    let Some(_emitter) = emitter else { return };
    let item_id = harness_call_item_id(harness_item_prefix, tool_call_id, tool_name);
    let _ = _emitter.emit(tool_updated_event(item_id, raw_tool_call_id(tool_call_id), output));
}

fn emit_terminal_finished_event(
    emitter: Option<&HarnessEventEmitter>,
    harness_item_prefix: &str,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    status: ToolCallStatus,
    output: String,
) {
    let Some(emitter) = emitter else { return };
    let item_id = harness_call_item_id(harness_item_prefix, tool_call_id, tool_name);
    let raw_id = raw_tool_call_id(tool_call_id);
    let _ = emitter.emit(tool_invocation_completed_event(
        item_id.clone(),
        tool_name,
        Some(arguments),
        raw_id,
        status.clone(),
    ));
    let _ = emitter.emit(tool_output_completed_event(item_id, raw_id, status, None, None, output));
}

fn lock_local_terminal_state(
    state: &Arc<Mutex<LocalTerminalSessionState>>,
) -> MutexGuard<'_, LocalTerminalSessionState> {
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn observed_tool_command_display(update: &CopilotObservedToolCall) -> Option<String> {
    extract_command_from_args(update.arguments.as_ref()).or_else(|| {
        update
            .tool_name
            .strip_prefix("Run ")
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToString::to_string)
    })
}

fn observed_tool_output_delta<'a>(previous: Option<&str>, current: &'a str) -> Option<&'a str> {
    if current.is_empty() {
        return None;
    }

    match previous {
        None => Some(current),
        Some(prev) if prev == current => None,
        Some(prev) if current.starts_with(prev) => Some(&current[prev.len()..]),
        Some(prev) => {
            let prefix_len = calculate_common_prefix_len(prev, current);
            if prefix_len == 0 || prefix_len >= current.len() {
                Some(current)
            } else {
                Some(&current[prefix_len..])
            }
        }
    }
}

fn calculate_common_prefix_len(left: &str, right: &str) -> usize {
    let mut bytes = 0;
    for (left_char, right_char) in left.chars().zip(right.chars()) {
        if left_char != right_char {
            break;
        }
        bytes += left_char.len_utf8();
    }
    bytes
}

fn filter_copilot_tools(
    available_tools: Option<&Arc<Vec<ToolDefinition>>>,
    allowlist: &[String],
) -> Vec<ToolDefinition> {
    let allowlist: BTreeSet<&str> = allowlist.iter().map(String::as_str).collect();
    available_tools
        .into_iter()
        .flat_map(|tools| tools.iter())
        .filter(|tool| tool_definition_name(tool).is_some_and(|name| allowlist.contains(name)))
        .cloned()
        .collect()
}

fn tool_definition_name(tool: &ToolDefinition) -> Option<&str> {
    tool.function.as_ref().map(|function| function.name.as_str())
}

struct PermissionPromptSummary {
    cache_key: String,
    tool_name: String,
    display_name: String,
    learning_label: String,
    tool_args: Option<Value>,
    reason: Option<String>,
}

fn scoped_cache_key(prefix: &str, scope: Value) -> String {
    serde_json::to_string(&json!({
        "prefix": prefix,
        "scope": scope,
    }))
    .unwrap_or_else(|_| prefix.to_string())
}

fn map_builtin_permission_prompt_decision(
    decision: HitlDecision,
    feedback: Option<String>,
) -> (CopilotPermissionDecision, bool) {
    match decision {
        HitlDecision::Approved | HitlDecision::Enable => (CopilotPermissionDecision::Approved, false),
        HitlDecision::ApprovedSession | HitlDecision::ApprovedPermanent => {
            (CopilotPermissionDecision::ApprovedAlways, true)
        }
        HitlDecision::Denied | HitlDecision::DeniedOnce | HitlDecision::Exit | HitlDecision::Interrupt => {
            (CopilotPermissionDecision::DeniedInteractivelyByUser { feedback }, false)
        }
    }
}

fn auto_approve_builtin_permission(request: &CopilotPermissionRequest) -> Option<(CopilotPermissionDecision, bool)> {
    match request {
        // Copilot ACP keeps asking permission for these descriptive custom-tool
        // requests even though VT Code still controls the actual tool-call path.
        CopilotPermissionRequest::CustomTool { .. } => Some((CopilotPermissionDecision::ApprovedAlways, true)),
        _ => None,
    }
}

fn denied_tool_response(tool_name: &str, reason: &str) -> CopilotToolCallResponse {
    CopilotToolCallResponse::Failure(CopilotToolCallFailure {
        text_result_for_llm: format!("VT Code denied the tool `{tool_name}`."),
        error: format!("tool '{tool_name}' {reason}"),
    })
}

fn tool_not_exposed_response(tool_name: &str) -> CopilotToolCallResponse {
    denied_tool_response(tool_name, "is not allowlisted in VT Code")
}

fn tool_exceeded_budget_response(tool_name: &str, max_tool_calls: usize) -> CopilotToolCallResponse {
    CopilotToolCallResponse::Failure(CopilotToolCallFailure {
        text_result_for_llm: format!(
            "VT Code denied the tool `{tool_name}` because the turn exceeded its tool-call budget."
        ),
        error: format!("tool '{tool_name}' exceeded max tool calls per turn ({max_tool_calls})"),
    })
}

fn copilot_failure_response_with_diagnosis(
    tool_name: &str,
    evidence: &str,
    error: &str,
    diagnosis: &ToolFailureDiagnosis,
) -> CopilotToolCallResponse {
    let escaped_evidence = escape_untrusted_evidence(evidence);
    let failure_text =
        copilot_tool_result_text(tool_name, format!("VT Code failed to execute the tool `{tool_name}`."));
    CopilotToolCallResponse::Failure(CopilotToolCallFailure {
        text_result_for_llm: format!(
            "{failure_text}\n\n<untrusted_tool_evidence>\n{escaped_evidence}\n</untrusted_tool_evidence>\n\n{}",
            diagnosis.render_text(tool_name),
        ),
        error: bounded_diagnostic_field(error),
    })
}

fn copilot_tool_result_text(tool_name: &str, text: String) -> String {
    if vtcode_core::tools::tool_intent::is_command_tool(tool_name) {
        format!("{text}\n\n{COLLAPSED_TOOL_OUTPUT_NOTICE}")
    } else {
        text
    }
}

fn tool_cancelled_response(tool_name: &str) -> CopilotToolCallResponse {
    denied_tool_response(tool_name, "execution cancelled")
}

/// Copilot tool calls are answered directly through ACP rather than the normal
/// turn-processing context, so they cannot receive the shared system-message
/// flush. Append the same one-shot grant guidance to the tool result instead.
fn append_session_limit_grant_guidance(
    response: CopilotToolCallResponse,
    grant_pending: bool,
) -> CopilotToolCallResponse {
    if !grant_pending {
        return response;
    }

    let append_guidance = |text: &mut String| {
        text.push_str("\n\n");
        text.push_str(SESSION_LIMIT_GRANT_DIRECTIVE);
    };

    match response {
        CopilotToolCallResponse::Success(mut success) => {
            append_guidance(&mut success.text_result_for_llm);
            CopilotToolCallResponse::Success(success)
        }
        CopilotToolCallResponse::Failure(mut failure) => {
            append_guidance(&mut failure.text_result_for_llm);
            CopilotToolCallResponse::Failure(failure)
        }
    }
}

fn summarize_permission_request(request: &CopilotPermissionRequest) -> Option<PermissionPromptSummary> {
    #[derive(Debug)]
    struct SummaryDef {
        prefix: &'static str,
        tool_name: String,
        display_name: String,
        cache_scope: Value,
        tool_args: Option<Value>,
        reason: Option<String>,
    }
    let def = match request {
        CopilotPermissionRequest::Shell {
            full_command_text,
            intention,
            possible_paths,
            possible_urls,
            has_write_file_redirection,
            warning,
            ..
        } => SummaryDef {
            prefix: "copilot:shell",
            tool_name: "copilot_shell".to_string(),
            display_name: "GitHub Copilot shell command".to_string(),
            cache_scope: json!({
                "command": full_command_text,
                "paths": possible_paths,
                "urls": possible_urls,
                "write_redirection": has_write_file_redirection,
            }),
            tool_args: Some(json!({
                "command": full_command_text,
                "paths": possible_paths,
                "urls": possible_urls,
            })),
            reason: warning.clone().or_else(|| Some(intention.clone())),
        },
        CopilotPermissionRequest::Write { file_name, intention, .. } => SummaryDef {
            prefix: "copilot:write",
            tool_name: "copilot_write".to_string(),
            display_name: "GitHub Copilot file write".to_string(),
            cache_scope: json!({"file": file_name}),
            tool_args: Some(json!({"file": file_name, "intention": intention})),
            reason: Some(intention.clone()),
        },
        CopilotPermissionRequest::Read { path, intention, .. } => SummaryDef {
            prefix: "copilot:read",
            tool_name: "copilot_read".to_string(),
            display_name: "GitHub Copilot file read".to_string(),
            cache_scope: json!({"path": path}),
            tool_args: Some(json!({"path": path, "intention": intention})),
            reason: Some(intention.clone()),
        },
        CopilotPermissionRequest::Mcp {
            server_name,
            tool_name,
            tool_title,
            args,
            read_only,
            ..
        } => SummaryDef {
            prefix: "copilot:mcp",
            tool_name: format!("copilot_mcp_{tool_name}"),
            display_name: format!("GitHub Copilot MCP tool {tool_title}"),
            cache_scope: json!({"server": server_name, "tool": tool_name, "args": args, "read_only": read_only}),
            tool_args: args.clone(),
            reason: Some(format!("Server: {server_name}")),
        },
        CopilotPermissionRequest::Url { url, intention, .. } => SummaryDef {
            prefix: "copilot:url",
            tool_name: "copilot_url".to_string(),
            display_name: "GitHub Copilot URL access".to_string(),
            cache_scope: json!({"url": url}),
            tool_args: Some(json!({"url": url, "intention": intention})),
            reason: Some(intention.clone()),
        },
        CopilotPermissionRequest::Memory { subject, fact, .. } => SummaryDef {
            prefix: "copilot:memory",
            tool_name: "copilot_memory".to_string(),
            display_name: "GitHub Copilot memory update".to_string(),
            cache_scope: json!({"subject": subject, "fact": fact}),
            tool_args: Some(json!({"subject": subject, "fact": fact})),
            reason: Some("GitHub Copilot wants to store a memory fact.".to_string()),
        },
        CopilotPermissionRequest::CustomTool { tool_name, tool_description, args, .. } => SummaryDef {
            prefix: "copilot:custom-tool",
            tool_name: format!("copilot_custom_{tool_name}"),
            display_name: format!("GitHub Copilot custom tool {tool_name}"),
            cache_scope: json!({"tool": tool_name, "args": args}),
            tool_args: args.clone(),
            reason: Some(tool_description.clone()),
        },
        CopilotPermissionRequest::Hook { tool_name, tool_args, hook_message, .. } => SummaryDef {
            prefix: "copilot:hook",
            tool_name: format!("copilot_hook_{tool_name}"),
            display_name: format!("GitHub Copilot hook {tool_name}"),
            cache_scope: json!({"tool": tool_name, "args": tool_args}),
            tool_args: tool_args.clone(),
            reason: hook_message.clone(),
        },
        CopilotPermissionRequest::Unknown { .. } => return None,
    };
    Some(PermissionPromptSummary {
        cache_key: scoped_cache_key(def.prefix, def.cache_scope),
        tool_name: def.tool_name,
        display_name: def.display_name.clone(),
        learning_label: def.display_name,
        tool_args: def.tool_args,
        reason: def.reason,
    })
}

fn map_runtime_error(err: anyhow::Error) -> uni::LLMError {
    uni::LLMError::Provider {
        message: format!("GitHub Copilot runtime bridge failed: {err}"),
        metadata: None,
    }
}

fn raw_tool_call_id(tool_call_id: &str) -> Option<&str> {
    (!tool_call_id.trim().is_empty()).then_some(tool_call_id)
}

fn harness_call_item_id(prefix: &str, tool_call_id: &str, tool_name: &str) -> String {
    if tool_call_id.trim().is_empty() {
        format!("{prefix}-copilot-tool-{}", tool_name.replace(' ', "_"))
    } else {
        format!("{prefix}-copilot-tool-{tool_call_id}")
    }
}

#[cfg(test)]
#[path = "copilot_runtime_tests.rs"]
mod tests;
