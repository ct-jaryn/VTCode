<!-- markdownlint-disable MD013 -->
# VS Code companion

[Root guidance](../../AGENTS.md) | TypeScript extension host and CLI integration.

## Module ownership

- `extension.ts` owns activation, CLI availability, config updates, and disposal.
- `services/workspaceTrust.ts` owns stable trust dialogs; only host trust grants execution. Trust management stays callable in restricted workspaces; never access the `workspaceTrust` proposal.
- `commands/configurationCommands.ts` registers HITL/MCP/policy callbacks using live summary and trust services.
- `services/interactiveTerminal.ts` owns native CLI terminal reuse, pending context flush, close listeners, and disposal.
- `services/processExecution.ts` owns spawn/stream/progress/cancel/finish; callers retain admission and prepare config/env.
- `utils/vtcodeRunner.ts` owns modular-command preflight and shared executable/config-argument/logging helpers.
- `views/quickActions.ts` owns quick-action descriptions and their tree adapter; keep command IDs and ordering stable.
- `views/workspaceInsights.ts` owns status trees; inject path/tooltip getters and read them only when trusted.
- Both tree providers read current state through callbacks; do not cache trust or config snapshots in constructors.
- Keep modular commands in `commands/`, the registry in `commandRegistry.ts`, and TOML parsing/editing in `vtcodeConfig.ts`. The registry shares activation's output channel and disposes only its own fallback; `utils/manifestContributions.ts` validates tool/participant contribution shapes.

## Verification and boundaries

- `npm run bundle -- --production` is the real build. `npm run typecheck` checks the shipped entrypoint graph; `npm test` runs all `test/*.test.cjs`. Compile/lint scripts still print skip messages.
- Run `npm run typecheck:views` and `npm run test:views` for the extracted views.
- Run `npm run typecheck:services` and `npm run test:services` for process/config/terminal/trust services.
- View lint: `ESLINT_USE_FLAT_CONFIG=false ./node_modules/.bin/eslint src/views/*.ts` (existing legacy ESLint config).
- The shipped entrypoint typecheck passes; full-project typecheck and entrypoint lint retain diagnostics outside this increment. Report these boundaries; do not claim a full-source pass.
- Node tests compile real modules with local VS Code/config fixtures and mock process spawning;
  they do not verify rendering, full activation, filesystem config edits, or terminal profiles in a real host.
- Preserve workspace-trust gates, manual approvals, and the extension's full-auto execution block.
- Keep process launches and config mutations out of view providers; command handlers enforce the execution boundary.
