use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const TASKS_DIR: &str = ".vtcode/tasks";
const CURRENT_TASK_FILE: &str = "current_task.md";
const CURRENT_SPEC_FILE: &str = "current_spec.md";
const CURRENT_CONTRACT_FILE: &str = "current_contract.md";
const CURRENT_EVALUATION_FILE: &str = "current_evaluation.md";
const CURRENT_SPRINT_CONTRACT_FILE: &str = "current_sprint_contract.md";
const CURRENT_OUTCOME_VERIFICATION_FILE: &str = "current_outcome_verification.md";
const CURRENT_FEATURE_LIST_FILE: &str = "current_feature_list.md";
const SUMMARY_PREVIEW_CHARS: usize = 280;

/// Return the path to the current task tracker file.
pub fn current_task_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_TASK_FILE)
}

/// Return the path to the current context reset manifest file.
pub fn current_context_reset_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(TASKS_DIR)
        .join(crate::core::agent::context_reset::CONTEXT_RESET_FILE)
}

/// Return the path to the current spec artifact file.
pub fn current_spec_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_SPEC_FILE)
}

/// Return the path to the current contract artifact file.
pub fn current_contract_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_CONTRACT_FILE)
}

/// Return the path to the current evaluation artifact file.
pub fn current_evaluation_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_EVALUATION_FILE)
}

/// Return the path to the current sprint contract artifact file.
///
/// The sprint contract is the pre-sprint negotiation artifact: the generator
/// and evaluator agree on scope, acceptance criteria, and out-of-scope items
/// before implementation begins. This follows the long-running harness pattern
/// where "vague user stories become testable contracts."
pub fn current_sprint_contract_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_SPRINT_CONTRACT_FILE)
}

/// Return the path to the current outcome verification artifact file.
///
/// The outcome verification records what commands were run to verify, what the
/// actual output was, and whether tests/build passed. This enforces "evaluate
/// outcomes, not claims" -- the agent cannot declare success without showing
/// actual verification output.
pub fn current_outcome_verification_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_OUTCOME_VERIFICATION_FILE)
}

/// Return the paths of all harness artifacts that currently exist on disk.
pub fn existing_harness_artifact_paths(workspace_root: &Path) -> Vec<PathBuf> {
    [
        current_spec_path(workspace_root),
        current_contract_path(workspace_root),
        current_evaluation_path(workspace_root),
        current_sprint_contract_path(workspace_root),
        current_outcome_verification_path(workspace_root),
        current_feature_list_path(workspace_root),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

/// Read a short summary of the current spec artifact, or `None` if unavailable.
pub fn read_spec_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_spec_path(workspace_root), "Spec")
}

/// Like [`read_spec_summary`], but drop the file when it predates `not_before`.
pub fn read_spec_summary_fresh(workspace_root: &Path, not_before: Option<SystemTime>) -> Option<String> {
    read_markdown_summary_fresh(&current_spec_path(workspace_root), "Spec", not_before)
}

/// Read a short summary of the current contract artifact, or `None` if unavailable.
pub fn read_contract_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_contract_path(workspace_root), "Contract")
}

/// Read a short summary of the current evaluation artifact, or `None` if unavailable.
pub fn read_evaluation_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_evaluation_path(workspace_root), "Evaluation")
}

/// Like [`read_evaluation_summary`], but drop the file when it predates `not_before`.
pub fn read_evaluation_summary_fresh(workspace_root: &Path, not_before: Option<SystemTime>) -> Option<String> {
    read_markdown_summary_fresh(&current_evaluation_path(workspace_root), "Evaluation", not_before)
}

/// Like [`read_contract_summary`], but drop the file when it predates `not_before`.
pub fn read_contract_summary_fresh(workspace_root: &Path, not_before: Option<SystemTime>) -> Option<String> {
    read_markdown_summary_fresh(&current_contract_path(workspace_root), "Contract", not_before)
}

/// Like [`read_feature_list_summary`], but drop the file when it predates `not_before`.
pub fn read_feature_list_summary_fresh(workspace_root: &Path, not_before: Option<SystemTime>) -> Option<String> {
    read_markdown_summary_fresh(&current_feature_list_path(workspace_root), "FeatureList", not_before)
}

/// Like [`read_sprint_contract_summary`], but drop the file when it predates `not_before`.
pub fn read_sprint_contract_summary_fresh(workspace_root: &Path, not_before: Option<SystemTime>) -> Option<String> {
    read_markdown_summary_fresh(&current_sprint_contract_path(workspace_root), "SprintContract", not_before)
}

/// Like [`read_outcome_verification_summary`], but drop the file when it predates `not_before`.
pub fn read_outcome_verification_summary_fresh(
    workspace_root: &Path,
    not_before: Option<SystemTime>,
) -> Option<String> {
    read_markdown_summary_fresh(&current_outcome_verification_path(workspace_root), "OutcomeVerification", not_before)
}

/// Best-effort start time of `session_id`, used to reject leftover workspace
/// task artifacts that predate the session. Returns `None` when the session
/// directory is missing so callers can fall back to unfiltered reads.
pub fn session_artifact_cutoff(workspace_root: &Path, session_id: &str) -> Option<SystemTime> {
    // Canonical store path (full-length sanitize_id), plus the raw id as a
    // fallback for callers that never opened the store.
    let candidates = [
        vtcode_memory::session_directory(workspace_root, session_id),
        workspace_root.join(".vtcode").join("sessions").join(session_id),
    ];
    for dir in candidates {
        let Ok(metadata) = fs::metadata(&dir) else {
            continue;
        };
        if let Some(cutoff) = artifact_cutoff_from_metadata(&metadata) {
            return Some(cutoff);
        }
    }
    None
}

/// Cutoff derived from a session directory's timestamps.
///
/// True creation time is preferred. When the filesystem has no birth time
/// (btime-less Linux filesystems), `modified()` is the best available lower
/// bound: it sits slightly after the real session start, so stale leftovers
/// from earlier sessions still compare stale while artifacts written during
/// the session stay live. Returning `None` here would disable the leftover
/// filter entirely (fail-open) on those platforms.
fn artifact_cutoff_from_metadata(metadata: &fs::Metadata) -> Option<SystemTime> {
    metadata.created().ok().or_else(|| metadata.modified().ok())
}

/// Archive a fully-checked `current_task.md` so a finished checklist cannot
/// describe the next session. Incomplete checklists stay in place.
///
/// Returns the archive path when a move happened. The archive name carries a
/// timestamp and uuid (same contract as blocked-handoff archives) because the
/// live file is gone after the move: a deterministic session-id-only name
/// would let a resumed session's second completion destroy the only copy.
pub fn archive_completed_current_task(workspace_root: &Path, session_id: &str) -> Result<Option<PathBuf>> {
    let task_path = current_task_path(workspace_root);
    let Ok(content) = fs::read_to_string(&task_path) else {
        return Ok(None);
    };
    let checklist: Vec<&str> = content
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with("- ["))
        .collect();
    let is_checked = |line: &str| line.starts_with("- [x]") || line.starts_with("- [X]");
    if checklist.is_empty() || !checklist.iter().all(|line| is_checked(line)) {
        return Ok(None);
    }
    let archive_dir = workspace_root.join(TASKS_DIR).join("archive");
    fs::create_dir_all(&archive_dir).with_context(|| format!("create task archive dir {}", archive_dir.display()))?;
    let archive_name = format!(
        "current_task-{}-{}-{}.md",
        filename_safe_id(session_id, 64),
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4()
    );
    let archive_path = archive_dir.join(archive_name);
    fs::rename(&task_path, &archive_path)
        .with_context(|| format!("archive completed task tracker to {}", archive_path.display()))?;
    Ok(Some(archive_path))
}

/// Filename-safe session id prefix for archive side-cars (not envelope names —
/// those use `sanitize_session_id`'s fixed 32-char contract).
fn filename_safe_id(id: &str, max_chars: usize) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(max_chars)
        .collect()
}

/// Write the spec artifact content to disk and return the path.
pub async fn write_spec(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_spec_path(workspace_root);
    write_artifact(path.as_path(), content, "current spec").await?;
    Ok(path)
}

/// Write the evaluation artifact content to disk and return the path.
pub async fn write_evaluation(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_evaluation_path(workspace_root);
    write_artifact(path.as_path(), content, "current evaluation").await?;
    Ok(path)
}

/// Write the contract artifact content to disk and return the path.
pub async fn write_contract(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_contract_path(workspace_root);
    write_artifact(path.as_path(), content, "current contract").await?;
    Ok(path)
}

/// Read a short summary of the sprint contract artifact, or `None` if unavailable.
pub fn read_sprint_contract_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_sprint_contract_path(workspace_root), "SprintContract")
}

/// Write the sprint contract artifact content to disk and return the path.
///
/// The sprint contract is the pre-sprint negotiation artifact where generator
/// and evaluator agree on scope and acceptance criteria before code is written.
pub async fn write_sprint_contract(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_sprint_contract_path(workspace_root);
    write_artifact(path.as_path(), content, "sprint contract").await?;
    Ok(path)
}

/// Read a short summary of the outcome verification artifact, or `None` if unavailable.
pub fn read_outcome_verification_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_outcome_verification_path(workspace_root), "OutcomeVerification")
}

/// Return the path to the current feature list artifact file.
///
/// The feature list is a persistent artifact the planner creates and the
/// evaluator modifies during feedback-driven replanning. It lists the
/// project's features with their acceptance criteria, so each agent session
/// can pick up an incremental unit of work. Following the long-running
/// harness pattern: "the planner can achieve replanning by modifying external
/// files: feature_list, sprint_contract, known_issues, next_actions."
pub fn current_feature_list_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TASKS_DIR).join(CURRENT_FEATURE_LIST_FILE)
}

/// Read a short summary of the feature list artifact, or `None` if unavailable.
pub fn read_feature_list_summary(workspace_root: &Path) -> Option<String> {
    read_markdown_summary(&current_feature_list_path(workspace_root), "FeatureList")
}

/// Write the feature list artifact content to disk and return the path.
pub async fn write_feature_list(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_feature_list_path(workspace_root);
    write_artifact(path.as_path(), content, "feature list").await?;
    Ok(path)
}

/// Write the outcome verification artifact content to disk and return the path.
///
/// This records actual verification commands and their output, enforcing
/// "evaluate outcomes, not claims" -- the agent must show proof of verification.
pub async fn write_outcome_verification(workspace_root: &Path, content: &str) -> Result<PathBuf> {
    let path = current_outcome_verification_path(workspace_root);
    write_artifact(path.as_path(), content, "outcome verification").await?;
    Ok(path)
}

async fn write_artifact(path: &Path, content: &str, label: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {} directory {}", label, parent.display()))?;
    }

    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("write {} {}", label, path.display()))?;
    Ok(())
}

fn read_markdown_summary(path: &Path, label: &str) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .take(4)
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }

    let joined = lines.join(" | ");
    Some(format!("{label}: {}", truncate_summary(&joined)))
}

/// Grace applied when comparing artifact mtime to session start.
///
/// A spec written as a handoff before `vtcode` starts is still live for this
/// session; only leftovers from *earlier* sessions (days/weeks old) must be
/// dropped. 24h covers normal handoff workflows; the Jul-24 fixture class is
/// far outside it.
const ARTIFACT_FRESHNESS_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Whether `path` predates the freshness window of `not_before`.
///
/// Single source of the artifact staleness rule shared by the fresh readers
/// and the memory-envelope constraints channel. Unreadable metadata counts as
/// stale so a checked artifact never falls back to content of unknown age;
/// `not_before == None` disables the check.
pub fn artifact_is_stale(path: &Path, not_before: Option<SystemTime>) -> bool {
    let Some(not_before) = not_before else {
        return false;
    };
    let Some(modified) = fs::metadata(path).ok().and_then(|meta| meta.modified().ok()) else {
        return true;
    };
    let stale_before = not_before
        .checked_sub(ARTIFACT_FRESHNESS_GRACE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    modified < stale_before
}

/// Read a markdown summary only when the file is at least as new as `not_before`.
///
/// Workspace-global task artifacts outlive their session. A leftover fixture
/// must not describe a later session's memory envelope or orient snapshot.
/// Files within [`ARTIFACT_FRESHNESS_GRACE`] of `not_before` still count as
/// live so a just-written handoff artifact is kept.
fn read_markdown_summary_fresh(path: &Path, label: &str, not_before: Option<SystemTime>) -> Option<String> {
    if artifact_is_stale(path, not_before) {
        return None;
    }
    read_markdown_summary(path, label)
}

fn truncate_summary(text: &str) -> String {
    vtcode_commons::formatting::truncate_within(text, SUMMARY_PREVIEW_CHARS, "...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_and_summarizes_spec_and_evaluation_artifacts() {
        let temp = tempdir().expect("tempdir");

        write_spec(temp.path(), "# Spec\n\nBuild a stronger exec harness.\n\nKeep it resumable.\n")
            .await
            .expect("write spec");
        write_contract(temp.path(), "# Contract\n\n- Deliver the requested change.\n- Verify with cargo check.\n")
            .await
            .expect("write contract");
        write_evaluation(temp.path(), "# Evaluation\n\nVerdict: fail\n\nNeed another revision round.\n")
            .await
            .expect("write evaluation");

        let paths = existing_harness_artifact_paths(temp.path());
        assert_eq!(paths.len(), 3);
        assert_eq!(
            read_spec_summary(temp.path()),
            Some("Spec: Build a stronger exec harness. | Keep it resumable.".to_string())
        );
        assert_eq!(
            read_contract_summary(temp.path()),
            Some("Contract: - Deliver the requested change. | - Verify with cargo check.".to_string())
        );
        assert_eq!(
            read_evaluation_summary(temp.path()),
            Some("Evaluation: Verdict: fail | Need another revision round.".to_string())
        );
    }

    #[tokio::test]
    async fn writes_and_summarizes_sprint_contract() {
        let temp = tempdir().expect("tempdir");

        write_sprint_contract(
            temp.path(),
            "# Sprint Contract\n\nScope: implement login endpoint.\nAcceptance: POST /login returns JWT.\n",
        )
        .await
        .expect("write sprint contract");

        let paths = existing_harness_artifact_paths(temp.path());
        assert_eq!(paths.len(), 1);
        assert_eq!(
            read_sprint_contract_summary(temp.path()),
            Some("SprintContract: Scope: implement login endpoint. | Acceptance: POST /login returns JWT.".to_string())
        );
    }

    #[tokio::test]
    async fn writes_and_summarizes_outcome_verification() {
        let temp = tempdir().expect("tempdir");

        write_outcome_verification(
            temp.path(),
            "# Outcome Verification\n\nCommand: cargo nextest run\nResult: 12 passed, 0 failed\nBuild: cargo check PASSED\n",
        )
        .await
        .expect("write outcome verification");

        let paths = existing_harness_artifact_paths(temp.path());
        assert_eq!(paths.len(), 1);
        assert_eq!(
            read_outcome_verification_summary(temp.path()),
            Some(
                "OutcomeVerification: Command: cargo nextest run | Result: 12 passed, 0 failed | Build: cargo check PASSED"
                    .to_string()
            )
        );
    }

    #[tokio::test]
    async fn writes_and_summarizes_feature_list() {
        let temp = tempdir().expect("tempdir");

        write_feature_list(
            temp.path(),
            "# Feature List\n\n- [ ] Auth: login endpoint returns JWT\n- [x] API: health check endpoint\n",
        )
        .await
        .expect("write feature list");

        let paths = existing_harness_artifact_paths(temp.path());
        assert_eq!(paths.len(), 1);
        assert_eq!(
            read_feature_list_summary(temp.path()),
            Some("FeatureList: - [ ] Auth: login endpoint returns JWT | - [x] API: health check endpoint".to_string())
        );
    }

    #[tokio::test]
    async fn all_artifacts_counted_in_existing_paths() {
        let temp = tempdir().expect("tempdir");

        write_spec(temp.path(), "# Spec\ncontent\n").await.unwrap();
        write_contract(temp.path(), "# Contract\ncontent\n").await.unwrap();
        write_evaluation(temp.path(), "# Evaluation\ncontent\n").await.unwrap();
        write_sprint_contract(temp.path(), "# Sprint\ncontent\n").await.unwrap();
        write_outcome_verification(temp.path(), "# Outcome\ncontent\n").await.unwrap();
        write_feature_list(temp.path(), "# Features\ncontent\n").await.unwrap();

        let paths = existing_harness_artifact_paths(temp.path());
        assert_eq!(paths.len(), 6);
    }

    #[test]
    fn stale_spec_summary_is_dropped_for_later_sessions() {
        let temp = tempdir().expect("tempdir");
        let spec_path = current_spec_path(temp.path());
        fs::create_dir_all(spec_path.parent().expect("parent")).expect("tasks dir");
        fs::write(&spec_path, "# Execution Spec\nExplore the codebase and summarize.\n").expect("write spec");
        // Make the fixture look like a leftover from a prior session (days old).
        let old = SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60);
        let file = fs::File::options().write(true).open(&spec_path).expect("open");
        file.set_modified(old).expect("set mtime");

        assert!(read_spec_summary(temp.path()).is_some(), "unfiltered read still sees the file");
        assert!(
            read_spec_summary_fresh(temp.path(), Some(SystemTime::now())).is_none(),
            "a leftover fixture must not describe a later session"
        );
        assert!(
            read_spec_summary_fresh(temp.path(), Some(old + std::time::Duration::from_secs(10))).is_some(),
            "fresh reads still accept artifacts written during the session"
        );
    }

    #[test]
    fn handoff_artifact_written_just_before_session_start_stays_live() {
        let temp = tempdir().expect("tempdir");
        let spec_path = current_spec_path(temp.path());
        fs::create_dir_all(spec_path.parent().expect("parent")).expect("tasks dir");
        fs::write(&spec_path, "# Spec\n\nShip the residual hygiene fix.\n").expect("write spec");

        // Session directory is created *after* the handoff spec is written.
        let session_start = SystemTime::now();
        assert!(
            read_spec_summary_fresh(temp.path(), Some(session_start)).is_some(),
            "a just-written handoff artifact must survive the freshness cutoff"
        );
    }

    #[test]
    fn archive_completed_current_task_moves_only_fully_checked_trackers() {
        let temp = tempdir().expect("tempdir");
        let task_path = current_task_path(temp.path());
        fs::create_dir_all(task_path.parent().expect("parent")).expect("tasks dir");

        fs::write(&task_path, "# Work\n\n- [ ] open item\n- [x] done item\n").expect("write partial");
        assert!(
            archive_completed_current_task(temp.path(), "session-a")
                .expect("archive")
                .is_none(),
            "incomplete checklists stay live"
        );
        assert!(task_path.exists());

        fs::write(&task_path, "# Work\n\n- [x] done one\n- [x] done two\n").expect("write complete");
        let archived = archive_completed_current_task(temp.path(), "session-a")
            .expect("archive")
            .expect("fully-checked tracker is archived");
        assert!(!task_path.exists(), "live path must be clear for the next plan");
        assert!(
            archived
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| { name.starts_with("current_task-session-a-") && name.ends_with(".md") })
        );
        assert!(archived.exists());
    }

    #[test]
    fn archive_completed_current_task_never_overwrites_a_prior_archive() {
        // Resume-in-place reuses the session id, so a second completion must
        // not rename over the first archive: the live file is gone after the
        // move and the archive is the only copy.
        let temp = tempdir().expect("tempdir");
        let task_path = current_task_path(temp.path());
        fs::create_dir_all(task_path.parent().expect("parent")).expect("tasks dir");

        fs::write(&task_path, "# Work\n\n- [x] first completion\n").expect("write complete");
        let first = archive_completed_current_task(temp.path(), "session-a")
            .expect("archive")
            .expect("first archive");
        fs::write(&task_path, "# Work\n\n- [x] second completion\n").expect("write complete");
        let second = archive_completed_current_task(temp.path(), "session-a")
            .expect("archive")
            .expect("second archive");

        assert_ne!(first, second, "each completion gets its own archive file");
        assert!(first.exists(), "first archive must survive the second completion");
        assert!(second.exists());
        assert_eq!(fs::read_to_string(&first).expect("first content"), "# Work\n\n- [x] first completion\n");
    }

    #[test]
    fn session_artifact_cutoff_is_some_for_existing_dir_and_none_for_missing() {
        let temp = tempdir().expect("tempdir");
        assert!(
            session_artifact_cutoff(temp.path(), "sess-missing").is_none(),
            "missing session dir keeps the unfiltered-read fallback"
        );

        let session_dir = temp.path().join(".vtcode").join("sessions").join("sess-live");
        fs::create_dir_all(&session_dir).expect("session dir");
        let cutoff = session_artifact_cutoff(temp.path(), "sess-live").expect("cutoff");
        assert!(
            cutoff <= SystemTime::now(),
            "an existing session dir must yield a real cutoff so the leftover filter stays enabled"
        );
    }
}
