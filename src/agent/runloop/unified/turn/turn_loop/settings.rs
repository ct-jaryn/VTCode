use anyhow::Result;
use vtcode_core::llm::provider::{self as uni, LLMProvider};
use vtcode_core::utils::ansi::MessageStyle;

use super::TurnLoopContext;
use crate::agent::runloop::ui::build_inline_header_context;
use crate::agent::runloop::unified::model_selection::{ModelSwitchCompactionTargets, finalize_model_selection};
use crate::agent::runloop::unified::session_settings::SessionSettingsControl;
use crate::agent::runloop::unified::turn::session::slash_commands::persist_effort_preference;

/// Consume completed selections just before the next model request is built.
pub(super) async fn apply_pending_session_settings(
    ctx: &mut TurnLoopContext<'_>,
    working_history: &mut Vec<uni::Message>,
) -> Result<bool> {
    let mut controls = Vec::new();
    if let Some(settings) = ctx.settings.as_mut() {
        while let Ok(control) = settings.receiver.try_recv() {
            controls.push(control);
        }
    }
    let applied = !controls.is_empty();

    for control in controls {
        match control {
            SessionSettingsControl::Model { picker, selection } => {
                let settings = ctx.settings.as_mut().expect("settings context exists while applying selection");
                let vt_cfg = ctx
                    .live_vt_cfg
                    .as_deref_mut()
                    .expect("live config exists while applying selection");
                let snapshot = ctx.tool_registry.harness_context_snapshot();
                if let Err(error) = finalize_model_selection(
                    ctx.renderer,
                    &picker,
                    selection,
                    ctx.config,
                    vt_cfg,
                    ctx.provider_client,
                    settings.session_bootstrap,
                    ctx.handle,
                    settings.header_context,
                    ctx.full_auto,
                    ModelSwitchCompactionTargets {
                        history: working_history,
                        session_stats: ctx.session_stats,
                        context_manager: ctx.context_manager,
                        session_id: &snapshot.session_id,
                        thread_id: settings.thread_id,
                        lifecycle_hooks: ctx.lifecycle_hooks,
                        harness_emitter: ctx.harness_emitter,
                    },
                )
                .await
                {
                    ctx.renderer
                        .line(MessageStyle::Error, &format!("Failed to apply model selection: {error:#}"))?;
                } else if let Some(mut metadata) = settings.thread_handle.metadata() {
                    metadata.provider.clone_from(&ctx.config.provider);
                    metadata.model.clone_from(&ctx.config.model);
                    metadata.reasoning_effort = ctx.config.reasoning_effort.as_str().to_string();
                    settings.thread_handle.replace_metadata(Some(metadata));
                }
            }
            SessionSettingsControl::Effort { level, persist } => {
                let supported = supported_efforts(ctx.provider_client.as_ref(), &ctx.config.model);
                if !supported.contains(&level) {
                    ctx.renderer.line(
                        MessageStyle::Error,
                        &format!("Effort level '{}' is not supported by '{}'.", level.as_str(), ctx.config.model),
                    )?;
                    continue;
                }
                let vt_cfg = ctx
                    .live_vt_cfg
                    .as_deref_mut()
                    .expect("live config exists while applying selection");
                if persist {
                    if let Err(error) = persist_effort_preference(ctx.config.workspace.as_path(), vt_cfg, level) {
                        ctx.renderer
                            .line(MessageStyle::Error, &format!("Failed to persist effort: {error:#}"))?;
                        continue;
                    }
                }
                let cfg = vt_cfg.get_or_insert_with(Default::default);
                cfg.agent.reasoning_effort = level;
                ctx.config.reasoning_effort = level;
                let settings = ctx.settings.as_mut().expect("settings context exists while applying selection");
                if let Some(mut metadata) = settings.thread_handle.metadata() {
                    metadata.reasoning_effort = level.as_str().to_string();
                    settings.thread_handle.replace_metadata(Some(metadata));
                }
                let provider_label = ctx.config.provider.clone();
                let header = build_inline_header_context(
                    ctx.config,
                    Some(cfg),
                    settings.session_bootstrap,
                    provider_label,
                    ctx.config.model.clone(),
                    ctx.provider_client.effective_context_size(&ctx.config.model),
                    level.as_str().to_string(),
                )
                .await?;
                settings.header_context.clone_from(&header);
                ctx.handle.set_header_context(header);
                ctx.renderer
                    .line(MessageStyle::Info, &format!("Effort set to {} for the next request.", level.as_str()))?;
            }
        }
    }
    Ok(applied)
}

fn supported_efforts(provider: &dyn LLMProvider, model: &str) -> Vec<vtcode_core::config::types::ReasoningEffortLevel> {
    use vtcode_core::config::types::ReasoningEffortLevel;
    provider
        .supported_reasoning_efforts(model)
        .iter()
        .filter_map(|value| ReasoningEffortLevel::parse(value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::supported_efforts;
    use crate::agent::runloop::unified::session_settings::SessionSettingsControl;
    use vtcode_core::config::types::ReasoningEffortLevel;

    struct StubProvider {
        efforts: &'static [&'static str],
    }

    #[async_trait::async_trait]
    impl super::LLMProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn generate(
            &self,
            _request: super::uni::LLMRequest,
        ) -> Result<super::uni::LLMResponse, super::uni::LLMError> {
            panic!("stub provider never generates in settings tests")
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["stub-model".to_string()]
        }

        fn supported_reasoning_efforts(&self, _model: &str) -> &'static [&'static str] {
            self.efforts
        }

        fn validate_request(&self, _request: &super::uni::LLMRequest) -> Result<(), super::uni::LLMError> {
            Ok(())
        }
    }

    #[test]
    fn supported_efforts_parses_provider_strings_and_drops_unknown() {
        let provider = StubProvider { efforts: &["low", "medium", "bogus"] };
        let efforts = supported_efforts(&provider, "stub-model");
        assert_eq!(efforts, vec![ReasoningEffortLevel::Low, ReasoningEffortLevel::Medium]);
    }

    #[test]
    fn supported_efforts_empty_when_provider_exposes_none() {
        let provider = StubProvider { efforts: &[] };
        assert!(supported_efforts(&provider, "stub-model").is_empty());
    }

    #[test]
    fn effective_config_prefers_live_over_fallback() {
        use super::super::effective_vt_cfg;
        use vtcode_core::config::loader::VTCodeConfig;
        let mut fallback = VTCodeConfig::default();
        fallback.agent.reasoning_effort = ReasoningEffortLevel::Low;
        let mut live_value = Some(VTCodeConfig::default());
        if let Some(cfg) = live_value.as_mut() {
            cfg.agent.reasoning_effort = ReasoningEffortLevel::High;
        }
        let mut live: Option<&mut Option<VTCodeConfig>> = Some(&mut live_value);
        let effective = effective_vt_cfg(Some(&fallback), &live).expect("live config wins");
        assert_eq!(effective.agent.reasoning_effort, ReasoningEffortLevel::High);

        let mut live_none: Option<VTCodeConfig> = None;
        let mut live_empty: Option<&mut Option<VTCodeConfig>> = Some(&mut live_none);
        let effective_fallback =
            effective_vt_cfg(Some(&fallback), &live_empty).expect("fallback used when live is empty");
        assert_eq!(effective_fallback.agent.reasoning_effort, ReasoningEffortLevel::Low);
        let _ = &mut live;
        let _ = &mut live_empty;
    }

    /// Drain effort controls through the real request-boundary entry point on a
    /// headless turn context. Returns (applied, live config, header, rendered).
    async fn drain_effort_controls(
        controls: Vec<SessionSettingsControl>,
    ) -> (
        bool,
        Option<vtcode_core::config::loader::VTCodeConfig>,
        vtcode_ui::tui::app::InlineHeaderContext,
        String,
    ) {
        use crate::agent::runloop::unified::turn::turn_processing::test_support::TestTurnProcessingBacking;
        use crate::agent::runloop::welcome::SessionBootstrap;

        let mut backing = TestTurnProcessingBacking::new(4).await;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        for control in controls {
            sender.send(control).expect("queue boundary control");
        }
        drop(sender);
        let mut header = vtcode_ui::tui::app::InlineHeaderContext::default();
        let bootstrap = SessionBootstrap::default();
        let manager = vtcode_core::core::threads::ThreadManager::new();
        let thread = manager.start_thread_with_identifier(
            "test-thread".to_string(),
            vtcode_core::core::threads::ThreadBootstrap::new(None),
        );
        let thread_id = thread.thread_id().to_string();
        let mut live_cfg: Option<vtcode_core::config::loader::VTCodeConfig> = None;
        let mut working_history = Vec::new();
        let applied = {
            let mut ctx = backing.turn_loop_context();
            ctx.live_vt_cfg = Some(&mut live_cfg);
            ctx.settings = Some(super::super::ActiveSettingsContext {
                receiver: &mut receiver,
                header_context: &mut header,
                session_bootstrap: &bootstrap,
                thread_id: thread_id.as_str(),
                thread_handle: &thread,
            });
            super::apply_pending_session_settings(&mut ctx, &mut working_history)
                .await
                .expect("boundary drain succeeds")
        };
        let rendered = backing.rendered_inline_output();
        (applied, live_cfg, header, rendered)
    }

    fn effort_control(level: ReasoningEffortLevel) -> SessionSettingsControl {
        SessionSettingsControl::Effort { level, persist: false }
    }

    #[tokio::test]
    async fn pending_effort_applies_at_next_request_boundary() {
        let (applied, live_cfg, header, rendered) =
            drain_effort_controls(vec![effort_control(ReasoningEffortLevel::High)]).await;

        assert!(applied, "a queued selection must report applied");
        assert_eq!(
            live_cfg.expect("live config must exist after apply").agent.reasoning_effort,
            ReasoningEffortLevel::High,
            "boundary must update the effective setting"
        );
        assert!(header.reasoning.contains("high"), "header must reflect the effective setting");
        assert!(rendered.contains("high"), "selection must be acknowledged: {rendered}");
    }

    #[tokio::test]
    async fn unsupported_effort_leaves_effective_setting_unchanged() {
        let (applied, live_cfg, header, rendered) =
            drain_effort_controls(vec![effort_control(ReasoningEffortLevel::Minimal)]).await;

        assert!(applied, "the unsupported selection is still consumed");
        assert!(live_cfg.is_none(), "live config must stay untouched on unsupported effort");
        assert_eq!(header.reasoning, "unavailable", "header must keep the prior setting");
        assert!(rendered.contains("not supported"), "failure must surface: {rendered}");
    }

    #[tokio::test]
    async fn no_pending_controls_leaves_turn_untouched() {
        let (applied, live_cfg, header, rendered) = drain_effort_controls(vec![]).await;

        assert!(!applied, "empty queue must report nothing applied");
        assert!(live_cfg.is_none());
        assert_eq!(header.reasoning, "unavailable");
        assert!(rendered.is_empty(), "nothing may render without a selection: {rendered}");
    }

    #[tokio::test]
    async fn queued_controls_drain_in_order_last_wins() {
        let (applied, live_cfg, header, _) = drain_effort_controls(vec![
            effort_control(ReasoningEffortLevel::High),
            effort_control(ReasoningEffortLevel::Low),
        ])
        .await;

        assert!(applied);
        assert_eq!(
            live_cfg.expect("live config must exist").agent.reasoning_effort,
            ReasoningEffortLevel::Low,
            "controls must apply in queue order"
        );
        assert!(header.reasoning.contains("low"));
    }
}
