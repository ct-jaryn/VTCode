const { test, beforeEach } = require("node:test");
const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const childProcess = require("node:child_process");
const compileModules = require("./helpers/compileModules.cjs");
const { load, vscode } = compileModules([
    "services/processExecution", "utils/vtcodeRunner", "commands/configurationCommands",
], { vtcodeConfig: "vtcodeConfig.cjs" });
const { executeVtcodeProcess } = load("services/processExecution");
const { runVtcodeCommand } = load("utils/vtcodeRunner");
const { getConfigArguments } = load("utils/vtcodeRunner");
const { registerConfigurationCommands } = load("commands/configurationCommands");
const config = load("vtcodeConfig").state;

beforeEach(() => {
    vscode.workspace.isTrusted = true;
    vscode.env.uiKind = vscode.UIKind.Desktop;
    vscode.testState.handlers.clear();
    vscode.testState.messages.length = 0;
    vscode.testState.picks.length = 0;
    vscode.testState.beforeProgress = undefined;
    config.calls.length = 0;
    config.uri = undefined;
    config.summary = undefined;
    config.pick = undefined;
    config.updated = true;
    config.providerResult = "updated";
});
function output() {
    const text = [];
    return { text, append: (value) => text.push(value), appendLine() {}, show() {} };
}
function child() {
    const process = new EventEmitter();
    process.stdout = new EventEmitter();
    process.stderr = new EventEmitter();
    process.killed = false;
    process.kill = () => { process.killed = true; };
    return process;
}
function uri(path) { return { path, toString: () => path }; }
function configServices(getSummary, trusted = true) {
    const errors = [];
    const lines = [];
    const trustRequests = [];
    return {
        errors, lines, trustRequests,
        getCurrentConfigSummary: getSummary,
        ensureWorkspaceTrustedForCommand: async (action) => { trustRequests.push(action); return trusted; },
        getOutputChannel: () => ({ appendLine: (line) => lines.push(line) }),
        handleCommandError: (...args) => errors.push(args),
        openToolsPolicyGuide: async () => {}, openMcpGuide: async () => {},
    };
}

test("config argument adapter preserves absent configuration and literal paths", () => {
    assert.deepEqual(getConfigArguments(), []);
    assert.deepEqual(getConfigArguments({ fsPath: "/workspace/config;with spaces.toml" }),
        ["--config", "/workspace/config;with spaces.toml"]);
});

test("shared process preserves literal argv, stream callbacks and success", async (t) => {
    const process = child();
    const calls = [];
    t.mock.method(childProcess, "spawn", (...args) => { calls.push(args); return process; });
    const channel = output();
    const stdout = [];
    const stderr = [];
    const args = ["--config", "/workspace/a;$(printf injected).toml", "exec", "text with spaces & | >"];
    const spawnOptions = { cwd: "/workspace", env: { VT_IDE_CONTEXT_FILE: "/context.json" } };
    const pending = executeVtcodeProcess("/custom/vtcode", args, channel, () => spawnOptions, {
        showProgress: false, onStdout: (text) => stdout.push(text), onStderr: (text) => stderr.push(text),
    });
    assert.deepEqual(calls, [["/custom/vtcode", args, spawnOptions]]);
    assert.equal(calls[0][1], args);
    assert.equal(calls[0][2].shell, undefined);
    process.stdout.emit("data", Buffer.from("out"));
    process.stderr.emit("data", Buffer.from("warning"));
    process.emit("close", 0);
    await pending;
    assert.deepEqual(channel.text, ["out", "warning"]);
    assert.deepEqual(stdout, ["out"]);
    assert.deepEqual(stderr, ["warning"]);
});

test("real process receives shell metacharacters as arguments and the supplied environment", async () => {
    const args = ["two words", "$(printf injected)", "; printf another", "quote'and\"text", "unicode-đ"];
    const channel = output();
    await executeVtcodeProcess(process.execPath, [
        "-e", "process.stdout.write(JSON.stringify({ args: process.argv.slice(1), marker: process.env.VTCODE_EXECUTION_TEST_VALUE }))",
        ...args,
    ], channel, () => ({ cwd: __dirname, env: { VTCODE_EXECUTION_TEST_VALUE: "only supplied marker" } }),
    { showProgress: false });
    assert.deepEqual(JSON.parse(channel.text.join("")), { args, marker: "only supplied marker" });
});

test("shared process keeps cancellation distinct from failure and disposes subscriptions", async (t) => {
    const process = child();
    t.mock.method(childProcess, "spawn", () => process);
    let cancel;
    let disposed = 0;
    const pending = executeVtcodeProcess("vtcode", [], output(), () => ({}), {
        showProgress: false,
        cancellationToken: {
            onCancellationRequested: (callback) => { cancel = callback; return { dispose: () => { disposed += 1; } }; },
        },
    });
    cancel();
    assert.equal(process.killed, true);
    process.emit("close", 0);
    await assert.rejects(pending, vscode.CancellationError);
    assert.equal(disposed, 1);

    const failed = child();
    t.mock.method(childProcess, "spawn", () => failed);
    const failedRun = executeVtcodeProcess("vtcode", [], output(), () => ({}), { showProgress: false });
    failed.emit("close", 9);
    await assert.rejects(failedRun, /VT Code exited with code 9/);
});

test("shared process propagates spawn errors and reads environment inside progress callback", async (t) => {
    const failure = new Error("spawn denied");
    t.mock.method(childProcess, "spawn", () => { throw failure; });
    await assert.rejects(executeVtcodeProcess("vtcode", [], output(), () => ({}), { showProgress: false }),
        (error) => error === failure);
    const process = child();
    let env = { snapshot: "before" };
    let captured;
    t.mock.method(childProcess, "spawn", (_path, _args, options) => { captured = options; return process; });
    vscode.testState.beforeProgress = () => { env = { snapshot: "after" }; };
    const pending = executeVtcodeProcess("vtcode", [], output(), () => ({ env }));
    assert.deepEqual(captured, { env: { snapshot: "after" } });
    process.emit("error", failure);
    await assert.rejects(pending, (error) => error === failure);
});

test("modular caller rejects untrusted, web and already-cancelled requests before spawning", async (t) => {
    const spawn = t.mock.method(childProcess, "spawn", () => { throw new Error("must not spawn"); });
    vscode.workspace.isTrusted = false;
    await assert.rejects(runVtcodeCommand(["analyze"]), /Trust this workspace/);
    vscode.workspace.isTrusted = true;
    vscode.env.uiKind = vscode.UIKind.Web;
    await assert.rejects(runVtcodeCommand(["analyze"]), /web extension host/);
    vscode.env.uiKind = vscode.UIKind.Desktop;
    await assert.rejects(runVtcodeCommand(["analyze"], { cancellationToken: { isCancellationRequested: true } }),
        vscode.CancellationError);
    assert.equal(spawn.mock.callCount(), 0);
});

test("configuration registrations preserve trust gates and disposable ownership", async () => {
    const services = configServices(() => undefined, false);
    const registrations = registerConfigurationCommands(services);
    assert.deepEqual([...vscode.testState.handlers.keys()], [
        "vtcode.toggleHumanInTheLoop", "vtcode.openToolsPolicyGuide", "vtcode.openToolsPolicyConfig", "vtcode.configureMcpProviders",
    ]);
    await vscode.testState.handlers.get("vtcode.toggleHumanInTheLoop")();
    await vscode.testState.handlers.get("vtcode.configureMcpProviders")();
    assert.equal(services.trustRequests.length, 2);
    assert.deepEqual(config.calls, []);
    assert.deepEqual(services.errors, []);
    for (const registration of registrations) registration.dispose();
    assert.equal(vscode.testState.handlers.size, 0);
});

test("HITL toggle reads the current selected configuration after awaiting the picker", async () => {
    const first = uri("/workspace/first.toml");
    const second = uri("/workspace/second.toml");
    let summary = { hasConfig: true, uri: first, humanInTheLoop: false, mcpProviders: [] };
    config.pick = async () => {
        summary = { ...summary, uri: second, humanInTheLoop: true };
        return second;
    };
    const services = configServices(() => summary);
    registerConfigurationCommands(services);
    await vscode.testState.handlers.get("vtcode.toggleHumanInTheLoop")();
    assert.deepEqual(config.calls, [["pick", first], ["hitl", second, false]]);
    assert.equal(services.lines.length, 1);
    assert.deepEqual(services.errors, []);
});

test("HITL toggle loads mismatched summaries and reports unsuccessful updates", async () => {
    const selected = uri("/workspace/other.toml");
    config.uri = selected;
    config.summary = { hasConfig: true, uri: selected, humanInTheLoop: false, mcpProviders: [] };
    config.updated = false;
    const services = configServices(() => ({ uri: uri("/workspace/stale.toml"), humanInTheLoop: true }));
    registerConfigurationCommands(services);
    await vscode.testState.handlers.get("vtcode.toggleHumanInTheLoop")();
    assert.deepEqual(config.calls.slice(1), [["load", selected], ["hitl", selected, true]]);
    assert.match(vscode.testState.messages[0], /Failed to update human_in_the_loop/);
    assert.deepEqual(services.lines, []);
});

test("configuration errors retain command context and original failure", async () => {
    const failure = new Error("Config picker failed");
    config.pick = async () => { throw failure; };
    const services = configServices(() => undefined);
    registerConfigurationCommands(services);
    await vscode.testState.handlers.get("vtcode.toggleHumanInTheLoop")();
    assert.deepEqual(services.errors, [["toggle human-in-the-loop mode", failure]]);
    assert.ok(config.calls.every((call) => call[0] === "pick"));
    assert.deepEqual(services.lines, []);
});

test("MCP toggle preserves provider-specific state and handles missing entries", async () => {
    const selected = uri("/workspace/vtcode.toml");
    config.uri = selected;
    const services = configServices(() => ({
        uri: selected, mcpProviders: [{ name: "on", enabled: true }, { name: "off", enabled: false }],
    }));
    registerConfigurationCommands(services);
    vscode.testState.picks.push({ action: "toggle", providerName: "off" });
    await vscode.testState.handlers.get("vtcode.configureMcpProviders")();
    assert.deepEqual(config.calls[1], ["mcp", selected, "off", true]);
    config.providerResult = "notfound";
    vscode.testState.picks.push({ action: "toggle", providerName: "on" });
    await vscode.testState.handlers.get("vtcode.configureMcpProviders")();
    assert.deepEqual(config.calls[3], ["mcp", selected, "on", false]);
    assert.match(vscode.testState.messages.at(-1), /was not found in vtcode.toml/);
    assert.equal(services.lines.length, 1);
});
