<!-- Compact maintainer rules retain the repository instruction line budget. -->
<!-- markdownlint-disable MD013 -->
# vtcode-utility-tool-specs

[Root AGENTS.md](../../../AGENTS.md) | Passive JSON schemas for VT Code utility, file, scheduling, and collaboration tool surfaces. Defines the wire format for tool invocations and results.

## Conventions

- Schemas are defined using `serde` derive macros. Every schema type must derive `Serialize` and `Deserialize`. This crate is a leaf dependency -- do not add dependencies on other vtcode workspace crates (except `vtcode-commons` if needed). Schema types are passive data containers with no behavior or validation logic.
- Uses `rmcp` for MCP schema compatibility.
- Use `with_max_output_tokens_parameter` when exposing object-shaped function schemas to models; execution validation belongs in `vtcode-core`.

## Module Groups

| Area | Modules |
| --- | --- |
| Schemas | `collaboration/`, `json_schema/`, `responses_api/`, `mcp_tool/` |
| Taxonomy | `tool_kind/` (`ToolKind`, `ToolNamespace`, `CanonicalToolMeta`, `TokenBucket`) |

## Dependencies

- `rmcp` (MCP schema types)
- `serde` / `serde_json` (serialization)

`write_stdin` keeps legacy `session_id`/`chars` calls valid; explicit `action: "wait"` is the bounded long-command wait form. `exec_command.background` is opt-in and defaults to false.
