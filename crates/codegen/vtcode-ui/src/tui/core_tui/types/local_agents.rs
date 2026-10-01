use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalAgentKind {
    Delegated,
    Background,
    ExecSession,
}

impl LocalAgentKind {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Delegated => "delegated",
            Self::Background => "background",
            Self::ExecSession => "exec-session",
        }
    }
}

/// Actions that the runloop performs for a raw background exec session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecSessionAction {
    Inspect,
    GracefulTerminate,
    ForceTerminateOrClose,
    Focus,
    Preview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAgentEntry {
    pub id: String,
    pub display_label: String,
    pub agent_name: String,
    pub color: Option<String>,
    pub kind: LocalAgentKind,
    pub status: String,
    pub summary: Option<String>,
    pub preview: String,
    pub transcript_path: Option<PathBuf>,
}

impl LocalAgentEntry {
    #[must_use]
    pub(crate) fn is_loading(&self) -> bool {
        match self.kind {
            LocalAgentKind::Delegated => {
                matches!(self.status.as_str(), "queued" | "running" | "waiting")
            }
            LocalAgentKind::Background => matches!(self.status.as_str(), "starting" | "running"),
            LocalAgentKind::ExecSession => self.status == "running",
        }
    }

    /// Non-live history row used for expanded-window header counts.
    /// Live work is [`Self::is_loading`]; everything retained that is not live
    /// counts as finished for the `N running · M finished` summary.
    #[must_use]
    pub(crate) fn is_finished(&self) -> bool {
        !self.is_loading()
    }
}
