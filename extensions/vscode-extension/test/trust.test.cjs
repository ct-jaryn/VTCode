const { test, beforeEach, afterEach } = require("node:test");
const assert = require("node:assert/strict");
const compileModules = require("./helpers/compileModules.cjs");
const { load, vscode } = compileModules([
    "services/workspaceTrust", "commands/trustWorkspaceCommand", "types/command", "commandRegistry",
]);
const { requestWorkspaceTrust } = load("services/workspaceTrust");
const { TrustWorkspaceCommand } = load("commands/trustWorkspaceCommand");
const { CommandRegistry } = load("commandRegistry");
const { BaseCommand } = load("types/command");
let proposalReads;

beforeEach(() => {
    vscode.workspace.isTrusted = false;
    vscode.testState.handlers.clear();
    vscode.testState.messages.length = 0;
    proposalReads = 0;
    Object.defineProperty(vscode.workspace, "requestWorkspaceTrust", {
        configurable: true,
        get() {
            proposalReads += 1;
            throw new Error("workspaceTrust API proposal is unavailable");
        },
    });
});
afterEach(() => {
    assert.equal(proposalReads, 0, "stable trust flows must never access the proposed API");
    delete vscode.workspace.requestWorkspaceTrust;
});

test("already trusted workspaces return without prompting or managing trust", async (t) => {
    vscode.workspace.isTrusted = true;
    const warning = t.mock.method(vscode.window, "showWarningMessage");
    const info = t.mock.method(vscode.window, "showInformationMessage");
    const manage = t.mock.method(vscode.commands, "executeCommand");
    assert.equal(await requestWorkspaceTrust("unused", "warning"), true);
    assert.equal(warning.mock.callCount(), 0);
    assert.equal(info.mock.callCount(), 0);
    assert.equal(manage.mock.callCount(), 0);
});

test("warning flow uses stable management and accepts only the host's granted state", async (t) => {
    const prompts = [];
    const commands = [];
    t.mock.method(vscode.window, "showWarningMessage", async (...args) => {
        prompts.push(args);
        return "Manage Workspace Trust";
    });
    t.mock.method(vscode.commands, "executeCommand", async (...args) => {
        commands.push(args);
        vscode.workspace.isTrusted = true;
    });
    const message = "Trust required to run a literal ; $(action).";
    assert.equal(await requestWorkspaceTrust(message, "warning"), true);
    assert.deepEqual(prompts, [[message, "Manage Workspace Trust"]]);
    assert.deepEqual(commands, [["workbench.action.manageTrust"]]);
});

test("opening management without granting trust remains denied", async (t) => {
    t.mock.method(vscode.window, "showInformationMessage", async () => "Manage Workspace Trust");
    const manage = t.mock.method(vscode.commands, "executeCommand", async () => true);
    assert.equal(await requestWorkspaceTrust("Review trust", "information"), false);
    assert.equal(manage.mock.callCount(), 1);
    assert.equal(vscode.workspace.isTrusted, false);
});

test("dismissed prompts neither open management nor grant trust", async (t) => {
    t.mock.method(vscode.window, "showInformationMessage", async () => undefined);
    const manage = t.mock.method(vscode.commands, "executeCommand");
    assert.equal(await requestWorkspaceTrust("Review trust", "information"), false);
    assert.equal(manage.mock.callCount(), 0);
});

test("trust is re-read after awaiting management rather than cached before it", async (t) => {
    let finish;
    t.mock.method(vscode.window, "showWarningMessage", async () => "Manage Workspace Trust");
    t.mock.method(vscode.commands, "executeCommand", () => new Promise((resolve) => { finish = resolve; }));
    const pending = requestWorkspaceTrust("Trust before execution", "warning");
    await Promise.resolve();
    assert.equal(typeof finish, "function");
    assert.equal(vscode.workspace.isTrusted, false);
    vscode.workspace.isTrusted = true;
    finish();
    assert.equal(await pending, true);
});

test("prompt and management failures propagate with their original identity", async (t) => {
    const promptFailure = new Error("Prompt unavailable");
    const manageFailure = new Error("Management unavailable");
    const prompt = t.mock.method(vscode.window, "showWarningMessage", async () => { throw promptFailure; });
    const manage = t.mock.method(vscode.commands, "executeCommand", async () => { throw manageFailure; });
    await assert.rejects(requestWorkspaceTrust("Trust required", "warning"), (error) => error === promptFailure);
    assert.equal(manage.mock.callCount(), 0);
    prompt.mock.mockImplementation(async () => "Manage Workspace Trust");
    await assert.rejects(requestWorkspaceTrust("Trust required", "warning"), (error) => error === manageFailure);
    assert.equal(vscode.workspace.isTrusted, false);
});

test("registered trust command is available while execution commands stay blocked", async (t) => {
    let runs = 0;
    class ExecutionCommand extends BaseCommand {
        id = "fixture.execute";
        title = "Execution fixture";
        async execute() { runs += 1; }
    }
    t.mock.method(vscode.window, "showInformationMessage", async (message) => {
        vscode.testState.messages.push(message);
        return "Manage Workspace Trust";
    });
    const management = [];
    vscode.testState.handlers.set("workbench.action.manageTrust", () => {
        management.push("opened");
        vscode.workspace.isTrusted = true;
    });
    const registry = new CommandRegistry();
    t.after(() => registry.dispose());
    registry.registerAll([new TrustWorkspaceCommand(), new ExecutionCommand()]);
    await vscode.testState.handlers.get("fixture.execute")();
    assert.equal(runs, 0);
    await vscode.testState.handlers.get("vtcode.trustWorkspace")();
    assert.deepEqual(management, ["opened"]);
    assert.equal(vscode.workspace.isTrusted, true);
    assert.match(vscode.testState.messages.at(-1), /Workspace trust granted/);
    assert.equal(runs, 0);
});

test("registered trust command does not claim a grant after dismissal", async (t) => {
    const registry = new CommandRegistry();
    t.after(() => registry.dispose());
    registry.register(new TrustWorkspaceCommand());
    await vscode.testState.handlers.get("vtcode.trustWorkspace")();
    assert.equal(vscode.workspace.isTrusted, false);
    assert.equal(vscode.testState.messages.length, 1);
    assert.match(vscode.testState.messages[0], /Workspace trust is still required/);
    assert.doesNotMatch(vscode.testState.messages[0], /granted/);
});
