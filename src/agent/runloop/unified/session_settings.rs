use crate::agent::runloop::model_picker::{ModelPickerState, ModelSelectionResult};
use vtcode_core::config::types::ReasoningEffortLevel;

/// A completed UI selection waiting for the next request boundary.
pub(crate) enum SessionSettingsControl {
    Model {
        picker: Box<ModelPickerState>,
        selection: ModelSelectionResult,
    },
    Effort {
        level: ReasoningEffortLevel,
        persist: bool,
    },
}
