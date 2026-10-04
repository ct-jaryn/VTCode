# VS Code companion

[Root guidance](../../AGENTS.md) | TypeScript extension host and CLI integration.

## Module ownership

- `extension.ts` owns activation, workspace trust, CLI availability, config updates, terminal launch, and disposal.
- `views/quickActions.ts` owns quick-action descriptions and their tree adapter; keep command IDs and ordering stable.
- `views/workspaceInsights.ts` owns workspace-status descriptions and their tree adapter;
  inject executable-path and shared-tooltip services, and read them only in the existing trusted branch.
- Both tree providers read current state through callbacks; do not cache trust or config snapshots in constructors.
- Keep modular commands in `commands/`, the registry in `commandRegistry.ts`, and TOML parsing/editing in `vtcodeConfig.ts`.

## Verification and boundaries

- `npm run bundle -- --production` is the real build. Compile/typecheck/lint/test scripts currently print skip messages.
- Run `npm run typecheck:views` and `npm run test:views` for the extracted views.
- View lint: `ESLINT_USE_FLAT_CONFIG=false ./node_modules/.bin/eslint src/views/*.ts` (existing legacy ESLint config).
- Direct entry-point and full-project typechecks have existing diagnostics; compare against baseline and report limits.
- Node view tests use a local VS Code fixture. They do not verify rendering, activation, or terminal lifecycle in a host.
- Preserve workspace-trust gates, manual approvals, and the extension's full-auto execution block.
- Keep process launches and config mutations out of view providers; command handlers enforce the execution boundary.
