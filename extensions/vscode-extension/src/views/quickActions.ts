import * as vscode from "vscode";
import type { VtcodeConfigSummary } from "../vtcodeConfig";

export interface QuickActionDescription {
    readonly label: string;
    readonly description: string;
    readonly command: string;
    readonly icon?: string;
    readonly args?: unknown[];
}

class QuickActionTreeItem extends vscode.TreeItem {
    constructor(public readonly action: QuickActionDescription) {
        super(action.label, vscode.TreeItemCollapsibleState.None);
        this.description = action.description;
        this.iconPath = new vscode.ThemeIcon(action.icon ?? "rocket");
        this.command = {
            command: action.command,
            title: action.label,
            arguments: action.args,
        };
        this.contextValue = "vtcodeQuickAction";
    }
}

export class QuickActionTreeDataProvider
    implements vscode.TreeDataProvider<QuickActionTreeItem>
{
    private readonly onDidChangeTreeDataEmitter =
        new vscode.EventEmitter<void>();
    readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

    constructor(private readonly getActions: () => QuickActionDescription[]) {}

    getTreeItem(element: QuickActionTreeItem): vscode.TreeItem {
        return element;
    }

    getChildren(): vscode.ProviderResult<QuickActionTreeItem[]> {
        return this.getActions().map(
            (action) => new QuickActionTreeItem(action)
        );
    }

    refresh(): void {
        this.onDidChangeTreeDataEmitter.fire();
    }
}

export function createQuickActions(
    cliAvailableState: boolean,
    summary: VtcodeConfigSummary | undefined,
    trusted: boolean
): QuickActionDescription[] {
    const actions: QuickActionDescription[] = [];

    if (!trusted) {
        actions.push(
            {
                label: "Trust this workspace for VT Code",
                description:
                    "Grant workspace trust to enable VT Code automation and CLI access.",
                command: "vtcode.trustWorkspace",
                icon: "shield",
            },
            {
                label: "Verify workspace trust flow",
                description:
                    "Run the VT Code trust checklist so chat prompts stop requesting trust while tools stay gated.",
                command: "vtcode.verifyWorkspaceTrust",
                icon: "shield-check",
            },
            {
                label: "Review VT Code CLI requirements",
                description:
                    "Learn how the VT Code CLI integrates once the workspace is trusted.",
                command: "vtcode.openInstallGuide",
                icon: "tools",
            }
        );
    }

    if (trusted && cliAvailableState) {
        actions.push(
            {
                label: "Verify workspace trust flow",
                description:
                    "Confirm VT Code chat prompts avoid the trust modal while tool executions still require approval.",
                command: "vtcode.verifyWorkspaceTrust",
                icon: "shield-check",
            },
            {
                label: "Refresh IDE context snapshot",
                description:
                    "Force a new IDE context snapshot so the agent sees your latest editor state.",
                command: "vtcode.flushIdeContextSnapshot",
                icon: "history",
            },
            {
                label: "Ask the VT Code agent…",
                description:
                    "Send a one-off question and stream the answer in VS Code.",
                command: "vtcode.askAgent",
                icon: "comment-discussion",
            },
            {
                label: "Ask about highlighted selection",
                description:
                    "Right-click or trigger VT Code to explain the selected text.",
                command: "vtcode.askSelection",
                icon: "comment",
            },
            {
                label: "Run VT Code task tracker",
                description:
                    "Run the predefined VS Code task that drives the task_tracker tool.",
                command: "vtcode.runTaskTrackerTask",
                icon: "checklist",
            },
            {
                label: "Launch interactive VT Code terminal",
                description:
                    "Open an integrated terminal session running vtcode chat.",
                command: "vtcode.launchAgentTerminal",
                icon: "terminal",
            },
            {
                label: "Analyze workspace with VT Code",
                description:
                    "Run vtcode analyze and stream the report to the VT Code output channel.",
                command: "vtcode.runAnalyze",
                icon: "pulse",
            }
        );
    } else if (trusted) {
        actions.push({
            label: "Review VT Code CLI installation",
            description:
                "Open the VT Code CLI installation instructions required for automation.",
            command: "vtcode.openInstallGuide",
            icon: "tools",
        });
    }

    if (trusted && summary?.hasConfig) {
        if (summary.automationFullAutoEnabled === true) {
            actions.push({
                label: "Full-auto automation detected (blocked)",
                description:
                    "Open vtcode.toml to disable [automation.full_auto]; VS Code will not run autonomous tasks.",
                command: "vtcode.openConfig",
                icon: "shield-off",
            });
        }

        const hitlEnabled = summary.humanInTheLoop !== false;
        actions.push({
            label: hitlEnabled
                ? "Disable human-in-the-loop safeguards"
                : "Enable human-in-the-loop safeguards",
            description: hitlEnabled
                ? "Allow VT Code to automate tool execution without manual approval."
                : "Require confirmation before VT Code executes high-impact tools.",
            command: "vtcode.toggleHumanInTheLoop",
            icon: "shield",
        });

        const providerCount = summary.mcpProviders.length;
        const enabledCount = summary.mcpProviders.filter(
            (provider) => provider.enabled !== false
        ).length;
        actions.push({
            label:
                providerCount > 0
                    ? "Manage MCP providers"
                    : "Configure MCP providers",
            description:
                providerCount > 0
                    ? `Adjust ${enabledCount}/${providerCount} enabled Model Context Protocol providers.`
                    : "Connect VT Code to external Model Context Protocol tools.",
            command: "vtcode.configureMcpProviders",
            icon: "plug",
        });

        const toolPoliciesCount = summary.toolPoliciesCount ?? 0;
        actions.push({
            label: "Review tool policy configuration",
            description:
                toolPoliciesCount > 0
                    ? `Inspect ${toolPoliciesCount} explicit tool policy overrides.`
                    : "Define allow/prompt/deny rules for VT Code tools.",
            command: "vtcode.openToolsPolicyConfig",
            icon: "law",
        });
    }

    const toolGuideDescription =
        summary?.hasConfig && trusted
            ? "Read documentation covering VT Code tool governance and HITL flows."
            : "Learn how VT Code enforces tool governance and human-in-the-loop safeguards.";
    actions.push({
        label: "Open VT Code tool policy guide",
        description: toolGuideDescription,
        command: "vtcode.openToolsPolicyGuide",
        icon: "book",
    });

    const configDescription = summary?.uri
        ? `Open ${vscode.workspace.asRelativePath(
              summary.uri,
              false
          )} to adjust VT Code settings.`
        : "Jump directly to the workspace VT Code configuration file.";

    actions.push(
        {
            label: "Open vtcode.toml",
            description: configDescription,
            command: "vtcode.openConfig",
            icon: "gear",
        },
        {
            label: "View VT Code documentation",
            description: "Open the VT Code README in your browser.",
            command: "vtcode.openDocumentation",
            icon: "book",
        },
        {
            label: "Review VT Code DeepWiki overview",
            description: "Open the DeepWiki page for VT Code capabilities.",
            command: "vtcode.openDeepWiki",
            icon: "globe",
        },
        {
            label: "Explore the VT Code walkthrough",
            description:
                "Open the getting-started walkthrough to learn about VT Code features.",
            command: "vtcode.openWalkthrough",
            icon: "rocket",
        }
    );

    return actions;
}
