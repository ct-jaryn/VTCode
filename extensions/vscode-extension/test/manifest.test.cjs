const { test } = require("node:test");
const assert = require("node:assert/strict");
const compileModules = require("./helpers/compileModules.cjs");
const { load } = compileModules(["utils/manifestContributions"]);
const { hasManifestContribution } = load("utils/manifestContributions");

test("manifest matching distinguishes tool names, participant IDs, and literal identifiers", () => {
    const entries = [
        { name: "another-tool", id: "vtcode.agent" },
        { name: "vtcode-taskTracker", id: "another-participant" },
    ];
    const manifest = { contributes: { languageModelTools: entries, chatParticipants: entries } };
    assert.equal(hasManifestContribution(manifest, "languageModelTools", "vtcode-taskTracker"), true);
    assert.equal(hasManifestContribution(manifest, "chatParticipants", "vtcode.agent"), true);
    assert.equal(hasManifestContribution(manifest, "chatParticipants", "vtcode-taskTracker"), false);
    assert.equal(hasManifestContribution(manifest, "languageModelTools", "vtcode.agent"), false);
    assert.equal(hasManifestContribution(manifest, "languageModelTools", "VTCODE-taskTracker"), false);
    assert.equal(hasManifestContribution(manifest, "chatParticipants", "vtcode.agent "), false);
    assert.equal(hasManifestContribution({ contributes: { languageModelTools: entries } }, "chatParticipants", "vtcode.agent"), false);
});

test("absent and malformed contribution arrays do not enable integrations", () => {
    for (const value of [undefined, null, false, 7, "vtcode.agent", { id: "vtcode.agent" }, []]) {
        assert.equal(hasManifestContribution(value, "chatParticipants", "vtcode.agent"), false);
        assert.equal(hasManifestContribution({ contributes: value }, "chatParticipants", "vtcode.agent"), false);
        assert.equal(hasManifestContribution({ contributes: { chatParticipants: value } }, "chatParticipants", "vtcode.agent"), false);
    }
    assert.equal(hasManifestContribution({ contributes: { chatParticipants: [
        null, undefined, false, 7, "vtcode.agent", {}, { id: false }, { id: ["vtcode.agent"] },
    ] } }, "chatParticipants", "vtcode.agent"), false);
});

test("malformed neighbors do not hide a later valid contribution or mutate the manifest", () => {
    const valid = Object.freeze({ name: "vtcode-taskTracker" });
    const contributions = Object.freeze([null, "invalid", { name: 7 }, valid]);
    const manifest = Object.freeze({ contributes: Object.freeze({ languageModelTools: contributions }) });
    assert.equal(hasManifestContribution(manifest, "languageModelTools", "vtcode-taskTracker"), true);
    assert.equal(contributions.at(-1), valid);
});
