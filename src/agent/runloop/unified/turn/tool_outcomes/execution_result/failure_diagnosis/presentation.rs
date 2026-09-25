//! Tool-response attachment and user-facing diagnosis emission.

use serde_json::json;
use vtcode_core::utils::ansi::MessageStyle;

use super::ToolFailureDiagnosis;
use super::evidence::bounded_field;
use crate::agent::runloop::unified::turn::context::TurnProcessingContext;

pub(super) fn attach_to_serialized_tool_response(serialized: String, diagnosis: &ToolFailureDiagnosis) -> String {
    let mut payload =
        serde_json::from_str::<serde_json::Value>(&serialized).unwrap_or_else(|_| json!({ "output": serialized }));
    if let Some(object) = payload.as_object_mut() {
        object.insert("diagnosis".to_string(), diagnosis.to_value());
    } else {
        payload = json!({ "output": payload, "diagnosis": diagnosis.to_value() });
    }
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
}

pub(crate) async fn push_tool_response_with_diagnosis(
    t_ctx: &mut super::super::super::handlers::ToolOutcomeContext<'_, '_>,
    tool_call_id: String,
    tool_name: &str,
    content_for_model: String,
    diagnosis: &ToolFailureDiagnosis,
) -> anyhow::Result<()> {
    let diagnosed_content = attach_to_serialized_tool_response(content_for_model, diagnosis);
    super::super::auto_permission_probe::push_tool_response_with_auto_permission_probe(
        t_ctx,
        tool_call_id,
        tool_name,
        diagnosed_content,
    )
    .await
}

fn concise_diagnosis_line(tool_name: &str, diagnosis: &ToolFailureDiagnosis) -> String {
    let display_tool_name = bounded_field(tool_name);
    let observed_headline = diagnosis
        .observed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("failed");
    let observed_headline: String = if observed_headline.chars().count() > 160 {
        format!("{}…", observed_headline.chars().take(159).collect::<String>())
    } else {
        observed_headline.to_string()
    };
    format!("Tool '{display_tool_name}' failed: {observed_headline}")
}

pub(crate) fn render_diagnosis(
    renderer: &mut vtcode_core::utils::ansi::AnsiRenderer,
    harness_emitter: Option<&crate::agent::runloop::unified::inline_events::harness::HarnessEventEmitter>,
    turn_id: &str,
    tool_name: &str,
    diagnosis: &ToolFailureDiagnosis,
) {
    // TUI stays to one concise line; the full Observed/Likely-cause/Next-action
    // triple lives in the tool payload `diagnosis` JSON (model) + the harness
    // `diagnosis` reasoning item (agent forensics).
    let display_tool_name = bounded_field(tool_name);
    let line = concise_diagnosis_line(tool_name, diagnosis);
    if let Err(error) = renderer.line(MessageStyle::Info, &line) {
        tracing::warn!(tool = %display_tool_name, error = %error, "failed to render tool failure diagnosis");
    }

    if let Some(emitter) = harness_emitter
        && let Err(error) = emitter.emit_diagnosis(turn_id, &diagnosis.render_text(tool_name))
    {
        tracing::warn!(tool = %display_tool_name, error = %error, "failed to emit tool failure diagnosis event");
    }
}

pub(crate) fn render_and_emit(ctx: &mut TurnProcessingContext<'_>, tool_name: &str, diagnosis: &ToolFailureDiagnosis) {
    render_diagnosis(ctx.renderer, ctx.harness_emitter, &ctx.harness_state.turn_id.0, tool_name, diagnosis);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concise_line_uses_headline_and_stays_single_line() {
        // `bounded_field` flattens newlines to spaces, so multi-line input
        // arrives single-line; TUI must still stay single-line without leaking
        // the verbose Likely-cause/Next-action fields or old header.
        let multi = ToolFailureDiagnosis::new("first line\nsecond line with detail\nthird line", "cause", "next");
        let line = concise_diagnosis_line("request_user_input", &multi);
        assert!(line.starts_with("Tool 'request_user_input' failed: first line"));
        assert!(!line.contains("Likely cause"), "no verbose fields leak: {line}");
        assert!(!line.contains("Next action"), "no verbose fields leak: {line}");
        assert!(!line.contains('\n'), "TUI stays single-line: {line}");
        assert!(!line.contains("Diagnosis:"), "old verbose header gone: {line}");
    }

    #[test]
    fn concise_line_bounds_long_observed_asymmetrically() {
        let short = ToolFailureDiagnosis::new("exit 1", "cause", "next");
        let short_line = concise_diagnosis_line("exec_command", &short);
        assert!(short_line.contains("exit 1"));
        assert!(short_line.chars().count() < 120, "short stays short: {short_line}");

        let long_observed = "x".repeat(500);
        let long = ToolFailureDiagnosis::new(long_observed, "cause", "next");
        let long_line = concise_diagnosis_line("exec_command", &long);
        assert!(long_line.chars().count() <= "Tool 'exec_command' failed: ".len() + 160);
        assert!(!long_line.contains("Likely cause"), "no verbose fields leak: {long_line}");
    }
}
