import { spawn, type SpawnOptionsWithoutStdio } from "node:child_process";
import * as vscode from "vscode";

export interface VtcodeProcessOptions {
    readonly title?: string;
    readonly showProgress?: boolean;
    readonly onStdout?: (text: string) => void;
    readonly onStderr?: (text: string) => void;
    readonly cancellationToken?: vscode.CancellationToken;
}

/** Callers own admission and live config/context preparation before execution. */
export async function executeVtcodeProcess(
    commandPath: string,
    finalArgs: string[],
    channel: vscode.OutputChannel,
    getSpawnOptions: () => SpawnOptionsWithoutStdio,
    options: VtcodeProcessOptions = {}
): Promise<void> {
    const runCommand = async () =>
        new Promise<void>((resolve, reject) => {
            const child = spawn(
                commandPath,
                finalArgs,
                getSpawnOptions()
            );

            let cancellationRegistration: vscode.Disposable | undefined;
            let cancelled = false;
            if (options.cancellationToken) {
                cancellationRegistration =
                    options.cancellationToken.onCancellationRequested(() => {
                        cancelled = true;
                        if (!child.killed) {
                            child.kill();
                        }
                    });
            }

            child.stdout.on("data", (data: Buffer) => {
                const text = data.toString();
                channel.append(text);
                options.onStdout?.(text);
            });

            child.stderr.on("data", (data: Buffer) => {
                const text = data.toString();
                channel.append(text);
                options.onStderr?.(text);
            });

            child.on("error", (error: Error) => {
                cancellationRegistration?.dispose();
                reject(error);
            });

            child.on("close", (code) => {
                cancellationRegistration?.dispose();
                if (cancelled) {
                    reject(new vscode.CancellationError());
                    return;
                }

                if (code === 0) {
                    resolve();
                } else {
                    reject(
                        new Error(
                            `VT Code exited with code ${code ?? "unknown"}`
                        )
                    );
                }
            });
        });

    if (options.showProgress === false) {
        await runCommand();
        return;
    }

    await vscode.window.withProgress(
        {
            location: vscode.ProgressLocation.Notification,
            title: options.title ?? "Running VT Code…",
        },
        runCommand
    );
}
