import * as vscode from "vscode";
import { BaseCommand, type CommandContext } from "../types/command";
import { requestWorkspaceTrust } from "../services/workspaceTrust";

/**
 * Command to trust the workspace for VT Code
 */
export class TrustWorkspaceCommand extends BaseCommand {
    public readonly id = "vtcode.trustWorkspace";
    public readonly title = "Trust Workspace";
    public readonly description =
        "Grant workspace trust to enable VT Code automation";
    public readonly icon = "shield";

    // Trust management itself must remain available in a restricted workspace.
    // eslint-disable-next-line @typescript-eslint/no-unused-vars -- Retain the BaseCommand signature.
    canExecute(_context: CommandContext): boolean {
        return true;
    }

    // eslint-disable-next-line @typescript-eslint/no-unused-vars -- Retain the BaseCommand signature.
    async execute(_context: CommandContext): Promise<void> {
        if (vscode.workspace.isTrusted) {
            void vscode.window.showInformationMessage(
                "This workspace is already trusted for VT Code automation."
            );
            return;
        }

        const trustedNow = await requestWorkspaceTrust(
            "Workspace trust is still required for VT Code. Open the trust management settings?",
            "information"
        );
        if (trustedNow) {
            void vscode.window.showInformationMessage(
                "Workspace trust granted. VT Code can now process prompts with human-in-the-loop safeguards."
            );
        }
    }
}
