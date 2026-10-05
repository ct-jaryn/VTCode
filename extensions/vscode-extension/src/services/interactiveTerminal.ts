import * as vscode from "vscode";

interface TerminalServices {
    getEnvironment(): Record<string, string | undefined>;
    getConfigArguments(): string[];
    flushIdeContext(): Promise<void>;
    isWorkspaceTrusted(): boolean;
    onError(error: unknown): void;
}

interface TerminalSession {
    terminal?: vscode.Terminal;
    closeListener?: vscode.Disposable;
    launch?: Promise<vscode.Terminal | undefined>;
}

/** Owns one native CLI terminal, including an in-flight context flush. */
export class InteractiveTerminal implements vscode.Disposable {
    private session: TerminalSession | undefined;
    private disposed = false;

    constructor(private readonly services: TerminalServices) {}

    async ensure(commandPath: string, cwd: string): Promise<
        { terminal: vscode.Terminal; created: boolean } | undefined
    > {
        if (this.disposed) {
            return undefined;
        }
        const created = !this.session;
        const session = this.session ?? {};
        if (created) {
            this.session = session;
            session.launch = this.launch(session, commandPath, cwd);
        }
        const terminal = session.terminal ?? await session.launch;
        if (!terminal || this.session !== session) {
            return undefined;
        }
        return { terminal, created };
    }

    dispose(): void {
        this.disposed = true;
        const session = this.session;
        if (session) {
            this.release(session);
            session.terminal?.dispose();
        }
    }

    private release(session: TerminalSession): void {
        if (this.session !== session) {
            return;
        }
        this.session = undefined;
        session.closeListener?.dispose();
        session.closeListener = undefined;
    }

    private canLaunch(session: TerminalSession): boolean {
        return this.session === session && this.services.isWorkspaceTrusted();
    }

    private async launch(
        session: TerminalSession, commandPath: string, cwd: string
    ): Promise<vscode.Terminal | undefined> {
        try {
            if (!this.canLaunch(session)) {
                this.release(session);
                return undefined;
            }
            await this.services.flushIdeContext();
            if (!this.canLaunch(session)) {
                this.release(session);
                return undefined;
            }
            // Run the executable directly; no user-controlled value becomes shell text.
            const terminal = vscode.window.createTerminal({
                name: "VT Code Agent", cwd,
                shellPath: commandPath,
                shellArgs: ["chat", ...this.services.getConfigArguments()],
                env: this.services.getEnvironment(),
                iconPath: new vscode.ThemeIcon("comment-discussion"),
            });
            session.terminal = terminal;
            session.closeListener = vscode.window.onDidCloseTerminal((closed) => {
                if (closed === terminal) {
                    this.release(session);
                }
            });
            return terminal;
        } catch (error) {
            if (this.session === session) {
                this.release(session);
                session.terminal?.dispose();
                this.services.onError(error);
            }
            return undefined;
        }
    }
}
