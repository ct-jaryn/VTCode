const state = { calls: [], uri: undefined, summary: undefined, updated: true, providerResult: "updated", pick: undefined };
module.exports = {
    state,
    pickVtcodeConfigUri: async (preferred) => {
        state.calls.push(["pick", preferred]);
        return state.pick ? state.pick() : state.uri;
    },
    loadConfigSummaryFromUri: async (uri) => { state.calls.push(["load", uri]); return state.summary; },
    setHumanInTheLoop: async (...args) => { state.calls.push(["hitl", ...args]); return state.updated; },
    setMcpProviderEnabled: async (...args) => { state.calls.push(["mcp", ...args]); return state.providerResult; },
    revealToolsPolicySection: async (...args) => { state.calls.push(["policy", ...args]); },
    revealMcpSection: async (...args) => { state.calls.push(["reveal", ...args]); },
    appendMcpProvider: async (...args) => { state.calls.push(["add", ...args]); return true; },
};
