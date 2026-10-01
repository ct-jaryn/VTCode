use anyhow::Result;
use vtcode_core::utils::ansi::MessageStyle;

use crate::agent::runloop::unified::auto_permission::{
    PROBE_TIMEOUT, ProbeWarning, probe_tool_output, recent_user_context,
};
use crate::agent::runloop::unified::turn::context::TurnProcessingContext;

async fn auto_permission_probe_warning(
    ctx: &mut TurnProcessingContext<'_>,
    tool_name: &str,
    content_for_model: &str,
) -> Option<ProbeWarning> {
    if !ctx.full_auto || ctx.is_planning_active() {
        return None;
    }
    let Some(permissions) = ctx.vt_cfg.map(|cfg| &cfg.permissions) else {
        return None;
    };
    // Skip the pre-filter-free spend: empty outputs carry nothing to probe,
    // and they must not consume the per-turn probe budget.
    if content_for_model.trim().is_empty() {
        return None;
    }
    if !ctx.harness_state.can_spend_auto_permission_probe_model_call() {
        tracing::debug!(tool = %tool_name, "auto permission review prompt probe budget exhausted for this turn");
        return None;
    }
    // The probe reads only the last two user messages: extract them up front
    // instead of cloning the whole conversation for every tool result.
    let user_context = recent_user_context(ctx.working_history);
    ctx.harness_state.record_auto_permission_probe_model_call();
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        probe_tool_output(
            ctx.provider_client.as_mut(),
            ctx.config,
            ctx.vt_cfg,
            permissions,
            &user_context,
            content_for_model,
        ),
    )
    .await
    {
        Ok(Ok(warning)) => warning,
        Ok(Err(err)) => {
            tracing::warn!(tool = %tool_name, error = %err, "auto permission review prompt probe failed");
            None
        }
        Err(_) => {
            tracing::warn!(tool = %tool_name, "auto permission review prompt probe timed out");
            None
        }
    }
}

fn append_probe_warning(
    ctx: &mut TurnProcessingContext<'_>,
    tool_name: &str,
    probe_warning: ProbeWarning,
) -> Result<()> {
    tracing::trace!(tool = %tool_name, probe_hit = true, "auto permission review prompt probe flagged tool output");
    let queued = ctx.harness_state.queue_auto_permission_probe_warning(probe_warning.warning);
    tracing::trace!(tool = %tool_name, queued, "queued auto permission review prompt probe warning");
    ctx.renderer.line(
        MessageStyle::Warning,
        "Auto permission review flagged the latest tool output as suspicious prompt injection.",
    )?;
    Ok(())
}

pub(super) fn flush_auto_permission_probe_warning(ctx: &mut TurnProcessingContext<'_>) {
    if let Some(warning) = ctx.harness_state.take_auto_permission_probe_warning() {
        ctx.push_system_message(warning);
    }
}

pub(super) async fn push_tool_response_with_auto_permission_probe(
    t_ctx: &mut super::super::handlers::ToolOutcomeContext<'_, '_>,
    tool_call_id: String,
    tool_name: &str,
    content_for_model: String,
) -> Result<()> {
    let probe_warning = auto_permission_probe_warning(t_ctx.ctx, tool_name, &content_for_model).await;
    t_ctx.ctx.push_tool_response(tool_call_id, Some(tool_name), content_for_model);
    if let Some(probe_warning) = probe_warning {
        append_probe_warning(t_ctx.ctx, tool_name, probe_warning)?;
    }
    Ok(())
}
