# Vendored: mac-notification-sys

Patched copy of [`mac-notification-sys` 0.6.15](https://github.com/h4llow3En/mac-notification-sys)
(MIT/Apache-2.0). Wired in via `[patch.crates-io]` at the workspace root.

## Why

Upstream `set_application` used `Once::call_once`, which:

1. consumed the one-shot even when the native `setApplication` failed, and
2. returned `AlreadySet` on every later call, so the failure could never be
   retried and callers could not tell success from "already set".

That made a single failed macOS notification setup disable desktop
notifications for the process lifetime.

## Patch

- Application registration state is an explicit `Unset` / `Set` / `Failed`
  mutex instead of `Once`.
- Failed setup is retryable; successful setup is idempotent (`Ok`).
- `ensure_application_set` does not fall through to AppleScript app discovery
  after an explicit failure (that path triggers Automation permission).

See `src/lib.rs` (`ApplicationState`, `set_application`,
`apply_set_application_result`).
