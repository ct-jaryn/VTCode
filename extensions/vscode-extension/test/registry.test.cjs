const { test, beforeEach } = require("node:test");
const assert = require("node:assert/strict");
const compileModules = require("./helpers/compileModules.cjs");
const { load, vscode } = compileModules(["commandRegistry", "types/command"]);
const { CommandRegistry } = load("commandRegistry");
const { BaseCommand } = load("types/command");

beforeEach(() => {
    vscode.testState.handlers.clear();
    vscode.workspace.isTrusted = true;
    vscode.window.activeTextEditor = undefined;
    vscode.window.activeTerminal = undefined;
});

test("registered commands share the supplied channel and read fresh host context", async (t) => {
    const output = { appendLine() {}, dispose() {} };
    const dispose = t.mock.method(output, "dispose");
    const create = t.mock.method(vscode.window, "createOutputChannel");
    const registry = new CommandRegistry(output);
    const admission = [];
    const executions = [];
    registry.register({
        id: "fixture.context", title: "Context fixture",
        canExecute(context) { admission.push(context); return true; },
        async execute(context) { executions.push(context); },
    });
    const first = { document: { uri: "/first.rs" }, selection: { start: 2 } };
    const second = { document: { uri: "/second.rs" }, selection: { start: 7 } };
    const terminal = { name: "second terminal" };
    vscode.window.activeTextEditor = first;
    await vscode.testState.handlers.get("fixture.context")();
    vscode.window.activeTextEditor = second;
    vscode.window.activeTerminal = terminal;
    await vscode.testState.handlers.get("fixture.context")();

    assert.equal(create.mock.callCount(), 0);
    assert.equal(executions.length, 2);
    assert.equal(admission[0], executions[0]);
    assert.equal(admission[1], executions[1]);
    assert.notEqual(executions[0], executions[1]);
    assert.equal(executions[0].output, output);
    assert.equal(executions[1].output, output);
    assert.equal(executions[0].activeTextEditor, first);
    assert.equal(executions[0].selection, first.selection);
    assert.equal(executions[0].terminal, undefined);
    assert.equal(executions[1].activeTextEditor, second);
    assert.equal(executions[1].selection, second.selection);
    assert.equal(executions[1].terminal, terminal);
    registry.dispose();
    assert.equal(dispose.mock.callCount(), 0, "activation owns the supplied channel");
    assert.equal(vscode.testState.handlers.has("fixture.context"), false);
});

test("no-argument registry creates one fallback channel and disposes its own channel", async (t) => {
    const output = { appendLine() {}, dispose() {} };
    const dispose = t.mock.method(output, "dispose");
    const create = t.mock.method(vscode.window, "createOutputChannel", () => output);
    const registry = new CommandRegistry();
    const channels = [];
    registry.register({
        id: "fixture.fallback", title: "Fallback fixture",
        canExecute() { return true; },
        async execute(context) { channels.push(context.output); },
    });
    await vscode.testState.handlers.get("fixture.fallback")();
    await vscode.testState.handlers.get("fixture.fallback")();
    registry.clear();
    assert.equal(dispose.mock.callCount(), 0, "clear does not end registry ownership");
    registry.dispose();
    assert.equal(create.mock.callCount(), 1);
    assert.deepEqual(create.mock.calls[0].arguments, ["VT Code"]);
    assert.deepEqual(channels, [output, output]);
    assert.equal(dispose.mock.callCount(), 1);
});

test("registered command logs failed snapshot refreshes to the supplied channel", async (t) => {
    const lines = [];
    const registry = new CommandRegistry({ appendLine: (line) => lines.push(line) });
    t.after(() => registry.dispose());
    class SnapshotCommand extends BaseCommand {
        id = "fixture.snapshot";
        title = "Snapshot fixture";
        async execute(context) { await this.flushIdeContextSnapshot(context); }
    }
    registry.register(new SnapshotCommand());
    vscode.testState.handlers.set("vtcode.flushIdeContextSnapshot", () => false);
    await vscode.testState.handlers.get("fixture.snapshot")();
    assert.deepEqual(lines, ["[warn] IDE context snapshot is unavailable; continuing without supplemental context."]);
});
