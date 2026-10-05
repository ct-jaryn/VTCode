# Tool execution pipeline ownership

The registry keeps one execution path. These private modules divide ownership:

| Module | Responsibility |
| --- | --- |
| `execution_facade.rs` | Public entrypoints, routing, preflight, execution policy, handler execution, settlement, and history |
| `execution_attempts.rs` | Structured-request safety admission, error interpretation, retry scheduling, attempt counts, and outcome metadata |
| `execution_kernel.rs` | Shared argument normalization, dispatch authority, and preflight validation |
| `execution_stages.rs` | Canonical/display names, execution argument preparation, and base route metadata |
| `reentrancy.rs` | Task/thread recursion frames and drop cleanup |

`execute_public_tool_request` delegates directly to the request-attempt lifecycle.
The prepared-request wrapper constructs the policy snapshot and requires fresh
safety admission. Preflight validation and safety admission are separate flags;
neither grants operator approval for unsandboxed shell execution.

The request lifecycle rejects approval-required shell requests before attempting
execution. Each remaining attempt checks safety unless admission was explicitly
prevalidated, then dispatches through the existing harness route. Dispatch retains
the supplied execution settlement mode.

Safety denials, structured error outputs, and dispatch errors use one retry step.
It preserves category-specific recovery guidance and retry hints. Success reports
the actual attempt count and the last recovered category; terminal failure keeps
the final error context. The facade retains handler history and timeout metadata.

Argument preparation normalizes through the existing kernel, then classifies
verification commands and resolves their preview budgets before stripping
metadata for legacy handlers. Metadata-aware handlers retain borrowed arguments
when unchanged and own normalized payloads when conversion is needed. This stage
does not grant execution admission.

Name resolution runs before the canonical hot-cache lookup. Base route resolution
stays after policy constraints; it supplies registered-tool and canonical MCP
metadata. Awaited legacy MCP discovery, unknown-tool diagnostics, circuit-breaker
history, and PTY acquisition remain in the facade. Base route metadata alone does
not establish remote availability or approval.

The [reentrancy guide](tool-reentrancy.md) describes task isolation and cleanup.
Keep extraction call sites in their original order; new modules must not bypass
preflight, safety, policy, snapshot, or settlement behavior.

Run the request lifecycle tests and registry regressions with:

```sh
cargo nextest run --locked -p vtcode-core -E 'test(execution_attempts) | test(tools::registry)'
```
