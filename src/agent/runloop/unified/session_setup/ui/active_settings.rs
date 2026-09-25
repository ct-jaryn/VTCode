use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::config::types::{AgentConfig, ReasoningEffortLevel};
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};
use vtcode_ui::tui::app::{
    InlineEvent, InlineHandle, InlineListItem, InlineListSelection, TransientEvent, TransientSubmission,
};

use crate::agent::runloop::model_picker::{ModelPickerProgress, ModelPickerStart, ModelPickerState};
use crate::agent::runloop::slash_commands::parse_effort_args;
use crate::agent::runloop::unified::session_settings::SessionSettingsControl;
use crate::agent::runloop::unified::state::CtrlCState;
use crate::agent::runloop::unified::turn::session::slash_commands::effort_description;

enum Picker {
    Model(Box<ModelPickerState>),
    Effort { persist: bool },
}

/// Picker rows for the busy-turn effort selector. Mirrors the idle
/// `/effort` rows (title, description subtitle, current badge, search text)
/// so both surfaces describe levels identically.
fn effort_picker_items(current: ReasoningEffortLevel, model: &str) -> Vec<InlineListItem> {
    ReasoningEffortLevel::allowed_values()
        .iter()
        .filter_map(|value| ReasoningEffortLevel::parse(value))
        .map(|level| InlineListItem {
            title: level.as_str().to_string(),
            subtitle: Some(effort_description(level, model).to_string()),
            badge: (level == current).then_some("Current".to_string()),
            indent: 0,
            selection: Some(InlineListSelection::ConfigAction(format!("effort:{}", level.as_str()))),
            search_value: Some(format!("{} {}", level.as_str(), effort_description(level, model))),
        })
        .collect()
}

pub(super) async fn run(
    mut events: mpsc::UnboundedReceiver<InlineEvent>,
    settings: mpsc::UnboundedSender<SessionSettingsControl>,
    handle: InlineHandle,
    config: AgentConfig,
    vt_cfg: Option<VTCodeConfig>,
    ctrl_c_state: Arc<CtrlCState>,
    ctrl_c_notify: Arc<Notify>,
) {
    let mut renderer = AnsiRenderer::with_inline_ui(handle.clone(), Default::default());
    let mut picker = None;
    let mut provider = config.provider;
    let mut model = config.model;
    let mut effort = config.reasoning_effort;
    while let Some(event) = events.recv().await {
        match event {
            InlineEvent::Steer(input) => {
                let command = input.text.trim();
                let (is_model, model_has_args) = match command.strip_prefix("/model") {
                    Some(rest) => (rest.is_empty() || rest.starts_with(char::is_whitespace), !rest.is_empty()),
                    None => (false, false),
                };
                if is_model {
                    if picker.is_some() {
                        let _ =
                            renderer.line(MessageStyle::Error, "Complete or cancel the current settings picker first.");
                        continue;
                    }
                    if model_has_args {
                        let _ = renderer.line(
                            MessageStyle::Info,
                            "The busy-turn model picker takes no arguments; opening the picker.",
                        );
                    }
                    match ModelPickerState::new(
                        &mut renderer,
                        vt_cfg.clone(),
                        effort,
                        vt_cfg.as_ref().and_then(|cfg| cfg.provider.openai.service_tier),
                        Some(config.workspace.clone()),
                        provider.clone(),
                        model.clone(),
                        Some(Arc::clone(&ctrl_c_state)),
                        Some(Arc::clone(&ctrl_c_notify)),
                    )
                    .await
                    {
                        Ok(ModelPickerStart::InProgress(state)) => picker = Some(Picker::Model(Box::new(state))),
                        Ok(ModelPickerStart::Completed { state, selection }) => {
                            provider.clone_from(&selection.provider);
                            model.clone_from(&selection.model);
                            effort = selection.reasoning;
                            let _ = settings.send(SessionSettingsControl::Model { picker: Box::new(state), selection });
                            let _ = renderer.line(MessageStyle::Info, "Model selected; pending the next request.");
                        }
                        Ok(ModelPickerStart::Exit) => {}
                        Err(error) => {
                            let _ =
                                renderer.line(MessageStyle::Error, &format!("Failed to start model picker: {error:#}"));
                        }
                    }
                } else if let Some(args) = command.strip_prefix("/effort") {
                    if !args.is_empty() && !args.starts_with(char::is_whitespace) {
                        continue;
                    }
                    if picker.is_some() {
                        let _ =
                            renderer.line(MessageStyle::Error, "Complete or cancel the current settings picker first.");
                        continue;
                    }
                    match parse_effort_args(args) {
                        Ok((Some(level), persist)) => {
                            effort = level;
                            let _ = settings.send(SessionSettingsControl::Effort { level, persist });
                            let _ = renderer.line(
                                MessageStyle::Info,
                                &format!("Effort {level} selected; pending the next request."),
                            );
                        }
                        Ok((None, persist)) => {
                            let items = effort_picker_items(effort, &model);
                            handle.show_list_modal(
                                "Effort level".to_string(),
                                vec![format!("Select effort for {model}; it applies on the next request.")],
                                items,
                                Some(InlineListSelection::ConfigAction(format!("effort:{}", effort.as_str()))),
                                None,
                            );
                            picker = Some(Picker::Effort { persist });
                        }
                        Err(error) => {
                            let _ = renderer.line(MessageStyle::Error, &error);
                        }
                    }
                }
            }
            InlineEvent::Transient(TransientEvent::Submitted(TransientSubmission::Selection(selection))) => {
                match picker.take() {
                    Some(Picker::Model(mut state)) => match state.handle_list_selection(&mut renderer, selection) {
                        Ok(ModelPickerProgress::Completed(selection)) => {
                            provider.clone_from(&selection.provider);
                            model.clone_from(&selection.model);
                            effort = selection.reasoning;
                            let _ = settings.send(SessionSettingsControl::Model { picker: state, selection });
                            let _ = renderer.line(MessageStyle::Info, "Model selected; pending the next request.");
                        }
                        Ok(ModelPickerProgress::NeedsRefresh) => {
                            if let Err(error) = state.refresh_dynamic_models(&mut renderer).await {
                                let _ = renderer
                                    .line(MessageStyle::Error, &format!("Model list refresh failed: {error:#}"));
                            }
                            picker = Some(Picker::Model(state));
                        }
                        Ok(ModelPickerProgress::InProgress) => picker = Some(Picker::Model(state)),
                        Ok(ModelPickerProgress::Cancelled | ModelPickerProgress::Exit) => {}
                        Err(error) => {
                            let _ = renderer.line(MessageStyle::Error, &format!("Invalid model selection: {error:#}"));
                            picker = Some(Picker::Model(state));
                        }
                    },
                    Some(Picker::Effort { persist }) => {
                        let level = match selection {
                            InlineListSelection::ConfigAction(action) => {
                                action.strip_prefix("effort:").and_then(ReasoningEffortLevel::parse)
                            }
                            _ => None,
                        };
                        if let Some(level) = level {
                            effort = level;
                            let _ = settings.send(SessionSettingsControl::Effort { level, persist });
                            let _ = renderer.line(
                                MessageStyle::Info,
                                &format!("Effort {level} selected; pending the next request."),
                            );
                        } else {
                            let _ = renderer.line(MessageStyle::Error, "Invalid effort selection.");
                            picker = Some(Picker::Effort { persist });
                        }
                    }
                    None => {}
                }
            }
            InlineEvent::Transient(TransientEvent::Cancelled) => picker = None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::effort_picker_items;
    use vtcode_core::config::types::ReasoningEffortLevel;

    #[test]
    fn busy_effort_rows_mirror_idle_descriptions() {
        let items = effort_picker_items(ReasoningEffortLevel::Medium, "some-model");
        assert!(!items.is_empty(), "picker must offer levels");
        for item in &items {
            assert!(item.subtitle.as_deref().is_some_and(|text| !text.is_empty()));
            assert!(item.search_value.as_deref().is_some_and(|text| text.contains(&item.title)));
        }
        let current = items.iter().find(|item| item.title == "medium").expect("medium row exists");
        assert_eq!(current.badge.as_deref(), Some("Current"));
        assert!(
            items
                .iter()
                .filter(|item| item.title != "medium")
                .all(|item| item.badge.is_none())
        );
    }

    #[test]
    fn busy_effort_rows_use_model_specific_copy() {
        let generic = effort_picker_items(ReasoningEffortLevel::High, "other-model");
        let opus = effort_picker_items(ReasoningEffortLevel::High, "claude-opus-5");
        let generic_xhigh = generic.iter().find(|item| item.title == "xhigh").expect("xhigh row");
        let opus_xhigh = opus.iter().find(|item| item.title == "xhigh").expect("xhigh row");
        assert_ne!(
            generic_xhigh.subtitle, opus_xhigh.subtitle,
            "model-specific descriptions must flow through to the busy picker"
        );
    }
}
