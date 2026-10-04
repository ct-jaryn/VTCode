import type { SpawnOptionsWithoutStdio } from "node:child_process";
import * as vscode from "vscode";
import { executeVtcodeProcess, type VtcodeProcessOptions } from "../services/processExecution";

export interface RunVtcodeCommandOptions extends VtcodeProcessOptions {
    readonly revealOutput?: boolean;
    readonly output?: vscode.OutputChannel;
}

export interface VtcodeTaskDefinition extends vscode.TaskDefinition {
    type: "vtcode";
    command: "task-tracker";
    summary?: string;
    steps?: string[];
    label?: string;
}

export function getConfiguredCommandPath(): string {
    return (
        vscode.workspace
            .getConfiguration("vtcode")
            .get<string>("commandPath", "vtcode")
            .trim() || "vtcode"
    );
}

export function getWorkspaceRoot(): string | undefined {
    const activeEditor = vscode.window.activeTextEditor;
    if (activeEditor) {
        const workspaceFolder = vscode.workspace.getWorkspaceFolder(
            activeEditor.document.uri
        );
        return workspaceFolder?.uri.fsPath;
    }

    const [firstWorkspace] = vscode.workspace.workspaceFolders ?? [];
    return firstWorkspace?.uri.fsPath;
}

export function getConfigArguments(configUri?: vscode.Uri): string[] {
    if (!configUri) {
        return [];
    }
    return ["--config", configUri.fsPath];
}

export function createSpawnOptions(
    overrides: Partial<SpawnOptionsWithoutStdio> = {},
    environmentProvider?: () => Record<string, string>
): SpawnOptionsWithoutStdio {
    const { env: overrideEnv, ...rest } = overrides;
    const baseEnv = { ...process.env } as NodeJS.ProcessEnv;

    if (environmentProvider) {
        try {
            const overlay = environmentProvider();
            for (const [key, value] of Object.entries(overlay)) {
                baseEnv[key] = value;
            }
        } catch {
            // Error handling will be done by caller
        }
    }

    return {
        env: { ...baseEnv, ...(overrideEnv || {}) },
        ...rest,
    };
}

export function formatArgsForLogging(args: string[]): string {
    return args
        .map((arg) => {
            const value = String(arg);
            return /(\s|"|')/.test(value) ? JSON.stringify(value) : value;
        })
        .join(" ");
}

export async function runVtcodeCommand(
    args: string[],
    options: RunVtcodeCommandOptions = {}
): Promise<void> {
    if (vscode.env.uiKind === vscode.UIKind.Web) {
        throw new Error(
            "VT Code commands that spawn the CLI are not available in the web extension host."
        );
    }

    if (!vscode.workspace.isTrusted) {
        throw new Error(
            "Trust this workspace to run VT Code CLI commands from VS Code."
        );
    }

    const commandPath = getConfiguredCommandPath();
    const cwd = getWorkspaceRoot();
    if (!cwd) {
        throw new Error(
            "Open a workspace folder before running VT Code commands."
        );
    }

    const output =
        options.output || vscode.window.createOutputChannel("VT Code");

    if (options.cancellationToken?.isCancellationRequested) {
        throw new vscode.CancellationError();
    }

    const configArgs = getConfigArguments();
    const normalizedArgs = args.map((arg) => String(arg));
    const finalArgs = [...configArgs, ...normalizedArgs];
    const displayArgs = formatArgsForLogging(finalArgs);
    const revealOutput = options.revealOutput ?? true;

    if (revealOutput) {
        output.show(true);
    }
    output.appendLine(`$ ${commandPath} ${displayArgs}`);

    await executeVtcodeProcess(
        commandPath, finalArgs, output, () => createSpawnOptions({ cwd }), options
    );
}
