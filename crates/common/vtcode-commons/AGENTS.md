# vtcode-commons

[Root AGENTS.md](../AGENTS.md) | Shared traits and utilities. Zero business logic — pure infrastructure.

## Module Groups

- Traits: `paths/`, `errors/`, `telemetry/`.
- Display: `ansi/`, `colors/`, `styling/`, `color256_theme/`, `color_policy/`; LLM: `llm/`; `diff/` and `diff_preview/` are one-release `vtcode-diff` compatibility re-exports.
- Filesystem: `fs/`, `paths/`, `vtcode_paths/`, `diff_paths/`, `vtcodegitignore/`, `workspace_snapshot/`; text: `tokens/`, `unicode/`, `sanitizer/`, `slug/`, `formatting/`.
- Async: `async_utils/`, `task_guard/`, `thread_safety/`, `runtime_diagnostics/`; interjection: `interjection/`; UI protocol: `ui_protocol/` (including global activity state); other: `editor/`, `http/`, `project/`, `validation/`, `serde_helpers/`, `env_lock/`.
## Rules
- Re-export key types from `lib.rs`: `WorkspacePaths`, `TelemetrySink`, `ErrorFormatter`, `BackendKind`, etc.
- `reference.rs` provides in-memory test adapters: `StaticWorkspacePaths`, `MemoryTelemetry`, `MemoryErrorReporter`.
- `ui_protocol/` is a submodule, not a flat module.
- `diff_theme` is the shared diff palette and capability boundary: color-capable consumers layer row and intraline backgrounds, while `NO_COLOR` resolves to `Ansi16` so fallbacks stay foreground-only.

## Gotchas

- `paths` has two containment tiers: lexical `ensure_path_within_workspace` and async symlink-resolving `ensure_path_within_workspace_resolved`; `workspace_relative_display` resolves existing candidates before lexical fallback so symlink escapes remain external. Downstream crates delegate here — do not fork the logic.
- `vtcode_paths::VtCodePaths` owns immutable global XDG/native resolution; `vtcode_paths_migration::LegacyMigrator` owns retryable legacy scanning. Keep workspace-local `.vtcode` paths in `paths` consumers; migration copies only regular files, never follows links, and reports per-item conflicts/failures. Preserve pre-XDG DotManager cache/state mappings, installer backoff cache names, canonical-over-legacy precedence, and `with_private_file_lock` for cross-process cache read-modify-write operations when extending migration.
- `retry` owns the canonical `RetryPolicy` (delay math, jitter, `RetryDecision`/`RetryStep`, `simple()` constructor). vtcode-core only layers domain adapters on top.
- `error_category/` classifies LLM errors for retry — `is_retryable_llm_error_message()` is the key function; `classify_anyhow_error` → `ErrorCategory` is the single classifier for tool errors. `is_context_capacity_error` remains a separate, specific provider-request signal for bounded context recovery. `misconfiguration/` is the fail-first settings/config check before retry.
- `errors/` provides `MultiErrors<E>` — a reusable error collection type implementing the "error parameter" pattern for continuing work while collecting failures. Use it instead of ad-hoc `Vec<String>` or `Vec<ErrorEnum>` for batch/parallel operations where individual items can fail independently.
- `env_lock/` is macOS-specific env mutex — used by `vtcode` binary, not by library crates; `startup_trace/` is an opt-in pre-tracing phase recorder whose `record_duration` calls must stay silent unless `VTCODE_STARTUP_TRACE=1`; `sanitizer::StreamingSecretRedactor` carries a bounded suffix across pipe/PTY chunks, so use it for streamed spool writes. `runtime_diagnostics/` exposes only the *stable* `RuntimeMetrics` subset (`num_workers`, `num_alive_tasks`, `global_queue_depth`, `worker_total_busy_duration`); blocking-pool depth, per-worker queue/steal counts, and poll-time histograms need `--cfg tokio_unstable`. `VTCODE_RUNTIME_METRICS=1` enables boot + 60s `DEBUG` snapshots; `configured_worker_threads()` reads `VTCODE_RUNTIME_WORKERS`. `vtcodegitignore`'s global is an `ArcSwap` — keep whole-value swap semantics, not per-entry mutation.
- `utils/` contains `calculate_sha256()` used by `vtcode-indexer`.
- `VtCodePaths::open_private_append_file` opens a symlink-safe `0600` read/write append handle for private logs that also need seek/read access.
- `formatting/` owns the canonical middle-truncation helpers `truncate_middle` (head+tail, control chars sanitized) and `truncate_path_middle` (separator-aware, for path display), plus `lowercase_leading_word` for splicing a reason into a sentence (skips acronyms and "I"). Downstream crates delegate here — do not re-implement per crate.
- `ui_protocol::SessionSurface` defaults to `Inline`; callers requiring alternate-screen detection must request `Auto` or `Alternate` explicitly. New diff consumers use `vtcode-diff` directly; its computation preserves CR, CRLF, LF, hunk numbering, and byte-safe intraline ranges.
- `ui_protocol::tool_summary` contains renderer-independent compact activity metadata; keep grouping/output boundaries independent of TUI/runtime types and out of `ThreadEvent`. `MessageMetadata.intent_id` is optional wire metadata for durable steering recovery; preserve it through message serialization.
- `task_guard::TaskGuard` is the canonical abort-on-drop task owner — use it instead of adding per-crate guard structs; `disarm()` releases the handle for documented-detached handoff.
- `llm::Usage::billable_totals()` is the canonical aggregation boundary for prompt/completion/reasoning/cache usage; preserve raw provider counters separately from billable totals.
