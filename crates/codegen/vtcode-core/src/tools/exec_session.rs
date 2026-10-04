use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use hashbrown::HashMap;
use parking_lot::Mutex as ParkingMutex;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};
use tokio::task::JoinHandle;
#[cfg(windows)]
use vtcode_bash_runner::GracefulTerminationResult;
use vtcode_bash_runner::{
    PipeSpawnOptions, PipeStdinMode, ProcessHandle, graceful_kill_process_group_default_async,
    spawn_pipe_process_with_options,
};

use crate::sandboxing::build_sanitized_env;
use crate::tools::ExecSessionId;
use crate::tools::output_spooler::{SpoolIntegrity, SpoolLineCounter, encode_digest_hex};
use crate::tools::pty::{PtyCloseMode, PtySize};
use crate::tools::registry::{PtySessionGuard, PtySessionManager};
use crate::tools::types::VTCodeExecSession;
use crate::utils::path::{canonicalize_workspace, ensure_path_within_workspace};
use crate::zsh_exec_bridge::ZshExecBridgeSession;

const PIPE_OUTPUT_HEAD_BYTES: usize = 8 * 1024;
const PIPE_OUTPUT_TAIL_BYTES: usize = 8 * 1024;
const EXEC_SESSION_PREVIEW_HEAD_BYTES: usize = 8 * 1024;
const EXEC_SESSION_PREVIEW_TAIL_BYTES: usize = 8 * 1024;
const EXEC_SESSION_COMPLETION_COMMAND_MAX_BYTES: usize = 512;
const EXEC_SESSION_COMPLETION_DRAIN_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(1);
const EXEC_SESSION_COMPLETION_DRAIN_POLL: tokio::time::Duration = tokio::time::Duration::from_millis(15);
/// Upper bound for watcher abort + backend close. Must cover the sum of inner
/// bounds (2s watch abort x2 + 2s child reap + 5s reader join) plus margin so
/// the outer timeout is not spurious while `spawn_blocking` finishes residual work.
const EXEC_SESSION_CLOSE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(12);
/// Budget for aborting a lifecycle watcher before abandoning it.
const EXEC_SESSION_WATCH_ABORT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(1);
/// Budget for acquiring the output-read lock during close. An in-flight peek
/// that never releases must not park close (and therefore the runloop).
const EXEC_SESSION_OUTPUT_READ_LOCK_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(1);

/// A missing runtime handle does not establish whether its command failed.
#[derive(Debug, thiserror::Error)]
#[error(
    "exec session '{session_id}' not found in this runtime. Call `write_stdin` with the exact `session_id` copied from the original run response (`next_wait_args.session_id` or `next_continue_args` `s`/`session_id`); never guess the id. If a background completion or turn diagnostics already recorded output for this id, reuse its output. Missing session state does not prove failure; rerun only if fresh execution is still needed."
)]
pub(crate) struct ExecSessionNotFound {
    pub(crate) session_id: crate::types::CompactStr,
}

fn missing_exec_session_error(session_id: &str) -> anyhow::Error {
    ExecSessionNotFound { session_id: session_id.into() }.into()
}

/// Maximum number of live background command sessions owned by one runtime.
pub const MAX_BACKGROUND_PROCESSES: usize = 3;

/// Result of the Ctrl+B foreground-session handoff request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundShortcutResult {
    /// A live foreground session was reserved for background promotion.
    Requested,
    /// The runtime already owns the maximum number of background processes.
    AtCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipeOutputStats {
    pub total_bytes: u64,
    pub truncated: bool,
    pub spool_path: String,
    pub spool_available: bool,
    pub spool_complete: bool,
    pub spool_integrity: Option<SpoolIntegrity>,
}

#[derive(Default)]
struct PipeOutputBuffer {
    pending: Mutex<PipeOutputWindow>,
    total_bytes: AtomicU64,
    truncated: AtomicBool,
}

#[derive(Default)]
struct PipeOutputWindow {
    head: String,
    tail: String,
    total_bytes: u64,
    truncated: bool,
}

impl PipeOutputBuffer {
    async fn append(&self, chunk: &str, raw_byte_count: usize) {
        let mut pending = self.pending.lock().await;
        pending.total_bytes = pending.total_bytes.saturating_add(raw_byte_count as u64);
        self.total_bytes.fetch_add(raw_byte_count as u64, Ordering::Relaxed);
        if pending.head.len() < PIPE_OUTPUT_HEAD_BYTES {
            let remaining = PIPE_OUTPUT_HEAD_BYTES - pending.head.len();
            let end = chunk.floor_char_boundary(remaining.min(chunk.len()));
            pending.head.push_str(&chunk[..end]);
            if end < chunk.len() {
                pending.truncated = true;
                self.truncated.store(true, Ordering::Relaxed);
            }
        }

        // Operate on `pending.tail` in place. The previous code cloned the tail,
        // mutated the clone, then assigned it back — an O(tail) copy on every
        // output chunk. Since `pending` is already a &mut MutexGuard, in-place
        // mutation is equivalent and avoids the per-chunk allocation.
        pending.tail.push_str(chunk);
        if pending.tail.len() > PIPE_OUTPUT_TAIL_BYTES {
            let start = pending.tail.ceil_char_boundary(pending.tail.len() - PIPE_OUTPUT_TAIL_BYTES);
            pending.tail.drain(..start);
            pending.truncated = true;
            self.truncated.store(true, Ordering::Relaxed);
        }
    }

    async fn peek_pending(&self) -> Option<String> {
        let pending = self.pending.lock().await;
        if pending.total_bytes == 0 {
            None
        } else {
            Some(pending.preview())
        }
    }

    async fn drain_pending(&self) -> Option<String> {
        let mut pending = self.pending.lock().await;
        if pending.total_bytes == 0 {
            None
        } else {
            Some(std::mem::take(&mut *pending).preview())
        }
    }

    async fn stats(&self) -> (u64, bool) {
        (self.total_bytes.load(Ordering::Relaxed), self.truncated.load(Ordering::Relaxed))
    }
}

impl PipeOutputWindow {
    fn preview(&self) -> String {
        if !self.truncated {
            return self.head.clone();
        }
        if self.head == self.tail {
            return format!("{}\n[output preview truncated]", self.head);
        }
        format!("{}\n[output preview truncated]\n{}", self.head, self.tail)
    }
}

#[derive(Clone, Default)]
struct RetainedSessionPreview {
    head: String,
    tail: String,
    total_bytes: u64,
    truncated: bool,
}

impl RetainedSessionPreview {
    fn append(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }

        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        if self.head.len() < EXEC_SESSION_PREVIEW_HEAD_BYTES {
            let remaining = EXEC_SESSION_PREVIEW_HEAD_BYTES - self.head.len();
            let end = chunk.floor_char_boundary(remaining.min(chunk.len()));
            self.head.push_str(&chunk[..end]);
            if end < chunk.len() {
                self.truncated = true;
            }
        }

        self.tail.push_str(chunk);
        if self.tail.len() > EXEC_SESSION_PREVIEW_TAIL_BYTES {
            let start = self.tail.ceil_char_boundary(self.tail.len() - EXEC_SESSION_PREVIEW_TAIL_BYTES);
            self.tail.drain(..start);
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        if self.total_bytes == 0 {
            return String::new();
        }
        if !self.truncated {
            return self.head.clone();
        }
        if self.head == self.tail {
            return format!("{}\n[output preview truncated]", self.head);
        }
        format!("{}\n[output preview truncated]\n{}", self.head, self.tail)
    }
}

struct PipeSessionRecord {
    metadata: VTCodeExecSession,
    handle: Arc<ProcessHandle>,
    output: Arc<PipeOutputBuffer>,
    output_task: Mutex<Option<JoinHandle<()>>>,
    exit_task: Mutex<Option<JoinHandle<()>>>,
    activity_tx: watch::Sender<u64>,
    spool: PipeSpoolState,
}

struct PipeSpoolState {
    path: String,
    ready: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    integrity: Arc<ParkingMutex<Option<SpoolIntegrity>>>,
}

pub(crate) fn create_live_spool_file(workspace_root: &Path, session_id: &str) -> (PathBuf, Option<std::fs::File>) {
    let output_directory = Path::new(".vtcode/context/tool_outputs");
    for attempt in 0..16_u32 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("_{attempt}")
        };
        let relative_path = output_directory.join(format!("write_stdin_{session_id}{suffix}.txt"));
        match vtcode_commons::fs::bound_file::create_file_beneath(workspace_root, &relative_path) {
            Ok(file) => return (relative_path, Some(file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return (relative_path, None),
        }
    }
    (output_directory.join(format!("write_stdin_{session_id}_unavailable.txt")), None)
}

impl PipeSessionRecord {
    fn new(
        metadata: VTCodeExecSession,
        handle: Arc<ProcessHandle>,
        output: Arc<PipeOutputBuffer>,
        output_task: JoinHandle<()>,
        exit_task: JoinHandle<()>,
        activity_tx: watch::Sender<u64>,
        spool: PipeSpoolState,
    ) -> Self {
        Self {
            metadata,
            handle,
            output,
            output_task: Mutex::new(Some(output_task)),
            exit_task: Mutex::new(Some(exit_task)),
            activity_tx,
            spool,
        }
    }
}

#[derive(Clone)]
struct PipeSessionManager {
    workspace_root: PathBuf,
    sessions: Arc<RwLock<HashMap<ExecSessionId, Arc<PipeSessionRecord>>>>,
}

impl PipeSessionManager {
    fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root: canonicalize_workspace(&workspace_root),
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn create_session(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
        background: bool,
        stdin_mode: PipeStdinMode,
    ) -> Result<VTCodeExecSession> {
        if command.is_empty() {
            return Err(anyhow!("exec session command cannot be empty"));
        }
        // Canonicalization does sync fs I/O; keep it off the runtime worker
        // since this runs per spawned exec session.
        let working_dir = tokio::task::spawn_blocking({
            let working_dir = working_dir.clone();
            move || vtcode_commons::canonicalize(&working_dir)
        })
        .await
        .context("join exec-session working-directory canonicalization")?
        .with_context(|| format!("canonicalize exec-session working directory {}", working_dir.display()))?;
        self.ensure_within_workspace(&working_dir)?;

        // Hold the write lock across check → spawn → insert so two concurrent
        // creates with the same session_id cannot both pass the existence check
        // and spawn; the second insert would silently overwrite the first and
        // leak its spawned process and background tasks.
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(session_id.as_str()) {
            return Err(anyhow!("exec session '{}' already exists", session_id.as_str()));
        }

        let mut command_parts = command;
        let program = command_parts.remove(0);
        let args = command_parts;

        let opts = PipeSpawnOptions::new(program.clone(), working_dir.clone())
            .args(args.clone())
            .env(env)
            .stdin_mode(stdin_mode)
            .lossless_output(true);
        let spawned = spawn_pipe_process_with_options(opts)
            .await
            .with_context(|| format!("failed to spawn pipe session '{session_id}'"))?;

        let metadata = VTCodeExecSession {
            id: session_id.clone(),
            backend: "pipe".to_string(),
            command: program,
            args,
            working_dir: Some(self.format_working_dir(&working_dir)),
            background,
            rows: None,
            cols: None,
            child_pid: Some(spawned.process_id),
            started_at: Some(Utc::now()),
            lifecycle_state: Some(crate::tools::types::VTCodeSessionLifecycleState::Running),
            exit_code: None,
        };

        let handle = Arc::new(spawned.session);
        let output = Arc::new(PipeOutputBuffer::default());
        let output_clone = Arc::clone(&output);
        let mut output_rx = spawned.reliable_output_rx;
        let output_handle = Arc::clone(&handle);
        let (activity_tx, _) = watch::channel(0u64);
        let output_activity_tx = activity_tx.clone();
        let workspace_root = tokio::task::spawn_blocking({
            let workspace_root = self.workspace_root.clone();
            move || vtcode_commons::canonicalize(&workspace_root)
        })
        .await
        .context("join exec-session workspace canonicalization")?
        .with_context(|| format!("canonicalize exec-session workspace {}", self.workspace_root.display()))?;
        let spool_session_id = session_id.as_str().to_owned();
        let (spool_relative_path, spool_file) =
            tokio::task::spawn_blocking(move || create_live_spool_file(&workspace_root, &spool_session_id))
                .await
                .unwrap_or_else(|_| {
                    (
                        PathBuf::from(format!(".vtcode/context/tool_outputs/write_stdin_{session_id}_unavailable.txt")),
                        None,
                    )
                });
        let spool_path = spool_relative_path.to_string_lossy().replace('\\', "/");
        let spool_ready = Arc::new(AtomicBool::new(false));
        let spool_ready_for_task = Arc::clone(&spool_ready);
        let spool_failed = Arc::new(AtomicBool::new(false));
        let spool_failed_for_task = Arc::clone(&spool_failed);
        let spool_finished = Arc::new(AtomicBool::new(false));
        let spool_finished_for_task = Arc::clone(&spool_finished);
        let spool_integrity = Arc::new(ParkingMutex::new(None));
        let spool_integrity_for_task = Arc::clone(&spool_integrity);
        let output_task = tokio::spawn(async move {
            let mut spool_file = spool_file.map(tokio::fs::File::from_std);
            if spool_file.is_none() {
                spool_failed_for_task.store(true, Ordering::Release);
            } else {
                spool_ready_for_task.store(true, Ordering::Release);
            }
            let mut spool_redactor = vtcode_commons::sanitizer::StreamingSecretRedactor::default();
            let mut spool_hasher = Sha256::new();
            let mut spool_byte_count = 0_u64;
            let mut spool_lines = SpoolLineCounter::default();
            loop {
                match tokio::time::timeout(tokio::time::Duration::from_millis(15), output_rx.recv()).await {
                    Ok(Some(chunk)) => {
                        // Keep the decoded text as a `Cow<str>` so that the
                        // common case (valid UTF-8) borrows `chunk` with zero
                        // allocation. `.into_owned()` would force a String copy
                        // on every chunk even when the bytes are already valid.
                        let text = String::from_utf8_lossy(&chunk);
                        if let Some(file) = spool_file.as_mut() {
                            let sanitized = spool_redactor.push(&text);
                            if !sanitized.is_empty() {
                                if file.write_all(sanitized.as_bytes()).await.is_err() {
                                    spool_failed_for_task.store(true, Ordering::Release);
                                    spool_file = None;
                                } else {
                                    spool_hasher.update(sanitized.as_bytes());
                                    spool_lines.append(sanitized.as_bytes());
                                    spool_byte_count = spool_byte_count.saturating_add(sanitized.len() as u64);
                                }
                            }
                        }
                        output_clone.append(&text, chunk.len()).await;
                        output_activity_tx.send_modify(|version| *version += 1);
                    }
                    Ok(None) => break,
                    Err(_) if output_handle.has_exited() && output_handle.is_output_drained() => {
                        break;
                    }
                    Err(_) => continue,
                }
            }
            if let Some(file) = spool_file.as_mut() {
                let sanitized = spool_redactor.finish();
                let write_failed = !sanitized.is_empty() && file.write_all(sanitized.as_bytes()).await.is_err();
                if !write_failed && !sanitized.is_empty() {
                    spool_hasher.update(sanitized.as_bytes());
                    spool_lines.append(sanitized.as_bytes());
                    spool_byte_count = spool_byte_count.saturating_add(sanitized.len() as u64);
                }
                // Spool files are live-read while the session runs, so every
                // chunk must reach disk immediately; buffering would hide
                // output from concurrent readers until `flush`.
                if write_failed || file.sync_all().await.is_err() {
                    spool_failed_for_task.store(true, Ordering::Release);
                } else {
                    *spool_integrity_for_task.lock() = Some(SpoolIntegrity {
                        byte_count: spool_byte_count,
                        line_count: spool_lines.line_count(),
                        sha256: encode_digest_hex(spool_hasher.finalize()),
                    });
                }
            }
            spool_finished_for_task.store(true, Ordering::Release);
        });
        let exit_rx = spawned.exit_rx;
        let exit_activity_tx = activity_tx.clone();
        let exit_task = tokio::spawn(async move {
            let _ = exit_rx.await;
            exit_activity_tx.send_modify(|version| *version += 1);
        });
        let record = Arc::new(PipeSessionRecord::new(
            metadata.clone(),
            handle,
            output,
            output_task,
            exit_task,
            activity_tx,
            PipeSpoolState {
                path: spool_path,
                ready: spool_ready,
                failed: spool_failed,
                finished: spool_finished,
                integrity: spool_integrity,
            },
        ));

        sessions.insert(session_id, record);

        Ok(metadata)
    }

    async fn read_session_output(&self, session_id: &str, drain: bool) -> Result<Option<String>> {
        let record = self.session_record(session_id).await?;
        if drain {
            Ok(record.output.drain_pending().await)
        } else {
            Ok(record.output.peek_pending().await)
        }
    }

    async fn output_stats(&self, session_id: &str) -> Result<PipeOutputStats> {
        let record = self.session_record(session_id).await?;
        let (total_bytes, truncated) = record.output.stats().await;
        let spool_available =
            record.spool.ready.load(Ordering::Acquire) && !record.spool.failed.load(Ordering::Acquire);
        Ok(PipeOutputStats {
            total_bytes,
            truncated,
            spool_path: record.spool.path.clone(),
            spool_available,
            spool_complete: spool_available && record.spool.finished.load(Ordering::Acquire),
            spool_integrity: record.spool.integrity.lock().clone(),
        })
    }

    async fn send_input_to_session(&self, session_id: &str, data: &[u8], append_newline: bool) -> Result<usize> {
        let record = self.session_record(session_id).await?;
        let mut input = Vec::with_capacity(data.len() + usize::from(append_newline));
        input.extend_from_slice(data);
        if append_newline {
            input.push(b'\n');
        }
        record
            .handle
            .write(input)
            .await
            .map_err(|e| anyhow!("exec session '{session_id}' is no longer writable: {e}"))?;

        Ok(data.len() + usize::from(append_newline))
    }

    async fn is_session_completed(&self, session_id: &str) -> Result<Option<i32>> {
        let record = self.session_record(session_id).await?;
        if record.handle.has_exited() {
            Ok(record.handle.exit_code())
        } else {
            Ok(None)
        }
    }

    async fn terminate_session(&self, session_id: &str) -> Result<()> {
        let record = self.session_record(session_id).await?;
        if let Some(pid) = record.metadata.child_pid {
            let termination = graceful_kill_process_group_default_async(pid).await;
            #[cfg(windows)]
            if matches!(termination, GracefulTerminationResult::AlreadyExited | GracefulTerminationResult::Error) {
                // The direct child may exit before descendants that inherited
                // the pipes, so finish by killing the cached process tree.
                record.handle.terminate_process();
            }
            #[cfg(not(windows))]
            {
                let _ = termination;
                // The direct child may exit before descendants that inherited
                // the pipes, so finish by killing the cached process group as
                // well.
                record.handle.terminate_process();
            }
        } else {
            record.handle.terminate_process();
        }
        Ok(())
    }

    async fn force_terminate_session(&self, session_id: &str) -> Result<()> {
        let record = self.session_record(session_id).await?;
        record.handle.terminate_process();
        Ok(())
    }

    async fn close_session(&self, session_id: &str) -> Result<VTCodeExecSession> {
        let record = {
            let mut sessions = self.sessions.write().await;
            sessions
                .remove(session_id)
                .ok_or_else(|| missing_exec_session_error(session_id))?
        };

        // Kill the whole process group even when the direct child has already
        // exited; descendants can keep the inherited pipe descriptors alive.
        record.handle.terminate_process();
        if let Some(mut task) = record.output_task.lock().await.take() {
            if tokio::time::timeout(tokio::time::Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                record.handle.terminate();
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(mut task) = record.exit_task.lock().await.take() {
            if tokio::time::timeout(tokio::time::Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                record.handle.terminate();
                task.abort();
                let _ = task.await;
            }
        }

        Ok(record.metadata.clone())
    }

    async fn activity_receiver(&self, session_id: &str) -> Result<watch::Receiver<u64>> {
        let record = self.session_record(session_id).await?;
        Ok(record.activity_tx.subscribe())
    }

    async fn is_output_drained(&self, session_id: &str) -> Result<bool> {
        let record = self.session_record(session_id).await?;
        let output_task = record.output_task.lock().await;
        let output_task_finished = match output_task.as_ref() {
            Some(task) => task.is_finished(),
            None => true,
        };
        Ok(record.handle.is_output_drained() && output_task_finished)
    }

    async fn terminate_all_sessions(&self) -> Result<()> {
        let ids = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect::<Vec<_>>()
        };

        for session_id in ids {
            self.close_session(&session_id).await?;
        }

        Ok(())
    }

    async fn session_record(&self, session_id: &str) -> Result<Arc<PipeSessionRecord>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| missing_exec_session_error(session_id))
    }

    fn ensure_within_workspace(&self, candidate: &Path) -> Result<()> {
        ensure_path_within_workspace(candidate, &self.workspace_root).map(|_| ())
    }

    fn format_working_dir(&self, path: &Path) -> String {
        match path.strip_prefix(&self.workspace_root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => relative.to_string_lossy().replace("\\", "/"),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecSessionBackend {
    Pipe,
    Pty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecSessionLaunchMode {
    Foreground,
    UserBackground,
    ManagedBackground,
}

impl ExecSessionLaunchMode {
    fn is_background(self) -> bool {
        !matches!(self, Self::Foreground)
    }

    fn reserves_background_slot(self) -> bool {
        matches!(self, Self::UserBackground)
    }

    fn shows_in_background_drawer(self) -> bool {
        matches!(self, Self::UserBackground)
    }

    fn sets_foreground_session(self) -> bool {
        matches!(self, Self::Foreground)
    }
}

fn bounded_completion_command(command: &str) -> String {
    let without_controls: String = command
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect();
    let collapsed = vtcode_commons::formatting::collapse_whitespace(&without_controls);
    let redacted = vtcode_commons::sanitizer::redact_secrets(collapsed);
    if redacted.len() <= EXEC_SESSION_COMPLETION_COMMAND_MAX_BYTES {
        return redacted;
    }
    vtcode_commons::formatting::truncate_byte_budget(
        &redacted,
        EXEC_SESSION_COMPLETION_COMMAND_MAX_BYTES.saturating_sub(3),
        "...",
    )
}

/// Bounded data used by the Local Agents drawer for one raw command session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSessionUiSnapshot {
    pub metadata: VTCodeExecSession,
    pub preview: String,
}

/// Notification emitted after a background process has been confirmed exited.
///
/// The watcher owns the terminal transition, so a record emits at most one
/// notification even if multiple lifecycle checks observe the same exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSessionCompletionEvent {
    /// Stable exec-session identity.
    pub session_id: ExecSessionId,
    /// Shell command associated with the session.
    pub command: String,
    /// Whether the session belongs to the managed subagent background flow.
    /// User-launched background exec sessions are delivered directly to the
    /// parent run loop; managed sessions are forwarded by their controller.
    pub managed_background: bool,
    /// Whether the runtime intentionally terminated the session.
    pub termination_requested: bool,
    /// Confirmed process exit status.
    pub exit_code: i32,
}

struct ExecSessionRecord {
    metadata: VTCodeExecSession,
    backend: ExecSessionBackend,
    _pty_guard: Option<PtySessionGuard>,
    foreground_pty_counter: ParkingMutex<Option<Arc<AtomicUsize>>>,
    background: AtomicBool,
    background_promoted_from_foreground: AtomicBool,
    show_in_background_drawer: AtomicBool,
    background_slot_reserved: AtomicBool,
    background_completion_published: AtomicBool,
    termination_requested: AtomicBool,
    preview: ParkingMutex<SessionPreviewState>,
    output_read_lock: Mutex<()>,
    background_watch: ParkingMutex<Option<JoinHandle<()>>>,
    foreground_watch: ParkingMutex<Option<JoinHandle<()>>>,
}

impl ExecSessionRecord {
    fn new(
        metadata: VTCodeExecSession,
        backend: ExecSessionBackend,
        pty_guard: Option<PtySessionGuard>,
        launch_mode: ExecSessionLaunchMode,
        background_slot_reserved: bool,
    ) -> Self {
        Self {
            metadata,
            backend,
            _pty_guard: pty_guard,
            foreground_pty_counter: ParkingMutex::new(None),
            background: AtomicBool::new(launch_mode.is_background()),
            background_promoted_from_foreground: AtomicBool::new(false),
            show_in_background_drawer: AtomicBool::new(launch_mode.shows_in_background_drawer()),
            background_slot_reserved: AtomicBool::new(background_slot_reserved),
            background_completion_published: AtomicBool::new(false),
            termination_requested: AtomicBool::new(false),
            preview: ParkingMutex::new(SessionPreviewState::default()),
            output_read_lock: Mutex::new(()),
            background_watch: ParkingMutex::new(None),
            foreground_watch: ParkingMutex::new(None),
        }
    }

    fn remember_output(&self, output: Option<&str>, drain: bool) {
        let Some(output) = output else {
            return;
        };

        let mut preview = self.preview.lock();
        if drain {
            preview.retained.append(output);
            preview.pending = None;
        } else {
            preview.pending = Some(output.to_string());
        }
    }

    fn preview(&self) -> String {
        let preview = self.preview.lock();
        let mut retained = preview.retained.clone();
        if let Some(pending) = preview.pending.as_deref() {
            retained.append(pending);
        }
        retained.render()
    }
}

#[derive(Default)]
struct SessionPreviewState {
    retained: RetainedSessionPreview,
    pending: Option<String>,
}

#[derive(Clone)]
pub struct ExecSessionManager {
    pipe_sessions: PipeSessionManager,
    pty_sessions: PtySessionManager,
    sessions: Arc<RwLock<HashMap<ExecSessionId, Arc<ExecSessionRecord>>>>,
    create_lock: Arc<Mutex<()>>,
    active_background_processes: Arc<AtomicUsize>,
    foreground_pty_counter: Arc<ParkingMutex<Option<Arc<AtomicUsize>>>>,
    foreground_session: Arc<ParkingMutex<Option<ExecSessionId>>>,
    focused_session: Arc<ParkingMutex<Option<ExecSessionId>>>,
    background_request: Arc<ParkingMutex<Option<ExecSessionId>>>,
    background_shortcut_result: Arc<ParkingMutex<Option<BackgroundShortcutResult>>>,
    completion_tx: broadcast::Sender<ExecSessionCompletionEvent>,
    completion_notify: Arc<Notify>,
}

impl ExecSessionManager {
    #[must_use]
    pub fn new(workspace_root: PathBuf, pty_sessions: PtySessionManager) -> Self {
        let (completion_tx, _) = broadcast::channel(64);
        Self {
            pipe_sessions: PipeSessionManager::new(workspace_root),
            pty_sessions,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            create_lock: Arc::new(Mutex::new(())),
            active_background_processes: Arc::new(AtomicUsize::new(0)),
            foreground_pty_counter: Arc::new(ParkingMutex::new(None)),
            foreground_session: Arc::new(ParkingMutex::new(None)),
            focused_session: Arc::new(ParkingMutex::new(None)),
            background_request: Arc::new(ParkingMutex::new(None)),
            background_shortcut_result: Arc::new(ParkingMutex::new(None)),
            completion_tx,
            completion_notify: Arc::new(Notify::new()),
        }
    }

    /// Subscribe to confirmed terminal transitions for background exec sessions.
    pub fn subscribe_completion(&self) -> broadcast::Receiver<ExecSessionCompletionEvent> {
        self.completion_tx.subscribe()
    }

    /// Wake an idle interaction loop when a background exec session completes.
    #[must_use]
    pub fn completion_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.completion_notify)
    }

    pub(crate) fn set_foreground_pty_counter(&self, counter: Arc<AtomicUsize>) {
        *self.foreground_pty_counter.lock() = Some(counter);
    }

    /// Test-only convenience: production paths go through
    /// [`Self::create_pipe_session_with_stdin`].
    #[cfg(test)]
    pub(crate) async fn create_pipe_session(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
    ) -> Result<VTCodeExecSession> {
        self.create_pipe_session_with_sandbox_and_background(session_id, command, working_dir, env, false, false)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn create_pipe_session_with_sandbox_and_background(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
        sandbox_active: bool,
        background: bool,
    ) -> Result<VTCodeExecSession> {
        self.create_pipe_session_with_stdin(
            session_id,
            command,
            working_dir,
            env,
            sandbox_active,
            background,
            PipeStdinMode::Piped,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Launch options preserve the existing internal session API while public pipe runs select stdin explicitly."
    )]
    pub(crate) async fn create_pipe_session_with_stdin(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
        sandbox_active: bool,
        background: bool,
        stdin_mode: PipeStdinMode,
    ) -> Result<VTCodeExecSession> {
        let launch_mode = if background {
            ExecSessionLaunchMode::UserBackground
        } else {
            ExecSessionLaunchMode::Foreground
        };
        self.create_pipe_session_with_launch_mode(
            session_id,
            command,
            working_dir,
            env,
            sandbox_active,
            launch_mode,
            stdin_mode,
        )
        .await
    }

    pub(crate) async fn create_pipe_session_for_managed_background(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
    ) -> Result<VTCodeExecSession> {
        self.create_pipe_session_with_launch_mode(
            session_id,
            command,
            working_dir,
            env,
            false,
            ExecSessionLaunchMode::ManagedBackground,
            PipeStdinMode::Null,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Internal session launch carries sandbox, lifecycle, and stdin policy through one spawning boundary."
    )]
    async fn create_pipe_session_with_launch_mode(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        env: HashMap<String, String>,
        sandbox_active: bool,
        launch_mode: ExecSessionLaunchMode,
        stdin_mode: PipeStdinMode,
    ) -> Result<VTCodeExecSession> {
        let _create_guard = self.create_lock.lock().await;
        self.ensure_session_absent(&session_id).await?;
        let slot_reserved = if launch_mode.reserves_background_slot() {
            self.reserve_background_slot()?
        } else {
            false
        };
        let env = if sandbox_active {
            build_sanitized_env(&env, true, false, "exec-session", &[])
        } else {
            env
        };
        let metadata = match self
            .pipe_sessions
            .create_session(session_id.clone(), command, working_dir, env, launch_mode.is_background(), stdin_mode)
            .await
        {
            Ok(metadata) => metadata,
            Err(error) => {
                self.release_reserved_background_slot(slot_reserved);
                return Err(error);
            }
        };
        let record = match self
            .insert_session(metadata.clone(), ExecSessionBackend::Pipe, None, launch_mode, slot_reserved)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                let _ = self.pipe_sessions.close_session(session_id.as_str()).await;
                self.release_reserved_background_slot(slot_reserved);
                return Err(error);
            }
        };
        if launch_mode.is_background() {
            self.start_background_watcher(record, session_id.to_string());
        } else if launch_mode.sets_foreground_session() {
            self.set_foreground_session(metadata.id.clone());
            self.start_foreground_watcher(record, session_id.to_string());
        }
        Ok(metadata)
    }

    /// Test-only convenience: production paths go through
    /// [`Self::create_pty_session_with_sandbox_and_background`].
    #[cfg(test)]
    pub(crate) async fn create_pty_session(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        size: PtySize,
        extra_env: HashMap<String, String>,
        zsh_exec_bridge: Option<ZshExecBridgeSession>,
    ) -> Result<VTCodeExecSession> {
        self.create_pty_session_with_sandbox_and_background(
            session_id,
            command,
            working_dir,
            size,
            extra_env,
            zsh_exec_bridge,
            HashMap::new(),
            false,
            false,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "The constructor keeps sandbox, bridge, and background launch settings explicit at the session boundary."
    )]
    pub(crate) async fn create_pty_session_with_sandbox_and_background(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        size: PtySize,
        extra_env: HashMap<String, String>,
        zsh_exec_bridge: Option<ZshExecBridgeSession>,
        trusted_env: HashMap<String, String>,
        sandbox_active: bool,
        background: bool,
    ) -> Result<VTCodeExecSession> {
        let launch_mode = if background {
            ExecSessionLaunchMode::UserBackground
        } else {
            ExecSessionLaunchMode::Foreground
        };
        self.create_pty_session_with_launch_mode(
            session_id,
            command,
            working_dir,
            size,
            extra_env,
            zsh_exec_bridge,
            trusted_env,
            sandbox_active,
            launch_mode,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "The managed background constructor keeps PTY launch settings explicit at the session boundary."
    )]
    pub(crate) async fn create_pty_session_for_managed_background(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        size: PtySize,
        extra_env: HashMap<String, String>,
        zsh_exec_bridge: Option<ZshExecBridgeSession>,
        trusted_env: HashMap<String, String>,
        sandbox_active: bool,
    ) -> Result<VTCodeExecSession> {
        self.create_pty_session_with_launch_mode(
            session_id,
            command,
            working_dir,
            size,
            extra_env,
            zsh_exec_bridge,
            trusted_env,
            sandbox_active,
            ExecSessionLaunchMode::ManagedBackground,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "The constructor keeps sandbox, bridge, and background launch settings explicit at the session boundary."
    )]
    async fn create_pty_session_with_launch_mode(
        &self,
        session_id: ExecSessionId,
        command: Vec<String>,
        working_dir: PathBuf,
        size: PtySize,
        extra_env: HashMap<String, String>,
        zsh_exec_bridge: Option<ZshExecBridgeSession>,
        trusted_env: HashMap<String, String>,
        sandbox_active: bool,
        launch_mode: ExecSessionLaunchMode,
    ) -> Result<VTCodeExecSession> {
        let _create_guard = self.create_lock.lock().await;
        self.ensure_session_absent(&session_id).await?;
        let slot_reserved = if launch_mode.reserves_background_slot() {
            self.reserve_background_slot()?
        } else {
            false
        };
        let pty_guard = match self.pty_sessions.start_session() {
            Ok(guard) => guard,
            Err(error) => {
                self.release_reserved_background_slot(slot_reserved);
                return Err(error);
            }
        };
        let metadata = match self.pty_sessions.manager().create_session_with_bridge_sandboxed(
            session_id.clone().into(),
            command,
            working_dir,
            size,
            extra_env,
            zsh_exec_bridge,
            trusted_env,
            sandbox_active,
        ) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.release_reserved_background_slot(slot_reserved);
                return Err(error);
            }
        };
        let mut exec_metadata = VTCodeExecSession::from(metadata);
        exec_metadata.background = launch_mode.is_background();
        let record = match self
            .insert_session(exec_metadata.clone(), ExecSessionBackend::Pty, Some(pty_guard), launch_mode, slot_reserved)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                let _ = self.pty_sessions.manager().close_session(session_id.as_str());
                self.release_reserved_background_slot(slot_reserved);
                return Err(error);
            }
        };
        if launch_mode.is_background() {
            self.start_background_watcher(record, session_id.to_string());
        } else if launch_mode.sets_foreground_session() {
            self.set_foreground_session(exec_metadata.id.clone());
            self.start_foreground_watcher(record, session_id.to_string());
        }
        Ok(exec_metadata)
    }

    pub(crate) async fn snapshot_session(&self, session_id: &str) -> Result<VTCodeExecSession> {
        let record = self.session_record(session_id).await?;
        match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.session_record(session_id).await.map(|r| {
                let mut metadata = r.metadata.clone();
                metadata.background = record.background.load(Ordering::Acquire);
                let exit_code = if r.handle.has_exited() {
                    r.handle.exit_code()
                } else {
                    None
                };
                metadata.exit_code = exit_code;
                metadata.lifecycle_state = Some(if exit_code.is_some() {
                    crate::tools::types::VTCodeSessionLifecycleState::Exited
                } else {
                    crate::tools::types::VTCodeSessionLifecycleState::Running
                });
                metadata
            }),
            ExecSessionBackend::Pty => self.pty_sessions.manager().snapshot_session(session_id).map(|metadata| {
                let mut metadata = VTCodeExecSession::from(metadata);
                metadata.background = record.background.load(Ordering::Acquire);
                metadata
            }),
        }
    }

    /// Return one retained background session snapshot for the Local Agents drawer.
    pub async fn background_session_snapshot(&self, session_id: &str) -> Result<ExecSessionUiSnapshot> {
        let record = self.session_record(session_id).await?;
        if !record.background.load(Ordering::Acquire) || !record.show_in_background_drawer.load(Ordering::Acquire) {
            bail!("exec session '{session_id}' is a foreground session and is not visible in the background drawer");
        }

        // Capture output that has not yet been consumed by the tool wait loop
        // before reading the retained preview. Drained output is remembered by
        // `read_session_output`, so this remains inspectable after a tool turn.
        let _ = self.read_session_output(session_id, false).await?;
        let metadata = self.snapshot_session(session_id).await?;
        Ok(ExecSessionUiSnapshot { metadata, preview: record.preview() })
    }

    /// Return all background raw command sessions, including exited sessions
    /// that have not been explicitly closed.
    pub async fn background_session_snapshots(&self) -> Vec<ExecSessionUiSnapshot> {
        let ids = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|record| {
                    record.background.load(Ordering::Acquire)
                        && record.show_in_background_drawer.load(Ordering::Acquire)
                })
                .map(|record| record.metadata.id.clone())
                .collect::<Vec<_>>()
        };

        let mut snapshots = Vec::new();
        for id in ids {
            if let Ok(snapshot) = self.background_session_snapshot(id.as_str()).await {
                snapshots.push(snapshot);
            }
        }
        snapshots.sort_by(|left, right| match (left.metadata.started_at, right.metadata.started_at) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right.metadata.id.cmp(&left.metadata.id),
        });
        snapshots
    }

    pub(crate) async fn list_sessions(&self) -> Vec<VTCodeExecSession> {
        let sessions = self.sessions.read().await;
        let mut listed = sessions
            .values()
            .map(|record| {
                let mut metadata = record.metadata.clone();
                metadata.background = record.background.load(Ordering::Acquire);
                metadata
            })
            .collect::<Vec<_>>();
        listed.sort_by(|left, right| left.id.cmp(&right.id));
        listed
    }

    /// Bounded snapshot of exec sessions that are still running (not exited).
    ///
    /// Used for turn-end diagnostics and telemetry. Ordered newest-first by
    /// `started_at` (sessions without a timestamp last), capped so a
    /// pathological session count cannot inflate the recorded state.
    /// Completion is checked against the backend (not the cached metadata) so
    /// a session that exited after its last metadata refresh is correctly
    /// excluded. Cross-turn resume hints use the foreground-only variant
    /// below so retained background work does not become mandatory follow-up.
    pub(crate) async fn in_progress_exec_sessions(&self, cap: usize) -> Vec<VTCodeExecSession> {
        self.collect_in_progress_exec_sessions(cap, true).await
    }

    /// Bounded snapshot of running foreground sessions for cross-turn resume
    /// hints. Retained background sessions are deliberately excluded because
    /// they are not work the next turn must settle before proceeding.
    pub(crate) async fn in_progress_foreground_exec_sessions(&self, cap: usize) -> Vec<VTCodeExecSession> {
        self.collect_in_progress_exec_sessions(cap, false).await
    }

    async fn collect_in_progress_exec_sessions(&self, cap: usize, include_background: bool) -> Vec<VTCodeExecSession> {
        if cap == 0 {
            return Vec::new();
        }
        let ids = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect::<Vec<_>>()
        };
        let mut in_progress = Vec::new();
        for id in ids {
            let Ok(session) = self.snapshot_session(id.as_str()).await else {
                continue;
            };
            if session.exit_code.is_none() && (include_background || !session.background) {
                in_progress.push(session);
            }
        }
        in_progress.sort_by(|left, right| match (left.started_at, right.started_at) {
            (Some(left), Some(right)) => right.cmp(&left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right.id.cmp(&left.id),
        });
        in_progress.truncate(cap);
        in_progress
    }

    pub(crate) async fn read_session_output(&self, session_id: &str, drain: bool) -> Result<Option<String>> {
        let record = self.session_record(session_id).await?;
        // Serialize the backend read with the preview update. A peek followed
        // by a concurrent drain must not leave the same chunk pending after
        // it has already been retained.
        let _output_read_guard = record.output_read_lock.lock().await;
        let output = match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.read_session_output(session_id, drain).await,
            ExecSessionBackend::Pty => self.pty_sessions.manager().read_session_output(session_id, drain),
        }?;
        record.remember_output(output.as_deref(), drain);
        Ok(output)
    }

    pub(crate) async fn output_stats(&self, session_id: &str) -> Result<Option<PipeOutputStats>> {
        let record = self.session_record(session_id).await?;
        match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.output_stats(session_id).await.map(Some),
            ExecSessionBackend::Pty => self.pty_sessions.manager().output_stats(session_id).map(|stats| {
                stats.map(|stats| PipeOutputStats {
                    total_bytes: stats.total_bytes,
                    truncated: stats.truncated,
                    spool_path: stats.spool_path,
                    spool_available: stats.spool_available,
                    spool_complete: stats.spool_complete,
                    spool_integrity: stats.spool_integrity,
                })
            }),
        }
    }

    pub async fn send_input_to_session(&self, session_id: &str, data: &[u8], append_newline: bool) -> Result<usize> {
        let record = self.session_record(session_id).await?;
        match record.backend {
            ExecSessionBackend::Pipe => {
                self.pipe_sessions.send_input_to_session(session_id, data, append_newline).await
            }
            ExecSessionBackend::Pty => {
                self.pty_sessions
                    .manager()
                    .send_input_to_session(session_id, data, append_newline)
            }
        }
    }

    pub async fn is_session_completed(&self, session_id: &str) -> Result<Option<i32>> {
        let record = self.session_record(session_id).await?;
        let completed = match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.is_session_completed(session_id).await,
            ExecSessionBackend::Pty => self.pty_sessions.manager().is_session_completed(session_id),
        }?;
        if completed.is_some() {
            self.release_pending_background_request(session_id);
            self.clear_focused_session_if_matches(session_id);
        }
        Ok(completed)
    }

    pub(crate) async fn activity_receiver(&self, session_id: &str) -> Result<Option<watch::Receiver<u64>>> {
        let record = self.session_record(session_id).await?;
        match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.activity_receiver(session_id).await.map(Some),
            ExecSessionBackend::Pty => Ok(None),
        }
    }

    pub(crate) async fn is_output_drained(&self, session_id: &str) -> Result<bool> {
        let record = self.session_record(session_id).await?;
        let drained = match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.is_output_drained(session_id).await,
            ExecSessionBackend::Pty => self.pty_sessions.manager().is_output_drained(session_id),
        }?;
        Ok(drained)
    }

    pub async fn terminate_session(&self, session_id: &str) -> Result<()> {
        let record = self.session_record(session_id).await?;
        self.clear_focused_session_if_matches(session_id);
        record.termination_requested.store(true, Ordering::Release);
        let result = match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.terminate_session(session_id).await,
            ExecSessionBackend::Pty => {
                let manager = self.pty_sessions.manager().clone();
                let id = session_id.to_string();
                tokio::task::spawn_blocking(move || manager.terminate_session(&id))
                    .await
                    .map_err(|join_error| anyhow!("exec session terminate task failed: {join_error}"))?
            }
        };
        if result.is_err() {
            record.termination_requested.store(false, Ordering::Release);
        }
        result
    }

    pub async fn force_terminate_session(&self, session_id: &str) -> Result<()> {
        let record = self.session_record(session_id).await?;
        self.clear_focused_session_if_matches(session_id);
        record.termination_requested.store(true, Ordering::Release);
        let result = match record.backend {
            ExecSessionBackend::Pipe => self.pipe_sessions.force_terminate_session(session_id).await,
            ExecSessionBackend::Pty => {
                // PTY terminate performs a bounded child reap that sleeps;
                // keep it off the async worker so ForceCancel over N sessions
                // cannot stall the runloop.
                let manager = self.pty_sessions.manager().clone();
                let id = session_id.to_string();
                tokio::task::spawn_blocking(move || manager.force_terminate_session(&id))
                    .await
                    .map_err(|join_error| anyhow!("exec session force-terminate task failed: {join_error}"))?
            }
        };
        if result.is_err() {
            record.termination_requested.store(false, Ordering::Release);
        }
        result
    }

    pub async fn close_session(&self, session_id: &str) -> Result<VTCodeExecSession> {
        self.close_session_with_mode(session_id, PtyCloseMode::Graceful).await
    }

    /// Close with an explicit termination mode. `Immediate` is reserved for
    /// exit-driven teardown: it skips the `exit\n` courtesy and the SIGTERM
    /// grace window so a live PTY child cannot cost ~600 ms of shell-return
    /// latency per session. The OS reaps survivors at process exit.
    pub async fn close_session_with_mode(&self, session_id: &str, mode: PtyCloseMode) -> Result<VTCodeExecSession> {
        // Serialize removal with Ctrl+B promotion and new session creation so
        // a promotion cannot reserve a slot on a record that close has already
        // detached from the unified session map.
        let (record, pending_background_request) = {
            let _lifecycle_guard = self.create_lock.lock().await;
            let record = {
                let mut sessions = self.sessions.write().await;
                sessions
                    .remove(session_id)
                    .ok_or_else(|| missing_exec_session_error(session_id))?
            };

            let pending_background_request = self.clear_foreground_and_take_pending_request(session_id);
            self.clear_focused_session_if_matches(session_id);
            (record, pending_background_request)
        };

        // Capacity release must happen even if backend close times out: the
        // record is already detached, so leaving counters elevated pins
        // `Running PTY command...` and the composer lock forever.
        let close_result = self.close_session_backend_bounded(session_id, &record, mode).await;

        self.release_foreground_pty_count(&record);
        if pending_background_request {
            self.release_reserved_background_slot(true);
        } else {
            self.release_background_slot(&record);
        }

        let mut metadata = close_result?;
        metadata.background = record.background.load(Ordering::Acquire);
        Ok(metadata)
    }

    /// Abort lifecycle watchers and close the backend under a hard timeout.
    ///
    /// Watcher abort and the blocking PTY close are themselves the hang sites
    /// (unbounded `join`/`wait`, or a watcher stuck in a sync section), so
    /// this entire unit is time-bounded and the PTY close runs on
    /// `spawn_blocking` to keep the async worker responsive. Pipe close stays
    /// on the runtime (already async and internally timed).
    async fn close_session_backend_bounded(
        &self,
        session_id: &str,
        record: &Arc<ExecSessionRecord>,
        mode: PtyCloseMode,
    ) -> Result<VTCodeExecSession> {
        let background_watch = record.background_watch.lock().take();
        if let Some(watch) = background_watch {
            watch.abort();
            if tokio::time::timeout(EXEC_SESSION_WATCH_ABORT_TIMEOUT, watch).await.is_err() {
                tracing::warn!(%session_id, "background watcher did not stop within abort timeout");
            }
        }
        let foreground_watch = record.foreground_watch.lock().take();
        if let Some(watch) = foreground_watch {
            watch.abort();
            if tokio::time::timeout(EXEC_SESSION_WATCH_ABORT_TIMEOUT, watch).await.is_err() {
                tracing::warn!(%session_id, "foreground watcher did not stop within abort timeout");
            }
        }

        // Do not close the backend while an output peek/drain is still using
        // it. The unified record has already been removed, so this lock only
        // waits for in-flight readers acquired before close. The acquire is
        // itself time-bounded: an abandoned peek holding the lock must not
        // make close (and the runloop) wait forever.
        let _output_read_guard =
            match tokio::time::timeout(EXEC_SESSION_OUTPUT_READ_LOCK_TIMEOUT, record.output_read_lock.lock()).await {
                Ok(guard) => Some(guard),
                Err(_elapsed) => {
                    tracing::warn!(
                        %session_id,
                        "output read lock not available within timeout; closing session anyway"
                    );
                    None
                }
            };
        let metadata = match record.backend {
            ExecSessionBackend::Pipe => {
                let pipe_sessions = self.pipe_sessions.clone();
                let session_id_owned = session_id.to_string();
                tokio::time::timeout(EXEC_SESSION_CLOSE_TIMEOUT, async move {
                    pipe_sessions.close_session(&session_id_owned).await
                })
                .await
            }
            ExecSessionBackend::Pty => {
                let pty_manager = self.pty_sessions.manager().clone();
                let session_id_owned = session_id.to_string();
                tokio::time::timeout(EXEC_SESSION_CLOSE_TIMEOUT, async move {
                    tokio::task::spawn_blocking(move || {
                        pty_manager
                            .close_session_with_mode(&session_id_owned, mode)
                            .map(VTCodeExecSession::from)
                    })
                    .await
                    .map_err(|join_error| anyhow!("exec session close task failed: {join_error}"))?
                })
                .await
            }
        };

        match metadata {
            Ok(result) => result,
            Err(_elapsed) => Err(anyhow!(
                "exec session '{session_id}' close timed out after {}s; record detached and counters released",
                EXEC_SESSION_CLOSE_TIMEOUT.as_secs()
            )),
        }
    }

    /// Force-stop an active session, or close it when it has already exited.
    /// Returns `true` when the session was already complete and was closed.
    pub async fn force_terminate_or_close(&self, session_id: &str) -> Result<bool> {
        let completed = self.is_session_completed(session_id).await?.is_some();
        if completed {
            self.close_session(session_id).await?;
        } else {
            self.force_terminate_session(session_id).await?;
        }
        Ok(completed)
    }

    /// Focus a running background session so subsequent submitted lines are
    /// written to its stdin instead of becoming a model prompt.
    pub async fn focus_background_session(&self, session_id: &str) -> Result<()> {
        let record = self.session_record(session_id).await?;
        if !record.background.load(Ordering::Acquire) {
            bail!("exec session '{session_id}' is a foreground session and cannot be focused from the drawer");
        }
        if self.is_session_completed(session_id).await?.is_some() {
            bail!("exec session '{session_id}' has already exited and cannot receive input");
        }
        *self.focused_session.lock() = Some(record.metadata.id.clone());
        Ok(())
    }

    /// Return the focused background session, if any.
    #[must_use]
    pub fn focused_session_id(&self) -> Option<String> {
        self.focused_session.lock().as_ref().map(|id| id.as_str().to_string())
    }

    pub fn clear_focused_session(&self) {
        *self.focused_session.lock() = None;
    }

    pub(crate) async fn prune_exited_session(&self, session_id: &str) -> Result<Option<VTCodeExecSession>> {
        let record = self.session_record(session_id).await?;
        if self.is_session_completed(session_id).await?.is_some() {
            // Background completion owns the final output capture and parent
            // notification. A synchronous wait/stop can observe process exit
            // first; pruning here would abort that watcher and lose the
            // terminal event. Retain the session until the watcher has
            // finished, after which a later prune may close it safely.
            let completion_pending = record.background.load(Ordering::Acquire)
                && record
                    .background_watch
                    .lock()
                    .as_ref()
                    .is_some_and(|watch| !watch.is_finished());
            if completion_pending {
                return Ok(None);
            }
            return self.close_session(session_id).await.map(Some);
        }
        Ok(None)
    }

    pub(crate) async fn terminate_all_sessions_async(&self) -> Result<()> {
        self.terminate_all_sessions_with_mode_async(PtyCloseMode::Graceful).await
    }

    /// Exit-path terminator: closes every session in `Immediate` mode so a
    /// live PTY child is group-SIGKILLed instead of waiting out the SIGTERM
    /// grace window. Callers still bound this with an outer timeout; the OS
    /// reaps any remainder at process exit.
    pub(crate) async fn terminate_all_sessions_for_exit_async(&self) -> Result<()> {
        self.terminate_all_sessions_with_mode_async(PtyCloseMode::Immediate).await
    }

    async fn terminate_all_sessions_with_mode_async(&self, mode: PtyCloseMode) -> Result<()> {
        let ids = {
            let sessions = self.sessions.read().await;
            sessions.keys().cloned().collect::<Vec<_>>()
        };

        let mut failures = Vec::new();
        for session_id in ids {
            if let Err(err) = self.close_session_with_mode(&session_id, mode).await {
                failures.push(format!("{session_id}: {err}"));
            }
        }

        if let Err(err) = self.pipe_sessions.terminate_all_sessions().await {
            failures.push(err.to_string());
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("failed to terminate all exec sessions: {}", failures.join("; ")))
        }
    }

    pub(crate) async fn terminate_active_sessions_async(&self) -> Result<()> {
        self.terminate_active_sessions_with_mode_async(PtyCloseMode::Graceful).await
    }

    /// Exit-path variant of [`Self::terminate_active_sessions_async`] using
    /// `Immediate` PTY termination (see [`Self::terminate_all_sessions_for_exit_async`]).
    pub(crate) async fn terminate_active_sessions_for_exit_async(&self) -> Result<()> {
        self.terminate_active_sessions_with_mode_async(PtyCloseMode::Immediate).await
    }

    async fn terminate_active_sessions_with_mode_async(&self, mode: PtyCloseMode) -> Result<()> {
        let ids = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|record| !record.background.load(Ordering::Acquire))
                .map(|record| record.metadata.id.clone())
                .collect::<Vec<_>>()
        };

        let mut failures = Vec::new();
        for session_id in ids {
            let should_close = self
                .session_record(session_id.as_str())
                .await
                .map(|record| !record.background.load(Ordering::Acquire))
                .unwrap_or(false);
            if should_close && let Err(err) = self.close_session_with_mode(&session_id, mode).await {
                failures.push(format!("{session_id}: {err}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("failed to terminate active exec sessions: {}", failures.join("; ")))
        }
    }

    /// Force-stop every foreground exec session and close it. Used by the TUI
    /// ForceCancel escape hatch so a stuck PTY cannot keep the composer locked.
    /// Closing after the kill is intentional: `force_terminate_or_close` alone
    /// leaves live sessions attached ("remains visible until closed"), which
    /// would keep the foreground counter elevated. Background sessions are
    /// user-owned and are left alone. Returns `(stopped, closed, failed)`
    /// counts where `stopped` are live kills and `closed` are already-exited
    /// sessions removed from the map.
    pub async fn force_cancel_foreground_sessions(&self) -> (usize, usize, usize) {
        let ids = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|record| !record.background.load(Ordering::Acquire))
                .map(|record| record.metadata.id.clone())
                .collect::<Vec<_>>()
        };

        let mut stopped = 0usize;
        let mut closed = 0usize;
        let mut failed = 0usize;
        for session_id in ids {
            let was_running = self.is_session_completed(session_id.as_str()).await.ok().flatten().is_none();
            if was_running {
                if let Err(error) = self.force_terminate_session(session_id.as_str()).await {
                    tracing::warn!(%session_id, %error, "force-cancel terminate failed");
                }
            }
            match self.close_session(session_id.as_str()).await {
                Ok(_) => {
                    if was_running {
                        stopped += 1;
                    } else {
                        closed += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(%session_id, %error, "force-cancel close failed");
                    failed += 1;
                }
            }
        }
        (stopped, closed, failed)
    }

    /// Return the number of currently live background process reservations.
    #[must_use]
    pub fn active_background_processes(&self) -> usize {
        self.active_background_processes.load(Ordering::Acquire)
    }

    /// Request that the current foreground session be promoted to background.
    ///
    /// The request is synchronous because it is called by the TUI key-event
    /// callback. The execution wait loop consumes the request asynchronously
    /// and performs the metadata transition without killing the process.
    pub fn request_foreground_background(&self) -> Option<BackgroundShortcutResult> {
        let mut request = self.background_request.lock();
        if request.is_some() {
            *self.background_shortcut_result.lock() = Some(BackgroundShortcutResult::Requested);
            return Some(BackgroundShortcutResult::Requested);
        }

        // Take the foreground lock after `background_request`. Completion and
        // close paths use the same order when clearing a pending promotion;
        // holding both here prevents a completed session from being observed
        // between the foreground lookup and request reservation.
        let Some(session_id) = self.foreground_session.lock().clone() else {
            *self.background_shortcut_result.lock() = None;
            return None;
        };

        let result = match self.reserve_background_slot() {
            Ok(_) => {
                *request = Some(session_id);
                BackgroundShortcutResult::Requested
            }
            Err(_) => BackgroundShortcutResult::AtCapacity,
        };
        *self.background_shortcut_result.lock() = Some(result);
        Some(result)
    }

    /// Consume the result associated with the queued Ctrl+B event.
    pub fn take_background_shortcut_result(&self) -> Option<BackgroundShortcutResult> {
        self.background_shortcut_result.lock().take()
    }

    /// Apply a pending Ctrl+B promotion to the specified session.
    pub(crate) async fn promote_requested_session(&self, session_id: &str) -> Result<bool> {
        let _lifecycle_guard = self.create_lock.lock().await;
        if !self.take_foreground_promotion_request(session_id) {
            return Ok(false);
        }

        let record = match self.session_record(session_id).await {
            Ok(record) => record,
            Err(error) => {
                self.release_reserved_background_slot(true);
                return Err(error);
            }
        };
        if record.background.load(Ordering::Acquire) {
            self.release_reserved_background_slot(true);
            self.clear_foreground_session(session_id);
            return Ok(false);
        }
        match self.is_session_completed(session_id).await {
            Ok(Some(_)) => {
                self.release_reserved_background_slot(true);
                self.clear_foreground_session(session_id);
                return Ok(false);
            }
            Ok(None) => {}
            Err(error) => {
                self.release_reserved_background_slot(true);
                self.set_foreground_session(ExecSessionId::new(session_id));
                return Err(error);
            }
        }

        record.background_slot_reserved.store(true, Ordering::Release);
        self.release_foreground_pty_count(&record);
        record.background.store(true, Ordering::Release);
        record.show_in_background_drawer.store(true, Ordering::Release);
        self.clear_foreground_session(session_id);
        let promotion_marker = Arc::clone(&record);
        self.start_background_watcher(record, session_id.to_string());
        promotion_marker
            .background_promoted_from_foreground
            .store(true, Ordering::Release);
        Ok(true)
    }

    pub(crate) async fn take_foreground_promotion(&self, session_id: &str) -> Result<bool> {
        let record = self.session_record(session_id).await?;
        Ok(record.background_promoted_from_foreground.swap(false, Ordering::AcqRel))
    }

    fn reserve_background_slot(&self) -> Result<bool> {
        let mut current = self.active_background_processes.load(Ordering::Acquire);
        loop {
            if current >= MAX_BACKGROUND_PROCESSES {
                bail!(
                    "maximum background process limit reached ({MAX_BACKGROUND_PROCESSES}); wait for or close an existing background session before starting another"
                );
            }
            match self.active_background_processes.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(true),
                Err(observed) => current = observed,
            }
        }
    }

    fn release_reserved_background_slot(&self, reserved: bool) {
        if reserved {
            self.active_background_processes.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn release_background_slot(&self, record: &ExecSessionRecord) {
        if record.background_slot_reserved.swap(false, Ordering::AcqRel) {
            self.active_background_processes.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn count_foreground_pty_session(&self, record: &ExecSessionRecord) {
        let Some(counter) = self.foreground_pty_counter.lock().as_ref().map(Arc::clone) else {
            return;
        };
        counter.fetch_add(1, Ordering::Relaxed);
        *record.foreground_pty_counter.lock() = Some(counter);
    }

    fn release_foreground_pty_count(&self, record: &ExecSessionRecord) {
        let Some(counter) = record.foreground_pty_counter.lock().take() else {
            return;
        };
        let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| current.checked_sub(1));
    }

    fn clear_foreground_and_take_pending_request(&self, session_id: &str) -> bool {
        let mut request = self.background_request.lock();
        let taken = request.as_deref() == Some(session_id) && request.take().is_some();
        let mut foreground = self.foreground_session.lock();
        if foreground.as_deref() == Some(session_id) {
            *foreground = None;
        }
        drop(foreground);
        if taken {
            *self.background_shortcut_result.lock() = None;
        }
        taken
    }

    fn take_foreground_promotion_request(&self, session_id: &str) -> bool {
        let mut request = self.background_request.lock();
        if request.as_deref() != Some(session_id) || request.take().is_none() {
            return false;
        }
        let mut foreground = self.foreground_session.lock();
        if foreground.as_deref() == Some(session_id) {
            *foreground = None;
        }
        drop(foreground);
        *self.background_shortcut_result.lock() = None;
        true
    }

    fn release_pending_background_request(&self, session_id: &str) {
        if self.clear_foreground_and_take_pending_request(session_id) {
            self.release_reserved_background_slot(true);
        }
    }

    fn start_background_watcher(&self, record: Arc<ExecSessionRecord>, session_id: String) {
        let manager = self.clone();
        let record_for_task = Arc::clone(&record);
        let task = tokio::spawn(async move {
            loop {
                match manager.is_session_completed(session_id.as_str()).await {
                    Ok(Some(exit_code)) => {
                        manager.capture_background_completion_output(session_id.as_str()).await;
                        if record_for_task
                            .background_completion_published
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            let command = bounded_completion_command(&record_for_task.metadata.command_label());
                            let _ = manager.completion_tx.send(ExecSessionCompletionEvent {
                                session_id: ExecSessionId::new(session_id.clone()),
                                command,
                                managed_background: !record_for_task.show_in_background_drawer.load(Ordering::Acquire),
                                termination_requested: record_for_task.termination_requested.load(Ordering::Acquire),
                                exit_code,
                            });
                            // Keep one wake permit when the idle loop has not
                            // entered its wait yet; the broadcast receiver
                            // drains every completion that accumulated behind
                            // the single permit.
                            manager.completion_notify.notify_one();
                        }
                        manager.release_background_slot(&record_for_task);
                        break;
                    }
                    Ok(None) => tokio::time::sleep(tokio::time::Duration::from_millis(50)).await,
                    Err(_) => {
                        // A transient status error must not release the live
                        // background reservation. Only a removed session ends
                        // this watcher without a confirmed process exit; its
                        // close path owns the reservation release.
                        if manager.session_record(session_id.as_str()).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    }
                }
            }
        });
        if let Some(previous) = record.background_watch.lock().replace(task) {
            previous.abort();
        }
    }

    async fn capture_background_completion_output(&self, session_id: &str) {
        let deadline = tokio::time::Instant::now() + EXEC_SESSION_COMPLETION_DRAIN_TIMEOUT;
        loop {
            let _ = self.read_session_output(session_id, false).await;
            if self.is_output_drained(session_id).await.unwrap_or(false) || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(EXEC_SESSION_COMPLETION_DRAIN_POLL).await;
        }
        // Capture bytes that raced with the final drain-state observation.
        let _ = self.read_session_output(session_id, false).await;
    }

    fn start_foreground_watcher(&self, record: Arc<ExecSessionRecord>, session_id: String) {
        let manager = self.clone();
        let record_for_task = Arc::clone(&record);
        let task = tokio::spawn(async move {
            loop {
                if manager.promote_requested_session(session_id.as_str()).await.unwrap_or(false) {
                    break;
                }
                if record_for_task.background.load(Ordering::Acquire) {
                    break;
                }
                match manager.is_session_completed(session_id.as_str()).await {
                    Ok(Some(_)) => {
                        manager.release_foreground_pty_count(&record_for_task);
                        manager.release_pending_background_request(session_id.as_str());
                        break;
                    }
                    Ok(None) => tokio::time::sleep(tokio::time::Duration::from_millis(50)).await,
                    Err(_) => {
                        if manager.session_record(session_id.as_str()).await.is_err() {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    }
                }
            }
        });
        if let Some(previous) = record.foreground_watch.lock().replace(task) {
            previous.abort();
        }
    }

    fn set_foreground_session(&self, session_id: ExecSessionId) {
        *self.foreground_session.lock() = Some(session_id);
    }

    fn clear_foreground_session(&self, session_id: &str) {
        let mut foreground = self.foreground_session.lock();
        if foreground.as_deref() == Some(session_id) {
            *foreground = None;
        }
    }

    fn clear_focused_session_if_matches(&self, session_id: &str) {
        let mut focused = self.focused_session.lock();
        if focused.as_deref().is_some_and(|id| id == session_id) {
            *focused = None;
        }
    }

    async fn insert_session(
        &self,
        metadata: VTCodeExecSession,
        backend: ExecSessionBackend,
        pty_guard: Option<PtySessionGuard>,
        launch_mode: ExecSessionLaunchMode,
        background_slot_reserved: bool,
    ) -> Result<Arc<ExecSessionRecord>> {
        let mut sessions = self.sessions.write().await;
        use hashbrown::hash_map::Entry;
        match sessions.entry(metadata.id.clone()) {
            Entry::Occupied(_) => Err(anyhow!("exec session '{}' already exists", metadata.id.as_str())),
            Entry::Vacant(entry) => {
                let record = Arc::new(ExecSessionRecord::new(
                    metadata,
                    backend,
                    pty_guard,
                    launch_mode,
                    background_slot_reserved,
                ));
                // Foreground PTY and pipe sessions both support Ctrl+B backgrounding,
                // so both increment the shared foreground counter driving the TUI hint.
                if launch_mode.sets_foreground_session() {
                    self.count_foreground_pty_session(&record);
                }
                entry.insert(Arc::clone(&record));
                Ok(record)
            }
        }
    }

    async fn ensure_session_absent(&self, session_id: &str) -> Result<()> {
        let sessions = self.sessions.read().await;
        if sessions.contains_key(session_id) {
            return Err(anyhow!("exec session '{session_id}' already exists"));
        }
        Ok(())
    }

    async fn session_record(&self, session_id: &str) -> Result<Arc<ExecSessionRecord>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| missing_exec_session_error(session_id))
    }
}

#[cfg(all(test, unix))]
mod tests;
