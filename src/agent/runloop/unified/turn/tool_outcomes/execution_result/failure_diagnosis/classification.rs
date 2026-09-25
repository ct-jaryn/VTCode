//! Deterministic failure classification and safe fallback guidance.

use serde_json::Value;
use vtcode_commons::ErrorCategory;
use vtcode_core::tools::names::is_apply_patch_shell_collision_command;
use vtcode_core::tools::registry::ToolExecutionError;

use super::ToolFailureDiagnosis;
use super::evidence::bounded_field;
use crate::agent::runloop::unified::turn::tool_outcomes::is_grep_style_no_match;

pub(crate) fn deterministic_preflight_diagnosis(
    tool_name: &str,
    error: &str,
    circuit_tripped: bool,
) -> ToolFailureDiagnosis {
    let tool_name = bounded_field(tool_name);
    let error = bounded_field(error);
    let next_action = if circuit_tripped {
        "Stop retrying this malformed call; tools are disabled for the next pass."
    } else {
        "Correct the arguments using the declared schema, then retry once."
    };
    ToolFailureDiagnosis::new(
        format!("Tool preflight validation failed for '{tool_name}': {error}"),
        "The call was rejected before execution because its arguments or safety preflight were invalid.",
        next_action,
    )
}

pub(crate) fn deterministic_output_diagnosis(tool_name: &str, args: &Value, output: &Value) -> ToolFailureDiagnosis {
    let grep_no_match = is_grep_style_no_match(tool_name, args, output);
    let tool_name = bounded_field(tool_name);
    let exit_code = output.get("exit_code").and_then(Value::as_i64);
    let stderr = first_text_field(
        output,
        &[
            "stderr",
            "stderr_preview",
            "error",
            "message",
            "critical_note",
            "warning",
        ],
    )
    .map(bounded_field);
    let output_text = first_text_field(output, &["output", "stdout", "preview", "content"]).map(bounded_field);
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| output.get("command").and_then(Value::as_str))
        .map(bounded_field);

    let command_not_found = diagnostic_fields(output)
        .map(bounded_field)
        .map(|field| field.to_ascii_lowercase())
        .any(|field| field.contains("command not found"));

    let apply_patch_collision = command.as_deref().is_some_and(is_apply_patch_shell_collision_command)
        && (exit_code == Some(127) || command_not_found);

    let mut observed = match exit_code {
        Some(code) => format!("'{tool_name}' returned exit code {code}."),
        None => format!("'{tool_name}' returned a failure-like result."),
    };
    if let Some(command) = command.as_deref() {
        observed.push_str(" Command: ");
        observed.push_str(command);
        observed.push('.');
    }
    if let Some(stderr) = stderr.as_deref() {
        observed.push_str(" Stderr: ");
        observed.push_str(stderr);
    } else if let Some(output_text) = output_text.as_deref() {
        observed.push_str(" Output: ");
        observed.push_str(output_text);
    }

    let likely_cause = if grep_no_match {
        "The search command completed with no matching results; grep-style tools use exit code 1 for no matches."
    } else if apply_patch_collision {
        "`apply_patch` is not a shell binary; it is a core tool. The command was run via `exec_command` instead of calling the `apply_patch` tool."
    } else if exit_code == Some(127) || command_not_found {
        "The command or executable was not found in the runtime PATH."
    } else if exit_code.is_some() {
        "The command reported a non-zero exit status; the bounded output does not establish a more specific cause."
    } else {
        "The tool returned a failure-like result; the available evidence does not establish a more specific cause."
    };
    let next_action = if grep_no_match {
        "Treat this as an empty search result and refine the query only if a match is still needed."
    } else if apply_patch_collision {
        "Call the `apply_patch` tool with `input` containing the patch (run `search_tools` first if it is deferred); do not run `apply_patch` via `exec_command`."
    } else if exit_code == Some(127) || command_not_found {
        "Check the command name and PATH, then retry."
    } else if exit_code.is_some() {
        "Inspect the reported error and retry with corrected arguments or a narrower scope."
    } else {
        "Review the returned error and retry with corrected arguments."
    };

    ToolFailureDiagnosis::new(observed, likely_cause, next_action)
}

pub(crate) fn deterministic_error_diagnosis(error: &ToolExecutionError, failure_kind: &str) -> ToolFailureDiagnosis {
    let tool_name = bounded_field(&error.tool_name);
    let failure_kind = bounded_field(failure_kind);
    let error_message = bounded_field(&error.message);
    let observed = format!(
        "The '{}' tool reported a {failure_kind} failure ({}): {}",
        tool_name,
        error.category.user_label(),
        error_message
    );
    let likely_cause = match error.category {
        ErrorCategory::Authentication => "The provider rejected the configured credentials or authentication state.",
        ErrorCategory::PermissionDenied | ErrorCategory::PolicyViolation | ErrorCategory::PlanningPolicyViolation => {
            "The requested tool action is blocked by the active permission or policy configuration."
        }
        ErrorCategory::CircuitOpen => "The tool circuit breaker is open after repeated failures.",
        ErrorCategory::InvalidParameters => "The supplied tool arguments do not match the required schema or values.",
        ErrorCategory::ResourceNotFound => "The requested file or resource was not found at the supplied location.",
        ErrorCategory::Timeout => "The operation exceeded its configured time limit.",
        ErrorCategory::Network | ErrorCategory::ServiceUnavailable | ErrorCategory::RateLimit => {
            "The external service or network was temporarily unavailable."
        }
        ErrorCategory::SandboxFailure => "The operation failed inside the execution sandbox.",
        ErrorCategory::ResourceExhausted => "A required runtime resource or quota was exhausted.",
        ErrorCategory::ToolNotFound => "The requested tool is not available in the current runtime.",
        ErrorCategory::Cancelled => "The operation was cancelled before it completed.",
        ErrorCategory::ExecutionError if is_safeguard_failure(error) => {
            "The requested tool action was blocked by an active policy, permission, authentication, or sandbox safeguard."
        }
        ErrorCategory::ExecutionError => {
            "The tool reported an execution error without enough evidence for a narrower cause."
        }
    };
    let next_action = match error.category {
        ErrorCategory::Authentication
        | ErrorCategory::PermissionDenied
        | ErrorCategory::PolicyViolation
        | ErrorCategory::PlanningPolicyViolation
        | ErrorCategory::CircuitOpen
        | ErrorCategory::SandboxFailure
        | ErrorCategory::ResourceExhausted => {
            "Review the active policy and permission configuration; do not bypass safeguards."
        }
        ErrorCategory::InvalidParameters | ErrorCategory::ToolNotFound => {
            "Correct the tool name or arguments using the declared schema, then retry once."
        }
        _ if is_preflight_failure(error) => {
            "Correct the arguments using the declared schema, then retry once; do not bypass safety checks."
        }
        _ if is_safeguard_failure(error) => {
            "Review the active policy and permission configuration; do not bypass safeguards."
        }
        ErrorCategory::ResourceNotFound => "Verify the requested path or resource, then retry with a valid target.",
        ErrorCategory::Timeout => "Inspect the bounded evidence and retry with a narrower scope or shorter operation.",
        ErrorCategory::Network | ErrorCategory::ServiceUnavailable | ErrorCategory::RateLimit => {
            "Inspect the bounded error and retry after the external service is available."
        }
        ErrorCategory::Cancelled => "Wait for a new turn and retry only if the requested operation is still needed.",
        ErrorCategory::ExecutionError => "Inspect the bounded error evidence and retry with corrected arguments.",
    };
    ToolFailureDiagnosis::new(observed, likely_cause, next_action)
}

pub(super) fn is_policy_sensitive(category: ErrorCategory) -> bool {
    matches!(
        category,
        ErrorCategory::Authentication
            | ErrorCategory::PermissionDenied
            | ErrorCategory::PolicyViolation
            | ErrorCategory::PlanningPolicyViolation
            | ErrorCategory::CircuitOpen
            | ErrorCategory::SandboxFailure
            | ErrorCategory::ResourceExhausted
    )
}

pub(super) fn is_deterministic_only_error(error: &ToolExecutionError) -> bool {
    is_safeguard_failure(error)
        || matches!(error.category, ErrorCategory::InvalidParameters | ErrorCategory::ToolNotFound)
}

fn is_safeguard_failure(error: &ToolExecutionError) -> bool {
    if is_policy_sensitive(error.category) || is_preflight_failure(error) {
        return true;
    }

    let message = bounded_field(&error.message).to_ascii_lowercase();
    [
        "policy",
        "permission denied",
        "operation not permitted",
        "authentication",
        "unauthorized",
        "forbidden",
        "circuit breaker",
        "sandbox",
        "safety validation",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

pub(super) fn is_preflight_failure(error: &ToolExecutionError) -> bool {
    let message = bounded_field(&error.message).to_ascii_lowercase();
    ["preflight", "pre-flight", "command security check", "safety validation"]
        .iter()
        .any(|marker| message.contains(marker))
}

fn first_text_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|text| !text.is_empty())
}

fn diagnostic_fields(value: &Value) -> impl Iterator<Item = &str> {
    [
        "stdout",
        "output",
        "preview",
        "content",
        "stderr",
        "stderr_preview",
        "error",
        "message",
        "critical_note",
        "warning",
        "hint",
    ]
    .into_iter()
    .filter_map(|key| value.get(key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|text| !text.is_empty())
}
