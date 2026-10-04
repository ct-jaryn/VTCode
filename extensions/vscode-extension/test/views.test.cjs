const { test, after } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const ts = require("typescript");

// Exercise the actual compiled view modules against a small VS Code API fixture.
// Keep the fixture local to this temporary module tree; never patch global require.
const root = fs.mkdtempSync(path.join(os.tmpdir(), "vtcode-vscode-views-"));
after(() => fs.rmSync(root, { recursive: true, force: true }));
const vscodeDir = path.join(root, "node_modules", "vscode");
fs.mkdirSync(vscodeDir, { recursive: true });
fs.copyFileSync(path.join(__dirname, "fixtures", "vscode.cjs"), path.join(vscodeDir, "index.js"));
const sourceDir = path.join(__dirname, "..", "src");
for (const name of ["quickActions", "workspaceInsights"]) {
    const source = fs.readFileSync(path.join(sourceDir, "views", `${name}.ts`), "utf8");
    const compiled = ts.transpileModule(source, {
        fileName: `${name}.ts`,
        compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
    });
    fs.writeFileSync(path.join(root, `${name}.cjs`), compiled.outputText);
}
const { createQuickActions, QuickActionTreeDataProvider } = require(path.join(root, "quickActions.cjs"));
const { createWorkspaceInsights, WorkspaceInsightsTreeDataProvider } = require(path.join(root, "workspaceInsights.cjs"));
const vscode = require(path.join(vscodeDir, "index.js"));

function summary(overrides = {}) {
    return {
        hasConfig: true, uri: { path: "/workspace/vtcode.toml" },
        mcpProviders: [{ name: "enabled" }, { name: "disabled", enabled: false }],
        toolPoliciesCount: 2, toolDefaultPolicy: "prompt", ...overrides,
    };
}
function services() {
    const paths = [];
    const tooltipCalls = [];
    return {
        paths, tooltipCalls,
        getConfiguredCommandPath: () => { paths.push("read"); return "/opt/custom/vtcode"; },
        createStatusBarTooltip: (...args) => {
            tooltipCalls.push(args);
            return new vscode.MarkdownString("Shared status tooltip");
        },
    };
}

test("untrusted quick actions expose trust and inspection without CLI or configuration mutations", () => {
    for (const available of [false, true]) {
        const commands = createQuickActions(available, summary({ automationFullAutoEnabled: true }), false)
            .map((action) => action.command);
        assert.deepEqual(commands, [
            "vtcode.trustWorkspace", "vtcode.verifyWorkspaceTrust", "vtcode.openInstallGuide",
            "vtcode.openToolsPolicyGuide", "vtcode.openConfig", "vtcode.openDocumentation",
            "vtcode.openDeepWiki", "vtcode.openWalkthrough",
        ]);
    }
});

test("trusted quick actions preserve CLI availability and configuration policy branches", () => {
    const missing = createQuickActions(false, undefined, true).map((action) => action.command);
    assert.deepEqual(missing, [
        "vtcode.openInstallGuide", "vtcode.openToolsPolicyGuide", "vtcode.openConfig",
        "vtcode.openDocumentation", "vtcode.openDeepWiki", "vtcode.openWalkthrough",
    ]);
    const ready = createQuickActions(true, summary({ automationFullAutoEnabled: true }), true);
    assert.deepEqual(ready.slice(0, 7).map((action) => action.command), [
        "vtcode.verifyWorkspaceTrust", "vtcode.flushIdeContextSnapshot", "vtcode.askAgent",
        "vtcode.askSelection", "vtcode.runTaskTrackerTask", "vtcode.launchAgentTerminal", "vtcode.runAnalyze",
    ]);
    assert.equal(ready[7].label, "Full-auto automation detected (blocked)");
    assert.equal(ready[7].command, "vtcode.openConfig");
    assert.equal(ready.find((action) => action.command === "vtcode.configureMcpProviders").description,
        "Adjust 1/2 enabled Model Context Protocol providers.");
    assert.equal(ready.find((action) => action.command === "vtcode.toggleHumanInTheLoop").label,
        "Disable human-in-the-loop safeguards");
    const manual = createQuickActions(true, summary({ humanInTheLoop: false }), true);
    assert.equal(manual.find((action) => action.command === "vtcode.toggleHumanInTheLoop").label,
        "Enable human-in-the-loop safeguards");
    assert.ok(!manual.some((action) => action.label === "Full-auto automation detected (blocked)"));
});

test("restricted workspace insights avoid reading executable configuration and hide mutation commands", () => {
    const dependencies = services();
    const items = createWorkspaceInsights(false, true, summary(), dependencies);
    assert.equal(items[0].label, "Workspace trust required");
    assert.equal(items[1].label, "CLI access blocked");
    assert.equal(items.find((item) => item.label === "Human-in-the-loop safeguards").command, undefined);
    assert.equal(items.find((item) => item.label === "MCP providers").command, undefined);
    assert.deepEqual(dependencies.paths, []);
    assert.deepEqual(dependencies.tooltipCalls, []);
});

test("trusted insights preserve shared tooltip, provider warnings, automation and parse feedback", () => {
    const dependencies = services();
    const items = createWorkspaceInsights(true, false, summary({
        automationFullAutoEnabled: true, automationFullAutoAllowedTools: ["read_file", "grep_file"],
        agentProvider: "ollama", agentDefaultModel: "gpt-oss:20b", humanInTheLoop: false,
        parseError: "Malformed config <literal>",
    }), dependencies);
    assert.equal(items[1].label, "VT Code CLI unavailable");
    assert.equal(items[1].description, "Check /opt/custom/vtcode or adjust vtcode.commandPath");
    assert.equal(items[1].tooltip.value, "Shared status tooltip");
    assert.deepEqual(dependencies.tooltipCalls, [["/opt/custom/vtcode", false, true]]);
    assert.equal(items.find((item) => item.label.startsWith("Agent provider:")).icon, "alert");
    const automation = items.find((item) => item.label === "Full-auto automation detected (blocked)");
    assert.match(automation.description, /Allowed tools: read_file, grep_file/);
    assert.equal(automation.command.command, "vtcode.openConfig");
    assert.equal(items.find((item) => item.label === "Human-in-the-loop safeguards").description,
        "Disabled (manual approvals required)");
    assert.equal(items.find((item) => item.label === "MCP providers").description, "1/2 enabled");
    assert.equal(items.find((item) => item.label === "Tool policy coverage").description, "2 overrides · Default: prompt");
    assert.equal(items.find((item) => item.label === "Configuration parsing error").description,
        "Malformed config <literal>");
    const openRouter = createWorkspaceInsights(true, true, summary({
        agentProvider: "openrouter", agentDefaultModel: "gpt-oss:20b",
    }), dependencies);
    assert.equal(openRouter.find((item) => item.label.startsWith("Agent provider:")).icon, "globe");
});

test("tree providers read current state, preserve item metadata, and signal refresh once", () => {
    let trusted = false;
    const quick = new QuickActionTreeDataProvider(() => createQuickActions(true, undefined, trusted));
    let refreshes = 0;
    const subscription = quick.onDidChangeTreeData(() => { refreshes += 1; });
    const restricted = quick.getChildren();
    assert.equal(restricted[0].label, "Trust this workspace for VT Code");
    assert.equal(restricted[0].command.command, "vtcode.trustWorkspace");
    assert.equal(restricted[0].iconPath.id, "shield");
    assert.equal(restricted[0].contextValue, "vtcodeQuickAction");
    assert.equal(quick.getTreeItem(restricted[0]), restricted[0]);
    trusted = true;
    quick.refresh();
    assert.equal(refreshes, 1);
    assert.equal(quick.getChildren()[0].command.command, "vtcode.verifyWorkspaceTrust");
    subscription.dispose();
    quick.refresh();
    assert.equal(refreshes, 1);

    let available = false;
    const insights = new WorkspaceInsightsTreeDataProvider(() =>
        createWorkspaceInsights(true, available, undefined, services()));
    const missing = insights.getChildren()[1];
    assert.equal(missing.command.command, "vtcode.openInstallGuide");
    assert.equal(missing.contextValue, "vtcodeWorkspaceInsight");
    assert.equal(missing.tooltip.value, "Shared status tooltip");
    assert.equal(insights.getTreeItem(missing), missing);
    available = true;
    let insightRefreshes = 0;
    insights.onDidChangeTreeData(() => { insightRefreshes += 1; });
    insights.refresh();
    assert.equal(insightRefreshes, 1);
    assert.equal(insights.getChildren()[1].command.command, "vtcode.openQuickActions");
});
