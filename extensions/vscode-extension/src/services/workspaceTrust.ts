import * as vscode from "vscode";

/** Request manual trust management through stable APIs; the host owns approval. */
export async function requestWorkspaceTrust(
    message: string,
    prompt: "information" | "warning"
): Promise<boolean> {
    if (vscode.workspace.isTrusted) {
        return true;
    }

    const manageTrust = "Manage Workspace Trust";
    const selection = prompt === "warning"
        ? await vscode.window.showWarningMessage(message, manageTrust)
        : await vscode.window.showInformationMessage(message, manageTrust);

    if (selection === manageTrust) {
        await vscode.commands.executeCommand("workbench.action.manageTrust");
    }

    // Opening the settings page is not approval. Re-read the host's trust state.
    return vscode.workspace.isTrusted;
}
