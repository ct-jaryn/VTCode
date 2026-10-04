import * as vscode from "vscode";
import {
    appendMcpProvider, loadConfigSummaryFromUri, pickVtcodeConfigUri,
    revealMcpSection, revealToolsPolicySection, setHumanInTheLoop, setMcpProviderEnabled,
    type VtcodeConfigSummary,
} from "../vtcodeConfig";

interface ConfigurationCommandServices {
    readonly getCurrentConfigSummary: () => VtcodeConfigSummary | undefined;
    readonly ensureWorkspaceTrustedForCommand: (action: string) => Promise<boolean>;
    readonly getOutputChannel: () => vscode.OutputChannel;
    readonly handleCommandError: (contextLabel: string, error: unknown) => void;
    readonly openToolsPolicyGuide: () => Promise<void>;
    readonly openMcpGuide: () => Promise<void>;
}

export function registerConfigurationCommands(services: ConfigurationCommandServices): vscode.Disposable[] {
    const {
        getCurrentConfigSummary, ensureWorkspaceTrustedForCommand,
        getOutputChannel, handleCommandError, openToolsPolicyGuide, openMcpGuide,
    } = services;

    const toggleHumanInTheLoopCommand = vscode.commands.registerCommand(
        "vtcode.toggleHumanInTheLoop",
        async () => {
            if (
                !(await ensureWorkspaceTrustedForCommand(
                    "change VT Code human-in-the-loop settings"
                ))
            ) {
                return;
            }

            try {
                const configUri = await pickVtcodeConfigUri(
                    getCurrentConfigSummary()?.uri
                );
                if (!configUri) {
                    void vscode.window.showWarningMessage(
                        "No vtcode.toml file was found in this workspace."
                    );
                    return;
                }

                const currentConfigSummary = getCurrentConfigSummary();
                const activeSummary =
                    currentConfigSummary &&
                    currentConfigSummary.uri?.toString() ===
                        configUri.toString()
                        ? currentConfigSummary
                        : await loadConfigSummaryFromUri(configUri);

                const newValue = activeSummary.humanInTheLoop === false;
                const updated = await setHumanInTheLoop(configUri, newValue);
                if (!updated) {
                    void vscode.window.showWarningMessage(
                        "Failed to update human_in_the_loop in vtcode.toml."
                    );
                    return;
                }

                const relativePath = vscode.workspace.asRelativePath(
                    configUri,
                    false
                );
                const channel = getOutputChannel();
                channel.appendLine(
                    `[info] human_in_the_loop set to ${newValue} in ${relativePath}.`
                );
                void vscode.window.showInformationMessage(
                    `Human-in-the-loop safeguards are now ${
                        newValue ? "enabled" : "disabled"
                    } in vtcode.toml.`
                );
            } catch (error) {
                handleCommandError("toggle human-in-the-loop mode", error);
            }
        }
    );

    const openToolsPolicyGuideCommand = vscode.commands.registerCommand(
        "vtcode.openToolsPolicyGuide",
        async () => {
            try {
                await openToolsPolicyGuide();
            } catch (error) {
                handleCommandError("open tool policy guide", error);
            }
        }
    );

    const openToolsPolicyConfigCommand = vscode.commands.registerCommand(
        "vtcode.openToolsPolicyConfig",
        async () => {
            try {
                const configUri = await pickVtcodeConfigUri(
                    getCurrentConfigSummary()?.uri
                );
                if (!configUri) {
                    void vscode.window.showWarningMessage(
                        "No vtcode.toml file was found in this workspace."
                    );
                    return;
                }

                await revealToolsPolicySection(configUri);
            } catch (error) {
                handleCommandError("open tool policy configuration", error);
            }
        }
    );

    const configureMcpProvidersCommand = vscode.commands.registerCommand(
        "vtcode.configureMcpProviders",
        async () => {
            if (
                !(await ensureWorkspaceTrustedForCommand(
                    "edit VT Code MCP provider settings"
                ))
            ) {
                return;
            }

            try {
                const configUri = await pickVtcodeConfigUri(
                    getCurrentConfigSummary()?.uri
                );
                if (!configUri) {
                    void vscode.window.showWarningMessage(
                        "No vtcode.toml file was found in this workspace."
                    );
                    return;
                }

                const currentConfigSummary = getCurrentConfigSummary();
                const activeSummary =
                    currentConfigSummary &&
                    currentConfigSummary.uri?.toString() ===
                        configUri.toString()
                        ? currentConfigSummary
                        : await loadConfigSummaryFromUri(configUri);

                const providers = activeSummary.mcpProviders;
                const enabledCount = providers.filter(
                    (provider) => provider.enabled !== false
                ).length;

                const quickItems: Array<
                    vscode.QuickPickItem & {
                        action: "toggle" | "add" | "guide" | "open";
                        providerName?: string;
                    }
                > = providers.map((provider) => ({
                    label: `${
                        provider.enabled === false
                            ? "$(circle-slash)"
                            : "$(check)"
                    } ${provider.name}`,
                    description: provider.command ?? "No command configured",
                    detail:
                        provider.args && provider.args.length > 0
                            ? `Args: ${provider.args.join(" ")}`
                            : provider.enabled === false
                            ? "Provider disabled"
                            : undefined,
                    action: "toggle",
                    providerName: provider.name,
                }));

                quickItems.push(
                    {
                        label: "$(add) Add MCP provider",
                        description:
                            "Define a new Model Context Protocol provider entry.",
                        action: "add",
                    },
                    {
                        label: "$(gear) Open MCP configuration",
                        description: "Edit the MCP section in vtcode.toml.",
                        action: "open",
                    },
                    {
                        label: "$(book) Open MCP integration guide",
                        description:
                            "Read the VT Code MCP configuration walkthrough.",
                        action: "guide",
                    }
                );

                const selection = await vscode.window.showQuickPick(
                    quickItems,
                    {
                        placeHolder:
                            providers.length > 0
                                ? `Manage ${providers.length} MCP provider${
                                      providers.length === 1 ? "" : "s"
                                  } (${enabledCount} enabled)`
                                : "No MCP providers defined. Add one to enable external tools.",
                    }
                );

                if (!selection) {
                    return;
                }

                switch (selection.action) {
                    case "toggle": {
                        if (!selection.providerName) {
                            return;
                        }

                        const provider = providers.find(
                            (candidate) =>
                                candidate.name === selection.providerName
                        );
                        if (!provider) {
                            void vscode.window.showWarningMessage(
                                `Provider “${selection.providerName}” is no longer available.`
                            );
                            return;
                        }

                        const newState = provider.enabled === false;
                        const result = await setMcpProviderEnabled(
                            configUri,
                            selection.providerName,
                            newState
                        );
                        if (result === "notfound") {
                            void vscode.window.showWarningMessage(
                                `Provider “${selection.providerName}” was not found in vtcode.toml.`
                            );
                            return;
                        }

                        if (result === "updated") {
                            const channel = getOutputChannel();
                            const relativePath =
                                vscode.workspace.asRelativePath(
                                    configUri,
                                    false
                                );
                            channel.appendLine(
                                `[info] MCP provider "${selection.providerName}" enabled=${newState} in ${relativePath}.`
                            );
                            void vscode.window.showInformationMessage(
                                `MCP provider “${
                                    selection.providerName
                                }” is now ${newState ? "enabled" : "disabled"}.`
                            );
                        }
                        break;
                    }
                    case "add": {
                        const name = await vscode.window.showInputBox({
                            prompt: "Provider name",
                            ignoreFocusOut: true,
                        });

                        if (!name || !name.trim()) {
                            return;
                        }

                        if (
                            providers.some(
                                (provider) =>
                                    provider.name.toLowerCase() ===
                                    name.trim().toLowerCase()
                            )
                        ) {
                            void vscode.window.showWarningMessage(
                                `An MCP provider named “${name.trim()}” already exists.`
                            );
                            return;
                        }

                        const command = await vscode.window.showInputBox({
                            prompt: "Command used to launch the provider",
                            value: "uvx",
                            ignoreFocusOut: true,
                        });

                        if (!command || !command.trim()) {
                            return;
                        }

                        const argsInput = await vscode.window.showInputBox({
                            prompt: "Arguments (separate with spaces, leave blank for none)",
                            ignoreFocusOut: true,
                        });

                        const args = argsInput
                            ? argsInput
                                  .split(" ")
                                  .map((value) => value.trim())
                                  .filter((value) => value.length > 0)
                            : [];

                        const enableChoice = await vscode.window.showQuickPick(
                            ["Enable provider", "Keep disabled"],
                            {
                                placeHolder:
                                    "Should the provider start enabled?",
                            }
                        );

                        if (!enableChoice) {
                            return;
                        }

                        const appended = await appendMcpProvider(configUri, {
                            name: name.trim(),
                            command: command.trim(),
                            args,
                            enabled: enableChoice === "Enable provider",
                        });

                        if (appended) {
                            const channel = getOutputChannel();
                            const relativePath =
                                vscode.workspace.asRelativePath(
                                    configUri,
                                    false
                                );
                            channel.appendLine(
                                `[info] Added MCP provider "${name.trim()}" to ${relativePath}.`
                            );
                            void vscode.window.showInformationMessage(
                                `Added MCP provider “${name.trim()}” to vtcode.toml.`
                            );
                        } else {
                            void vscode.window.showWarningMessage(
                                `Provider “${name.trim()}” already exists in vtcode.toml.`
                            );
                        }
                        break;
                    }
                    case "guide": {
                        await openMcpGuide();
                        break;
                    }
                    case "open": {
                        await revealMcpSection(configUri);
                        break;
                    }
                }
            } catch (error) {
                handleCommandError("configure MCP providers", error);
            }
        }
    );

    return [
        toggleHumanInTheLoopCommand, openToolsPolicyGuideCommand,
        openToolsPolicyConfigCommand, configureMcpProvidersCommand,
    ];
}
