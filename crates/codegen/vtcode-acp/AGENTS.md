<!-- Compact maintainer rules retain the repository instruction line budget. -->
<!-- markdownlint-disable MD013 -->
# vtcode-acp

[Root AGENTS.md](../../../AGENTS.md) | Agent Client Protocol (Zed integration). Canonical ACP entrypoint.

## Modules

`capabilities/` protocol negotiation | `client/` legacy client (deprecated) | `client_v2/` current ACP client | `discovery/` agent registry | `session/` session lifecycle | `transport/` StdioTransport | `jsonrpc/` JSON-RPC types | `tooling/` tool adapters | `tooling_provider.rs` registry→provider `ToolDefinition` bridge | `zed/` Zed-specific adapter | `workspace/` workspace helpers | `permissions/` permission flow | `reports/` reporting | `error/` AcpError

`zed/agent/handlers.rs` is the canonical SACP handler wiring; `zed/connection.rs` wraps the SACP `ConnectionTo<Client>` handle.

## Rules

- `AcpClientV2` is the current API. `AcpClient` is deprecated (legacy since 0.60.x). `StandardAcpAdapter` / `ZedAcpAdapter` in `zed/` bridge protocol to Zed. `register_acp_connection()` is a global `OnceLock<Arc<ConnectionHandle>>` — call once from the host protocol after the SACP `connect_with` closure receives the `cx`. `acp` module in `vtcode-core` is the compatibility facade; canonical code lives here. ACP 1.0.1 uses SACP builder + handlers, not the old `impl acp::Agent` trait. `handlers.rs` registers SACP request/notification handlers around `ZedAgent`. `ZedAgent` is `Send + Sync` (`Arc<Mutex<_>>` + `AtomicBool`) so it can be moved into SACP `cx.spawn` tasks. Tool execution RPCs (`fs/read_text_file`, `terminal/create`, `session/request_permission`) must be called from inside a `cx.spawn(...)` task — invoking them directly from an SACP request handler deadlocks the dispatch loop.
- `ZedAgent` carries the configured `AuthCredentialsStoreMode`; provider-key resolution must use that mode rather than the platform default.

## Gotchas

- `PROTOCOL_VERSION` + `SUPPORTED_VERSIONS` control negotiation — update both when protocol changes.
- `messages.rs` types are deprecated — use `jsonrpc/` module instead.
- `ConnectionHandle` wraps `agent_client_protocol::ConnectionTo<Client>`. The `block_task()` future returned by `cx.send_request(...).block_task()` is **only safe in a `cx.spawn` task**; calling it from a request handler deadlocks.
- The `acp` module re-exports `agent_client_protocol::schema::v1::*` plus `ProtocolVersion` from `schema::*`. `Client` and `Agent` (role structs) are at the crate root.
- `SessionUpdateNotification` decodes through a direct wire shape for SSE performance; new update variants must update that shape and its regression tests.
- Permission-flow tests use the duplex SACP connection harness; preserve allow, deny, cancel, unknown-option, and request-failure coverage. Stdio EOF, timeout, and cancellation paths must clear pending calls; frames and stderr remain bounded and sanitized.
