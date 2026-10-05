const { test } = require("node:test");
const assert = require("node:assert/strict");
const { load, vscode } = require("./helpers/compileModules.cjs")(["services/interactiveTerminal"]);
const { InteractiveTerminal } = load("services/interactiveTerminal");
function setup(t) {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const terminals = [], errors = [], listeners = new Set();
    const state = { trusted: true, args: [], flushes: 0 };
    t.mock.method(vscode.window, "createTerminal", (options) => {
        const terminal = { options, sent: [], disposed: 0,
            sendText(...args) { this.sent.push(args); },
            dispose() { this.disposed++; for (const listener of listeners) listener(this); },
        };
        terminals.push(terminal); return terminal;
    });
    t.mock.method(vscode.window, "onDidCloseTerminal", (listener) => {
        listeners.add(listener); return { dispose: () => listeners.delete(listener) };
    });
    const services = {
        getEnvironment: () => ({ VT_IDE_CONTEXT_FILE: "/context.json" }),
        getConfigArguments: () => state.args,
        flushIdeContext: async () => { state.flushes++; },
        isWorkspaceTrusted: () => state.trusted,
        onError: (error) => errors.push(error),
    };
    const service = new InteractiveTerminal(services);
    t.after(() => service.dispose());
    return { service, services, state, terminals, listeners, errors,
        close: (terminal) => { for (const listener of listeners) listener(terminal); },
        tick: async (ms = 800) => { t.mock.timers.tick(ms); await Promise.resolve(); await Promise.resolve(); },
    };
}
function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}
test("reuse preserves terminal options, delay, and live config after flush", async (t) => {
    const h = setup(t);
    const { terminal, created } = h.service.ensure("/custom path/vtcode", "/workspace");
    assert.equal(created, true);
    assert.deepEqual(h.service.ensure("other", "/other"), { terminal, created: false });
    assert.equal(h.terminals.length, 1);
    assert.equal(terminal.options.name, "VT Code Agent");
    assert.equal(terminal.options.cwd, "/workspace");
    assert.deepEqual(terminal.options.env, { VT_IDE_CONTEXT_FILE: "/context.json" });
    assert.equal(terminal.options.iconPath.id, "comment-discussion");
    await h.tick(799);
    assert.equal(h.state.flushes, 0);
    assert.deepEqual(terminal.sent, []);
    h.services.flushIdeContext = async () => { h.state.args = ["--config", "/new config.toml"]; };
    await h.tick(1);
    assert.deepEqual(terminal.sent, [['"/custom path/vtcode" chat --config "/new config.toml"', true]]);
    await h.tick(); assert.equal(terminal.sent.length, 1);
});
test("own close cancels pending work and permits reopening; unrelated close does not", async (t) => {
    const h = setup(t);
    const first = h.service.ensure("vtcode", "/first").terminal;
    h.close({}); assert.equal(h.listeners.size, 1);
    h.close(first); assert.equal(h.listeners.size, 0);
    const second = h.service.ensure("vtcode", "/second").terminal;
    await h.tick();
    assert.deepEqual(first.sent, []);
    assert.deepEqual(second.sent, [["vtcode chat", true]]);
    assert.equal(h.state.flushes, 1);
});
test("close during flush cannot send into the old terminal or release its replacement", async (t) => {
    const h = setup(t), gate = deferred();
    h.services.flushIdeContext = () => gate.promise;
    const first = h.service.ensure("old", "/first").terminal;
    await h.tick(); h.close(first);
    h.services.flushIdeContext = async () => {};
    const second = h.service.ensure("new", "/second").terminal;
    gate.resolve(); await h.tick();
    assert.deepEqual(first.sent, []);
    assert.deepEqual(second.sent, [["new chat", true]]);
    assert.equal(h.service.ensure("unused", "/unused").terminal, second);
    assert.equal(h.listeners.size, 1);
});
for (const duringFlush of [false, true]) {
    test(`shutdown cancels launch ${duringFlush ? "during flush" : "before delay"}`, async (t) => {
        const h = setup(t), gate = deferred();
        h.services.flushIdeContext = () => gate.promise;
        const terminal = h.service.ensure("vtcode", "/workspace").terminal;
        if (duringFlush) await h.tick();
        h.service.dispose(); h.service.dispose(); gate.resolve(); await h.tick();
        assert.deepEqual(terminal.sent, []);
        assert.equal(terminal.disposed, 1);
        assert.equal(h.listeners.size, 0);
        assert.throws(() => h.service.ensure("vtcode", "/workspace"), /disposed/);
    });
    test(`trust revocation blocks launch ${duringFlush ? "during flush" : "before delay"}`, async (t) => {
        const h = setup(t), gate = deferred();
        if (duringFlush) h.services.flushIdeContext = () => gate.promise;
        const terminal = h.service.ensure("vtcode", "/workspace").terminal;
        if (duringFlush) await h.tick();
        h.state.trusted = false; gate.resolve(); await h.tick();
        assert.deepEqual(terminal.sent, []);
        if (!duringFlush) assert.equal(h.state.flushes, 0);
    });
}
test("flush failure reports the original error and does not send", async (t) => {
    const h = setup(t), failure = new Error("context unavailable");
    h.services.flushIdeContext = async () => { throw failure; };
    const terminal = h.service.ensure("vtcode", "/workspace").terminal;
    await h.tick();
    assert.deepEqual(terminal.sent, []); assert.deepEqual(h.errors, [failure]);
});
test("stale flush rejection does not report against the replacement", async (t) => {
    const h = setup(t), gate = deferred();
    h.services.flushIdeContext = () => gate.promise;
    const terminal = h.service.ensure("old", "/workspace").terminal;
    await h.tick(); h.close(terminal);
    h.services.flushIdeContext = async () => {};
    const replacement = h.service.ensure("new", "/workspace").terminal;
    gate.reject(new Error("stale failure")); await h.tick();
    assert.deepEqual(h.errors, []);
    assert.deepEqual(replacement.sent, [["new chat", true]]);
});
