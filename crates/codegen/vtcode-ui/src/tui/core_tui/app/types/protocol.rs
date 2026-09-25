use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use chrono::{DateTime, Utc};
use hashbrown::HashMap;
use tokio::sync::{
    Notify,
    mpsc::{UnboundedReceiver, UnboundedSender},
};
use unicode_width::UnicodeWidthStr;
use vtcode_commons::ui_protocol::{CompactActivityMetadata, SlashCommandItem, TaskItemStatus, ToolOutputId};

use super::overlay::{
    AgentPaletteItem, AgentPaletteTransientRequest, FilePaletteTransientRequest, ListOverlayRequest,
    LocalAgentsTransientRequest, ModalOverlayRequest, TaskPanelMetadata, TaskPanelTransientRequest, TransientEvent,
    TransientRequest,
};
use crate::tui::core_tui::session::config::AppearanceConfig;
pub use crate::tui::core_tui::types::SubmittedInput;
use crate::tui::core_tui::types::{
    ActivityState, ExecSessionAction, InlineHeaderContext, InlineLinkRange, InlineListItem, InlineListSearchConfig,
    InlineListSelection, InlineMessageKind, InlineSegment, InlineTextStyle, InlineTheme, LocalAgentEntry,
    SecurePromptConfig,
};

const MAX_DEFERRED_EVENTS: usize = 32;

/// Shared state and wake-up signal for deferred bridge input.
///
/// The UI can close a captured-input surface without emitting an inline event
/// (for example, Esc on the local-agents drawer). A boolean alone cannot wake
/// an already waiting [`InlineSession::next_event`], so the state carries a
/// notification alongside the atomic flag.
#[derive(Default)]
pub(crate) struct TransientActivitySignal {
    active: AtomicBool,
    changed: Notify,
}

impl TransientActivitySignal {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn set_active(&self, active: bool) {
        let previous = self.active.swap(active, Ordering::AcqRel);
        if previous != active {
            self.changed.notify_one();
        }
    }

    async fn wait_for_change(&self) {
        self.changed.notified().await;
    }
}

/// A user prompt from a previous session archive, used to populate the history picker.
#[derive(Debug, Clone)]
pub struct ArchivedPromptEntry {
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub session_label: String,
}

pub enum InlineCommand {
    AppendLine {
        kind: InlineMessageKind,
        segments: Vec<InlineSegment>,
    },
    AppendPastedMessage {
        kind: InlineMessageKind,
        text: String,
        line_count: usize,
    },
    Inline {
        kind: InlineMessageKind,
        segment: InlineSegment,
    },
    ReplaceLast {
        count: usize,
        kind: InlineMessageKind,
        lines: Vec<Vec<InlineSegment>>,
        link_ranges: Option<Vec<Vec<InlineLinkRange>>>,
    },
    /// Retain one complete tool-call capture for the session-local viewer.
    RecordToolOutput {
        id: ToolOutputId,
        lines: Vec<String>,
    },
    /// Append a summary line and associate it with a previously recorded
    /// capture. This is a UI-only identity edge, not a transcript event.
    AppendToolOutputLine {
        id: ToolOutputId,
        kind: InlineMessageKind,
        segments: Vec<InlineSegment>,
    },
    /// Append a compact successful-command activity row.
    ///
    /// UI-only identity edge derived from the canonical `ThreadEvent` tool
    /// outcome, not a new source of truth: grouping must stay consistent with
    /// `vtcode_commons::ui_protocol::tool_summary` boundaries.
    AppendCompactActivity(CompactActivityMetadata),
    /// Store a completed-edit review payload for explicit expand activation.
    RecordDiffReview(vtcode_commons::ui_protocol::DiffReviewAnchor),
    /// Replace the current compact successful-command activity row with an
    /// updated contiguous group.
    ///
    /// UI-only identity edge; see [`InlineCommand::AppendCompactActivity`].
    /// Only contiguous successful command activity may group; transient PTY
    /// rows stay separate.
    ReplaceCompactActivity(CompactActivityMetadata),
    /// Replace the live PTY preview block with a compact activity row after
    /// the command has completed. Complete output is retained separately.
    ///
    /// UI-only identity edge; see [`InlineCommand::AppendCompactActivity`].
    CollapsePtyBlock(CompactActivityMetadata),
    SetPrompt {
        prefix: String,
        style: InlineTextStyle,
    },
    SetPlaceholder {
        hint: Option<String>,
        style: Option<InlineTextStyle>,
    },
    SetMessageLabels {
        agent: Option<String>,
        user: Option<String>,
    },
    SetHeaderContext {
        context: Box<InlineHeaderContext>,
    },
    SetInputStatus {
        left: Option<String>,
        right: Option<String>,
    },
    SetActivityState(ActivityState),
    SetTerminalTitleItems {
        items: Option<Vec<String>>,
    },
    SetTerminalTitleThreadLabel {
        label: Option<String>,
    },
    SetTerminalTitleGitBranch {
        branch: Option<String>,
    },
    SetTheme {
        theme: InlineTheme,
    },
    SetColorSchemeAuto {
        enabled: bool,
    },
    SetAppearance {
        appearance: AppearanceConfig,
    },
    /// Replace the live action bindings after a valid configuration reload.
    SetKeyBindings {
        bindings: HashMap<String, Vec<String>>,
    },
    SetVimModeEnabled(bool),
    SetQueuedInputs {
        entries: Vec<String>,
    },
    SetSubprocessEntries {
        entries: Vec<String>,
    },
    SetSubagentPreview {
        text: Option<String>,
    },
    SetLocalAgents {
        entries: Vec<LocalAgentEntry>,
    },
    /// Inject archived prompts from previous sessions into the history picker.
    SetArchivedHistory {
        entries: Vec<ArchivedPromptEntry>,
    },
    SetPrimaryAgent {
        name: Option<String>,
        color: Option<String>,
    },
    SetCursorVisible(bool),
    SetInputEnabled(bool),
    SetImageInputEnabled(bool),
    SetInput(String),
    RestoreInputDraft(SubmittedInput),
    ApplySuggestedPrompt(String),
    SetInlinePromptSuggestion {
        suggestion: String,
        llm_generated: bool,
    },
    ClearInlinePromptSuggestion,
    ClearInput,
    ForceRedraw,
    ShowTransient {
        request: Box<TransientRequest>,
    },
    /// Deliver the full recursive workspace file list (discovered in the
    /// background) so the file palette's Search mode can match against it.
    UpdateFilePaletteSearch {
        files: Vec<String>,
    },
    /// Replace the slash-command palette after background prompt-template
    /// discovery. Lets first paint spawn with built-ins only; templates merge
    /// in without blocking `spawn_session_with_options`.
    SetSlashCommands {
        commands: Vec<SlashCommandItem>,
    },
    CloseTransient,
    ClearScreen,
    SuspendEventLoop,
    ResumeEventLoop,
    ClearInputQueue,
    StopEventStream,
    StartEventStream,
    SetSkipConfirmations(bool),
    Shutdown,
    /// Update reasoning stage in header context
    SetReasoningStage(Option<String>),
}

#[derive(Debug, Clone)]
pub enum InlineEvent {
    Submit(SubmittedInput),
    /// Submit text from the WebMCP bridge without interpreting slash commands.
    WebmcpSubmit(SubmittedInput),
    QueueSubmit(SubmittedInput),
    Steer(SubmittedInput),
    ProcessLatestQueued,
    /// Edit the newest queued input (pop into input buffer)
    EditQueue,
    Transient(TransientEvent),
    Cancel,
    Exit,
    Interrupt,
    Pause,
    Resume,
    BackgroundOperation,
    ExecSessionAction {
        id: String,
        action: ExecSessionAction,
    },
    ScrollLineUp,
    ScrollLineDown,
    ScrollPageUp,
    ScrollPageDown,
    FileSelected(String),
    OpenFileInEditor(String),
    OpenUrl(String),
    LaunchEditor {
        draft: String,
    },
    OpenToolOutputInEditor(String),
    OpenToolOutputScrollback(String),
    ForceCancelPtySession,
    RequestInlinePromptSuggestion(String),
    CyclePrimaryAgent,
    CyclePrimaryAgentPrevious,
    SelectPrimaryAgent {
        name: Option<String>,
    },
    HistoryPrevious,
    HistoryNext,
    ToggleToolDisplayMode,
}

pub type InlineEventCallback = Arc<dyn Fn(&InlineEvent) + Send + Sync + 'static>;

impl From<crate::tui::core_tui::types::InlineEvent> for InlineEvent {
    fn from(value: crate::tui::core_tui::types::InlineEvent) -> Self {
        match value {
            crate::tui::core_tui::types::InlineEvent::Submit(text) => Self::Submit(text),
            crate::tui::core_tui::types::InlineEvent::QueueSubmit(text) => Self::QueueSubmit(text),
            crate::tui::core_tui::types::InlineEvent::Steer(text) => Self::Steer(text),
            crate::tui::core_tui::types::InlineEvent::ProcessLatestQueued => Self::ProcessLatestQueued,
            crate::tui::core_tui::types::InlineEvent::EditQueue => Self::EditQueue,
            crate::tui::core_tui::types::InlineEvent::Overlay(event) => Self::Transient(event.into()),
            crate::tui::core_tui::types::InlineEvent::Cancel => Self::Cancel,
            crate::tui::core_tui::types::InlineEvent::Exit => Self::Exit,
            crate::tui::core_tui::types::InlineEvent::Interrupt => Self::Interrupt,
            crate::tui::core_tui::types::InlineEvent::Pause => Self::Pause,
            crate::tui::core_tui::types::InlineEvent::Resume => Self::Resume,
            crate::tui::core_tui::types::InlineEvent::BackgroundOperation => Self::BackgroundOperation,
            crate::tui::core_tui::types::InlineEvent::ExecSessionAction { id, action } => {
                Self::ExecSessionAction { id, action }
            }
            crate::tui::core_tui::types::InlineEvent::ScrollLineUp => Self::ScrollLineUp,
            crate::tui::core_tui::types::InlineEvent::ScrollLineDown => Self::ScrollLineDown,
            crate::tui::core_tui::types::InlineEvent::ScrollPageUp => Self::ScrollPageUp,
            crate::tui::core_tui::types::InlineEvent::ScrollPageDown => Self::ScrollPageDown,
            crate::tui::core_tui::types::InlineEvent::OpenFileInEditor(path) => Self::OpenFileInEditor(path),
            crate::tui::core_tui::types::InlineEvent::OpenUrl(url) => Self::OpenUrl(url),
            crate::tui::core_tui::types::InlineEvent::LaunchEditor { draft } => Self::LaunchEditor { draft },
            crate::tui::core_tui::types::InlineEvent::ForceCancelPtySession => Self::ForceCancelPtySession,
            crate::tui::core_tui::types::InlineEvent::RequestInlinePromptSuggestion(draft) => {
                Self::RequestInlinePromptSuggestion(draft)
            }
            crate::tui::core_tui::types::InlineEvent::CyclePrimaryAgent => Self::CyclePrimaryAgent,
            crate::tui::core_tui::types::InlineEvent::CyclePrimaryAgentPrevious => Self::CyclePrimaryAgentPrevious,
            crate::tui::core_tui::types::InlineEvent::SelectPrimaryAgent { name } => Self::SelectPrimaryAgent { name },
            crate::tui::core_tui::types::InlineEvent::HistoryPrevious => Self::HistoryPrevious,
            crate::tui::core_tui::types::InlineEvent::HistoryNext => Self::HistoryNext,
            crate::tui::core_tui::types::InlineEvent::ToggleToolDisplayMode => Self::ToggleToolDisplayMode,
        }
    }
}

#[derive(Default)]
struct InlineLayoutState {
    agent_label_frame_width: AtomicUsize,
}

#[derive(Clone)]
pub struct InlineHandle {
    pub(crate) sender: UnboundedSender<InlineCommand>,
    message_layout: Arc<InlineLayoutState>,
    deferred_events: Arc<Mutex<VecDeque<InlineEvent>>>,
    transient_activity: Arc<TransientActivitySignal>,
    /// Whether the app session owns the authoritative transient stack. A
    /// standalone handle has no stack to resync after a close, so it updates
    /// the signal eagerly; the UI-owned handle waits for AppSession to expose
    /// the next visible surface before releasing input ownership.
    ui_owned_transient_activity: bool,
    next_tool_output_id: Arc<AtomicU64>,
}

impl InlineHandle {
    pub fn new_for_tests(sender: UnboundedSender<InlineCommand>) -> Self {
        Self::new(sender)
    }

    pub(crate) fn new(sender: UnboundedSender<InlineCommand>) -> Self {
        Self::new_with_transient_signal_and_ownership(sender, Arc::new(TransientActivitySignal::default()), false)
    }

    pub(crate) fn new_with_transient_signal(
        sender: UnboundedSender<InlineCommand>,
        transient_activity: Arc<TransientActivitySignal>,
    ) -> Self {
        Self::new_with_transient_signal_and_ownership(sender, transient_activity, true)
    }

    fn new_with_transient_signal_and_ownership(
        sender: UnboundedSender<InlineCommand>,
        transient_activity: Arc<TransientActivitySignal>,
        ui_owned_transient_activity: bool,
    ) -> Self {
        Self {
            sender,
            message_layout: Arc::new(InlineLayoutState::default()),
            deferred_events: Arc::new(Mutex::new(VecDeque::new())),
            transient_activity,
            ui_owned_transient_activity,
            next_tool_output_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Defer an input event until the current transient overlay closes.
    pub fn defer_event(&self, event: InlineEvent) -> anyhow::Result<()> {
        let mut deferred_events = self
            .deferred_events
            .lock()
            .map_err(|error| anyhow::anyhow!("deferred input queue poisoned: {error}"))?;
        if deferred_events.len() >= MAX_DEFERRED_EVENTS {
            return Err(anyhow::anyhow!("deferred input queue is full"));
        }
        deferred_events.push_back(event);
        Ok(())
    }

    fn take_deferred_event(&self) -> Option<InlineEvent> {
        self.deferred_events.lock().ok()?.pop_front()
    }

    pub fn has_deferred_event(&self) -> bool {
        self.deferred_events.lock().is_ok_and(|events| !events.is_empty())
    }

    fn transient_active(&self) -> bool {
        self.transient_activity.is_active()
    }

    async fn wait_for_transient_activity_change(&self) {
        self.transient_activity.wait_for_change().await;
    }

    fn send_command(&self, command: InlineCommand) {
        if self.sender.is_closed() {
            return;
        }
        let _ = self.sender.send(command);
    }

    pub fn append_line(&self, kind: InlineMessageKind, segments: Vec<InlineSegment>) {
        self.send_command(InlineCommand::AppendLine { kind, segments });
    }

    pub fn append_pasted_message(&self, kind: InlineMessageKind, text: String, line_count: usize) {
        self.send_command(InlineCommand::AppendPastedMessage { kind, text, line_count });
    }

    pub fn inline(&self, kind: InlineMessageKind, segment: InlineSegment) {
        self.send_command(InlineCommand::Inline { kind, segment });
    }

    pub fn replace_last(&self, count: usize, kind: InlineMessageKind, lines: Vec<Vec<InlineSegment>>) {
        self.send_command(InlineCommand::ReplaceLast { count, kind, lines, link_ranges: None });
    }

    pub fn replace_last_with_links(
        &self,
        count: usize,
        kind: InlineMessageKind,
        lines: Vec<Vec<InlineSegment>>,
        link_ranges: Vec<Vec<InlineLinkRange>>,
    ) {
        self.send_command(InlineCommand::ReplaceLast { count, kind, lines, link_ranges: Some(link_ranges) });
    }

    pub fn record_tool_output(&self, lines: Vec<String>) -> ToolOutputId {
        let id = self.next_tool_output_id.fetch_add(1, Ordering::Relaxed);
        self.send_command(InlineCommand::RecordToolOutput { id, lines });
        id
    }

    pub fn append_tool_output_line(&self, id: ToolOutputId, kind: InlineMessageKind, segments: Vec<InlineSegment>) {
        self.send_command(InlineCommand::AppendToolOutputLine { id, kind, segments });
    }

    pub fn append_compact_activity(&self, activity: CompactActivityMetadata) {
        self.send_command(InlineCommand::AppendCompactActivity(activity));
    }
    pub fn record_diff_review(&self, anchor: vtcode_commons::ui_protocol::DiffReviewAnchor) {
        self.send_command(InlineCommand::RecordDiffReview(anchor));
    }

    pub fn replace_compact_activity(&self, activity: CompactActivityMetadata) {
        self.send_command(InlineCommand::ReplaceCompactActivity(activity));
    }

    pub fn collapse_pty_block(&self, activity: CompactActivityMetadata) {
        self.send_command(InlineCommand::CollapsePtyBlock(activity));
    }

    pub fn suspend_event_loop(&self) {
        self.send_command(InlineCommand::SuspendEventLoop);
    }

    pub fn resume_event_loop(&self) {
        self.send_command(InlineCommand::ResumeEventLoop);
    }

    pub fn clear_input_queue(&self) {
        self.send_command(InlineCommand::ClearInputQueue);
    }

    pub fn stop_event_stream(&self) {
        self.send_command(InlineCommand::StopEventStream);
    }

    pub fn start_event_stream(&self) {
        self.send_command(InlineCommand::StartEventStream);
    }

    pub fn set_prompt(&self, prefix: String, style: InlineTextStyle) {
        self.send_command(InlineCommand::SetPrompt { prefix, style });
    }

    pub fn set_placeholder(&self, hint: Option<String>) {
        self.set_placeholder_with_style(hint, None);
    }

    fn set_placeholder_with_style(&self, hint: Option<String>, style: Option<InlineTextStyle>) {
        self.send_command(InlineCommand::SetPlaceholder { hint, style });
    }

    pub fn set_message_labels(&self, agent: Option<String>, user: Option<String>) {
        let agent_label_frame_width = agent
            .as_deref()
            .filter(|label| !label.is_empty())
            .map(|label| UnicodeWidthStr::width(label) + 1)
            .unwrap_or_default();
        self.message_layout
            .agent_label_frame_width
            .store(agent_label_frame_width, Ordering::Relaxed);
        self.send_command(InlineCommand::SetMessageLabels { agent, user });
    }

    /// Return the display width of the current agent label and its separator.
    pub fn agent_label_frame_width(&self) -> usize {
        self.message_layout.agent_label_frame_width.load(Ordering::Relaxed)
    }

    pub fn set_header_context(&self, context: InlineHeaderContext) {
        self.send_command(InlineCommand::SetHeaderContext { context: Box::new(context) });
    }

    pub fn set_input_status(&self, left: Option<String>, right: Option<String>) {
        self.send_command(InlineCommand::SetInputStatus { left, right });
    }

    pub fn set_activity_state(&self, state: ActivityState) {
        self.send_command(InlineCommand::SetActivityState(state));
    }

    pub fn set_terminal_title_items(&self, items: Option<Vec<String>>) {
        self.send_command(InlineCommand::SetTerminalTitleItems { items });
    }

    pub fn set_terminal_title_thread_label(&self, label: Option<String>) {
        self.send_command(InlineCommand::SetTerminalTitleThreadLabel { label });
    }

    pub fn set_terminal_title_git_branch(&self, branch: Option<String>) {
        self.send_command(InlineCommand::SetTerminalTitleGitBranch { branch });
    }

    pub fn set_theme(&self, theme: InlineTheme) {
        self.send_command(InlineCommand::SetTheme { theme });
    }

    pub fn set_color_scheme_auto(&self, enabled: bool) {
        self.send_command(InlineCommand::SetColorSchemeAuto { enabled });
    }

    pub fn set_appearance(&self, appearance: AppearanceConfig) {
        self.send_command(InlineCommand::SetAppearance { appearance });
    }

    pub fn set_key_bindings(&self, bindings: HashMap<String, Vec<String>>) {
        self.send_command(InlineCommand::SetKeyBindings { bindings });
    }

    pub fn set_vim_mode_enabled(&self, enabled: bool) {
        self.send_command(InlineCommand::SetVimModeEnabled(enabled));
    }

    pub fn set_queued_inputs(&self, entries: Vec<String>) {
        self.send_command(InlineCommand::SetQueuedInputs { entries });
    }

    pub fn set_subprocess_entries(&self, entries: Vec<String>) {
        self.send_command(InlineCommand::SetSubprocessEntries { entries });
    }

    pub fn set_subagent_preview(&self, text: Option<String>) {
        self.send_command(InlineCommand::SetSubagentPreview { text });
    }

    pub fn set_local_agents(&self, entries: Vec<LocalAgentEntry>) {
        self.send_command(InlineCommand::SetLocalAgents { entries });
    }

    pub fn set_archived_history(&self, entries: Vec<ArchivedPromptEntry>) {
        self.send_command(InlineCommand::SetArchivedHistory { entries });
    }

    pub fn set_primary_agent(&self, name: Option<String>, color: Option<String>) {
        self.send_command(InlineCommand::SetPrimaryAgent { name, color });
    }

    pub fn set_cursor_visible(&self, visible: bool) {
        self.send_command(InlineCommand::SetCursorVisible(visible));
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        self.send_command(InlineCommand::SetInputEnabled(enabled));
    }

    pub fn set_image_input_enabled(&self, enabled: bool) {
        self.send_command(InlineCommand::SetImageInputEnabled(enabled));
    }

    pub fn set_input(&self, content: String) {
        self.send_command(InlineCommand::SetInput(content));
    }

    pub fn restore_input_draft(&self, input: SubmittedInput) {
        self.send_command(InlineCommand::RestoreInputDraft(input));
    }

    pub fn apply_suggested_prompt(&self, content: String) {
        self.send_command(InlineCommand::ApplySuggestedPrompt(content));
    }

    pub fn set_inline_prompt_suggestion(&self, suggestion: String, llm_generated: bool) {
        self.send_command(InlineCommand::SetInlinePromptSuggestion { suggestion, llm_generated });
    }

    pub fn clear_inline_prompt_suggestion(&self) {
        self.send_command(InlineCommand::ClearInlinePromptSuggestion);
    }

    pub fn clear_input(&self) {
        self.send_command(InlineCommand::ClearInput);
    }

    pub fn force_redraw(&self) {
        self.send_command(InlineCommand::ForceRedraw);
    }

    pub fn shutdown(&self) {
        self.send_command(InlineCommand::Shutdown);
    }

    pub fn show_transient(&self, request: TransientRequest) {
        if let Some(active) = transient_input_state(&request) {
            // Eagerly acquire ownership so bridge input cannot overtake the
            // queued UI command. A UI-owned close is released by AppSession,
            // which can account for any lower captured surface on its stack.
            if self.sender.is_closed() && self.ui_owned_transient_activity {
                // A production UI that has already torn down cannot process
                // this request, so do not strand deferred input as active.
                self.transient_activity.set_active(false);
            } else if active || !self.ui_owned_transient_activity {
                self.transient_activity.set_active(active);
            }
        }
        self.send_command(InlineCommand::ShowTransient { request: Box::new(request) });
    }

    pub fn show_modal(&self, title: String, lines: Vec<String>, secure_prompt: Option<SecurePromptConfig>) {
        self.show_transient(TransientRequest::Modal(ModalOverlayRequest {
            title,
            lines,
            secure_prompt,
            is_help_modal: false,
        }));
    }

    pub fn show_list_modal(
        &self,
        title: String,
        lines: Vec<String>,
        items: Vec<InlineListItem>,
        selected: Option<InlineListSelection>,
        search: Option<InlineListSearchConfig>,
    ) {
        self.show_list_modal_with_footer(title, lines, items, selected, search, None);
    }

    pub fn show_list_modal_with_footer(
        &self,
        title: String,
        lines: Vec<String>,
        items: Vec<InlineListItem>,
        selected: Option<InlineListSelection>,
        search: Option<InlineListSearchConfig>,
        footer_hint: Option<String>,
    ) {
        self.show_transient(TransientRequest::List(ListOverlayRequest {
            title,
            lines,
            items,
            selected,
            search,
            footer_hint,
            hotkeys: Vec::new(),
        }));
    }

    pub fn configure_file_palette(
        &self,
        workspace: std::path::PathBuf,
        dir_lister: crate::tui::core_tui::app::session::file_palette::DirLister,
    ) {
        self.show_transient(TransientRequest::FilePalette(FilePaletteTransientRequest {
            dir_lister,
            workspace,
            visible: None,
        }));
    }

    /// Push the full recursive file list discovered in the background so Search
    /// mode has a corpus to match against. Browse mode does not require it.
    pub fn set_file_palette_search_index(&self, files: Vec<String>) {
        self.send_command(InlineCommand::UpdateFilePaletteSearch { files });
    }

    /// Replace slash palette commands after background template discovery.
    pub fn set_slash_commands(&self, commands: Vec<SlashCommandItem>) {
        self.send_command(InlineCommand::SetSlashCommands { commands });
    }

    pub fn configure_agent_palette(&self, agents: Vec<AgentPaletteItem>) {
        self.show_transient(TransientRequest::AgentPalette(AgentPaletteTransientRequest { agents, visible: None }));
    }

    pub fn show_history_picker(&self) {
        self.show_transient(TransientRequest::HistoryPicker);
    }

    pub fn show_task_panel(&self) {
        self.show_transient(TransientRequest::TaskPanel(TaskPanelTransientRequest {
            lines: Vec::new(),
            statuses: Vec::new(),
            current: None,
            visible: Some(true),
            metadata: None,
        }));
    }

    pub fn show_local_agents(&self) {
        self.show_transient(TransientRequest::LocalAgents(LocalAgentsTransientRequest { visible: Some(true) }));
    }

    pub fn hide_local_agents(&self) {
        self.show_transient(TransientRequest::LocalAgents(LocalAgentsTransientRequest { visible: Some(false) }));
    }

    pub fn hide_task_panel(&self) {
        self.show_transient(TransientRequest::TaskPanel(TaskPanelTransientRequest {
            lines: Vec::new(),
            statuses: Vec::new(),
            current: None,
            visible: Some(false),
            metadata: None,
        }));
    }

    pub fn update_task_panel(&self, lines: Vec<String>) {
        self.update_task_panel_with_metadata(lines, None);
    }

    pub fn update_task_panel_with_metadata(&self, lines: Vec<String>, metadata: Option<TaskPanelMetadata>) {
        self.update_task_panel_with_statuses(lines, Vec::new(), None, metadata);
    }

    /// Update the panel body with per-row statuses for text-styling.
    ///
    /// `statuses` runs parallel to `lines`; `current` is the focused-row
    /// index for accent emphasis. Length mismatches fall back to the uniform
    /// base style so legacy callers keep working.
    pub fn update_task_panel_with_statuses(
        &self,
        lines: Vec<String>,
        statuses: Vec<TaskItemStatus>,
        current: Option<usize>,
        metadata: Option<TaskPanelMetadata>,
    ) {
        self.show_transient(TransientRequest::TaskPanel(TaskPanelTransientRequest {
            lines,
            statuses,
            current,
            visible: None,
            metadata,
        }));
    }

    pub fn close_transient(&self) {
        // AppSession synchronizes the shared signal from its nested transient
        // stack. Clearing eagerly here would briefly release deferred bridge
        // input while a lower captured-input surface remains visible.
        if !self.ui_owned_transient_activity || self.sender.is_closed() {
            self.transient_activity.set_active(false);
        }
        self.send_command(InlineCommand::CloseTransient);
    }

    pub fn close_modal(&self) {
        self.close_transient();
    }

    pub fn clear_screen(&self) {
        self.send_command(InlineCommand::ClearScreen);
    }

    pub fn set_skip_confirmations(&self, skip: bool) {
        self.send_command(InlineCommand::SetSkipConfirmations(skip));
    }

    pub fn set_reasoning_stage(&self, stage: Option<String>) {
        self.send_command(InlineCommand::SetReasoningStage(stage));
    }
}

/// Return the deferred-input state for requests that own or release input.
/// Passive updates and palette preloads leave the current state unchanged.
fn transient_input_state(request: &TransientRequest) -> Option<bool> {
    match request {
        TransientRequest::Modal(_)
        | TransientRequest::List(_)
        | TransientRequest::Wizard(_)
        | TransientRequest::Diff(_)
        | TransientRequest::HistoryPicker
        | TransientRequest::SlashPalette => Some(true),
        TransientRequest::FilePalette(request) => request.visible,
        TransientRequest::AgentPalette(request) => request.visible,
        TransientRequest::LocalAgents(request) => request.visible,
        // The task panel is a passive surface; it keeps the input enabled and
        // therefore must not hold bridge events while it is visible.
        TransientRequest::TaskPanel(_) => None,
    }
}

pub struct InlineSession {
    pub handle: InlineHandle,
    pub events: UnboundedReceiver<InlineEvent>,
    /// Background task running the terminal event loop. The host must await
    /// it after `shutdown()` before restoring terminal state itself;
    /// otherwise the task's final frames are painted onto the main screen.
    pub worker: Option<tokio::task::JoinHandle<()>>,
}

impl InlineSession {
    pub async fn next_event(&mut self) -> Option<InlineEvent> {
        loop {
            if !self.handle.transient_active()
                && let Some(event) = self.handle.take_deferred_event()
            {
                return Some(event);
            }

            tokio::select! {
                event = self.events.recv() => match event {
                    Some(event) => return Some(event),
                    None if self.handle.transient_active() && self.handle.has_deferred_event() => {
                        // A closed UI channel must not strand bridge input
                        // behind a captured-input surface. Wait for the
                        // surface to release ownership, then drain it.
                        self.handle.wait_for_transient_activity_change().await;
                    }
                    None => return self.handle.take_deferred_event(),
                },
                _ = self.handle.wait_for_transient_activity_change() => {}
            }
        }
    }

    /// Wait for the TUI task to finish its own terminal teardown.
    ///
    /// Returns `true` when the task exited (or no task was spawned); `false`
    /// when it was still running after `timeout`, in which case the caller
    /// should force-restore the terminal as a backstop. On timeout the task
    /// is aborted to guarantee it cannot keep raw mode or the alternate
    /// screen active after the host has restored the terminal.
    pub async fn wait_for_exit(&mut self, timeout: std::time::Duration) -> bool {
        let Some(mut worker) = self.worker.take() else {
            return true;
        };
        let result = tokio::time::timeout(timeout, &mut worker).await;
        match result {
            Ok(_) => true,
            Err(_) => {
                worker.abort();
                // Give abort a brief moment to run drop handlers.
                let _ = tokio::time::timeout(std::time::Duration::from_millis(50), worker).await;
                false
            }
        }
    }

    pub fn set_skip_confirmations(&mut self, skip: bool) {
        self.handle.set_skip_confirmations(skip);
    }

    pub fn set_color_scheme_auto(&mut self, enabled: bool) {
        self.handle.set_color_scheme_auto(enabled);
    }

    pub fn clone_inline_handle(&self) -> InlineHandle {
        self.handle.clone()
    }
}

impl crate::tui::core_tui::runner::TuiCommand for InlineCommand {
    fn is_suspend_event_loop(&self) -> bool {
        matches!(self, InlineCommand::SuspendEventLoop)
    }

    fn is_resume_event_loop(&self) -> bool {
        matches!(self, InlineCommand::ResumeEventLoop)
    }

    fn is_clear_input_queue(&self) -> bool {
        matches!(self, InlineCommand::ClearInputQueue)
    }

    fn is_force_redraw(&self) -> bool {
        matches!(self, InlineCommand::ForceRedraw)
    }

    fn is_stop_event_stream(&self) -> bool {
        matches!(self, InlineCommand::StopEventStream)
    }

    fn is_start_event_stream(&self) -> bool {
        matches!(self, InlineCommand::StartEventStream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> (InlineHandle, InlineSession, UnboundedSender<InlineEvent>) {
        let (command_sender, _command_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let handle = InlineHandle::new_for_tests(command_sender);
        let session = InlineSession {
            handle: handle.clone(),
            events: event_receiver,
            worker: None,
        };
        (handle, session, event_sender)
    }

    #[tokio::test]
    async fn deferred_event_waits_until_transient_closes() {
        let (handle, mut session, event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.show_list_modal("test".into(), Vec::new(), Vec::new(), None, None);
        event_sender.send(InlineEvent::Cancel).expect("send modal result");

        let modal_event = session.next_event().await;
        assert!(matches!(modal_event, Some(InlineEvent::Cancel)));

        handle.close_transient();
        let deferred_event = session.next_event().await;
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn transient_close_wakes_waiting_deferred_event_consumer() {
        let (handle, session, _event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.show_local_agents();

        let waiter = tokio::spawn(async move {
            let mut session = session;
            session.next_event().await
        });
        tokio::task::yield_now().await;

        // A captured-input surface can close without sending a user event.
        handle.transient_activity.set_active(false);

        let deferred_event = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("deferred consumer should wake after transient close")
            .expect("deferred consumer task should finish");
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn closed_event_stream_waits_for_transient_close_before_deferred_event() {
        let (handle, session, event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.show_local_agents();
        drop(event_sender);

        let mut waiter = tokio::spawn(async move {
            let mut session = session;
            session.next_event().await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "closed UI channel must not bypass an active captured-input surface"
        );

        handle.transient_activity.set_active(false);
        let deferred_event = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("deferred consumer should wake after transient close")
            .expect("deferred consumer task should finish");
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn palette_preload_does_not_hold_deferred_input() {
        let (handle, mut session, _event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.configure_agent_palette(Vec::new());

        let deferred_event = session.next_event().await;
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn passive_task_panel_updates_do_not_hold_deferred_input() {
        let (handle, mut session, _event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.show_task_panel();
        handle.update_task_panel(Vec::new());

        let deferred_event = session.next_event().await;
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn captured_local_agents_drawer_releases_deferred_input_when_hidden() {
        let (handle, mut session, event_sender) = test_session();

        handle
            .defer_event(InlineEvent::WebmcpSubmit("deferred".into()))
            .expect("defer bridge event");
        handle.show_local_agents();
        event_sender.send(InlineEvent::Cancel).expect("send drawer event");

        let drawer_event = session.next_event().await;
        assert!(matches!(drawer_event, Some(InlineEvent::Cancel)));

        handle.hide_local_agents();
        let deferred_event = session.next_event().await;
        assert!(matches!(deferred_event, Some(InlineEvent::WebmcpSubmit(input)) if input.text == "deferred"));
    }

    #[tokio::test]
    async fn deferred_queue_overflow_fails_closed_without_dropping() {
        let (handle, _session, _event_sender) = test_session();

        for index in 0..MAX_DEFERRED_EVENTS {
            handle
                .defer_event(InlineEvent::WebmcpSubmit(format!("deferred-{index}").into()))
                .expect("queue should accept up to the bound");
        }
        assert!(handle.has_deferred_event());

        let overflow = handle.defer_event(InlineEvent::WebmcpSubmit("overflow".into()));
        assert!(overflow.is_err(), "32-event bound must fail closed, not silently drop");
        assert!(handle.has_deferred_event(), "overflow must not drain retained events");
    }
}
