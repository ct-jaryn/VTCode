#![allow(
    missing_docs,
    clippy::expect_used,
    reason = "Intentional compatibility, platform, or test-only suppression."
)]

use std::hint::black_box;
use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use criterion::criterion_group;
use criterion::criterion_main;
use criterion::{BatchSize, BenchmarkId, Criterion};
use tokio::runtime::Runtime;
use tokio::sync::RwLock;
use vtcode_core::config::types::CapabilityLevel;
use vtcode_core::config::{ToolDocumentationMode, ToolProfile};
use vtcode_core::core::agent::harness_kernel::{
    HarnessRequestPlanInput, PreparedToolBatch, PreparedToolCall, build_harness_request_plan,
};
use vtcode_core::llm::provider::{Message, ToolChoice, ToolDefinition};
use vtcode_core::prompts::{FewShotExample, FewShotStore, resolve_system_prompt_layers, sort_tool_definitions};
use vtcode_core::tools::handlers::{
    DeferredToolPolicy, SessionSurface, SessionToolCatalog, SessionToolsConfig, ToolModelCapabilities,
};
use vtcode_core::tools::registry::SessionToolCatalogState;
use vtcode_core::tools::registry::ToolRegistration;
use vtcode_indexer::file_search::{FileIndexCache, FileSearchConfig, run, run_with_index};

fn sample_tool(name: &str) -> ToolDefinition {
    ToolDefinition::function(
        name.to_string(),
        format!("Tool {name}"),
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        }),
    )
}

fn sample_tools(count: usize) -> Arc<Vec<ToolDefinition>> {
    Arc::new((0..count).map(|index| sample_tool(&format!("tool_{index}"))).collect())
}

fn sample_messages(count: usize) -> Vec<Message> {
    (0..count).map(|index| Message::user(format!("message {index}"))).collect()
}

fn benchmark_tool_catalog_registrations() -> Vec<ToolRegistration> {
    (0..128)
        .map(|index| {
            ToolRegistration::new(
                format!("catalog_projection_{index:03}"),
                CapabilityLevel::CodeSearch,
                false,
                |_, _| Box::pin(async { Ok(serde_json::Value::Null) }),
            )
            .with_description(format!(
                "Project a repeated catalog tool with a realistic description for benchmark iteration {index}."
            ))
            .with_parameter_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "query": {"type": "string", "description": "Search query."}
                }
            }))
        })
        .collect()
}

fn benchmark_tool_catalog() -> SessionToolCatalog {
    let registrations = benchmark_tool_catalog_registrations();
    SessionToolCatalog::rebuild_from_registrations(registrations)
}

fn benchmark_tool_catalog_config() -> SessionToolsConfig {
    SessionToolsConfig::full_public(
        SessionSurface::AgentRunner,
        CapabilityLevel::CodeSearch,
        ToolDocumentationMode::Progressive,
        ToolModelCapabilities::default(),
    )
    .with_tool_profile(ToolProfile::AdvancedVtCode)
}

fn request_plan_benchmark(c: &mut Criterion) {
    let tools = sample_tools(24);
    let messages = sample_messages(32);

    let _benchmark = c.bench_function("agent_harness_request_plan_with_tools", |b| {
        b.iter(|| {
            black_box(build_harness_request_plan(HarnessRequestPlanInput {
                messages: Arc::new(messages.clone()),
                system_prompt: Arc::from("System prompt\n[Runtime Context]\nturn=12"),
                tools: Some(Arc::clone(&tools)),
                model: "gpt-5".to_string(),
                max_tokens: Some(2000),
                temperature: Some(0.7),
                top_p: None,
                top_k: None,
                presence_penalty: None,
                frequency_penalty: None,
                stream: true,
                tool_choice: Some(ToolChoice::auto()),
                parallel_tool_config: None,
                reasoning_effort: None,
                verbosity: None,
                metadata: None,
                context_management: None,
                previous_response_id: Some("resp_123".to_string()),
                prompt_cache_key: Some("session:test".to_string()),
                prompt_cache_profile: None,
                tool_catalog_hash: None,
                system_prompt_prefix_hash: None,
            }))
        })
    });
}

fn prepared_batch_planning_benchmark(c: &mut Criterion) {
    let calls: Vec<PreparedToolCall> = (0..48)
        .map(|index| {
            let readonly = index % 5 != 0;
            PreparedToolCall::new(
                format!("tool_{index}"),
                readonly,
                readonly,
                serde_json::json!({ "path": format!("src/file_{index}.rs") }),
            )
        })
        .collect();

    let _benchmark = c.bench_function("agent_harness_prepared_batch_plan", |b| {
        b.iter(|| black_box(PreparedToolBatch::plan(calls.clone(), true)))
    });
}

fn tool_catalog_projection_benchmark(c: &mut Criterion) {
    let runtime = Runtime::new().expect("criterion tokio runtime");
    let state = Arc::new(SessionToolCatalogState::new());
    let tools = Arc::new(RwLock::new((*sample_tools(32)).clone()));

    runtime.block_on(async {
        let _ = state.filtered_snapshot_with_stats(&tools, true, false).await;
    });

    let _benchmark = c.bench_function("agent_harness_tool_catalog_cache_hit", |b| {
        b.iter(|| {
            let state = Arc::clone(&state);
            let tools = Arc::clone(&tools);
            runtime.block_on(async move { black_box(state.filtered_snapshot_with_stats(&tools, true, false).await) })
        })
    });

    let _benchmark = c.bench_function("agent_harness_tool_catalog_cache_miss", |b| {
        b.iter(|| {
            let state = Arc::clone(&state);
            let tools = Arc::clone(&tools);
            runtime.block_on(async move {
                let _refresh_count = state.note_explicit_refresh("benchmark");
                black_box(state.filtered_snapshot_with_stats(&tools, true, false).await)
            })
        })
    });

    let catalog = benchmark_tool_catalog();
    let catalog_config = benchmark_tool_catalog_config();
    let _ = catalog.schema_entries(catalog_config.clone());
    let _ = catalog.model_tools(catalog_config.clone());
    let _benchmark = c.bench_function("agent_harness_tool_catalog_projection_repeat", |b| {
        b.iter(|| {
            let schemas = catalog.schema_entries(catalog_config.clone());
            let definitions = catalog.model_tools(catalog_config.clone());
            black_box((schemas.len(), definitions.len()))
        })
    });
}

fn tool_catalog_deferred_policy_benchmark(c: &mut Criterion) {
    let registrations = benchmark_tool_catalog_registrations();
    let policies = [
        ("hosted", DeferredToolPolicy::openai_hosted(Vec::new())),
        ("client_local", DeferredToolPolicy::client_local(Vec::new())),
        ("disabled", DeferredToolPolicy::default()),
    ];
    let mut group = c.benchmark_group("agent_harness_tool_catalog_deferred_policy");

    for (policy_name, deferred_tool_policy) in policies {
        let config = benchmark_tool_catalog_config().with_deferred_tool_policy(deferred_tool_policy);
        let expected =
            SessionToolCatalog::rebuild_from_registrations(registrations.clone()).model_tools(config.clone());
        assert!(!expected.is_empty(), "{policy_name} catalog benchmark must emit tool definitions");
        let actual = SessionToolCatalog::rebuild_from_registrations(registrations.clone()).model_tools(config.clone());
        assert_eq!(actual, expected, "{policy_name} catalog output must remain stable");

        let _benchmark = group.bench_function(BenchmarkId::from_parameter(policy_name), |b| {
            b.iter_batched(
                || SessionToolCatalog::rebuild_from_registrations(registrations.clone()),
                |catalog| {
                    let definitions = catalog.model_tools(config.clone());
                    black_box(definitions)
                },
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

fn prompt_resource_cache_hit_benchmark(c: &mut Criterion) {
    let workspace = tempfile::tempdir().expect("benchmark workspace");
    let examples_dir = workspace.path().join(".vtcode/prompts/examples");
    std::fs::create_dir_all(&examples_dir).expect("benchmark examples directory");
    std::fs::write(
        examples_dir.join("benchmark.md"),
        "---\ntags: [benchmark, cache]\n---\nA cached prompt resource.\n",
    )
    .expect("benchmark example");

    let first = FewShotStore::load(Some(workspace.path()), None);
    assert_eq!(first.len(), 1);
    let _benchmark = c.bench_function("prompt_resource_cache_hit_few_shot", |b| {
        b.iter(|| black_box(FewShotStore::load(Some(workspace.path()), None)))
    });

    let runtime = Runtime::new().expect("criterion tokio runtime");
    let _warmup_layers = runtime.block_on(resolve_system_prompt_layers(workspace.path()));
    let _benchmark = c.bench_function("prompt_resource_cache_hit_system_layers", |b| {
        b.iter(|| runtime.block_on(async { black_box(resolve_system_prompt_layers(workspace.path()).await) }))
    });
}

fn few_shot_selection_benchmark(c: &mut Criterion) {
    let examples = (0..128)
        .map(|index| FewShotExample {
            id: format!("example-{index:03}"),
            tags: vec!["search".to_string(), format!("topic-{index}")],
            summary: "benchmark example".to_string(),
            body: "search and inspect a source file before editing it".to_string(),
            token_count: 16,
            source_path: std::path::PathBuf::from(format!("/tmp/example-{index:03}.md")),
        })
        .collect();
    let store = FewShotStore::from_examples(examples);

    let _benchmark = c.bench_function("few_shot_selection_normalized_query", |b| {
        b.iter(|| black_box(store.select("please search topic-37 before editing", 800)))
    });
}

fn tool_definition_sorting_benchmark(c: &mut Criterion) {
    let tools = (0..96)
        .rev()
        .map(|index| sample_tool(&format!("catalog_tool_{index:03}")))
        .collect::<Vec<_>>();

    let _benchmark = c.bench_function("tool_definition_sorting_catalog_refresh", |b| {
        b.iter(|| black_box(sort_tool_definitions(tools.clone())))
    });
}

fn populate_search_workspace(root: &std::path::Path, modules: usize, files_per_module: usize) {
    for module in 0..modules {
        for file in 0..files_per_module {
            let path = root.join(format!("src/module_{module:02}/widget_{file:02}.rs"));
            std::fs::create_dir_all(path.parent().expect("benchmark parent")).expect("benchmark directory");
            std::fs::write(path, "fn widget() {}\n").expect("benchmark source");
        }
    }
}

fn file_search_benchmarks(c: &mut Criterion) {
    let small_workspace = tempfile::tempdir().expect("benchmark workspace");
    populate_search_workspace(small_workspace.path(), 8, 8);
    let large_workspace = tempfile::tempdir().expect("benchmark workspace");
    populate_search_workspace(large_workspace.path(), 64, 64);

    let runtime = Runtime::new().expect("criterion tokio runtime");
    let _benchmark = c.bench_function("agent_harness_file_search_uncached_small", |b| {
        b.iter(|| {
            let result = run(indexed_search_config(small_workspace.path())).expect("uncached file search");
            black_box((result.matches.len(), result.total_match_count))
        })
    });
    let _benchmark = c.bench_function("agent_harness_file_search_uncached", |b| {
        b.iter(|| {
            let result = run(indexed_search_config(large_workspace.path())).expect("uncached file search");
            black_box((result.matches.len(), result.total_match_count))
        })
    });

    let _benchmark = c.bench_function("agent_harness_file_index_build_small", |b| {
        b.iter_batched(
            || FileIndexCache::new(small_workspace.path().to_path_buf(), Vec::new(), false, 4),
            |cache| {
                let result = runtime
                    .block_on(run_with_index(indexed_search_config(small_workspace.path()), &cache))
                    .expect("file index build");
                black_box((result.matches.len(), result.total_match_count))
            },
            BatchSize::SmallInput,
        )
    });

    let _benchmark = c.bench_function("agent_harness_file_index_build", |b| {
        b.iter_batched(
            || FileIndexCache::new(large_workspace.path().to_path_buf(), Vec::new(), false, 4),
            |cache| {
                let result = runtime
                    .block_on(run_with_index(indexed_search_config(large_workspace.path()), &cache))
                    .expect("file index build");
                black_box((result.matches.len(), result.total_match_count))
            },
            BatchSize::SmallInput,
        )
    });

    let large_cache = FileIndexCache::new(large_workspace.path().to_path_buf(), Vec::new(), false, 4);
    let _warmup_results = runtime
        .block_on(run_with_index(indexed_search_config(large_workspace.path()), &large_cache))
        .expect("warm indexed search");

    let small_cache = FileIndexCache::new(small_workspace.path().to_path_buf(), Vec::new(), false, 4);
    let _small_warmup = runtime
        .block_on(run_with_index(indexed_search_config(small_workspace.path()), &small_cache))
        .expect("warm indexed search");

    let _benchmark = c.bench_function("indexed_file_search_scoring_cache_hit_small", |b| {
        b.iter(|| {
            let result = runtime
                .block_on(run_with_index(indexed_search_config(small_workspace.path()), &small_cache))
                .expect("indexed search");
            black_box(result.matches.len())
        })
    });

    let _benchmark = c.bench_function("indexed_file_search_scoring_cache_hit", |b| {
        b.iter(|| {
            let result = runtime
                .block_on(run_with_index(indexed_search_config(large_workspace.path()), &large_cache))
                .expect("indexed search");
            black_box(result.matches.len())
        })
    });
}

fn indexed_search_config(workspace: &std::path::Path) -> FileSearchConfig {
    FileSearchConfig {
        pattern_text: "widget".to_string(),
        limit: NonZero::new(32).expect("non-zero limit"),
        search_directory: workspace.to_path_buf(),
        exclude: Vec::new(),
        threads: NonZero::new(4).expect("non-zero threads"),
        cancel_flag: Arc::new(AtomicBool::new(false)),
        compute_indices: false,
        respect_gitignore: false,
    }
}

criterion_group!(
    benches,
    request_plan_benchmark,
    prepared_batch_planning_benchmark,
    tool_catalog_projection_benchmark,
    tool_catalog_deferred_policy_benchmark,
    prompt_resource_cache_hit_benchmark,
    few_shot_selection_benchmark,
    tool_definition_sorting_benchmark,
    file_search_benchmarks
);
criterion_main!(benches);
