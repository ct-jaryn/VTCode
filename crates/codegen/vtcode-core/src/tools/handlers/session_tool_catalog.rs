use crate::config::ModelId;
use crate::config::ToolDocumentationMode;
use crate::config::constants::tools;
use crate::config::loader::VTCodeConfig;
use crate::config::models::Provider;
use crate::config::types::CapabilityLevel;
use crate::llm::provider::{ToolDefinition, ToolNamespace, ToolSearchAlgorithm};
use crate::llm::providers::gemini::wire::FunctionDeclaration;
use crate::tool_policy::ToolPolicy;
#[cfg(test)]
use crate::tools::handlers::compact::compact_tool_description;
use crate::tools::handlers::compact::{MCP_TOOL_DESCRIPTION_MAX_LEN, compact_parameters};
use crate::tools::mcp::MCP_QUALIFIED_TOOL_PREFIX;
use crate::tools::registry::{ToolHandler as RegistryToolHandler, ToolRegistration};
use crate::tools::tool_intent::ToolSurfaceKind;
use crate::utils::tool_name_parsing::parse_canonical_mcp_tool_name;
use rustc_hash::FxHashSet;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};
use vtcode_utility_tool_specs::{parse_tool_input_schema, with_max_output_tokens_parameter};

use super::session_tool_projection::{ToolEntryProjection, ToolProjectionCache};
use super::tool_handler::{ConfiguredToolSpec, ResponsesApiTool, ToolSpec};

pub use crate::config::ToolProfile;
pub use crate::tools::registry::ToolCatalogSource;

/// The surface (execution context) where tools are exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSurface {
    /// Interactive TUI session.
    Interactive,
    /// Non-interactive agent runner.
    AgentRunner,
    /// Agent Client Protocol (ACP) session.
    Acp,
}

/// Model-specific capabilities that affect tool catalog generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolModelCapabilities {
    /// Whether the model supports the native `apply_patch` tool.
    pub supports_apply_patch_tool: bool,
}

impl ToolModelCapabilities {
    /// Returns capabilities inferred from the model name.
    #[must_use]
    pub fn for_model_name(model_name: &str) -> Self {
        model_name
            .parse::<ModelId>()
            .ok()
            .map(|model_id| Self {
                supports_apply_patch_tool: model_id.supports_apply_patch_tool(),
            })
            .unwrap_or_default()
    }
}

/// The kind of deferred tool search supported by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredToolSearchKind {
    /// Anthropic's tool search with a specific algorithm.
    Anthropic(ToolSearchAlgorithm),
    /// OpenAI's hosted tool search.
    OpenAIHosted,
    /// Client-local MCP tool search for providers with no hosted tool search.
    /// `mcp_search_tools` remains available while matched MCP definitions are
    /// expanded into the next request. Deferred definitions are omitted from
    /// the current wire payload rather than sent with `defer_loading: true`.
    ClientLocal,
}

/// Above this many deferable (non-core, non-`always_available`) tools, a
/// catalog is exposed via deferred loading rather than sent eagerly. Below it,
/// eager exposure is cheaper and simpler. Ignored when the catalog contains any
/// MCP tool (see `model_tools`), since MCP schemas are the dominant token cost.
const DIRECT_TOOL_EXPOSURE_THRESHOLD: usize = 15;
/// Token budget (~4 chars/token) for the combined schema of a deferable
/// catalog. A catalog is deferred when its estimated schema size exceeds this,
/// even if the tool count is below `DIRECT_TOOL_EXPOSURE_THRESHOLD`. This catches
/// a single large server whose schema dwarfs the entire builtin set.
const DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET: usize = 4_000;

/// Whether a catalog is large enough that deferred loading would engage for it
/// (MCP tools present, at/over the count threshold, or over the schema-token
/// budget). Mirrors the "would defer" branch of the `model_tools` decision -- it
/// is the negation of `(count < THRESHOLD && !has_mcp && tokens <= BUDGET)`.
///
/// Used to decide whether to warn when deferred loading is *disabled*
/// (`tools.client_tool_search = false` on a provider without hosted tool
/// search): in that case the user pays the full schema tax a catalog this size
/// would otherwise shed. Extracted as pure logic so the warning condition is
/// unit-testable without capturing tracing output.
#[must_use]
fn catalog_would_benefit_from_deferral(
    has_mcp_tools: bool,
    deferable_tool_count: usize,
    estimated_schema_tokens: usize,
) -> bool {
    has_mcp_tools
        || deferable_tool_count >= DIRECT_TOOL_EXPOSURE_THRESHOLD
        || estimated_schema_tokens > DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET
}

/// Policy for deferred tool loading (tool search).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeferredToolPolicy {
    search_kind: Option<DeferredToolSearchKind>,
    always_available_tools: BTreeSet<String>,
}

impl DeferredToolPolicy {
    /// Creates a policy for Anthropic's tool search.
    #[must_use]
    pub fn anthropic(algorithm: ToolSearchAlgorithm, always_available_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            search_kind: Some(DeferredToolSearchKind::Anthropic(algorithm)),
            always_available_tools: always_available_tools.into_iter().collect(),
        }
    }

    /// Creates a policy for OpenAI's hosted tool search.
    #[must_use]
    pub fn openai_hosted(always_available_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            search_kind: Some(DeferredToolSearchKind::OpenAIHosted),
            always_available_tools: always_available_tools.into_iter().collect(),
        }
    }

    /// Creates a policy for client-local tool search. Used for providers
    /// with no hosted tool search when `client_tool_search` is enabled.
    #[must_use]
    pub fn client_local(always_available_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            search_kind: Some(DeferredToolSearchKind::ClientLocal),
            always_available_tools: always_available_tools.into_iter().collect(),
        }
    }

    fn is_enabled(&self) -> bool {
        self.search_kind.is_some()
    }

    /// Returns whether this policy defers via client-local tool search
    /// rather than a provider-hosted mechanism. Callers that assemble the
    /// wire-level request tool list use this to decide whether deferred
    /// tool definitions may be omitted from the payload -- hosted policies
    /// (Anthropic/OpenAI) require the full deferred definitions to remain
    /// on the wire, so this must stay `false` for those.
    #[must_use]
    pub fn is_client_local(&self) -> bool {
        matches!(self.search_kind, Some(DeferredToolSearchKind::ClientLocal))
    }

    fn keeps_entry_available(&self, entry: &ToolCatalogEntry) -> bool {
        self.always_available_tools.contains(entry.public_name.as_str())
            || self.always_available_tools.contains(entry.registration_name.as_str())
            || entry
                .aliases
                .iter()
                .any(|alias| self.always_available_tools.contains(alias.as_str()))
    }

    fn tool_search_definition(&self) -> Option<ToolDefinition> {
        match self.search_kind {
            Some(DeferredToolSearchKind::Anthropic(algorithm)) => Some(ToolDefinition::tool_search(algorithm)),
            Some(DeferredToolSearchKind::OpenAIHosted) => Some(ToolDefinition::hosted_tool_search()),
            // Client-local discovery is a core capability; there is
            // no separate wire-level tool to inject for client-local
            // deferral.
            Some(DeferredToolSearchKind::ClientLocal) | None => None,
        }
    }
}

/// Returns the deferred tool policy for the given provider and configuration.
#[must_use]
pub fn deferred_tool_policy_for_runtime(
    provider: Option<Provider>,
    model_supports_responses_compaction: bool,
    vtcode_config: Option<&VTCodeConfig>,
) -> DeferredToolPolicy {
    match provider {
        Some(Provider::Anthropic) => {
            let enabled = vtcode_config.is_none_or(|cfg| cfg.provider.anthropic.tool_search.enabled);
            let defer_by_default = vtcode_config.is_none_or(|cfg| cfg.provider.anthropic.tool_search.defer_by_default);
            if !enabled || !defer_by_default {
                return DeferredToolPolicy::default();
            }

            let algorithm = vtcode_config
                .map(|cfg| cfg.provider.anthropic.tool_search.algorithm)
                .unwrap_or_default();
            let always_available_tools = vtcode_config
                .map(|cfg| cfg.provider.anthropic.tool_search.always_available_tools.clone())
                .unwrap_or_default();
            DeferredToolPolicy::anthropic(algorithm, always_available_tools)
        }
        Some(Provider::OpenAI) if model_supports_responses_compaction => {
            let enabled = vtcode_config.is_none_or(|cfg| cfg.provider.openai.tool_search.enabled);
            let defer_by_default = vtcode_config.is_none_or(|cfg| cfg.provider.openai.tool_search.defer_by_default);
            if !enabled || !defer_by_default {
                return DeferredToolPolicy::default();
            }

            let always_available_tools = vtcode_config
                .map(|cfg| cfg.provider.openai.tool_search.always_available_tools.clone())
                .unwrap_or_default();
            DeferredToolPolicy::openai_hosted(always_available_tools)
        }
        _ => {
            // No provider-hosted tool search is available (e.g. Gemini).
            // Client-local deferral is now the default so MCP schemas are not
            // sent eagerly. Users can opt back to the eager catalog by setting
            // `tools.client_tool_search = false`. The `DIRECT_TOOL_EXPOSURE_THRESHOLD`
            // and `DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET` gating that decides whether
            // deferral is actually worthwhile for a given catalog lives downstream
            // in `SessionToolCatalog::model_tools`, exactly as it does for the
            // hosted arms above -- this function only decides whether deferral is
            // *possible* for the runtime, not whether it is *used* for the
            // current catalog.
            let client_tool_search_enabled = vtcode_config.is_some_and(|cfg| cfg.tools.client_tool_search);
            if client_tool_search_enabled {
                DeferredToolPolicy::client_local(Vec::new())
            } else {
                DeferredToolPolicy::default()
            }
        }
    }
}

/// Returns whether Anthropic native memory is enabled for the given runtime.
#[must_use]
pub fn anthropic_native_memory_enabled_for_runtime(
    provider: Option<Provider>,
    model: &str,
    vtcode_config: Option<&VTCodeConfig>,
) -> bool {
    matches!(provider, Some(Provider::Anthropic))
        && !matches!(
            crate::llm::factory::infer_provider(None, model),
            Some(resolved) if resolved != Provider::Anthropic
        )
        && vtcode_config.is_some_and(|cfg| cfg.provider.anthropic.memory.enabled)
}

/// Configuration for the session's tool catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToolsConfig {
    /// The execution surface (interactive, agent runner, ACP).
    pub surface: SessionSurface,
    /// Minimum capability level required for tools to be visible.
    pub capability_level: CapabilityLevel,
    /// Documentation detail mode for tool descriptions.
    pub documentation_mode: ToolDocumentationMode,
    /// Whether the planning workflow is active.
    pub planning_active: bool,
    /// Whether the request_user_input tool is enabled.
    pub request_user_input_enabled: bool,
    /// Model-specific capabilities.
    pub model_capabilities: ToolModelCapabilities,
    /// Policy for deferred tool loading.
    pub deferred_tool_policy: DeferredToolPolicy,
    /// Whether Anthropic native memory is enabled.
    pub anthropic_native_memory_enabled: bool,
    /// Model-facing tool profile.
    pub tool_profile: ToolProfile,
}

impl SessionToolsConfig {
    /// Creates a public configuration for a session outside the planning workflow.
    pub fn full_public(
        surface: SessionSurface,
        capability_level: CapabilityLevel,
        documentation_mode: ToolDocumentationMode,
        model_capabilities: ToolModelCapabilities,
    ) -> Self {
        Self {
            surface,
            capability_level,
            documentation_mode,
            planning_active: false,
            request_user_input_enabled: true,
            model_capabilities,
            deferred_tool_policy: DeferredToolPolicy::default(),
            anthropic_native_memory_enabled: false,
            tool_profile: ToolProfile::VtCode,
        }
    }

    /// Marks whether the planning workflow is active.
    #[must_use]
    pub fn with_planning_active(mut self, planning_active: bool) -> Self {
        self.planning_active = planning_active;
        self
    }

    /// Sets the deferred tool policy.
    #[must_use]
    pub fn with_deferred_tool_policy(mut self, deferred_tool_policy: DeferredToolPolicy) -> Self {
        self.deferred_tool_policy = deferred_tool_policy;
        self
    }

    /// Enables or disables Anthropic native memory.
    #[must_use]
    pub fn with_anthropic_native_memory_enabled(mut self, enabled: bool) -> Self {
        self.anthropic_native_memory_enabled = enabled;
        self
    }

    /// Selects the model-facing tool profile.
    #[must_use]
    pub fn with_tool_profile(mut self, tool_profile: ToolProfile) -> Self {
        self.tool_profile = tool_profile;
        self
    }
}

/// The kind of tool in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogToolKind {
    /// Standard function call tool.
    Function,
    /// Native apply_patch tool.
    ApplyPatch,
}

/// An entry in the session tool catalog.
#[derive(Debug, Clone)]
pub struct ToolCatalogEntry {
    /// Name exposed to the LLM.
    pub public_name: String,
    /// Internal registration name.
    pub registration_name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for tool parameters.
    pub parameters: Value,
    /// Alternative names for the tool.
    pub aliases: Vec<String>,
    /// Minimum capability level required to use this tool.
    pub capability: CapabilityLevel,
    /// Default permission policy for this tool.
    pub default_permission: ToolPolicy,
    /// Whether this tool supports parallel execution.
    pub supports_parallel_tool_calls: bool,
    /// Source of this tool in the catalog.
    pub source: ToolCatalogSource,
    /// The kind of tool (function or apply_patch).
    pub kind: CatalogToolKind,
    /// The configured tool specification.
    pub configured_spec: ConfiguredToolSpec,
    /// Optional per-tool description length cap. When set, overrides the
    /// documentation mode's default max length. Used for MCP tools whose
    /// descriptions can be arbitrarily long.
    pub max_description_length: Option<usize>,
    /// Namespace grouping derived from the registration (currently only MCP
    /// tools, keyed by server name). `None` for core/builtin tools.
    pub namespace: Option<ToolNamespace>,
}

/// A simplified tool schema entry for serialization.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolSchemaEntry {
    /// Tool name.
    pub name: String,
    /// Tool description.
    pub description: String,
    /// JSON Schema for tool parameters.
    pub parameters: Value,
}

/// The session's tool catalog containing all available tools.
#[derive(Debug, Clone)]
pub struct SessionToolCatalog {
    entries: Vec<ToolCatalogEntry>,
    projection_cache: Arc<ToolProjectionCache>,
}

impl Default for SessionToolCatalog {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl SessionToolCatalog {
    /// Creates a new catalog from the given entries.
    pub fn new(entries: Vec<ToolCatalogEntry>) -> Self {
        let projection_cache = Arc::new(ToolProjectionCache::new(entries.len()));
        Self { entries, projection_cache }
    }

    /// Rebuilds the catalog from tool registrations.
    pub fn rebuild_from_registrations(registrations: Vec<ToolRegistration>) -> Self {
        let mut entries = Vec::new();
        for registration in registrations {
            if let Some(entry) = ToolCatalogEntry::from_registration(&registration) {
                entries.push(entry);
            }
        }

        let mut seen_public_names = FxHashSet::default();
        entries.retain(|entry| seen_public_names.insert(entry.public_name.clone()));
        Self::new(entries)
    }

    /// Returns the names of all public tools visible with the given config.
    pub fn public_tool_names(&self, config: SessionToolsConfig) -> Vec<String> {
        self.visible_entry_indices(&config)
            .into_iter()
            .map(|index| self.entries[index].public_name.clone())
            .collect()
    }

    /// Returns schema entries for all visible tools.
    pub fn schema_entries(&self, config: SessionToolsConfig) -> Vec<ToolSchemaEntry> {
        self.visible_entry_indices(&config)
            .into_iter()
            .map(|index| {
                let entry = &self.entries[index];
                let projection = self.projection(index, entry, config.documentation_mode);
                ToolSchemaEntry {
                    name: entry.public_name.clone(),
                    description: self.description_for_entry(entry, projection, &config).to_owned(),
                    parameters: self.parameters_for_entry(entry, projection, &config),
                }
            })
            .collect()
    }

    /// Returns Gemini function declarations for all visible tools.
    pub fn function_declarations(&self, config: SessionToolsConfig) -> Vec<FunctionDeclaration> {
        self.schema_entries(config)
            .into_iter()
            .map(|entry| FunctionDeclaration {
                name: entry.name,
                description: entry.description,
                parameters: entry.parameters,
            })
            .collect()
    }

    /// Returns tool definitions for the LLM, including deferred loading support.
    pub fn model_tools(&self, config: SessionToolsConfig) -> Vec<ToolDefinition> {
        let visible_entry_indices = self.visible_entry_indices(&config);
        let deferable_tool_count = visible_entry_indices
            .iter()
            .filter(|&&index| should_defer_tool_loading(&self.entries[index], &config))
            .count();
        let has_mcp_tools = visible_entry_indices
            .iter()
            .any(|&index| matches!(self.entries[index].source, ToolCatalogSource::Mcp));
        // Hosted policies (Anthropic/OpenAI) rely on `defer_loading` markers in
        // the wire payload, so they defer whenever the policy is enabled. The
        // client-local policy instead OMITS deferred definitions from the wire;
        // applying it to a small builtin-only catalog (e.g. the plan-mode
        // read-only filter) can leave zero tools on the wire, which makes
        // models improvise textual XML tool calls. Below the exposure
        // threshold, eager exposure is cheaper and simpler -- exactly the gate
        // documented on `DIRECT_TOOL_EXPOSURE_THRESHOLD` and the
        // `deferred_tool_policy_for_runtime` fallback arm.
        let expose_tools_directly = if !config.deferred_tool_policy.is_enabled() {
            // The disabled policy needs the estimate for its advisory warning.
            let estimated_schema_tokens = self.estimate_schema_tokens(&visible_entry_indices, &config);

            // Advisory one-shot: if deferred loading is disabled but this catalog
            // is large enough that deferral would engage, the user is paying the
            // full tool-schema tax on every request they could shed by enabling
            // `tools.client_tool_search`. Warn once per process -- the config
            // choice is stable across a session and repeating per request is noise.
            // (The system-prompt-exceeds-budget warning is emitted separately in
            // `prompts::system::apply_token_budget`; this covers the tool side.)
            if catalog_would_benefit_from_deferral(has_mcp_tools, deferable_tool_count, estimated_schema_tokens) {
                static DEFERRAL_DISABLED_WARNING: OnceLock<()> = OnceLock::new();
                if DEFERRAL_DISABLED_WARNING.set(()).is_ok() {
                    tracing::warn!(
                        available_tools = visible_entry_indices.len(),
                        deferable_tools = deferable_tool_count,
                        has_mcp_tools,
                        estimated_schema_tokens,
                        threshold = DIRECT_TOOL_EXPOSURE_THRESHOLD,
                        token_budget = DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET,
                        "Deferred tool loading is disabled (tools.client_tool_search = false) \
                         but the catalog is large enough to benefit from it; the full tool \
                         schema tax is paid on every request. Enable tools.client_tool_search \
                         to omit MCP/large schemas from the wire payload until needed."
                    );
                }
            }

            true
        } else if config.deferred_tool_policy.is_client_local() {
            // Client-local deferral uses the estimate to decide whether omitting
            // the deferred definitions is worthwhile for this catalog.
            let estimated_schema_tokens = self.estimate_schema_tokens(&visible_entry_indices, &config);
            !catalog_would_benefit_from_deferral(has_mcp_tools, deferable_tool_count, estimated_schema_tokens)
        } else {
            // Hosted policies always keep their deferred definitions in the wire
            // payload and therefore never need the schema-token estimate.
            false
        };

        let mut tools = Vec::with_capacity(visible_entry_indices.len() + if expose_tools_directly { 0 } else { 1 });
        let mut has_deferred_tools = false;

        for index in visible_entry_indices {
            let entry = &self.entries[index];
            let projection = self.projection(index, entry, config.documentation_mode);
            let defer_loading = should_defer_tool_loading(entry, &config);
            match entry.kind {
                CatalogToolKind::ApplyPatch if config.model_capabilities.supports_apply_patch_tool => {
                    let mut tool =
                        ToolDefinition::apply_patch(self.description_for_entry(entry, projection, &config).to_owned());
                    if defer_loading && !expose_tools_directly {
                        tool = tool.with_defer_loading(true);
                        has_deferred_tools = true;
                    }
                    tools.push(tool);
                }
                _ => {
                    let mut tool = if entry.public_name == tools::MEMORY {
                        ToolDefinition::anthropic_memory()
                    } else {
                        ToolDefinition::function(
                            entry.public_name.clone(),
                            self.description_for_entry(entry, projection, &config).to_owned(),
                            self.parameters_for_entry(entry, projection, &config),
                        )
                    };
                    if defer_loading && !expose_tools_directly {
                        tool = tool.with_defer_loading(true);
                        has_deferred_tools = true;
                        // Namespace metadata is only attached to deferred
                        // tools. It never reaches the wire payload (provider
                        // formatters build their JSON manually field-by-field
                        // for function tools and never serde-serialize the
                        // whole `ToolDefinition`), but restricting it to the
                        // deferred case keeps the blast radius small and
                        // matches the article's design: namespace grouping
                        // only matters once a tool is discoverable-only.
                        if let Some(namespace) = entry.namespace.clone() {
                            tool = tool.with_namespace(namespace);
                        }
                    }
                    tools.push(tool);
                }
            }
        }

        if has_deferred_tools && let Some(search_tool) = config.deferred_tool_policy.tool_search_definition() {
            tools.push(search_tool);
        }

        tools
    }

    /// Returns the schema entry for a tool by name.
    pub fn schema_for_name(&self, name: &str, config: SessionToolsConfig) -> Option<ToolSchemaEntry> {
        self.schema_entries(config).into_iter().find(|entry| entry.name == name)
    }

    pub(crate) fn entries(&self) -> &[ToolCatalogEntry] {
        &self.entries
    }

    fn estimate_schema_tokens(&self, entry_indices: &[usize], config: &SessionToolsConfig) -> usize {
        entry_indices
            .iter()
            .map(|&index| {
                let entry = &self.entries[index];
                let projection = self.projection(index, entry, config.documentation_mode);
                if entry.public_name == tools::TASK_TRACKER {
                    serialized_schema_token_estimate(
                        entry.public_name.as_str(),
                        self.description_for_entry(entry, projection, config),
                        &self.parameters_for_entry(entry, projection, config),
                    )
                } else {
                    projection.serialized_token_estimate(entry.public_name.as_str())
                }
            })
            .sum()
    }

    fn projection(
        &self,
        entry_index: usize,
        entry: &ToolCatalogEntry,
        documentation_mode: ToolDocumentationMode,
    ) -> &ToolEntryProjection {
        self.projection_cache.get_or_init(entry_index, entry, documentation_mode)
    }

    fn parameters_for_entry(
        &self,
        entry: &ToolCatalogEntry,
        projection: &ToolEntryProjection,
        config: &SessionToolsConfig,
    ) -> Value {
        if entry.public_name == tools::TASK_TRACKER {
            return compact_parameters(
                with_max_output_tokens_parameter(super::task_tracker::task_tracker_parameter_schema_for_workflow(
                    config.planning_active,
                )),
                config.documentation_mode,
            );
        }

        projection.parameters().clone()
    }

    fn description_for_entry<'a>(
        &self,
        entry: &'a ToolCatalogEntry,
        projection: &'a ToolEntryProjection,
        config: &SessionToolsConfig,
    ) -> &'a str {
        if entry.public_name == tools::TASK_TRACKER {
            return super::task_tracker::task_tracker_description_for_workflow(config.planning_active);
        }

        projection.description()
    }

    fn visible_entry_indices(&self, config: &SessionToolsConfig) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.is_visible(config).then_some(index))
            .collect()
    }
}

fn serialized_schema_token_estimate(name: &str, description: &str, parameters: &Value) -> usize {
    serde_json::to_string(&json!({
        "name": name,
        "description": description,
        "parameters": parameters,
    }))
    .map(|serialized| serialized.len() / 4)
    .unwrap_or(0)
}

impl ToolCatalogEntry {
    fn from_registration(registration: &ToolRegistration) -> Option<Self> {
        let metadata = registration.metadata();
        let description = metadata.description()?.to_string();
        let parameters = with_max_output_tokens_parameter(metadata.parameter_schema().cloned().unwrap_or_else(|| {
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": true
            })
        }));
        let default_permission = metadata.default_permission().unwrap_or(ToolPolicy::Prompt);
        let supports_parallel_tool_calls = registration_supports_parallel_tool_calls(registration);
        let aliases = metadata.aliases().to_vec();
        let kind = registration_catalog_kind(registration);
        let source = registration_catalog_source(registration, kind);

        if matches!(kind, CatalogToolKind::ApplyPatch) {
            let public_name = tools::APPLY_PATCH.to_string();
            return Some(Self::new(
                public_name,
                registration.name().to_string(),
                description,
                parameters,
                aliases,
                registration.capability(),
                default_permission,
                supports_parallel_tool_calls,
                source,
                kind,
            ));
        }

        if registration.name().starts_with("mcp::") {
            let public_name = aliases
                .iter()
                .find(|alias| alias.starts_with(MCP_QUALIFIED_TOOL_PREFIX))
                .cloned()
                .or_else(|| aliases.first().cloned())?;
            let mut entry = Self::new(
                public_name,
                registration.name().to_string(),
                description,
                parameters,
                aliases,
                registration.capability(),
                default_permission,
                supports_parallel_tool_calls,
                source,
                kind,
            );
            // MCP tool descriptions from external servers can be arbitrarily
            // long. Cap them to prevent token inflation.
            entry.max_description_length = Some(MCP_TOOL_DESCRIPTION_MAX_LEN);
            if let Some((server, _tool)) = parse_canonical_mcp_tool_name(registration.name()) {
                let namespace_description = metadata
                    .server_hint()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("Tools provided by MCP server '{server}'"));
                entry.namespace = Some(ToolNamespace {
                    name: server.to_string(),
                    description: namespace_description,
                });
            }
            return Some(entry);
        }

        if !registration.expose_in_llm() {
            return None;
        }

        Some(Self::new(
            registration.name().to_string(),
            registration.name().to_string(),
            description,
            parameters,
            aliases,
            registration.capability(),
            default_permission,
            supports_parallel_tool_calls,
            source,
            kind,
        ))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Intentional compatibility, platform, test, or API-shape suppression."
    )]
    fn new(
        public_name: String,
        registration_name: String,
        description: String,
        parameters: Value,
        aliases: Vec<String>,
        capability: CapabilityLevel,
        default_permission: ToolPolicy,
        supports_parallel_tool_calls: bool,
        source: ToolCatalogSource,
        kind: CatalogToolKind,
    ) -> Self {
        let configured_spec = ConfiguredToolSpec::new(
            ToolSpec::Function(ResponsesApiTool {
                name: public_name.clone(),
                description: description.clone(),
                strict: false,
                parameters: parse_tool_input_schema(&parameters),
            }),
            supports_parallel_tool_calls,
        );

        Self {
            public_name,
            registration_name,
            description,
            parameters,
            aliases,
            capability,
            default_permission,
            supports_parallel_tool_calls,
            source,
            kind,
            configured_spec,
            max_description_length: None,
            namespace: None,
        }
    }

    fn is_visible(&self, config: &SessionToolsConfig) -> bool {
        if self.capability > config.capability_level {
            return false;
        }

        if !profile_allows_tool(config.tool_profile, self.public_name.as_str(), config.planning_active) {
            return false;
        }

        if !surface_allows_tool(config.surface, self.public_name.as_str()) {
            return false;
        }

        match self.public_name.as_str() {
            tools::MEMORY => config.anthropic_native_memory_enabled,
            tools::REQUEST_USER_INPUT => config.request_user_input_enabled,
            _ => true,
        }
    }
}

fn profile_allows_tool(profile: ToolProfile, tool_name: &str, planning_active: bool) -> bool {
    match profile {
        // Planning keeps every read-only inspection surface plus the
        // interview tool. A `code_search`-only catalog (turn_912/913) forced
        // planners to answer basic "read this file / list this directory"
        // questions with repeated fuzzy searches. Mutating tools stay
        // excluded; the planning dispatch gate also hard-blocks mutations.
        ToolProfile::VtCode if planning_active => matches!(
            tool_name,
            tools::EXEC_COMMAND
                | tools::READ_FILE
                | tools::LIST_FILES
                | tools::GREP_FILE
                | tools::CODE_SEARCH
                | tools::REQUEST_USER_INPUT
        ),
        ToolProfile::VtCode => {
            matches!(tool_name, tools::EXEC_COMMAND | tools::WRITE_STDIN | tools::APPLY_PATCH | tools::SEARCH_TOOLS)
        }
        ToolProfile::AdvancedVtCode => !matches!(
            tool_name,
            tools::UNIFIED_EXEC
                | tools::UNIFIED_FILE
                | tools::UNIFIED_SEARCH
                | tools::LIST_FILES
                | tools::READ_FILE
                | tools::WRITE_FILE
                | tools::EDIT_FILE
                | tools::CREATE_FILE
                | tools::DELETE_FILE
                | tools::MOVE_FILE
                | tools::COPY_FILE
                | tools::SEARCH_REPLACE
                | tools::FILE_OP
        ),
    }
}

fn registration_catalog_source(registration: &ToolRegistration, kind: CatalogToolKind) -> ToolCatalogSource {
    if matches!(kind, CatalogToolKind::ApplyPatch) {
        return ToolCatalogSource::Builtin;
    }

    registration.catalog_source()
}

fn should_defer_tool_loading(entry: &ToolCatalogEntry, config: &SessionToolsConfig) -> bool {
    if !config.deferred_tool_policy.is_enabled() {
        return false;
    }

    if config.deferred_tool_policy.keeps_entry_available(entry) || is_core_tool_entry(entry, config) {
        return false;
    }

    if config.deferred_tool_policy.is_client_local() {
        return matches!(
            entry.source,
            ToolCatalogSource::Builtin | ToolCatalogSource::Mcp | ToolCatalogSource::Dynamic
        );
    }

    matches!(entry.source, ToolCatalogSource::Builtin | ToolCatalogSource::Mcp | ToolCatalogSource::Dynamic)
}

fn is_core_tool_entry(entry: &ToolCatalogEntry, config: &SessionToolsConfig) -> bool {
    // `entry.public_name` is always the canonical registration name, never an
    // alias (spawn_agent/spawn_background_subprocess/send_input/wait_agent/
    // resume_agent/close_agent all route to the single `agent` registration),
    // so only the canonical name needs to be matched here.
    match entry.public_name.as_str() {
        tools::EXEC_COMMAND
        | tools::WRITE_STDIN
        | tools::TASK_TRACKER
        | tools::START_PLANNING
        | tools::AGENT
        | tools::LIST_SKILLS
        | tools::LOAD_SKILL
        | tools::LOAD_SKILL_RESOURCE
        | tools::SEARCH_TOOLS => true,
        // Planning keeps its read-only inspection surface on the wire even
        // when MCP tools force deferral: session-20260923 lost GREP_FILE from
        // the wire catalog and fell back to exec_command shell reads, blowing
        // the preview budget with 26-38 KiB spooled previews.
        tools::READ_FILE | tools::LIST_FILES | tools::GREP_FILE | tools::CODE_SEARCH if config.planning_active => true,
        tools::MCP_SEARCH_TOOLS | tools::MCP_GET_TOOL_DETAILS | tools::MCP_LIST_SERVERS => {
            config.deferred_tool_policy.is_client_local()
        }
        tools::MEMORY => config.anthropic_native_memory_enabled,
        tools::REQUEST_USER_INPUT => config.request_user_input_enabled,
        tools::APPLY_PATCH => config.model_capabilities.supports_apply_patch_tool,
        _ => false,
    }
}

fn surface_allows_tool(surface: SessionSurface, tool_name: &str) -> bool {
    match surface {
        SessionSurface::Interactive => !matches!(tool_name, tools::READ_FILE | tools::LIST_FILES),
        SessionSurface::AgentRunner => true,
        SessionSurface::Acp => {
            matches!(tool_name, tools::EXEC_COMMAND | tools::WRITE_STDIN | tools::APPLY_PATCH | tools::CODE_SEARCH)
        }
    }
}

fn registration_catalog_kind(registration: &ToolRegistration) -> CatalogToolKind {
    registration
        .metadata()
        .behavior()
        .map(|behavior| match behavior.surface_kind {
            ToolSurfaceKind::Function => CatalogToolKind::Function,
            ToolSurfaceKind::ApplyPatch => CatalogToolKind::ApplyPatch,
        })
        .unwrap_or(CatalogToolKind::Function)
}

fn registration_supports_parallel_tool_calls(registration: &ToolRegistration) -> bool {
    if let Some(behavior) = registration.metadata().behavior() {
        return behavior.supports_parallel_calls;
    }

    match registration.handler() {
        RegistryToolHandler::TraitObject(tool) => tool.is_parallel_safe(),
        RegistryToolHandler::RegistryFn(_) => false,
    }
}

#[cfg(test)]
use vtcode_utility_tool_specs::{apply_patch_parameters, exec_command_parameters, list_files_parameters};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VTCodeConfig;
    use crate::tools::constants::empty_object_schema;
    use crate::tools::registry::ToolRegistration;
    use crate::tools::request_user_input::RequestUserInputTool;
    use crate::tools::tool_intent::{ToolBehavior, ToolMutationModel};
    use crate::tools::traits::Tool;
    use serde_json::json;

    fn registration(name: &'static str) -> ToolRegistration {
        ToolRegistration::new(name, CapabilityLevel::CodeSearch, false, |_, _| Box::pin(async { Ok(Value::Null) }))
    }

    #[test]
    fn default_profile_exposes_only_codex_baseline_tools() {
        let registrations = vec![
            registration(tools::EXEC_COMMAND)
                .with_description("Run command")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::WRITE_STDIN)
                .with_description("Write stdin")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::APPLY_PATCH)
                .with_llm_visibility(false)
                .with_description("Apply patch")
                .with_parameter_schema(apply_patch_parameters())
                .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true)),
            registration(tools::CODE_SEARCH)
                .with_description("Search code")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::SEARCH_TOOLS)
                .with_description("Discover deferred tools")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::READ_FILE)
                .with_llm_visibility(false)
                .with_description("Read file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::WRITE_FILE)
                .with_llm_visibility(false)
                .with_description("Write file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::DELETE_FILE)
                .with_description("Delete file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::MOVE_FILE)
                .with_description("Move file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::COPY_FILE)
                .with_description("Copy file")
                .with_parameter_schema(empty_object_schema()),
            registration("ls")
                .with_description("List directory")
                .with_parameter_schema(empty_object_schema()),
            registration("rg")
                .with_description("Search text")
                .with_parameter_schema(empty_object_schema()),
            registration("find")
                .with_description("Find files")
                .with_parameter_schema(empty_object_schema()),
            registration("cat")
                .with_description("Print file")
                .with_parameter_schema(empty_object_schema()),
            registration("sed")
                .with_description("Stream edit")
                .with_parameter_schema(empty_object_schema()),
            registration("awk")
                .with_description("Process text")
                .with_parameter_schema(empty_object_schema()),
        ];

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let mut config = SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        );
        config.planning_active = false;
        let names = catalog.public_tool_names(config);

        assert_eq!(
            names,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::WRITE_STDIN.to_string(),
                tools::APPLY_PATCH.to_string(),
                tools::SEARCH_TOOLS.to_string(),
            ]
        );
        for command in ["ls", "rg", "find", "cat", "sed", "awk"] {
            assert!(
                !names.contains(&command.to_string()),
                "{command} must stay an exec_command.cmd example, not a default tool"
            );
        }
        for file_tool in [
            tools::READ_FILE,
            tools::WRITE_FILE,
            tools::DELETE_FILE,
            tools::MOVE_FILE,
            tools::COPY_FILE,
            tools::UNIFIED_FILE,
        ] {
            assert!(!names.contains(&file_tool.to_string()), "{file_tool} must stay out of the default file surface");
        }
    }

    #[test]
    fn default_profile_exposes_planning_tools_during_planning() {
        let registrations = vec![
            registration(tools::EXEC_COMMAND)
                .with_description("Run command")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::WRITE_STDIN)
                .with_description("Write stdin")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::APPLY_PATCH)
                .with_llm_visibility(false)
                .with_description("Apply patch")
                .with_parameter_schema(apply_patch_parameters())
                .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true)),
            registration(tools::SEARCH_TOOLS)
                .with_description("Discover deferred tools")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::CODE_SEARCH)
                .with_description("Search code")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::READ_FILE)
                .with_description("Read file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::LIST_FILES)
                .with_description("List files")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::GREP_FILE)
                .with_description("Grep file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::REQUEST_USER_INPUT)
                .with_description("Ask the user")
                .with_parameter_schema(empty_object_schema()),
        ];

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let normal_names = catalog.public_tool_names(SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));
        assert_eq!(
            normal_names,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::WRITE_STDIN.to_string(),
                tools::APPLY_PATCH.to_string(),
                tools::SEARCH_TOOLS.to_string(),
            ]
        );

        let planning_names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_planning_active(true),
        );

        // Planning must expose the full read-only inspection surface plus the
        // interview tool — never collapse to a bare `code_search` catalog
        // (turn_912/913 regression). The Interactive surface additionally
        // hides read_file/list_files (they stay reachable on AgentRunner);
        // interactive planners read files through exec_command per the
        // planning read-only notice.
        assert_eq!(
            planning_names,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::CODE_SEARCH.to_string(),
                tools::GREP_FILE.to_string(),
                tools::REQUEST_USER_INPUT.to_string(),
            ]
        );

        let agent_runner_planning_names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::AgentRunner,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_planning_active(true),
        );
        assert_eq!(
            agent_runner_planning_names,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::CODE_SEARCH.to_string(),
                tools::READ_FILE.to_string(),
                tools::LIST_FILES.to_string(),
                tools::GREP_FILE.to_string(),
                tools::REQUEST_USER_INPUT.to_string(),
            ]
        );
    }

    /// Turn_912/913 end-to-end regression: compose the same three wire
    /// filters the binary runloop applies (catalog profile+surface, primary
    /// agent tool policy, permission advertisement) for the built-in plan
    /// agent on the Interactive surface and assert the resulting planning
    /// catalog. Before the fix this collapsed to `["code_search"]`.
    #[test]
    fn plan_agent_interactive_wire_catalog_survives_all_filters() {
        use crate::config::PermissionsConfig;
        use crate::permissions::{build_advertised_permission_requests, evaluate_effective_permissions};
        use crate::primary_agent::{ActivePrimaryAgent, primary_agent_allows_tool};

        let registrations = [
            tools::EXEC_COMMAND,
            tools::CODE_SEARCH,
            tools::GREP_FILE,
            tools::READ_FILE,
            tools::LIST_FILES,
            tools::REQUEST_USER_INPUT,
            tools::APPLY_PATCH,
            tools::WRITE_FILE,
        ]
        .into_iter()
        .map(|name| {
            registration(name)
                .with_description("test tool")
                .with_parameter_schema(empty_object_schema())
        })
        .collect::<Vec<_>>();
        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_planning_active(true),
        );

        let agent = ActivePrimaryAgent::from_spec(&vtcode_config::builtin_plan_agent());
        let global = PermissionsConfig::default();
        let temp = tempfile::TempDir::new().expect("tempdir");
        let workspace = temp.path();
        let visible: Vec<String> = names
            .iter()
            .filter(|name| primary_agent_allows_tool(&agent, name))
            .filter(|name| {
                let requests = build_advertised_permission_requests(workspace, workspace, name);
                requests.is_empty()
                    || requests.iter().any(|request| {
                        evaluate_effective_permissions(&global, &agent.permissions, workspace, workspace, request)
                            != crate::permissions::ResolvedPermissionDecision::Deny
                    })
            })
            .cloned()
            .collect();

        assert_eq!(
            visible,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::CODE_SEARCH.to_string(),
                tools::GREP_FILE.to_string(),
                tools::REQUEST_USER_INPUT.to_string(),
            ],
            "plan agent Interactive wire catalog must keep the read-only inspection + interview set"
        );
    }

    #[test]
    fn exec_command_schema_models_unix_tools_as_cmd_examples() {
        let registration = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(exec_command_parameters());
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let entries = catalog.schema_entries(SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));
        let entry = entries
            .iter()
            .find(|entry| entry.name == tools::EXEC_COMMAND)
            .expect("exec_command schema entry");
        let properties = &entry.parameters["properties"];

        assert_eq!(entry.parameters["required"], json!(["cmd"]));
        assert!(properties["cmd"]["description"].as_str().is_some_and(|text| {
            ["ls", "rg", "find", "cat", "sed", "awk"]
                .iter()
                .all(|command| text.contains(command))
        }));
        assert_eq!(properties["tty"]["type"], "boolean");
        for command in ["ls", "rg", "find", "cat", "sed", "awk"] {
            assert!(properties.get(command).is_none(), "{command} must not be modelled as a separate schema property");
        }
    }

    #[test]
    fn advanced_profile_exposes_code_search_without_internal_search_names() {
        let registrations = vec![
            registration(tools::CODE_SEARCH)
                .with_description("Search code")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::LIST_FILES)
                .with_llm_visibility(false)
                .with_description("List files")
                .with_parameter_schema(list_files_parameters()),
            registration(tools::READ_FILE)
                .with_llm_visibility(false)
                .with_description("Read file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::WRITE_FILE)
                .with_llm_visibility(false)
                .with_description("Write file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::DELETE_FILE)
                .with_description("Delete file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::MOVE_FILE)
                .with_description("Move file")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::COPY_FILE)
                .with_description("Copy file")
                .with_parameter_schema(empty_object_schema()),
        ];

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::AgentRunner,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );

        assert_eq!(names, vec![tools::CODE_SEARCH.to_string()]);
    }

    #[test]
    fn advanced_profile_retains_eligible_specialised_and_dynamic_tools() {
        let registrations = vec![
            registration(tools::CODE_SEARCH)
                .with_description("Search code")
                .with_parameter_schema(empty_object_schema()),
            registration("mcp::context7::search")
                .with_catalog_source(ToolCatalogSource::Mcp)
                .with_llm_visibility(false)
                .with_description("Search documentation")
                .with_parameter_schema(empty_object_schema())
                .with_aliases(["mcp__context7__search"]),
            registration(tools::LOAD_SKILL)
                .with_catalog_source(ToolCatalogSource::Builtin)
                .with_description("Load a skill")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::START_PLANNING)
                .with_catalog_source(ToolCatalogSource::Builtin)
                .with_description("Start planning")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::SPAWN_AGENT)
                .with_catalog_source(ToolCatalogSource::Builtin)
                .with_description("Spawn an agent")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::CRON_CREATE)
                .with_catalog_source(ToolCatalogSource::Builtin)
                .with_description("Create a scheduled prompt")
                .with_parameter_schema(empty_object_schema()),
            registration("dynamic_plugin_tool")
                .with_catalog_source(ToolCatalogSource::Dynamic)
                .with_description("Run a dynamic plugin tool")
                .with_parameter_schema(empty_object_schema()),
        ];

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );

        assert_eq!(
            names,
            vec![
                tools::CODE_SEARCH.to_string(),
                "mcp__context7__search".to_string(),
                tools::LOAD_SKILL.to_string(),
                tools::START_PLANNING.to_string(),
                tools::SPAWN_AGENT.to_string(),
                tools::CRON_CREATE.to_string(),
                "dynamic_plugin_tool".to_string(),
            ]
        );
    }

    #[test]
    fn acp_surface_exposes_code_search_with_advanced_profile() {
        let registrations = vec![
            registration(tools::EXEC_COMMAND)
                .with_description("Run command")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::WRITE_STDIN)
                .with_description("Write stdin")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::APPLY_PATCH)
                .with_llm_visibility(false)
                .with_description("Apply patch")
                .with_parameter_schema(apply_patch_parameters())
                .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true)),
            registration(tools::CODE_SEARCH)
                .with_description("Search code")
                .with_parameter_schema(empty_object_schema()),
            registration(tools::LOAD_SKILL)
                .with_description("Load a skill")
                .with_parameter_schema(empty_object_schema()),
        ];

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Acp,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );

        assert_eq!(
            names,
            vec![
                tools::EXEC_COMMAND.to_string(),
                tools::WRITE_STDIN.to_string(),
                tools::APPLY_PATCH.to_string(),
                tools::CODE_SEARCH.to_string(),
            ]
        );
    }

    #[test]
    fn rebuild_catalog_uses_public_mcp_alias() {
        let registration = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let names = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );

        assert_eq!(names, vec!["mcp__context7__search".to_string()]);
    }

    #[test]
    fn schema_entries_hide_request_user_input_when_disabled() {
        let registration = registration(tools::REQUEST_USER_INPUT)
            .with_description("Ask the user")
            .with_parameter_schema(empty_object_schema());

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let names = catalog.public_tool_names(SessionToolsConfig {
            surface: SessionSurface::Interactive,
            capability_level: CapabilityLevel::CodeSearch,
            documentation_mode: ToolDocumentationMode::Full,
            planning_active: true,
            request_user_input_enabled: false,
            model_capabilities: ToolModelCapabilities::default(),
            deferred_tool_policy: DeferredToolPolicy::default(),
            anthropic_native_memory_enabled: false,
            tool_profile: ToolProfile::VtCode,
        });

        assert!(names.is_empty());
    }

    #[test]
    fn task_tracker_stays_visible_outside_planning_workflow() {
        let registration = registration(tools::TASK_TRACKER)
            .with_description("Track plan tasks")
            .with_parameter_schema(empty_object_schema());

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let names = catalog.public_tool_names(SessionToolsConfig {
            surface: SessionSurface::Interactive,
            capability_level: CapabilityLevel::CodeSearch,
            documentation_mode: ToolDocumentationMode::Full,
            planning_active: false,
            request_user_input_enabled: true,
            model_capabilities: ToolModelCapabilities::default(),
            deferred_tool_policy: DeferredToolPolicy::default(),
            anthropic_native_memory_enabled: false,
            tool_profile: ToolProfile::AdvancedVtCode,
        });

        assert_eq!(names, vec![tools::TASK_TRACKER.to_string()]);
    }

    #[test]
    fn memory_tool_is_hidden_unless_anthropic_native_memory_is_enabled() {
        let registration = registration(tools::MEMORY)
            .with_description("Native memory")
            .with_parameter_schema(empty_object_schema());
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);

        let hidden = catalog.public_tool_names(SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));
        assert!(hidden.is_empty());

        let visible = catalog.public_tool_names(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_anthropic_native_memory_enabled(true),
        );
        assert_eq!(visible, vec![tools::MEMORY.to_string()]);
    }

    #[test]
    fn memory_tool_uses_anthropic_native_definition_when_visible() {
        let registration = registration(tools::MEMORY)
            .with_description("Native memory")
            .with_parameter_schema(empty_object_schema());
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);

        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_anthropic_native_memory_enabled(true),
        );

        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].tool_type, "memory_20250818");
        assert_eq!(definitions[0].function_name(), tools::MEMORY);
    }

    #[test]
    fn apply_patch_uses_special_tool_when_supported() {
        let registration = registration(tools::APPLY_PATCH)
            .with_llm_visibility(false)
            .with_description("Apply patch")
            .with_parameter_schema(apply_patch_parameters())
            .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true));

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let tools = catalog.model_tools(SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities { supports_apply_patch_tool: true },
        ));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "apply_patch");
    }

    #[test]
    fn apply_patch_falls_back_to_function_tool_when_unsupported() {
        let registration = registration(tools::APPLY_PATCH)
            .with_llm_visibility(false)
            .with_description("Apply patch")
            .with_parameter_schema(apply_patch_parameters())
            .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true));

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let tools = catalog.model_tools(SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
    }

    #[test]
    fn agent_runner_default_hides_legacy_browse_tools() {
        let read_file = registration(tools::READ_FILE)
            .with_llm_visibility(false)
            .with_description("Read file contents in chunks")
            .with_parameter_schema(empty_object_schema());
        let list_files = registration(tools::LIST_FILES)
            .with_llm_visibility(false)
            .with_description("List files with pagination")
            .with_parameter_schema(list_files_parameters());
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![read_file, list_files]);

        let interactive_names = catalog.public_tool_names(SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));
        assert!(!interactive_names.contains(&tools::READ_FILE.to_string()));
        assert!(!interactive_names.contains(&tools::LIST_FILES.to_string()));

        let agent_runner_names = catalog.public_tool_names(SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        ));
        assert!(!agent_runner_names.contains(&tools::READ_FILE.to_string()));
        assert!(!agent_runner_names.contains(&tools::LIST_FILES.to_string()));
    }

    #[test]
    fn parallel_support_comes_from_behavior_metadata() {
        let registration = registration("parallel_catalog_tool")
            .with_description("parallel-safe test tool")
            .with_parameter_schema(empty_object_schema())
            .with_behavior(ToolBehavior::function(ToolMutationModel::ReadOnly, true, false));

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        assert_eq!(catalog.entries().len(), 1);
        assert!(catalog.entries()[0].supports_parallel_tool_calls);
    }

    #[test]
    fn model_tool_serialization_keeps_output_cap_separate_from_approval_policy() {
        let registration = registration(tools::EXEC_COMMAND)
            .with_description("Run the policy surface test")
            .with_parameter_schema(empty_object_schema())
            .with_permission(ToolPolicy::Allow);
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );
        let tool = definitions.first().expect("model tool definition");
        let serialized = serde_json::to_value(tool).expect("serialize model tool definition");

        assert_eq!(
            tool.function.as_ref().map(|function| function.description.as_str()),
            Some("Run the policy surface test")
        );
        assert_eq!(
            tool.function
                .as_ref()
                .map(|function| function.parameters["properties"]["max_output_tokens"]["default"].clone()),
            Some(json!(vtcode_utility_tool_specs::DEFAULT_MAX_OUTPUT_TOKENS))
        );
        for key in [
            "approval_policy",
            "default_permission",
            "permission",
            "tool_policy",
            "allow_patterns",
            "deny_patterns",
        ] {
            assert!(!contains_json_key(&serialized, key), "approval metadata leaked into model schema: {key}");
        }
    }

    fn contains_json_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(object) => object.iter().any(|(name, value)| name == key || contains_json_key(value, key)),
            Value::Array(values) => values.iter().any(|value| contains_json_key(value, key)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
        }
    }

    #[test]
    fn configured_spec_preserves_json_schema_field_names() {
        let registration = registration("schema_contract_tool")
            .with_description("schema contract")
            .with_parameter_schema(json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"}
                },
                "additionalProperties": false,
                "anyOf": [
                    {"required": ["input"]},
                    {"required": ["patch"]}
                ]
            }));

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![registration]);
        let entry = &catalog.entries()[0];
        let ToolSpec::Function(tool) = &entry.configured_spec.spec else {
            panic!("expected function tool spec");
        };

        let serialized = serde_json::to_value(&tool.parameters).expect("serialize parameters");
        assert_eq!(serialized["additionalProperties"], Value::Bool(false));
        assert!(serialized["anyOf"].is_array());
        assert!(serialized.get("additional_properties").is_none());
        assert!(serialized.get("any_of").is_none());
    }

    #[test]
    fn compact_parameters_preserves_property_named_description() {
        let schema = RequestUserInputTool.parameter_schema().expect("request_user_input schema");

        let compacted = compact_parameters(schema, ToolDocumentationMode::Progressive);
        let description_property = &compacted["properties"]["questions"]["items"]["properties"]["options"]["items"]["properties"]
            ["description"];

        assert!(description_property.is_object());
        assert_eq!(
            compacted["properties"]["questions"]["items"]["properties"]["options"]["items"]["required"],
            json!(["label", "description"])
        );
    }

    #[test]
    fn cached_projections_preserve_schema_and_deferral_across_documentation_modes() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run a command. Use the workspace shell safely.")
            .with_parameter_schema(json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Command to run."}
                },
                "required": ["cmd"]
            }));
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description(
                "Search the documentation server. This description is intentionally longer than minimal mode.",
            )
            .with_parameter_schema(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Documentation query."}
                }
            }))
            .with_aliases(["mcp__context7__search"]);
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command, mcp_tool]);

        for documentation_mode in [
            ToolDocumentationMode::Minimal,
            ToolDocumentationMode::Progressive,
            ToolDocumentationMode::Full,
        ] {
            let config = SessionToolsConfig::full_public(
                SessionSurface::AgentRunner,
                CapabilityLevel::CodeSearch,
                documentation_mode,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new()));

            let expected_schema = catalog
                .entries()
                .iter()
                .filter(|entry| entry.is_visible(&config))
                .map(|entry| ToolSchemaEntry {
                    name: entry.public_name.clone(),
                    description: compact_tool_description(
                        entry.description.as_str(),
                        documentation_mode,
                        entry.max_description_length,
                    ),
                    parameters: compact_parameters(entry.parameters.clone(), documentation_mode),
                })
                .collect::<Vec<_>>();
            assert_eq!(catalog.schema_entries(config.clone()), expected_schema);

            let visible_entries = catalog
                .entries()
                .iter()
                .filter(|entry| entry.is_visible(&config))
                .collect::<Vec<_>>();
            let estimated_schema_tokens = expected_schema
                .iter()
                .map(|entry| serde_json::to_string(entry).expect("serialize expected schema").len() / 4)
                .sum();
            let deferable_tool_count = visible_entries
                .iter()
                .filter(|entry| should_defer_tool_loading(entry, &config))
                .count();
            let expose_tools_directly = config.deferred_tool_policy.is_client_local()
                && !catalog_would_benefit_from_deferral(
                    visible_entries
                        .iter()
                        .any(|entry| matches!(entry.source, ToolCatalogSource::Mcp)),
                    deferable_tool_count,
                    estimated_schema_tokens,
                );
            let definitions = catalog.model_tools(config.clone());

            for entry in visible_entries {
                let expected_deferred = should_defer_tool_loading(entry, &config) && !expose_tools_directly;
                let definition = definitions
                    .iter()
                    .find(|tool| tool.function_name() == entry.public_name)
                    .unwrap_or_else(|| panic!("missing model definition for {}", entry.public_name));
                let actual_deferred = definition.defer_loading == Some(true);
                assert_eq!(actual_deferred, expected_deferred, "deferral changed for {}", entry.public_name);
            }

            assert_eq!(catalog.model_tools(config.clone()), definitions);
            assert!(definitions.iter().any(|tool| tool.function_name() == tools::EXEC_COMMAND));
        }
    }

    #[test]
    fn hosted_deferral_skips_schema_estimation_without_changing_tool_output() {
        let registrations = (0..(DIRECT_TOOL_EXPOSURE_THRESHOLD + 4))
            .map(|index| {
                ToolRegistration::new(
                    format!("hosted_catalog_tool_{index}"),
                    CapabilityLevel::CodeSearch,
                    false,
                    |_, _| Box::pin(async { Ok(Value::Null) }),
                )
                .with_description(format!("Search the hosted catalog for item {index}."))
                .with_parameter_schema(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query." }
                    },
                    "required": ["query"]
                }))
            })
            .collect();
        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let config = SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Progressive,
            ToolModelCapabilities::default(),
        )
        .with_tool_profile(ToolProfile::AdvancedVtCode)
        .with_deferred_tool_policy(DeferredToolPolicy::openai_hosted(Vec::new()));

        let expected_schema = catalog.schema_entries(config.clone());
        let definitions = catalog.model_tools(config.clone());

        assert_eq!(definitions.len(), expected_schema.len() + 1, "hosted search must be added to the catalog");
        assert!(definitions.iter().any(ToolDefinition::is_tool_search));
        for schema in expected_schema {
            let definition = definitions
                .iter()
                .find(|tool| tool.function_name() == schema.name)
                .unwrap_or_else(|| panic!("missing hosted definition for {}", schema.name));
            let function = definition.function.as_ref().expect("hosted catalog function definition");
            assert_eq!(function.name, schema.name);
            assert_eq!(function.description, schema.description);
            assert_eq!(function.parameters, schema.parameters);
            assert_eq!(definition.defer_loading, Some(true));
        }

        let estimates_initialized = catalog.visible_entry_indices(&config).into_iter().any(|index| {
            let entry = &catalog.entries[index];
            catalog
                .projection(index, entry, config.documentation_mode)
                .has_serialized_token_estimate()
        });
        assert!(!estimates_initialized, "hosted policies must not serialize schema-token estimates");
    }

    #[test]
    fn eager_and_client_local_policies_still_estimate_schema_tokens_when_needed() {
        for deferred_tool_policy in [
            DeferredToolPolicy::default(),
            DeferredToolPolicy::client_local(Vec::new()),
        ] {
            let registrations = (0..(DIRECT_TOOL_EXPOSURE_THRESHOLD + 1))
                .map(|index| {
                    ToolRegistration::new(
                        format!("estimated_catalog_tool_{index}"),
                        CapabilityLevel::CodeSearch,
                        false,
                        |_, _| Box::pin(async { Ok(Value::Null) }),
                    )
                    .with_description(format!("Estimate this catalog entry {index}."))
                    .with_parameter_schema(json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } }
                    }))
                })
                .collect();
            let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
            let config = SessionToolsConfig::full_public(
                SessionSurface::AgentRunner,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Progressive,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(deferred_tool_policy);

            let _ = catalog.model_tools(config.clone());
            let estimates_initialized = catalog.visible_entry_indices(&config).into_iter().any(|index| {
                let entry = &catalog.entries[index];
                catalog
                    .projection(index, entry, config.documentation_mode)
                    .has_serialized_token_estimate()
            });
            assert!(estimates_initialized, "the active policy must retain its schema-token decision");
        }
    }

    #[test]
    fn anthropic_policy_injects_tool_search_and_defers_non_core_tools() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let apply_patch = registration(tools::APPLY_PATCH)
            .with_llm_visibility(false)
            .with_description("Apply patch")
            .with_parameter_schema(apply_patch_parameters())
            .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true));
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let mut registrations = vec![exec_command, apply_patch, mcp_tool];
        for index in 0..DIRECT_TOOL_EXPOSURE_THRESHOLD {
            let name: &'static str = Box::leak(format!("mcp::context7::resolve_{index}").into_boxed_str());
            let alias = format!("mcp__context7__resolve_{index}");
            registrations.push(
                registration(name)
                    .with_catalog_source(ToolCatalogSource::Mcp)
                    .with_llm_visibility(false)
                    .with_description(format!("resolve docs {index}"))
                    .with_parameter_schema(empty_object_schema())
                    .with_aliases([alias]),
            );
        }

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::anthropic(ToolSearchAlgorithm::Regex, Vec::new())),
        );

        assert!(
            definitions
                .iter()
                .any(|tool| tool.tool_type == "tool_search_tool_regex_20251119"),
            "anthropic tool search should be injected when deferred tools exist"
        );
        let exec_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::EXEC_COMMAND)
            .expect("exec_command should be present");
        assert_eq!(exec_tool.defer_loading, None);

        let apply_patch = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::APPLY_PATCH)
            .expect("apply_patch fallback should be present");
        assert_eq!(apply_patch.defer_loading, Some(true));

        let mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(mcp_tool.defer_loading, Some(true));
    }

    #[test]
    fn mcp_tool_registration_derives_namespace_from_server_name() {
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![mcp_tool]);
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.public_name == "mcp__context7__search")
            .expect("mcp entry should be present");

        let namespace = entry
            .namespace
            .as_ref()
            .expect("mcp tool should derive a namespace from its server name");
        assert_eq!(namespace.name, "context7");
        assert_eq!(namespace.description, "Tools provided by MCP server 'context7'");
    }

    #[test]
    fn core_tool_registration_has_no_namespace() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command]);
        let entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.public_name == tools::EXEC_COMMAND)
            .expect("core tool entry should be present");

        assert!(entry.namespace.is_none(), "core/builtin tools should not derive a namespace");
    }

    #[test]
    fn model_tools_attach_namespace_only_to_deferred_mcp_tools() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let mut registrations = vec![exec_command, mcp_tool];
        for index in 0..DIRECT_TOOL_EXPOSURE_THRESHOLD {
            let name: &'static str = Box::leak(format!("mcp::context7::resolve_{index}").into_boxed_str());
            let alias = format!("mcp__context7__resolve_{index}");
            registrations.push(
                registration(name)
                    .with_catalog_source(ToolCatalogSource::Mcp)
                    .with_llm_visibility(false)
                    .with_description(format!("resolve docs {index}"))
                    .with_parameter_schema(empty_object_schema())
                    .with_aliases([alias]),
            );
        }

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::anthropic(ToolSearchAlgorithm::Regex, Vec::new())),
        );

        let core_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::EXEC_COMMAND)
            .expect("exec_command should be present");
        assert_eq!(core_tool.defer_loading, None);
        assert!(core_tool.namespace.is_none(), "non-deferred core tools should never carry namespace metadata");

        let deferred_mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("deferred mcp tool should be present");
        assert_eq!(deferred_mcp_tool.defer_loading, Some(true));
        let namespace = deferred_mcp_tool
            .namespace
            .as_ref()
            .expect("deferred mcp tool should carry namespace metadata");
        assert_eq!(namespace.name, "context7");
    }

    #[test]
    fn small_mcp_catalog_is_deferred_despite_low_tool_count() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command, mcp_tool]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::anthropic(ToolSearchAlgorithm::Regex, Vec::new())),
        );

        let mcp_definition = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(
            mcp_definition.defer_loading,
            Some(true),
            "even a single MCP tool should be deferred to avoid schema tax"
        );
    }

    #[test]
    fn client_local_policy_deferred_for_small_mcp_catalog() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let mcp_search_tools = registration(tools::MCP_SEARCH_TOOLS)
            .with_description("Search MCP tools")
            .with_parameter_schema(empty_object_schema());
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command, mcp_search_tools, mcp_tool]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new())),
        );

        assert!(
            definitions.iter().any(|tool| tool.function_name() == "mcp__context7__search"),
            "mcp tool should still be listed in the model-facing catalog for client-local search"
        );
        let mcp_definition = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(
            mcp_definition.defer_loading,
            Some(true),
            "client-local deferral should also apply to small MCP catalogs"
        );
        let search_definition = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::MCP_SEARCH_TOOLS)
            .expect("client-local MCP search should remain available");
        assert_eq!(search_definition.defer_loading, None);
    }

    #[test]
    fn client_local_policy_exposes_small_builtin_catalog_directly() {
        // Plan-mode policy filtering shrinks the visible catalog to a handful
        // of read-only builtins. Deferring that tiny set drops every tool from
        // the wire payload (on_wire_tools = 0), so the model improvises
        // textual XML tool calls instead of native ones (turn_887/turn_888).
        // Small catalogs must stay eager: deferral exists to shed schema tax,
        // not to hide the whole catalog.
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let code_search = registration(tools::CODE_SEARCH)
            .with_description("Search code")
            .with_parameter_schema(empty_object_schema());
        let read_file = registration(tools::READ_FILE)
            .with_description("Read file")
            .with_parameter_schema(empty_object_schema());

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command, code_search, read_file]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new())),
        );

        assert!(
            definitions.iter().all(|tool| tool.defer_loading.is_none()),
            "a small builtin-only catalog gains nothing from client-local deferral; every tool must stay on the wire"
        );
    }

    #[test]
    fn client_local_policy_defers_large_builtin_catalog() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let code_search = registration(tools::CODE_SEARCH)
            .with_description("Search code")
            .with_parameter_schema(empty_object_schema());

        let mut registrations = vec![exec_command, code_search];
        for index in 0..DIRECT_TOOL_EXPOSURE_THRESHOLD {
            let name: &'static str = Box::leak(format!("extra_builtin_{index}").into_boxed_str());
            registrations.push(
                registration(name)
                    .with_description(format!("extra builtin {index}"))
                    .with_parameter_schema(empty_object_schema()),
            );
        }

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new())),
        );

        let exec_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::EXEC_COMMAND)
            .expect("exec_command should be present");
        assert_eq!(exec_tool.defer_loading, None, "core tools stay eager even in large catalogs");
        let search_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == tools::CODE_SEARCH)
            .expect("code_search should be present");
        assert_eq!(
            search_tool.defer_loading,
            Some(true),
            "large builtin catalogs still defer non-core tools under client-local policy"
        );
    }

    /// Serialize the on-wire tool schemas (the definitions the model actually
    /// receives) and estimate their token cost at ~4 chars/token, matching the
    /// convention in `estimate_schema_tokens` and the first-request budget test
    /// in `tools/registry/builtins.rs`. Deferred tools (`defer_loading ==
    /// Some(true)`) are omitted under the client-local policy, so they never
    /// reach the wire payload and are excluded here.
    fn on_wire_schema_tokens(catalog: &SessionToolCatalog, config: SessionToolsConfig) -> usize {
        #[derive(Serialize)]
        struct Estimate<'a> {
            name: &'a str,
            description: &'a str,
            parameters: &'a Value,
        }
        let on_wire: FxHashSet<String> = catalog
            .model_tools(config.clone())
            .into_iter()
            .filter(|tool| tool.defer_loading != Some(true))
            .map(|tool| tool.function_name().to_string())
            .collect();
        catalog
            .schema_entries(config)
            .into_iter()
            .filter(|entry| on_wire.contains(&entry.name))
            .map(|entry| {
                serde_json::to_string(&Estimate {
                    name: &entry.name,
                    description: &entry.description,
                    parameters: &entry.parameters,
                })
                .map(|s| s.len() / 4)
                .unwrap_or(0)
            })
            .sum()
    }

    #[test]
    fn task_tracker_schema_token_estimate_tracks_workflow_specific_parameters() {
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![
            registration(tools::TASK_TRACKER)
                .with_description("Track plan tasks")
                .with_parameter_schema(empty_object_schema()),
        ]);
        let base_config = SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        )
        .with_tool_profile(ToolProfile::AdvancedVtCode);

        let standard_config = base_config.clone().with_planning_active(false);
        let standard_visible = catalog.visible_entry_indices(&standard_config);
        assert_eq!(
            catalog.estimate_schema_tokens(&standard_visible, &standard_config),
            on_wire_schema_tokens(&catalog, standard_config),
            "inactive task_tracker token estimate should match the emitted schema",
        );

        let planning_config = base_config.with_planning_active(true);
        let planning_visible = catalog.visible_entry_indices(&planning_config);
        assert_eq!(
            catalog.estimate_schema_tokens(&planning_visible, &planning_config),
            on_wire_schema_tokens(&catalog, planning_config),
            "planning task_tracker token estimate should match the emitted schema",
        );
    }

    #[test]
    fn task_tracker_schema_keeps_max_output_tokens_in_standard_and_planning_modes() {
        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![
            registration(tools::TASK_TRACKER)
                .with_description("Track plan tasks")
                .with_parameter_schema(empty_object_schema()),
        ]);
        let base_config = SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        )
        .with_tool_profile(ToolProfile::AdvancedVtCode);

        for planning_active in [false, true] {
            let function = catalog
                .model_tools(base_config.clone().with_planning_active(planning_active))
                .into_iter()
                .find_map(|tool| (tool.function_name() == tools::TASK_TRACKER).then_some(tool.function).flatten())
                .unwrap_or_else(|| panic!("missing task_tracker function for planning_active={planning_active}"));

            assert_eq!(
                function.parameters["properties"]["max_output_tokens"]["default"],
                json!(vtcode_utility_tool_specs::DEFAULT_MAX_OUTPUT_TOKENS),
                "task_tracker schema must keep max_output_tokens when planning_active={planning_active}",
            );
        }
    }

    /// Build a simulated MCP tool registration for `server`/`tool` with a
    /// realistic one-line description and a two-parameter schema, so the eager
    /// schema tax is pronounced enough to assert on.
    fn mcp_server_tool_registration(server: &str, tool: &str, description: &str) -> ToolRegistration {
        let name: &'static str = Box::leak(format!("mcp::{server}::{tool}").into_boxed_str());
        let alias = format!("mcp__{server}__{tool}");
        registration(name)
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description(description.to_string())
            .with_parameter_schema(json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The lookup query." },
                    "limit": { "type": "integer", "description": "Max results to return." }
                },
                "required": ["query"]
            }))
            .with_aliases([alias])
    }

    /// Phase 1.1 / 2.x regression: with several MCP servers attached, the
    /// client-local deferred-loading path must keep the first-request wire
    /// payload near the no-MCP baseline (because MCP schemas are omitted),
    /// while eager exposure would balloon it. This proves the win the
    /// `DIRECT_TOOL_EXPOSURE_THRESHOLD` / `has_mcp_tools` gating delivers.
    #[test]
    fn mcp_deferral_keeps_first_request_wire_payload_near_baseline() {
        let core_registrations = || {
            vec![
                registration(tools::EXEC_COMMAND)
                    .with_description("Run a shell command in the workspace sandbox.")
                    .with_parameter_schema(empty_object_schema()),
                registration(tools::APPLY_PATCH)
                    .with_llm_visibility(false)
                    .with_description("Apply a structured patch to files.")
                    .with_parameter_schema(apply_patch_parameters())
                    .with_behavior(ToolBehavior::apply_patch(ToolMutationModel::Mutating, false, true)),
                registration(tools::MCP_SEARCH_TOOLS)
                    .with_description("Search across deferred MCP tools by keyword.")
                    .with_parameter_schema(empty_object_schema()),
                registration(tools::CODE_SEARCH)
                    .with_description("Search the codebase symbol index.")
                    .with_parameter_schema(empty_object_schema()),
            ]
        };

        // Five simulated MCP servers, four tools each = 20 deferred MCP tools.
        let mut registrations = core_registrations();
        for server in ["context7", "filesystem", "github", "slack", "postgres"] {
            for index in 0..4 {
                let tool = format!("op_{index}");
                let description =
                    format!("{server} operation {index}: query and mutate {server} resources with paging and filters.");
                registrations.push(mcp_server_tool_registration(server, &tool, &description));
            }
        }
        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);

        let make_config = |policy: DeferredToolPolicy| {
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(policy)
        };

        // Eager: no deferral, so all 20 MCP schemas travel on the first request.
        let eager_tokens = on_wire_schema_tokens(&catalog, make_config(DeferredToolPolicy::default()));
        // Client-local deferral: MCP tools are omitted from the wire payload.
        let deferred_tokens =
            on_wire_schema_tokens(&catalog, make_config(DeferredToolPolicy::client_local(Vec::new())));
        // Baseline: the same core tools, no MCP servers, under deferral.
        let baseline_catalog = SessionToolCatalog::rebuild_from_registrations(core_registrations());
        let baseline_tokens =
            on_wire_schema_tokens(&baseline_catalog, make_config(DeferredToolPolicy::client_local(Vec::new())));

        assert!(
            eager_tokens >= deferred_tokens * 3,
            "eager payload ({eager_tokens}) must be at least 3x the deferred payload \
             ({deferred_tokens}) -- otherwise the MCP schema tax deferral removes is not real"
        );
        assert!(
            deferred_tokens <= baseline_tokens * 5 / 4,
            "with 5 MCP servers under deferral, the first-request payload ({deferred_tokens}) \
             must stay within 25% of the no-MCP baseline ({baseline_tokens})"
        );
        assert!(
            deferred_tokens < eager_tokens,
            "deferral must shrink the first-request wire payload: \
             eager={eager_tokens} deferred={deferred_tokens}"
        );
    }

    /// Phase 7.2: the advisory warning condition for "deferred loading is
    /// disabled but the catalog would benefit from it" is pure logic in
    /// `catalog_would_benefit_from_deferral`. A disabled policy + a catalog
    /// that would defer (MCP present, over the count threshold, or over the
    /// schema-token budget) is the only non-noisy warning case -- the count/
    /// budget thresholds *triggering* deferral when enabled is correct
    /// behavior, not a warning condition.
    #[test]
    fn catalog_would_benefit_from_deferral_detects_each_trigger() {
        // Small builtin-only catalog: no benefit (deferral would not engage).
        assert!(
            !catalog_would_benefit_from_deferral(false, 3, 500),
            "a small builtin-only catalog does not benefit from deferral"
        );
        // Any MCP tool present -> benefit (MCP schemas are the dominant cost).
        assert!(catalog_would_benefit_from_deferral(true, 1, 100), "any MCP tool means deferral would engage");
        // At the count threshold -> benefit.
        assert!(
            catalog_would_benefit_from_deferral(false, DIRECT_TOOL_EXPOSURE_THRESHOLD, 500),
            "meeting the count threshold means deferral would engage"
        );
        // Just under the count threshold but over the token budget -> benefit
        // (the single-large-server backstop).
        assert!(
            catalog_would_benefit_from_deferral(
                false,
                DIRECT_TOOL_EXPOSURE_THRESHOLD - 1,
                DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET + 1,
            ),
            "exceeding the schema-token budget means deferral would engage"
        );
        // Exactly at the token budget (<=, not >) and under the count threshold,
        // no MCP -> no benefit (boundary matches the `<=` in `model_tools`).
        assert!(
            !catalog_would_benefit_from_deferral(
                false,
                DIRECT_TOOL_EXPOSURE_THRESHOLD - 1,
                DIRECT_TOOL_EXPOSURE_TOKEN_BUDGET,
            ),
            "at exactly the token budget and below the count threshold, \
            deferral does not engage (boundary is <=)"
        );
    }

    #[test]
    fn planning_read_tools_stay_core_with_mcp_deferral() {
        let registrations = [
            tools::EXEC_COMMAND,
            tools::READ_FILE,
            tools::LIST_FILES,
            tools::GREP_FILE,
            tools::CODE_SEARCH,
            tools::REQUEST_USER_INPUT,
        ]
        .into_iter()
        .map(|name| {
            registration(name)
                .with_description("planning read tool")
                .with_parameter_schema(empty_object_schema())
        })
        .chain(std::iter::once(
            registration("mcp::context7::search")
                .with_catalog_source(ToolCatalogSource::Mcp)
                .with_llm_visibility(false)
                .with_description("search docs")
                .with_parameter_schema(empty_object_schema())
                .with_aliases(["mcp__context7__search"]),
        ))
        .collect::<Vec<_>>();
        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let planning_config = SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        )
        .with_planning_active(true)
        .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new()));
        let planning_definitions = catalog.model_tools(planning_config);
        for tool in [
            tools::READ_FILE,
            tools::LIST_FILES,
            tools::GREP_FILE,
            tools::CODE_SEARCH,
        ] {
            let definition = planning_definitions
                .iter()
                .find(|definition| definition.function_name() == tool)
                .unwrap_or_else(|| panic!("missing planning definition for {tool}"));
            assert_eq!(definition.defer_loading, None, "{tool} must stay on wire in planning even with MCP deferral");
        }

        let exec_config = SessionToolsConfig::full_public(
            SessionSurface::AgentRunner,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        )
        .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new()));
        let grep_entry = catalog
            .entries()
            .iter()
            .find(|entry| entry.public_name == tools::GREP_FILE)
            .expect("missing grep catalog entry");
        assert!(should_defer_tool_loading(grep_entry, &exec_config), "grep_file stays deferrable outside planning");

        let interactive_planning_config = SessionToolsConfig::full_public(
            SessionSurface::Interactive,
            CapabilityLevel::CodeSearch,
            ToolDocumentationMode::Full,
            ToolModelCapabilities::default(),
        )
        .with_planning_active(true)
        .with_deferred_tool_policy(DeferredToolPolicy::client_local(Vec::new()));
        let interactive_definitions = catalog.model_tools(interactive_planning_config);
        for tool in [tools::GREP_FILE, tools::CODE_SEARCH] {
            let definition = interactive_definitions
                .iter()
                .find(|definition| definition.function_name() == tool)
                .unwrap_or_else(|| panic!("missing interactive planning definition for {tool}"));
            assert_eq!(
                definition.defer_loading, None,
                "{tool} must stay on wire for Interactive planning with MCP deferral (session-20260923)"
            );
        }
    }

    #[test]
    fn openai_policy_injects_tool_search_for_large_catalogs() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let mut registrations = vec![exec_command, mcp_tool];
        for index in 0..DIRECT_TOOL_EXPOSURE_THRESHOLD {
            let name: &'static str = Box::leak(format!("mcp::context7::resolve_{index}").into_boxed_str());
            let alias = format!("mcp__context7__resolve_{index}");
            registrations.push(
                registration(name)
                    .with_catalog_source(ToolCatalogSource::Mcp)
                    .with_llm_visibility(false)
                    .with_description(format!("resolve docs {index}"))
                    .with_parameter_schema(empty_object_schema())
                    .with_aliases([alias]),
            );
        }

        let catalog = SessionToolCatalog::rebuild_from_registrations(registrations);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities { supports_apply_patch_tool: true },
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::openai_hosted(vec!["mcp__context7__search".to_string()])),
        );

        assert!(
            definitions.iter().any(|tool| tool.tool_type == "tool_search"),
            "openai hosted tool search should be injected when deferred tools exist"
        );
        let mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(mcp_tool.defer_loading, None);

        let deferred_mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__resolve_0")
            .expect("deferred mcp tool should be present");
        assert_eq!(deferred_mcp_tool.defer_loading, Some(true));
    }

    #[test]
    fn openai_policy_deferred_for_small_mcp_catalog() {
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);
        let second_mcp_tool = registration("mcp::context7::resolve")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("resolve docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__resolve"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![mcp_tool, second_mcp_tool]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::openai_hosted(vec!["mcp__context7__search".to_string()])),
        );

        assert!(
            definitions.iter().any(|tool| tool.tool_type == "tool_search"),
            "MCP presence should trigger tool search even for a small catalog"
        );
        let mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(mcp_tool.defer_loading, None, "always-available tool stays eager");

        let direct_mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__resolve")
            .expect("deferred mcp tool should be present");
        assert_eq!(direct_mcp_tool.defer_loading, Some(true), "non-always-available MCP tool should be deferred");
    }

    #[test]
    fn always_available_tools_match_registration_names_and_aliases() {
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);
        let dynamic_tool = registration("dynamic_skill_tool")
            .with_description("dynamic skill tool")
            .with_parameter_schema(empty_object_schema());

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![mcp_tool, dynamic_tool]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode)
            .with_deferred_tool_policy(DeferredToolPolicy::openai_hosted(vec![
                "mcp::context7::search".to_string(),
                "dynamic_skill_tool".to_string(),
            ])),
        );

        let mcp_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "mcp__context7__search")
            .expect("mcp tool should be present");
        assert_eq!(mcp_tool.defer_loading, None);

        let dynamic_tool = definitions
            .iter()
            .find(|tool| tool.function_name() == "dynamic_skill_tool")
            .expect("dynamic tool should be present");
        assert_eq!(dynamic_tool.defer_loading, None);
    }

    #[test]
    fn unsupported_providers_keep_catalog_eager() {
        let exec_command = registration(tools::EXEC_COMMAND)
            .with_description("Run command")
            .with_parameter_schema(empty_object_schema());
        let mcp_tool = registration("mcp::context7::search")
            .with_catalog_source(ToolCatalogSource::Mcp)
            .with_llm_visibility(false)
            .with_description("search docs")
            .with_parameter_schema(empty_object_schema())
            .with_aliases(["mcp__context7__search"]);

        let catalog = SessionToolCatalog::rebuild_from_registrations(vec![exec_command, mcp_tool]);
        let definitions = catalog.model_tools(
            SessionToolsConfig::full_public(
                SessionSurface::Interactive,
                CapabilityLevel::CodeSearch,
                ToolDocumentationMode::Full,
                ToolModelCapabilities::default(),
            )
            .with_tool_profile(ToolProfile::AdvancedVtCode),
        );

        assert!(!definitions.iter().any(|tool| tool.is_tool_search()));
        assert!(
            definitions.iter().all(|tool| tool.defer_loading.is_none()),
            "unsupported providers should keep the eager catalog"
        );
    }

    #[test]
    fn deferred_tool_policy_uses_provider_defaults() {
        let config = VTCodeConfig::default();

        let anthropic = deferred_tool_policy_for_runtime(Some(Provider::Anthropic), false, Some(&config));
        assert!(anthropic.is_enabled());
        assert_eq!(
            anthropic.tool_search_definition().map(|tool| tool.tool_type),
            Some("tool_search_tool_regex_20251119".to_string())
        );

        let openai = deferred_tool_policy_for_runtime(Some(Provider::OpenAI), true, Some(&config));
        assert!(openai.is_enabled());
        assert_eq!(openai.tool_search_definition().map(|tool| tool.tool_type), Some("tool_search".to_string()));

        // OpenAI without Responses compaction, and no explicit provider-hosted
        // tool search, falls through to client-local deferral now that
        // `client_tool_search` defaults to `true`.
        let unsupported = deferred_tool_policy_for_runtime(Some(Provider::OpenAI), false, Some(&config));
        assert!(unsupported.is_enabled());
        assert!(unsupported.is_client_local());
    }

    #[test]
    fn client_local_policy_selected_when_flag_enabled_for_unsupported_provider() {
        let mut config = VTCodeConfig::default();
        config.tools.client_tool_search = true;

        let gemini = deferred_tool_policy_for_runtime(Some(Provider::Gemini), false, Some(&config));
        assert!(gemini.is_enabled());
        assert!(gemini.is_client_local());
        assert_eq!(gemini.tool_search_definition(), None);

        // No provider inferred (e.g. unknown/custom model) is also covered
        // by the fallthrough arm.
        let no_provider = deferred_tool_policy_for_runtime(None, false, Some(&config));
        assert!(no_provider.is_enabled());
        assert!(no_provider.is_client_local());
    }

    #[test]
    fn client_local_policy_not_selected_when_flag_disabled() {
        let mut config = VTCodeConfig::default();
        // Default is enabled; explicitly disable it to test the fallback path.
        config.tools.client_tool_search = false;
        assert!(!config.tools.client_tool_search);

        let gemini = deferred_tool_policy_for_runtime(Some(Provider::Gemini), false, Some(&config));
        assert!(!gemini.is_enabled());
        assert!(!gemini.is_client_local());

        let no_config = deferred_tool_policy_for_runtime(Some(Provider::Gemini), false, None);
        assert!(!no_config.is_enabled());
        assert!(!no_config.is_client_local());
    }

    #[test]
    fn anthropic_native_memory_runtime_flag_tracks_provider_and_config() {
        let mut config = VTCodeConfig::default();
        config.provider.anthropic.memory.enabled = true;

        assert!(anthropic_native_memory_enabled_for_runtime(
            Some(Provider::Anthropic),
            "claude-sonnet-5",
            Some(&config),
        ));
        assert!(!anthropic_native_memory_enabled_for_runtime(
            Some(Provider::OpenAI),
            "claude-sonnet-5",
            Some(&config),
        ));
        assert!(!anthropic_native_memory_enabled_for_runtime(Some(Provider::Anthropic), "gpt-5", Some(&config),));
        assert!(anthropic_native_memory_enabled_for_runtime(
            Some(Provider::Anthropic),
            "my-private-claude-build",
            Some(&config),
        ));
    }
}
