import * as vscode from "vscode";

interface TerminalServices {
    getEnvironment(): Record<string, string | undefined>;
    getConfigArguments(): string[];
    flushIdeContext(): Promise<void>;
    isWorkspaceTrusted(): boolean;
    onError(error: unknown): void;
}

interface TerminalSession {
    terminal: vscode.Terminal;
    closeListener?: vscode.Disposable;
    launchTimer?: ReturnType<typeof setTimeout>;
}

/** Owns the integrated terminal and cancels delayed work when its session ends. */
export class InteractiveTerminal implements vscode.Disposable {
    private session: TerminalSession | undefined;
    private disposed = false;

    constructor(private readonly services: TerminalServices) {}

    ensure(commandPath: string, cwd: string): { terminal: vscode.Terminal; created: boolean } {
        if (this.disposed) {
            throw new Error("The VT Code terminal service has been disposed.");
        }
        if (this.session) {
            return { terminal: this.session.terminal, created: false };
        }
        const terminal = vscode.window.createTerminal({
            name: "VT Code Agent", cwd, env: this.services.getEnvironment(),
            iconPath: new vscode.ThemeIcon("comment-discussion"),
        });
        const session: TerminalSession = { terminal };
        this.session = session;
        session.closeListener = vscode.window.onDidCloseTerminal((closed) => {
            if (closed === terminal) {
                this.release(session);
            }
        });
        // Allow terminal profiles to finish automatic environment activation.
        session.launchTimer = setTimeout(() => {
            session.launchTimer = undefined;
            void this.launch(session, commandPath);
        }, 800);
        return { terminal, created: true };
    }

    dispose(): void {
        this.disposed = true;
        const session = this.session;
        if (session) {
            this.release(session);
            session.terminal.dispose();
        }
    }

    private release(session: TerminalSession): void {
        if (this.session !== session) {
            return;
        }
        this.session = undefined;
        if (session.launchTimer !== undefined) {
            clearTimeout(session.launchTimer);
            session.launchTimer = undefined;
        }
        session.closeListener?.dispose();
        session.closeListener = undefined;
    }

    private canLaunch(session: TerminalSession): boolean {
        return this.session === session && this.services.isWorkspaceTrusted();
    }

    private async launch(session: TerminalSession, commandPath: string): Promise<void> {
        try {
            if (!this.canLaunch(session)) {
                return;
            }
            await this.services.flushIdeContext();
            if (!this.canLaunch(session)) {
                return;
            }
            const quotedCommandPath = /\s/.test(commandPath)
                ? `"${commandPath.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`
                : commandPath;
            const configArgs = this.services.getConfigArguments();
            const terminalArgs = ["chat", ...configArgs];
            const argsText = formatArgsForShell(terminalArgs);
            const commandText =
                argsText.length > 0
                    ? `${quotedCommandPath} ${argsText}`
                    : quotedCommandPath;
            session.terminal.sendText(commandText, true);
        } catch (error) {
            if (this.session === session) {
                this.services.onError(error);
            }
        }
    }
}

function formatArgsForShell(args: string[]): string {
    return args
        .map((arg) => {
            const value = String(arg);
            return quoteForShell(value);
        })
        .filter((value) => value.length > 0)
        .join(" ");
}

function quoteForShell(value: string): string {
    if (!/[\s"'\\$`]/.test(value)) {
        return value;
    }

    return `"${value.replace(/(["\\$`])/g, "\\$1")}"`;
}
