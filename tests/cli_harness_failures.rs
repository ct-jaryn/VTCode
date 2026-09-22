#![allow(
    missing_docs,
    reason = "Intentional compatibility, platform, test, or API-shape suppression."
)]
use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::process::Command;
use vtcode_core::config::constants::models::openai::DEFAULT_MODEL;

#[path = "../crates/codegen/vtcode-core/tests/support/mod.rs"]
mod support;

use support::TestHarness;

/// Builds a CLI command using OpenAI's default model and a synthetic API key.
///
/// Pins the provider and model so startup does not depend on application defaults.
fn base_command(harness: &TestHarness) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("vtcode"));
    let _configured_command = cmd
        .args(["--provider", "openai", "--model", DEFAULT_MODEL])
        .env("OPENAI_API_KEY", "test-key")
        .env("NO_COLOR", "1")
        .current_dir(harness.workspace());
    cmd
}

#[test]
fn print_mode_requires_prompt_or_stdin() {
    let harness = TestHarness::new().expect("failed to init harness workspace");
    let _marker = harness
        .write_file(".vtcode/.keep", "")
        .expect("failed to mark workspace initialized");
    let mut cmd = base_command(&harness);
    let _argument = cmd.arg("--print");

    let _assertion = cmd.assert().failure().stderr(predicate::str::contains("No prompt provided"));
}

#[test]
fn config_override_failure_is_reported() {
    let harness = TestHarness::new().expect("failed to init harness workspace");
    let missing_config = harness.workspace().join("missing-config.toml");

    let mut cmd = base_command(&harness);
    let _argument = cmd
        .arg("--workspace")
        .arg(harness.workspace())
        .arg("--config")
        .arg(&missing_config)
        .arg("--print")
        .arg("hello")
        .current_dir(harness.workspace());

    let _assertion = cmd.assert().failure().stderr(
        predicate::str::contains("failed to initialize VT Code startup context")
            .and(predicate::str::contains(missing_config.to_string_lossy())),
    );
}

#[test]
fn unknown_positional_token_fails_without_forwarding_prompt_to_llm() {
    let harness = TestHarness::new().expect("failed to init harness workspace");
    let _marker = harness
        .write_file(".vtcode/.keep", "")
        .expect("failed to mark workspace initialized");

    let mut cmd = base_command(&harness);
    let _argument = cmd.arg("--").arg("hellp");

    let _assertion = cmd.assert().failure().stderr(
        predicate::str::contains("invalid value")
            .and(predicate::str::contains("is not a valid workspace path or subcommand"))
            .and(predicate::str::contains("try '--help'"))
            .and(predicate::str::contains("Sending prompt to").not()),
    );
}
