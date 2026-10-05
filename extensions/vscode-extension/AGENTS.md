# VS Code companion

[Root guidance](../../AGENTS.md) | TypeScript extension host and CLI integration.

## Module ownership

- `extension.ts` owns activation, workspace trust, CLI availability, config updates, and disposal.
- `commands/configurationCommands.ts` registers HITL/MCP/policy callbacks using live summary and trust services.
- `services/interactiveTerminal.ts` owns terminal reuse, delayed launch, close listeners, and cancellation.
- `services/processExecution.ts` owns shared spawning, streaming, progress, cancellation, and completion;
  callers retain admission checks and supply their own config/context/environment preparation.
- `utils/vtcodeRunner.ts` owns modular-command preflight and shared executable/config-argument/logging helpers.
- `views/quickActions.ts` owns quick-action descriptions and their tree adapter; keep command IDs and ordering stable.
- `views/workspaceInsights.ts` owns workspace-status descriptions and their tree adapter;
  inject executable-path and shared-tooltip services, and read them only in the existing trusted branch.
- Both tree providers read current state through callbacks; do not cache trust or config snapshots in constructors.
- Keep modular commands in `commands/`, the registry in `commandRegistry.ts`, and TOML parsing/editing in `vtcodeConfig.ts`.

## Verification and boundaries

- `npm run bundle -- --production` is the real build. Compile/typecheck/lint/test scripts currently print skip messages.
- Run `npm run typecheck:views` and `npm run test:views` for the extracted views.
- Run `npm run typecheck:services` and `npm run test:services` for process/config/terminal services.
- View lint: `ESLINT_USE_FLAT_CONFIG=false ./node_modules/.bin/eslint src/views/*.ts` (existing legacy ESLint config).
- Direct entry-point and full-project typechecks have existing diagnostics; compare against baseline and report limits.
- Node tests compile real modules with local VS Code/config fixtures and mock process spawning;
  they do not verify rendering, full activation, filesystem config edits, or terminal profiles in a real host.
- Preserve workspace-trust gates, manual approvals, and the extension's full-auto execution block.
- Keep process launches and config mutations out of view providers; command handlers enforce the execution boundary.
