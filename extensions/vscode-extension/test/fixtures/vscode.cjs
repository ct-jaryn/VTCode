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
module.exports = {
    TreeItem, ThemeIcon, MarkdownString, EventEmitter,
    TreeItemCollapsibleState: { None: 0 },
    workspace: { asRelativePath: (uri) => uri.path.replace(/^\/workspace\//, "") },
};
