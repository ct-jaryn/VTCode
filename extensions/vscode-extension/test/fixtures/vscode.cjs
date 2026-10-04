class TreeItem {
    constructor(label, collapsibleState) {
        this.label = label;
        this.collapsibleState = collapsibleState;
    }
}
class ThemeIcon {
    constructor(id) { this.id = id; }
}
class MarkdownString {
    constructor(value = "") { this.value = value; }
}
class EventEmitter {
    listeners = new Set();
    event = (listener) => {
        this.listeners.add(listener);
        return { dispose: () => this.listeners.delete(listener) };
    };
    fire(value) { for (const listener of this.listeners) listener(value); }
}
class CancellationError extends Error {}
const testState = {
    handlers: new Map(), messages: [], picks: [], beforeProgress: undefined,
    channel: { show() {}, append() {}, appendLine() {} },
};
module.exports = {
    TreeItem, ThemeIcon, MarkdownString, EventEmitter,
    TreeItemCollapsibleState: { None: 0 },
    CancellationError, testState, UIKind: { Desktop: 1, Web: 2 }, env: { uiKind: 1 },
    ProgressLocation: { Notification: 15 },
    workspace: {
        isTrusted: true, workspaceFolders: [{ uri: { fsPath: "/workspace" } }],
        asRelativePath: (uri) => uri.path.replace(/^\/workspace\//, ""),
        getConfiguration: () => ({ get: (_key, fallback) => fallback }),
    },
    commands: {
        registerCommand: (name, callback) => {
            testState.handlers.set(name, callback);
            return { dispose: () => testState.handlers.delete(name) };
        },
    },
    window: {
        createOutputChannel: () => testState.channel,
        withProgress: async (_options, run) => { testState.beforeProgress?.(); return run(); },
        showWarningMessage: (text) => { testState.messages.push(text); },
        showInformationMessage: (text) => { testState.messages.push(text); },
        showQuickPick: async () => testState.picks.shift(),
    },
};
