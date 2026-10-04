import * as vscode from "vscode";
import type { VtcodeConfigSummary } from "../vtcodeConfig";

export interface WorkspaceInsightDescription {
    readonly label: string;
    readonly description: string;
    readonly icon: string;
    readonly command?: vscode.Command;
    readonly tooltip?: string | vscode.MarkdownString;
}

class WorkspaceInsightTreeItem extends vscode.TreeItem {
    constructor(public readonly insight: WorkspaceInsightDescription) {
        super(insight.label, vscode.TreeItemCollapsibleState.None);
        this.description = insight.description;
        this.iconPath = new vscode.ThemeIcon(insight.icon);
        this.command = insight.command;
        if (insight.tooltip) {
            this.tooltip = insight.tooltip;
        }
        this.contextValue = "vtcodeWorkspaceInsight";
    }
}

export class WorkspaceInsightsTreeDataProvider
    implements vscode.TreeDataProvider<WorkspaceInsightTreeItem>
{
    private readonly onDidChangeTreeDataEmitter =
        new vscode.EventEmitter<void>();
    readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

    constructor(
        private readonly getInsights: () => WorkspaceInsightDescription[]
    ) {}

    getTreeItem(element: WorkspaceInsightTreeItem): vscode.TreeItem {
        return element;
    }

    getChildren(): vscode.ProviderResult<WorkspaceInsightTreeItem[]> {
        return this.getInsights().map(
            (insight) => new WorkspaceInsightTreeItem(insight)
        );
    }

    refresh(): void {
        this.onDidChangeTreeDataEmitter.fire();
    }
}

interface WorkspaceInsightServices {
    readonly getConfiguredCommandPath: () => string;
    readonly createStatusBarTooltip: (
        commandPath: string,
        available: boolean,
        trusted: boolean
    ) => vscode.MarkdownString;
}

export function createWorkspaceInsights(
    trusted: boolean,
    cliAvailableState: boolean,
    summary: VtcodeConfigSummary | undefined,
    services: WorkspaceInsightServices
): WorkspaceInsightDescription[] {
    const { getConfiguredCommandPath, createStatusBarTooltip } = services;
    const insights: WorkspaceInsightDescription[] = [];

    insights.push({
        label: trusted ? "Workspace trust granted" : "Workspace trust required",
        description: trusted
            ? "VT Code can run CLI automation in this workspace."
            : "Grant trust to enable VT Code CLI commands and automation features.",
        icon: trusted ? "shield" : "shield-off",
        command: trusted
            ? undefined
            : {
                  command: "vtcode.trustWorkspace",
                  title: "Trust Workspace for VT Code",
              },
        tooltip: trusted
            ? "Workspace trust allows VT Code to spawn CLI processes."
            : "Security-sensitive features are disabled until this workspace is trusted.",
    });

    if (!trusted) {
        insights.push({
            label: "CLI access blocked",
            description:
                "Trust the workspace to allow VT Code to detect and launch the CLI.",
            icon: "circle-slash",
            command: {
                command: "vtcode.openInstallGuide",
                title: "Review CLI Installation",
            },
        });
    } else {
        const commandPath = getConfiguredCommandPath();
        insights.push({
            label: cliAvailableState
                ? "VT Code CLI detected"
                : "VT Code CLI unavailable",
            description: cliAvailableState
                ? `Using ${commandPath}`
                : `Check ${commandPath} or adjust vtcode.commandPath`,
            icon: cliAvailableState ? "check" : "warning",
            command: cliAvailableState
                ? {
                      command: "vtcode.openQuickActions",
                      title: "Open Quick Actions",
                  }
                : {
                      command: "vtcode.openInstallGuide",
                      title: "Open Installation Guide",
                  },
            tooltip: createStatusBarTooltip(
                commandPath,
                cliAvailableState,
                trusted
            ),
        });
    }

    if (summary?.hasConfig) {
        const configPath = summary.uri
            ? vscode.workspace.asRelativePath(summary.uri, false)
            : "vtcode.toml";
        insights.push({
            label: "VT Code configuration detected",
            description: configPath,
            icon: "gear",
            command: {
                command: "vtcode.openConfig",
                title: "Open vtcode.toml",
            },
        });

        if (summary.agentProvider) {
            const provider = summary.agentProvider;
            const defaultModel = summary.agentDefaultModel;
            const providerLower = provider.toLowerCase();
            const modelLower = defaultModel?.toLowerCase() ?? "";
            const mismatch =
                (providerLower === "ollama" &&
                    (defaultModel?.includes(":") ?? false)) ||
                (providerLower !== "openrouter" &&
                    modelLower.startsWith("gpt-oss:"));

            insights.push({
                label: `Agent provider: ${provider}`,
                description: defaultModel
                    ? `Default model: ${defaultModel}`
                    : "No default model configured",
                icon: mismatch ? "alert" : "globe",
                command: mismatch
                    ? {
                          command: "vtcode.openConfig",
                          title: "Review agent provider configuration",
                      }
                    : undefined,
                tooltip: mismatch
                    ? "Provider and default_model may require different credentials. Update vtcode.toml to avoid CLI failures."
                    : undefined,
            });
        }

        const fullAutoEnabled = summary.automationFullAutoEnabled === true;
        const allowedTools = summary.automationFullAutoAllowedTools;
        const automationDescription = fullAutoEnabled
            ? allowedTools && allowedTools.length > 0
                ? `Allowed tools: ${allowedTools.join(
                      ", "
                  )}. VS Code blocks autonomous execution; disable automation.full_auto to avoid warnings.`
                : "automation.full_auto is enabled. VS Code blocks autonomous execution; disable the setting to silence this warning."
            : "automation.full_auto is disabled. VT Code prompts require explicit approval.";
        insights.push({
            label: fullAutoEnabled
                ? "Full-auto automation detected (blocked)"
                : "Full-auto automation disabled",
            description: automationDescription,
            icon: fullAutoEnabled ? "shield-off" : "shield",
            command: fullAutoEnabled
                ? {
                      command: "vtcode.openConfig",
                      title: "Disable automation.full_auto",
                  }
                : undefined,
        });

        const hitlStatus =
            summary.humanInTheLoop === false
                ? "Disabled (manual approvals required)"
                : "Enabled";
        insights.push({
            label: "Human-in-the-loop safeguards",
            description: hitlStatus,
            icon: summary.humanInTheLoop === false ? "person" : "shield",
            command:
                trusted && summary.uri
                    ? {
                          command: "vtcode.toggleHumanInTheLoop",
                          title: "Toggle human-in-the-loop safeguards",
                      }
                    : undefined,
        });

        const providerCount = summary.mcpProviders.length;
        const enabledCount = summary.mcpProviders.filter(
            (provider) => provider.enabled !== false
        ).length;
        insights.push({
            label: "MCP providers",
            description:
                providerCount > 0
                    ? `${enabledCount}/${providerCount} enabled`
                    : "No providers configured",
            icon: "plug",
            command:
                trusted && summary.uri
                    ? {
                          command: "vtcode.configureMcpProviders",
                          title: "Configure MCP providers",
                      }
                    : undefined,
        });

        const toolPoliciesCount = summary.toolPoliciesCount ?? 0;
        const toolPolicyLabel = summary.toolDefaultPolicy
            ? `Default: ${summary.toolDefaultPolicy}`
            : "No default policy set";
        insights.push({
            label: "Tool policy coverage",
            description:
                toolPoliciesCount > 0
                    ? `${toolPoliciesCount} overrides · ${toolPolicyLabel}`
                    : `No overrides · ${toolPolicyLabel}`,
            icon: "law",
            command: {
                command: "vtcode.openToolsPolicyConfig",
                title: "Review tool policy configuration",
            },
        });

        if (summary.parseError) {
            insights.push({
                label: "Configuration parsing error",
                description: summary.parseError,
                icon: "error",
                command: summary.uri
                    ? {
                          command: "vtcode.openConfig",
                          title: "Open vtcode.toml",
                      }
                    : undefined,
            });
        }
    } else {
        insights.push({
            label: "No vtcode.toml detected",
            description:
                "Use VT Code: Open Configuration to create a workspace configuration.",
            icon: "file",
            command: {
                command: "vtcode.openConfig",
                title: "Create vtcode.toml",
            },
        });
    }

    return insights;
}
