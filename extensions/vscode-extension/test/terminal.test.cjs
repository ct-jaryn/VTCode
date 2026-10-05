const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const { load, vscode } = require("./helpers/compileModules.cjs")(["services/interactiveTerminal"]);
const { InteractiveTerminal } = load("services/interactiveTerminal");
function setup(t) {
    const terminals = [], errors = [], listeners = new Set();
    const state = { trusted: true, args: [], flushes: 0, environment: { VT_IDE_CONTEXT_FILE: "/context.json" } };
    t.mock.method(vscode.window, "createTerminal", (options) => {
        const terminal = { options, disposed: 0,
            // Any shell-text fallback fails these tests.
            sendText() { assert.fail("must launch with native argv"); },
            dispose() { this.disposed++; for (const listener of listeners) listener(this); },
        };
        terminals.push(terminal); return terminal;
    });
    t.mock.method(vscode.window, "onDidCloseTerminal", (listener) => {
        listeners.add(listener); return { dispose: () => listeners.delete(listener) };
    });
    const services = {
        getEnvironment: () => state.environment,
        getConfigArguments: () => state.args,
        flushIdeContext: async () => { state.flushes++; },
        isWorkspaceTrusted: () => state.trusted,
        onError: (error) => errors.push(error),
    };
    const service = new InteractiveTerminal(services);
    t.after(() => service.dispose());
    return { service, services, state, terminals, listeners, errors,
        close: (terminal) => { for (const listener of listeners) listener(terminal); },
    };
}
function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}
test("native launch preserves options and reads live config/environment after context flush", async (t) => {
    const h = setup(t), gate = deferred();
    h.services.flushIdeContext = () => gate.promise;
    const pending = h.service.ensure("/custom path/vtcode", "/workspace");
    assert.equal(h.terminals.length, 0);
    h.state.args = ["--config", "/new config.toml"];
    h.state.environment = { VT_IDE_CONTEXT_FILE: "/new-context.json" };
    gate.resolve();
    const { terminal, created } = await pending;
    assert.equal(created, true);
    assert.equal(terminal.options.name, "VT Code Agent");
    assert.equal(terminal.options.cwd, "/workspace");
    assert.equal(terminal.options.shellPath, "/custom path/vtcode");
    assert.deepEqual(terminal.options.shellArgs, ["chat", "--config", "/new config.toml"]);
    assert.deepEqual(terminal.options.env, { VT_IDE_CONTEXT_FILE: "/new-context.json" });
    assert.equal(terminal.options.iconPath.id, "comment-discussion");
});
test("concurrent requests share pending launch, then reuse the same terminal", async (t) => {
    const h = setup(t), gate = deferred();
    let flushes = 0;
    h.services.flushIdeContext = () => { flushes++; return gate.promise; };
    const first = h.service.ensure("first", "/first");
    const second = h.service.ensure("other", "/other");
    gate.resolve();
    const a = await first, b = await second;
    assert.equal(a.created, true); assert.equal(b.created, false);
    assert.equal(a.terminal, b.terminal);
    assert.equal(h.terminals.length, 1); assert.equal(flushes, 1);
    assert.equal(a.terminal.options.shellPath, "first");
    assert.equal(a.terminal.options.cwd, "/first");
    assert.deepEqual(await h.service.ensure("unused", "/unused"), { terminal: a.terminal, created: false });
});
test("unrelated close leaves terminal intact; own close permits reopening", async (t) => {
    const h = setup(t);
    const first = (await h.service.ensure("vtcode", "/first")).terminal;
    h.close({}); assert.equal(h.listeners.size, 1);
    h.close(first); assert.equal(h.listeners.size, 0);
    const second = await h.service.ensure("new", "/second");
    assert.equal(second.created, true); assert.notEqual(first, second.terminal);
    assert.equal(h.state.flushes, 2);
});
test("shutdown during context flush prevents terminal creation", async (t) => {
    const h = setup(t), gate = deferred();
    h.services.flushIdeContext = () => gate.promise;
    const pending = h.service.ensure("vtcode", "/workspace");
    h.service.dispose(); h.service.dispose(); gate.resolve();
    assert.equal(await pending, undefined);
    assert.equal(await h.service.ensure("vtcode", "/workspace"), undefined);
    assert.equal(h.terminals.length, 0); assert.equal(h.listeners.size, 0);
});
test("shutdown disposes an existing terminal and listener exactly once", async (t) => {
    const h = setup(t);
    const { terminal } = await h.service.ensure("vtcode", "/workspace");
    h.service.dispose(); h.service.dispose();
    assert.equal(terminal.disposed, 1); assert.equal(h.listeners.size, 0);
});
for (const duringFlush of [false, true]) {
    test(`trust revocation blocks creation ${duringFlush ? "during flush" : "before launch"} and allows retry`, async (t) => {
        const h = setup(t), gate = deferred();
        if (duringFlush) h.services.flushIdeContext = () => gate.promise;
        else h.state.trusted = false;
        const pending = h.service.ensure("vtcode", "/workspace");
        h.state.trusted = false; gate.resolve();
        assert.equal(await pending, undefined); assert.equal(h.terminals.length, 0);
        h.state.trusted = true; h.services.flushIdeContext = async () => {};
        assert.equal((await h.service.ensure("vtcode", "/workspace")).created, true);
    });
}
test("flush rejection is reported and a later request can retry", async (t) => {
    const h = setup(t), failure = new Error("context unavailable");
    h.services.flushIdeContext = async () => { throw failure; };
    assert.equal(await h.service.ensure("vtcode", "/workspace"), undefined);
    assert.deepEqual(h.errors, [failure]); assert.equal(h.terminals.length, 0);
    h.services.flushIdeContext = async () => {};
    assert.equal((await h.service.ensure("vtcode", "/workspace")).created, true);
});
test("rejection after shutdown is consumed without reporting against a dead service", async (t) => {
    const h = setup(t), gate = deferred();
    h.services.flushIdeContext = () => gate.promise;
    const pending = h.service.ensure("vtcode", "/workspace");
    h.service.dispose(); gate.reject(new Error("stale failure"));
    assert.equal(await pending, undefined); assert.deepEqual(h.errors, []);
});
test("native creation failure is reported without shell fallback and allows retry", async (t) => {
    const h = setup(t), failure = new Error("cannot create terminal");
    const native = vscode.window.createTerminal;
    t.mock.method(vscode.window, "createTerminal", () => { throw failure; });
    assert.equal(await h.service.ensure("vtcode", "/workspace"), undefined);
    assert.deepEqual(h.errors, [failure]);
    t.mock.method(vscode.window, "createTerminal", native);
    assert.equal((await h.service.ensure("vtcode", "/workspace")).created, true);
});
test("executable and arguments retain metacharacters, quotes, newlines and empty values literally", async (t) => {
    const h = setup(t);
    const executable = '/bin/vt code;$(touch marker)&|<>`echo bad`%PATH%"';
    const values = ["a;b", "a&b", "a|b", "a>b", "a<b", "$(printf injected)", "`printf injected`", "%PATH%", "a'b", 'a"b', "line\nbreak", "", "unicode-đ"];
    h.state.args = ["--config", ...values];
    const { terminal } = await h.service.ensure(executable, "/workspace");
    assert.equal(terminal.options.shellPath, executable);
    assert.deepEqual(terminal.options.shellArgs, ["chat", "--config", ...values]);
});
test("real process receives the forwarded native argv as data", async (t) => {
    const h = setup(t);
    const values = ["a;b", "$(printf injected)", "quote'and\"text", "", "line\nbreak"];
    h.state.args = values;
    const { terminal } = await h.service.ensure(process.execPath, __dirname);
    const result = spawnSync(terminal.options.shellPath, [
        "-e", "process.stdout.write(JSON.stringify(process.argv.slice(1)))", ...terminal.options.shellArgs,
    ], { cwd: terminal.options.cwd, encoding: "utf8", shell: false });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), ["chat", ...values]);
});
