const { after } = require("node:test");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const ts = require("typescript");

module.exports = function compileModules(files, fixtures = {}) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), "vtcode-vscode-unit-"));
    after(() => fs.rmSync(root, { recursive: true, force: true }));
    const vscodeDir = path.join(root, "node_modules", "vscode");
    fs.mkdirSync(vscodeDir, { recursive: true });
    fs.copyFileSync(path.join(__dirname, "..", "fixtures", "vscode.cjs"), path.join(vscodeDir, "index.js"));
    for (const file of files) {
        const source = fs.readFileSync(path.join(__dirname, "..", "..", "src", `${file}.ts`), "utf8");
        const compiled = ts.transpileModule(source, {
            fileName: `${file}.ts`,
            compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
        });
        const target = path.join(root, `${file}.js`);
        fs.mkdirSync(path.dirname(target), { recursive: true });
        fs.writeFileSync(target, compiled.outputText);
    }
    for (const [file, fixture] of Object.entries(fixtures)) {
        const target = path.join(root, `${file}.js`);
        fs.mkdirSync(path.dirname(target), { recursive: true });
        fs.copyFileSync(path.join(__dirname, "..", "fixtures", fixture), target);
    }
    return {
        load: (file) => require(path.join(root, `${file}.js`)),
        vscode: require(path.join(vscodeDir, "index.js")),
    };
};
