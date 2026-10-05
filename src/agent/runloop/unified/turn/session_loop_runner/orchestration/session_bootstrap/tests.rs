use std::path::Path;

use chrono::Utc;
use tempfile::TempDir;
use vtcode_core::core::threads::{ArchivedSessionIntent, ThreadManager};
use vtcode_core::llm::provider::MessageRole;
use vtcode_core::utils::session_archive::{
    SessionContinuationMetadata, SessionForkMode, SessionListing, SessionMessage, SessionSnapshot,
};

use super::super::super::support::checkpoint_session_archive_start;
use super::{CoreAgentConfig, ResumeSession, SessionArchiveMetadata, prepare_session_thread};

fn config(workspace: &Path) -> CoreAgentConfig {
    CoreAgentConfig {
        model: "test-model".to_string(),
        api_key: String::new(),
        provider: "invalid-bootstrap-provider".to_string(),
        api_key_env: String::new(),
        workspace: workspace.to_path_buf(),
        verbose: false,
        quiet: true,
        theme: "mono".to_string(),
        reasoning_effort: Default::default(),
        ui_surface: Default::default(),
        prompt_cache: Default::default(),
        model_source: Default::default(),
        custom_api_keys: Default::default(),
        checkpointing_enabled: false,
        checkpointing_storage_dir: None,
        checkpointing_max_snapshots: 5,
        checkpointing_max_age_days: None,
        max_conversation_turns: 10,
        model_behavior: None,
        openai_chatgpt_auth: None,
    }
}

fn metadata(config: &CoreAgentConfig) -> SessionArchiveMetadata {
    SessionArchiveMetadata::new(
        "bootstrap-workspace",
        config.workspace.to_string_lossy(),
        &config.model,
        &config.provider,
        &config.theme,
        config.reasoning_effort.as_str(),
    )
}

fn resume(config: &CoreAgentConfig, intent: ArchivedSessionIntent) -> ResumeSession {
    let listing = SessionListing {
        path: config.workspace.join("session-source.json"),
        snapshot: SessionSnapshot {
            metadata: metadata(config)
                .with_prompt_cache_lineage_id("source-lineage")
                .with_continuation_metadata(Some(SessionContinuationMetadata::budget_limit(1.0, 1.25, true))),
            started_at: Utc::now(),
            ended_at: Utc::now(),
            total_messages: 2,
            distinct_tools: Vec::new(),
            transcript: Vec::new(),
            messages: vec![
                SessionMessage::new(MessageRole::User, "first user request"),
                SessionMessage::new(MessageRole::Assistant, "second assistant result"),
            ],
            progress: None,
            error_logs: Vec::new(),
        },
    };
    ResumeSession::from_listing(&listing, intent)
}

#[tokio::test]
async fn fresh_thread_without_history_uses_reserved_or_generated_identity_without_provider() {
    let temp = TempDir::new().unwrap();
    let workspace = temp.path().join("bootstrapworkspace");
    std::fs::create_dir(&workspace).unwrap();
    let config = config(&workspace);
    for reserved in [Some("reserved-fresh".to_string()), None] {
        let prepared = prepare_session_thread(&config, None, None, metadata(&config), reserved.clone(), false)
            .await
            .expect("fresh startup must not construct a provider");
        if let Some(identifier) = reserved {
            assert_eq!(prepared.thread_id, identifier);
        } else {
            assert!(prepared.thread_id.starts_with("session-bootstrapworkspace-"), "{}", prepared.thread_id);
        }
        assert!(prepared.session_archive.is_none());
        assert!(prepared.bootstrap.messages.is_empty());
        assert!(prepared.bootstrap.archive_listing.is_none());
        assert_eq!(prepared.bootstrap.metadata.as_ref().unwrap().model, "test-model");
        assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn in_place_resume_retains_identity_history_and_lineage_with_both_archive_policies() {
    let temp = TempDir::new().unwrap();
    let config = config(temp.path());
    let resume = resume(&config, ArchivedSessionIntent::ResumeInPlace);
    for history_enabled in [false, true] {
        let prepared = prepare_session_thread(
            &config,
            None,
            Some(&resume),
            metadata(&config),
            Some("unrelated-reserved-fork".to_string()),
            history_enabled,
        )
        .await
        .expect("resume bootstrap");
        assert_eq!(prepared.thread_id, "session-source");
        let meta = prepared.bootstrap.metadata.as_ref().unwrap();
        assert_eq!(meta.prompt_cache_lineage_id.as_deref(), Some("source-lineage"));
        assert_eq!(meta.continuation_metadata, resume.snapshot().metadata.continuation_metadata);
        assert_eq!(prepared.bootstrap.messages[0].content.as_text(), "first user request");
        assert_eq!(prepared.bootstrap.messages[1].content.as_text(), "second assistant result");
        assert_eq!(prepared.bootstrap.messages.len(), 2);
        assert_eq!(prepared.session_archive.is_some(), history_enabled);
        if let Some(archive) = prepared.session_archive {
            assert_eq!(archive.path(), temp.path().join("session-source.json"));
            let thread = ThreadManager::new().start_thread_with_identifier(prepared.thread_id, prepared.bootstrap);
            checkpoint_session_archive_start(&archive, &thread).await.unwrap();
            let snapshot: SessionSnapshot = serde_json::from_str(&std::fs::read_to_string(archive.path()).unwrap())
                .expect("persisted startup checkpoint");
            assert_eq!(snapshot.total_messages, 2);
            assert_eq!(snapshot.messages[0].role, MessageRole::User);
            assert_eq!(snapshot.messages[1].role, MessageRole::Assistant);
        }
    }
}

#[tokio::test]
async fn full_copy_fork_without_archive_keeps_parent_and_history_under_new_identity() {
    let temp = TempDir::new().unwrap();
    let config = config(temp.path());
    let resume = resume(
        &config,
        ArchivedSessionIntent::ForkNewArchive {
            custom_suffix: Some("branch".to_string()),
            summarize: false,
        },
    );
    let prepared = prepare_session_thread(
        &config,
        None,
        Some(&resume),
        metadata(&config),
        Some("reserved-fork".to_string()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(prepared.thread_id, "reserved-fork");
    assert!(prepared.session_archive.is_none());
    assert!(prepared.bootstrap.archive_listing.is_none());
    assert_eq!(prepared.bootstrap.messages.len(), 2);
    let meta = prepared.bootstrap.metadata.unwrap();
    assert_eq!(meta.parent_session_id.as_deref(), Some("session-source"));
    assert_eq!(meta.fork_mode, Some(SessionForkMode::FullCopy));
    assert_eq!(meta.prompt_cache_lineage_id.as_deref(), Some("source-lineage"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn summarized_fork_provider_errors_precede_archive_preparation_for_both_policies() {
    let temp = TempDir::new().unwrap();
    let config = config(temp.path());
    let resume = resume(&config, ArchivedSessionIntent::ForkNewArchive { custom_suffix: None, summarize: true });
    for history_enabled in [false, true] {
        let error = prepare_session_thread(
            &config,
            None,
            Some(&resume),
            metadata(&config),
            Some("reserved-summary".to_string()),
            history_enabled,
        )
        .await
        .err()
        .expect("invalid provider must reject summarized startup");
        assert!(format!("{error:#}").contains("invalid-bootstrap-provider"), "{error:#}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}

#[test]
fn config_poll_without_changes_leaves_session_config_and_ui_untouched() {
    let temp = TempDir::new().unwrap();
    let config = config(temp.path());
    let mut watcher = super::SimpleConfigWatcher::new(config.workspace.clone());
    watcher.set_check_interval(0);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let handle = vtcode_ui::tui::app::InlineHandle::new_for_tests(sender);
    let mut renderer = vtcode_core::utils::ansi::AnsiRenderer::with_inline_ui(handle, Default::default());
    let mut vt_cfg = None;
    super::poll_config_reload(&mut watcher, &mut vt_cfg, &config, &mut renderer, "test reload").unwrap();
    assert!(vt_cfg.is_none());
    assert!(receiver.try_recv().is_err());
}

#[test]
fn config_poll_retains_rejected_config_and_recovers_with_cli_overrides() {
    let temp = TempDir::new().unwrap();
    let mut config = config(temp.path());
    config.model_source = vtcode_core::config::types::ModelSelectionSource::CliOverride;
    let mut previous = super::VTCodeConfig::default();
    previous.agent.provider = config.provider.clone();
    previous.agent.default_model = config.model.clone();
    previous.ui.vim_mode = true;
    let mut vt_cfg = Some(previous.clone());
    let mut watcher = super::SimpleConfigWatcher::new(config.workspace.clone());
    watcher.set_check_interval(0);
    watcher.set_debounce_duration(0);
    watcher.set_last_known_config(previous);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let handle = vtcode_ui::tui::app::InlineHandle::new_for_tests(sender);
    let mut renderer = vtcode_core::utils::ansi::AnsiRenderer::with_inline_ui(handle, Default::default());
    super::poll_config_reload(&mut watcher, &mut vt_cfg, &config, &mut renderer, "test reload").unwrap();

    let path = temp.path().join("vtcode.toml");
    std::fs::write(&path, "agent.provider = [\n").unwrap();
    super::poll_config_reload(&mut watcher, &mut vt_cfg, &config, &mut renderer, "test rejection").unwrap();
    let retained = vt_cfg.as_ref().unwrap();
    assert!(retained.ui.vim_mode);
    assert_eq!(retained.agent.provider, config.provider);
    assert_eq!(retained.agent.default_model, config.model);
    let mut warnings = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        if let vtcode_ui::tui::app::InlineCommand::AppendLine { segments, .. } = command {
            warnings.push(segments.into_iter().map(|segment| segment.text).collect::<String>());
        }
    }
    assert_eq!(
        warnings
            .iter()
            .filter(|text| text.contains("Configuration reload rejected"))
            .count(),
        1
    );
    assert!(watcher.take_reload_error().is_none());

    std::fs::write(&path, "[agent]\nprovider = 'openai'\ndefault_model = 'file-model'\n[ui]\nvim_mode = false\n")
        .unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2))
        .unwrap();
    super::poll_config_reload(&mut watcher, &mut vt_cfg, &config, &mut renderer, "test recovery").unwrap();
    let reloaded = vt_cfg.as_ref().unwrap();
    assert!(!reloaded.ui.vim_mode);
    assert_eq!(reloaded.agent.provider, config.provider);
    assert_eq!(reloaded.agent.default_model, config.model);
    assert!(watcher.take_reload_error().is_none());
    assert!(receiver.try_recv().is_err());
}
