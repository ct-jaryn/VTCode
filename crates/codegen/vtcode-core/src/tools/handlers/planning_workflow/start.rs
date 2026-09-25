//! Enter-planning tool: `start_planning`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::config::constants::tools;
use crate::tools::handlers::planning_workflow::persistence::{
    detect_validation_command_hints, initialize_plan_file, plan_file_baseline, plan_title_seed, resolve_plan_path,
};
use crate::tools::handlers::planning_workflow::state::PlanningWorkflowState;
use crate::tools::traits::Tool;
use crate::utils::file_utils::ensure_dir_exists;
use vtcode_commons::workspace_relative_display;

/// Arguments for entering planning workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StartPlanningArgs {
    /// Optional: Name for the plan file (defaults to timestamp-based name)
    #[serde(default)]
    plan_name: Option<String>,

    /// Optional: Explicit output path for the plan file (absolute or workspace-relative)
    #[serde(default)]
    plan_path: Option<String>,

    /// Optional: Initial description of what you're planning
    #[serde(default)]
    description: Option<String>,

    /// Internal: when true, request confirmation instead of entering immediately.
    #[serde(default)]
    require_confirmation: bool,

    /// Internal: confirmation has already been granted.
    #[serde(default)]
    approved: bool,
}

/// Tool for entering planning workflow
pub struct StartPlanningTool {
    state: PlanningWorkflowState,
}

impl StartPlanningTool {
    pub fn new(state: PlanningWorkflowState) -> Self {
        Self { state }
    }

    fn display_path(&self, path: &Path) -> String {
        self.state
            .workspace_root()
            .map(|workspace| workspace_relative_display(&workspace, path))
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    }

    fn generate_plan_name(&self, provided: Option<&str>) -> String {
        match provided {
            Some(name) => {
                // Sanitize the name for filesystem
                name.to_lowercase()
                    .chars()
                    .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' })
                    .collect()
            }
            None => {
                // Generate human-readable slug with timestamp prefix
                // Format: {timestamp_millis}-{adjective}-{noun} (e.g., "1768330644696-gentle-harbor")
                // This follows the OpenCode pattern for memorable plan file names
                vtcode_commons::slug::create_timestamped()
            }
        }
    }
}

fn title_from_plan_name(plan_name: &str) -> String {
    plan_name
        .split('-')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => {
                    format!("{}{}", first.to_ascii_uppercase(), chars.as_str().to_ascii_lowercase())
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_plan_file_target(
    workspace_root: &Path,
    requested_path: Option<&str>,
    existing_plan_file: Option<&Path>,
    default_plan_file: PathBuf,
    fallback_plan_name: &str,
) -> (PathBuf, String) {
    if let Some(raw_path) = requested_path {
        let resolved = resolve_plan_path(workspace_root, raw_path);
        let seed = plan_title_seed(&resolved, fallback_plan_name);
        return (resolved, seed);
    }

    if let Some(existing_plan_file) = existing_plan_file {
        let resolved = existing_plan_file.to_path_buf();
        let seed = plan_title_seed(&resolved, fallback_plan_name);
        return (resolved, seed);
    }

    (default_plan_file, fallback_plan_name.to_string())
}

#[async_trait]
impl Tool for StartPlanningTool {
    async fn execute(&self, args: Value) -> Result<Value> {
        let args: StartPlanningArgs = serde_json::from_value(args).unwrap_or(StartPlanningArgs {
            plan_name: None,
            description: None,
            plan_path: None,
            require_confirmation: false,
            approved: false,
        });

        let workspace_root = self.state.workspace_root().unwrap_or_else(|| PathBuf::from("."));
        let validation_workspace = workspace_root.clone();
        let validation_hints =
            tokio::task::spawn_blocking(move || detect_validation_command_hints(&validation_workspace))
                .await
                .context("Validation command discovery task panicked")?;

        // Check if already in planning workflow
        if self.state.is_active() {
            let fallback_plan_name = self.generate_plan_name(args.plan_name.as_deref());
            let existing_plan_file = self.state.get_plan_file().await;
            let existing_plan_file_exists = match existing_plan_file.as_ref() {
                Some(path) => tokio::fs::try_exists(path).await.unwrap_or(false),
                None => false,
            };

            if existing_plan_file_exists {
                return Ok(json!({
                    "status": "already_active",
                    "message": "Planning workflow is already active. Continue with your planning workflow.",
                    "plan_file": existing_plan_file.map(|path| self.display_path(&path))
                }));
            }

            let (plan_file, plan_title_seed) = resolve_plan_file_target(
                &workspace_root,
                args.plan_path.as_deref(),
                existing_plan_file.as_deref(),
                self.state.plans_dir().join(format!("{fallback_plan_name}.md")),
                &fallback_plan_name,
            );
            let plan_title = title_from_plan_name(&plan_title_seed);

            if let Some(parent) = plan_file.parent() {
                ensure_dir_exists(parent)
                    .await
                    .with_context(|| format!("Failed to create plan directory: {}", parent.display()))?;
            }

            let mut created_plan_file = false;
            if !tokio::fs::try_exists(&plan_file).await.unwrap_or(false) {
                created_plan_file = true;
                let plan_file_display = self.display_path(&plan_file);
                initialize_plan_file(
                    &plan_file,
                    &plan_file_display,
                    &plan_title,
                    args.description.as_deref(),
                    &validation_hints,
                )
                .await?;
            }

            self.state.set_plan_file(Some(plan_file.clone())).await;
            let baseline = plan_file_baseline(&plan_file).await;
            self.state.set_plan_baseline(Some(baseline)).await;

            let message = if created_plan_file {
                "Planning workflow is already active. Initialized plan file for planning workflow."
            } else {
                "Planning workflow is already active. Using existing plan file for planning workflow."
            };

            return Ok(json!({
                "status": "already_active",
                "message": message,
                "plan_file": self.display_path(&plan_file)
            }));
        }

        // Resolve target plan path. Defaults to .vtcode/plans/, but allows explicit custom location.
        let plan_name = self.generate_plan_name(args.plan_name.as_deref());
        let (plan_file, plan_title_seed) = resolve_plan_file_target(
            &workspace_root,
            args.plan_path.as_deref(),
            None,
            self.state.plans_dir().join(format!("{plan_name}.md")),
            &plan_name,
        );
        let plan_title = title_from_plan_name(&plan_title_seed);
        if args.require_confirmation && !args.approved {
            return Ok(json!({
                "status": "pending_confirmation",
                "requires_confirmation": true,
                "message": "Planning workflow entry requires user confirmation.",
                "plan_file": self.display_path(&plan_file),
                "plan_title": plan_title.clone(),
                "description": args.description,
            }));
        }

        // Enable planning workflow only after explicit approval.
        self.state.enable();

        if let Some(parent) = plan_file.parent() {
            ensure_dir_exists(parent)
                .await
                .with_context(|| format!("Failed to create plan directory: {}", parent.display()))?;
        }

        let plan_file_display = self.display_path(&plan_file);
        initialize_plan_file(
            &plan_file,
            &plan_file_display,
            &plan_title,
            args.description.as_deref(),
            &validation_hints,
        )
        .await?;

        // Track the current plan file
        self.state.set_plan_file(Some(plan_file.clone())).await;
        let baseline = plan_file_baseline(&plan_file).await;
        self.state.set_plan_baseline(Some(baseline)).await;

        Ok(json!({
            "status": "success",
            "message": "Entered Planning workflow. Mutating actions are disabled for exploration and planning.",
            "plan_file": self.display_path(&plan_file),
            "instructions": [
                "1. Explore files and capture repository facts before drafting the plan",
                "2. Ask or close only material blocking decisions",
                "3. Emit one compact <proposed_plan> spec (fit ~1500 tokens; steps as `Action -> files: [path] -> verify: [command]`, file:symbol refs over prose) and persist it to the plan file"
            ],
            "workflow_phases": [
                "Phase A: Explore facts",
                "Phase B: Close open decisions",
                "Phase C: Draft one proposed plan"
            ]
        }))
    }

    fn name(&self) -> &str {
        tools::START_PLANNING
    }

    fn description(&self) -> &str {
        "Request entry into the read-only Planning workflow for demanding, ambiguous, or multi-phase tasks. Entry requires user confirmation unless the session runs in full-auto or skip-confirmations mode. While the workflow is active, exec_command accepts only read-only commands, mutating tools such as apply_patch are blocked, and the plan is kept in a file under .vtcode/plans/ (or plan_path). It is not needed for straightforward changes or when the implementation approach is already clear."
    }

    fn parameter_schema(&self) -> Option<Value> {
        Some(json!({
            "type": "object",
            "properties": {
                "plan_name": {
                    "type": "string",
                    "description": "Optional name for the plan file (e.g., 'add-auth-middleware'). Defaults to timestamp-based name."
                },
                "plan_path": {
                    "type": "string",
                    "description": "Optional explicit plan file path. Use this to persist plans in a custom workspace path instead of the default .vtcode/plans location."
                },
                "description": {
                    "type": "string",
                    "description": "Optional initial description of what you're planning to implement."
                }
            },
            "required": []
        }))
    }

    fn is_mutating(&self) -> bool {
        false // This changes the planning permission state, not files.
    }

    fn is_parallel_safe(&self) -> bool {
        false // Planning permission changes should be sequential.
    }
}
