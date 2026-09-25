# CI/CD and Code Quality

This document describes the CI/CD pipeline and code quality tools used in the vtcode project.

## GitHub Actions Workflows

The project uses several GitHub Actions workflows to ensure code quality and automate testing:

### 1. CI Workflow (`ci.yml`)

**Triggers:**

- Push to `main` (filtered by `.rs`, `.toml`, `.lock`, `.yml`, `.json`, `.md`, `scripts/`)
- Pull requests to `main` (same path filters)
- Weekly schedule (Monday 5 AM UTC)
- Manual `workflow_dispatch`

**Jobs:**

- **Format Check (rustfmt)**: Ensures code is properly formatted
- **Lint Check (clippy)**: Runs comprehensive linting with `-D warnings`
- **Test**: Runs `cargo nextest run` on Ubuntu (plus macOS and Windows for PRs)
- **Benchmarks**: Performance regression testing
- **Security Audit**: `cargo audit` for vulnerable dependencies
- **Documentation**: Builds and tests documentation (`cargo doc`)

### 2. Tool Eval Workflow (`tool-eval.yml`)

**Triggers:**

- Push and PR to `main` on `.rs`, `.toml`, `.lock`, `scripts/`, `.github/workflows/`

**Jobs:**

- **Tool Evaluation**: Validates built-in tool behavior and safety gateways
- **Integration tests**: End-to-end tool execution checks

### 3. Build Linux & Windows (`build-linux-windows.yml`)

**Triggers:**

- Manual `workflow_dispatch` with release tag input
- Called from `release.yml` on publish

**Jobs:**

- **Build Linux**: Compiles `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, and `aarch64-unknown-linux-gnu` binaries
- **Build Windows**: Compiles `x86_64-pc-windows-msvc` binary
- **Upload Artifacts**: Stores compiled binaries + extension-stripped `.sha256` sidecars for release

**Required release target matrix** (enforced by `scripts/release.sh`):

| Target | Built by | Archive |
| --- | --- | --- |
| `x86_64-apple-darwin` | local (`release.sh`) | `.tar.gz` |
| `aarch64-apple-darwin` | local (`release.sh`) | `.tar.gz` |
| `x86_64-unknown-linux-gnu` | `build-linux-windows.yml` | `.tar.gz` |
| `x86_64-unknown-linux-musl` | `build-linux-windows.yml` | `.tar.gz` |
| `aarch64-unknown-linux-gnu` | `build-linux-windows.yml` | `.tar.gz` |
| `x86_64-pc-windows-msvc` | `build-linux-windows.yml` | `.zip` (required by default) |

`release.sh` derives a raw `compat-vtcode-<v>-<target>.tar.gz.compat` executable from
each normal archive. These are the legacy updater compatibility bridge for
v0.141.0-v0.141.4 (see [Update System Guide](../guides/UPDATE_SYSTEM.md)). The
`compat-` prefix is load-bearing: GitHub returns release assets sorted alphabetically
by name, and the prefix makes the compat asset sort before `vtcode-<v>-<target>.tar.gz`
so the broken legacy updater picks the raw binary instead of the gzip archive it
cannot extract. The release fails if any required target archive (including
Windows by default) is missing. Set `RELEASE_REQUIRE_WINDOWS=false` only for an
emergency macOS/Linux rescue when Windows CI is flaky.

**Binary size & cold-start optimization:**

All release profiles inherit `[profile.release]` which uses `opt-level = "z"` (size
optimization) + full LTO + `codegen-units = 1`. Binary size directly impacts cold-start
time — dyld page-faults loading the Mach-O dominate the first-launch latency.

| Build path | Profile | Extra size flags |
| --- | --- | --- |
| macOS local (`release.sh`) | `release` | `-Wl,-dead_strip` via `CARGO_TARGET_*_RUSTFLAGS` |
| Linux CI | `release-fast` (thin LTO, 4 codegen units) | `-Wl,--gc-sections` via `RUSTFLAGS` |
| Windows CI | `release-fast-windows` (no LTO, 16 codegen units) | MSVC `/OPT:REF` (default) |

`release.sh` also runs a cold-start spot check (fresh `/tmp` copy → `--version` timing)
after the macOS aarch64 build to catch sub-1s regressions before shipping. All build
commands use `--locked` to ensure the Cargo.lock matches Cargo.toml so the size-optimized
profiles are actually applied.

### 4. Coverage (`coverage.yml`)

**Triggers:**

- Push and PR to `main` on `.rs`, `Cargo.toml`, `Cargo.lock`, `coverage.yml`

**Jobs:**

- **Code Coverage**: `cargo tarpaulin` with XML output
- **Coverage Report**: Uploads to code coverage service

### 5. Release (`release.yml`)

**Triggers:**

- Manual `workflow_dispatch` with version tag

**Jobs:**

- **Build Binaries**: Triggers `build-linux-windows.yml`
- **Create Release**: Drafts GitHub Release with changelog
- **Publish**: Publishes to crates.io and Homebrew

## Code Quality Tools

### rustfmt

**Installation:**

```bash
rustup component add rustfmt
```

**Usage:**

```bash
# Check formatting
cargo fmt --all -- --check

# Auto-format code
cargo fmt --all

# Print current configuration
cargo fmt --print-config default rustfmt.toml
```

**Configuration:**
Create a `rustfmt.toml` or `.rustfmt.toml` file in your project root:

```toml
edition = "2021"
max_width = 100
tab_spaces = 4
```

### clippy

**Installation:**

```bash
rustup component add clippy
```

**Usage:**

```bash
# Run clippy with warnings as errors
cargo clippy -- -D warnings

# Run on specific target
cargo clippy --lib

# Fix clippy suggestions automatically
cargo clippy --fix
```

**Common clippy lints:**

- `clippy::all`: Enable all lints
- `clippy::pedantic`: More strict lints
- `clippy::nursery`: Experimental lints
- `clippy::cargo`: Cargo.toml specific lints

### First-party debt scan

The lint migration keeps the actionable marker scan separate from generated or
fixture content. Run it from the repository root:

```bash
./scripts/first-party-debt-scan.sh
```

The scanner covers first-party `src/`, `crates/`, and `scripts/` content while
excluding vendored, generated, fixture, template, sample, and task-panel
content. New `TODO:`, `FIXME:`, `HACK:`, or `XXX:` markers fail the check.

The workspace lint gate also enforces the previously suppressed result,
indexing, string-slice, cast, and allow-without-reason lint families:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --locked --workspace
```

## Local Development

### Development Check Script

Use the provided development check script to run the same checks locally:

```bash
# Run all checks
./scripts/check.sh

# Run specific checks
./scripts/check.sh fmt      # Format check
./scripts/check.sh clippy   # Clippy check
./scripts/check.sh test     # Run tests
./scripts/check.sh build    # Build project
./scripts/check.sh docs     # Generate docs
```

### Manual Setup

To set up the development environment manually:

```bash
# Install required components
rustup component add rustfmt clippy

# Install additional tools
cargo install cargo-audit      # Security auditing
cargo install cargo-outdated   # Dependency checking
cargo install cargo-udeps      # Unused dependencies
cargo install cargo-msrv       # MSRV checking
cargo install cargo-license    # License checking
cargo install cargo-tarpaulin  # Code coverage
```

## Best Practices

### 1. Pre-commit Hooks

Set up pre-commit hooks to run checks before committing:

```bash
# Install pre-commit (if using)
pre-commit install

# Or create .git/hooks/pre-commit manually:
#!/bin/bash
./scripts/check.sh
```

### 2. Editor Integration

#### VS Code

Add to `.vscode/settings.json`:

```json
{
  "rust-analyzer.checkOnSave.command": "clippy",
  "editor.formatOnSave": true,
  "rust-analyzer.rustfmt.enableRangeFormatting": true
}
```

#### Vim/Neovim

```vim
autocmd BufWritePre *.rs :silent! !cargo fmt -- %:p
```

### 3. IDE Integration

Most Rust IDEs support rustfmt and clippy:

- **IntelliJ/CLion**: Built-in Rust plugin
- **VS Code**: rust-analyzer extension
- **Vim**: rust.vim plugin
- **Emacs**: rustic-mode

## CI/CD Configuration

### Branch Protection

Configure branch protection rules in GitHub:

1. Go to repository Settings → Branches
2. Add rule for `main`/`master` branch
3. Require status checks to pass:
   - `fmt`
   - `clippy`
   - `test`
   - `security-audit`

### Status Badges

Add these badges to your README:

```markdown
[![CI](https://github.com/yourusername/vtcode/actions/workflows/ci.yml/badge.svg)](https://github.com/yourusername/vtcode/actions/workflows/ci.yml)
[![Code Quality](https://github.com/yourusername/vtcode/actions/workflows/code-quality.yml/badge.svg)](https://github.com/yourusername/vtcode/actions/workflows/code-quality.yml)
```

## Troubleshooting

### Common Issues

#### rustfmt not found

```bash
rustup component add rustfmt
rustup update
```

#### clippy warnings not showing

```bash
cargo clippy -- -W clippy::all
```

#### MSRV issues

```bash
cargo msrv --workspace
cargo msrv --workspace set 1.98.1  # Set specific version
```

#### Dependency issues

```bash
cargo update
cargo outdated
cargo udeps
```

### Performance Optimization

#### Faster CI builds

```yaml
# In workflow
- uses: actions/cache@v3
  with:
    path: |
      ~/.cargo/registry
      ~/.cargo/git
      target
    key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
```

#### Parallel jobs

```yaml
strategy:
  matrix:
    os: [ubuntu-latest, macos-latest, windows-latest]
```

## Security

### Dependency Auditing

```bash
# Install cargo-audit
cargo install cargo-audit

# Run audit
cargo audit

# Fix vulnerabilities
cargo audit fix
```

### License Compliance

```bash
# Check licenses (quick overview)
cargo install cargo-license
cargo license --workspace

# Regenerate THIRD-PARTY-NOTICES from Cargo.lock (full automation)
cargo install --locked --features cli cargo-about
scripts/generate-notices.sh          # regenerate the file
scripts/generate-notices.sh --check  # CI mode: exit 1 if out of date
```

The `license-notices` CI job runs `scripts/generate-notices.sh --check` on
every PR to catch stale license notices before merge. The file has a manual
header (`scripts/templates/third-party-header.txt` for in-tree source ports)
and an auto-generated dependency listing (`scripts/templates/third-party-notices.hbs`
via cargo-about).

## References

- [rustfmt Documentation](https://rust-lang.github.io/rustfmt/)
- [clippy Documentation](https://rust-lang.github.io/rust-clippy/)
- [GitHub Actions Documentation](https://docs.github.com/en/actions)
- [Cargo Documentation](https://doc.rust-lang.org/cargo/)
