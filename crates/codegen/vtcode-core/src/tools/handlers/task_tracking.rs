use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TaskStepMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum TaskItemInput {
    Text(String),
    Structured(TaskItemInputObject),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TaskItemInputObject {
    pub description: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list")]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string_list")]
    pub verify: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskTrackingStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

impl std::fmt::Display for TaskTrackingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for TaskTrackingStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "blocked" => Ok(Self::Blocked),
            other => {
                bail!("Invalid status '{other}'. Use: pending, in_progress, completed, blocked")
            }
        }
    }
}

impl TaskTrackingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
        }
    }

    pub fn flat_checkbox(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[/]",
            Self::Completed => "[x]",
            Self::Blocked => "[!]",
        }
    }

    pub fn plan_checkbox(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[~]",
            Self::Completed => "[x]",
            Self::Blocked => "[!]",
        }
    }

    pub fn view_symbol(&self) -> &'static str {
        match self {
            Self::Pending => "•",
            Self::InProgress => ">",
            Self::Completed => "✔",
            Self::Blocked => "!",
        }
    }

    pub fn compact_view_symbol(&self) -> &'static str {
        match self {
            Self::Pending => "□",
            Self::InProgress => "[-]",
            Self::Completed => "[x]",
            Self::Blocked => "[!]",
        }
    }
}

pub fn parse_marked_status_prefix(value: &str) -> Option<(TaskTrackingStatus, String)> {
    let trimmed = value.trim_start();
    let mapping = [
        ("[x] ", TaskTrackingStatus::Completed),
        ("[X] ", TaskTrackingStatus::Completed),
        ("[~] ", TaskTrackingStatus::InProgress),
        ("[/] ", TaskTrackingStatus::InProgress),
        ("[!] ", TaskTrackingStatus::Blocked),
        ("[ ] ", TaskTrackingStatus::Pending),
    ];
    for (prefix, status) in mapping {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return Some((status, rest.to_string()));
        }
    }
    None
}

pub fn parse_status_prefix(value: &str) -> (TaskTrackingStatus, String) {
    parse_marked_status_prefix(value).unwrap_or((TaskTrackingStatus::Pending, value.trim_start().to_string()))
}

pub fn append_notes(existing: Option<String>, append: Option<&str>) -> Option<String> {
    match (existing, append) {
        (None, None) => None,
        (Some(text), None) => {
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        (None, Some(extra)) => {
            let trimmed = extra.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        (Some(text), Some(extra)) => {
            let left = text.trim();
            let right = extra.trim();
            if left.is_empty() && right.is_empty() {
                None
            } else if left.is_empty() {
                Some(right.to_string())
            } else if right.is_empty() {
                Some(left.to_string())
            } else {
                Some(format!("{left}\n{right}"))
            }
        }
    }
}

pub fn append_notes_section(markdown: &mut String, notes: Option<&str>) {
    if let Some(text) = notes {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            markdown.push_str("\n## Notes\n\n");
            markdown.push_str(trimmed);
            markdown.push('\n');
        }
    }
}

pub(crate) fn validate_update_shape<T>(
    items: Option<&[T]>,
    index: Option<usize>,
    index_path: Option<&str>,
    status: Option<&str>,
) -> Result<()> {
    if index.is_some() && index_path.is_some() {
        bail!("task-tracker updates cannot combine 'index' and 'index_path'");
    }

    if items.is_some() && (index.is_some() || index_path.is_some() || status.is_some()) {
        bail!("bulk task-tracker updates cannot combine 'items' with 'index', 'index_path', or 'status'");
    }

    Ok(())
}

pub(crate) fn validate_action_index_fields(action: &str, index: Option<usize>, index_path: Option<&str>) -> Result<()> {
    if action != "update" && (index.is_some() || index_path.is_some()) {
        bail!(
            "task-tracker action '{action}' cannot use 'index' or 'index_path'; indices are only valid with action='update'"
        );
    }

    Ok(())
}

pub fn is_bulk_sync_update<T>(
    items: Option<&[T]>,
    index: Option<usize>,
    index_path: Option<&str>,
    status: Option<&str>,
) -> bool {
    items.is_some() && index.is_none() && index_path.is_none() && status.is_none()
}

pub fn deserialize_optional_string_list<'de, D>(deserializer: D) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    let parsed = Option::<OneOrMany>::deserialize(deserializer)?;
    Ok(parsed.map(|value| match value {
        OneOrMany::One(item) => vec![item],
        OneOrMany::Many(items) => items,
    }))
}

pub fn normalize_string_items(items: Option<&[String]>) -> Vec<String> {
    items
        .unwrap_or(&[])
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn normalize_optional_text(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(ToOwned::to_owned)
}

pub fn append_task_step_metadata(markdown: &mut String, indent: &str, metadata: &TaskStepMetadata) {
    if !metadata.files.is_empty() {
        markdown.push_str(indent);
        markdown.push_str("  files: ");
        markdown.push_str(&metadata.files.join(", "));
        markdown.push('\n');
    }

    if let Some(outcome) = metadata.outcome.as_deref() {
        markdown.push_str(indent);
        markdown.push_str("  outcome: ");
        markdown.push_str(outcome);
        markdown.push('\n');
    }

    if metadata.verify.len() == 1 {
        markdown.push_str(indent);
        markdown.push_str("  verify: ");
        markdown.push_str(&metadata.verify[0]);
        markdown.push('\n');
    } else if !metadata.verify.is_empty() {
        markdown.push_str(indent);
        markdown.push_str("  verify:\n");
        for command in &metadata.verify {
            markdown.push_str(indent);
            markdown.push_str("    - ");
            markdown.push_str(command);
            markdown.push('\n');
        }
    }
}

pub fn metadata_from_input(
    files: Option<&[String]>,
    outcome: Option<&str>,
    verify: Option<&[String]>,
) -> TaskStepMetadata {
    TaskStepMetadata {
        files: normalize_string_items(files),
        outcome: normalize_optional_text(outcome),
        verify: normalize_string_items(verify),
    }
}

/// Markers that may trail a task description as inline `Action -> files: [...]
/// -> verify: [...]` metadata. `outcome` is accepted so a polluted suffix is
/// still stripped from the visible row even though the compact view never
/// renders it.
const INLINE_TASK_METADATA_MARKERS: &[&str] = &[" -> files:", " -> verify:", " -> outcome:"];

fn parse_inline_bracket_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(trimmed);
    super::planning_workflow::split_bracket_items(inner)
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// Split an inline `Action -> files: [...] -> verify: [...]` suffix out of a
/// task description.
///
/// Returns `(clean_description, files, verify)`. Markers are recognized only
/// outside inline-code spans so `` `a -> files: b` `` stays intact, and the
/// Unicode `→` arrow is normalized first so both spellings agree with plan
/// validation. When no marker exists outside backticks the description is
/// returned trimmed with empty metadata.
pub fn split_task_description_metadata(description: &str) -> (String, Vec<String>, Vec<String>) {
    let normalized;
    let source = if description.contains('→') {
        normalized = description.replace('→', "->");
        normalized.as_str()
    } else {
        description
    };
    let Some(first) = find_inline_metadata_marker(source, 0) else {
        return (source.trim().to_string(), Vec::new(), Vec::new());
    };
    let head = source[..first.0].trim();
    if head.is_empty() {
        return (source.trim().to_string(), Vec::new(), Vec::new());
    }

    let mut files = Vec::new();
    let mut verify = Vec::new();
    let mut cursor = first.0;
    while let Some((marker_start, marker)) = find_inline_metadata_marker(source, cursor) {
        let value_start = marker_start + marker.len();
        let next = find_inline_metadata_marker(source, value_start)
            .map(|(start, _)| start)
            .unwrap_or(source.len());
        let value = source[value_start..next].trim();
        if marker.contains("files:") {
            files.extend(parse_inline_bracket_list(value));
        } else if marker.contains("verify:") {
            verify.extend(parse_inline_bracket_list(value));
        }
        cursor = next;
        if cursor >= source.len() {
            break;
        }
    }
    (head.to_string(), files, verify)
}

/// Strip an inline `-> files:` / `-> verify:` / `-> outcome:` suffix for
/// display. Structured metadata stays in the payload; the visible compact row
/// must not reintroduce it as detail text.
pub fn strip_task_description_metadata(description: &str) -> String {
    split_task_description_metadata(description).0
}

/// One short description for a task row: the leading clause before detail
/// separators (` — `/` – `/` - `/`: `, `; `, `. `, ` (`).
///
/// Plan steps often carry a long detail tail
/// (`"Add X – document Y in Z, sourced from W"`); the TODO transcript and
/// panel must show only `"Add X"` so each item stays on one scannable row.
/// Inline `-> files:`/`-> verify:` suffixes are stripped first; the full text
/// stays in structured tracker metadata.
pub fn short_task_description(description: &str) -> String {
    let (clean, _, _) = split_task_description_metadata(description);
    let base = if clean.trim().is_empty() {
        description.trim()
    } else {
        clean.trim()
    };
    let first_line = base.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let collapsed = vtcode_commons::formatting::collapse_whitespace(first_line);
    if collapsed.is_empty() {
        return String::new();
    }
    let mut cut = collapsed.len();
    for separator in [" — ", " – ", " - ", ": ", "; ", ". ", " ("] {
        if let Some(index) = collapsed.find(separator) {
            cut = cut.min(index);
        }
    }
    let short = collapsed[..cut].trim().trim_end_matches([',', ';', ':', '.']).trim();
    if short.is_empty() { collapsed } else { short.to_string() }
}

fn find_inline_metadata_marker(source: &str, from: usize) -> Option<(usize, &'static str)> {
    let mut inline_ticks: Option<usize> = None;
    let mut cursor = from.min(source.len());
    while cursor < source.len() {
        let remainder = &source[cursor..];
        if remainder.starts_with('`') {
            let run = remainder.bytes().take_while(|byte| *byte == b'`').count();
            if inline_ticks.is_some_and(|ticks| ticks == run) {
                inline_ticks = None;
            } else if inline_ticks.is_none() {
                inline_ticks = Some(run);
            }
            cursor += run;
            continue;
        }
        if inline_ticks.is_none() {
            for marker in INLINE_TASK_METADATA_MARKERS {
                if remainder.starts_with(marker) {
                    return Some((cursor, marker));
                }
            }
        }
        let character = remainder.chars().next()?;
        cursor += character.len_utf8();
    }
    None
}

/// A renderer-independent task tree used by both task-tracker implementations.
///
/// The persisted checklist formats remain implementation-specific; this type is
/// only the shared shape required to produce the compact visible view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskTreeNode {
    pub(crate) index_path: String,
    pub(crate) description: String,
    pub(crate) status: TaskTrackingStatus,
    pub(crate) metadata: TaskStepMetadata,
    pub(crate) children: Vec<TaskTreeNode>,
}

/// Format task nodes as the compact tree shared by inline output and the task
/// panel. Root leaves retain `├`/`└` connectors; nested leaves use their status
/// glyph as the connector so the hierarchy stays readable without redundant
/// branch characters.
pub(crate) fn compact_task_tree_view(nodes: &[TaskTreeNode]) -> Vec<Value> {
    let mut lines = Vec::new();
    append_compact_task_tree_view(nodes, "  ", &mut lines);
    lines
}

fn append_compact_task_tree_view(nodes: &[TaskTreeNode], tree_prefix: &str, out: &mut Vec<Value>) {
    for (index, node) in nodes.iter().enumerate() {
        let is_last = index + 1 == nodes.len();
        let branch = if is_last { "└" } else { "├" };
        let display = if node.children.is_empty() {
            if tree_prefix == "  " {
                format!("{tree_prefix}{branch} {} {}", node.status.compact_view_symbol(), node.description)
            } else {
                format!("{tree_prefix}{} {}", node.status.compact_view_symbol(), node.description)
            }
        } else {
            format!("{tree_prefix}{branch} {}", node.description)
        };

        out.push(json!({
            "display": display,
            "index_path": node.index_path.clone(),
            "status": node.status.as_str(),
            "text": node.description.clone(),
            "files": node.metadata.files.clone(),
            "outcome": node.metadata.outcome.clone(),
            "verify": node.metadata.verify.clone(),
        }));

        let next_prefix = if is_last {
            format!("{tree_prefix}  ")
        } else {
            format!("{tree_prefix}│ ")
        };
        append_compact_task_tree_view(&node.children, &next_prefix, out);
    }
}

/// Build the compact tree from a flattened checklist summary.
///
/// Standard task trackers expose numeric `index` values while planning
/// trackers expose dotted `index_path` values. Accept both forms so callers at
/// the UI boundary do not need to know which tracker produced the payload.
pub fn compact_task_tree_view_from_items(items: &[Value]) -> Vec<Value> {
    let mut ordered_paths = Vec::with_capacity(items.len());
    let mut nodes_by_path = std::collections::HashMap::with_capacity(items.len());

    for (position, item) in items.iter().enumerate() {
        let index_path = item
            .get("index_path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| item.get("index").and_then(Value::as_u64).map(|index| index.to_string()))
            .unwrap_or_else(|| (position + 1).to_string());
        if nodes_by_path.contains_key(&index_path) {
            continue;
        }

        let status = match item.get("status").and_then(Value::as_str) {
            Some(value) => match TaskTrackingStatus::from_str(value) {
                Ok(status) => status,
                Err(_) => continue,
            },
            None => TaskTrackingStatus::Pending,
        };
        let Some(description) = item
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| item.get("text").and_then(Value::as_str))
            .filter(|description| !description.trim().is_empty())
        else {
            continue;
        };
        // Descriptions copied from plan steps may still carry an inline
        // `Action -> files: [...] -> verify: [...]` suffix plus a long detail
        // tail (`"Add X – document Y, sourced from Z"`). Keep only the short
        // leading clause for the visible row and fold the rest into structured
        // metadata so the compact view never reintroduces files/verify/detail
        // text.
        let (clean_description, parsed_files, parsed_verify) = split_task_description_metadata(description);
        let raw = if clean_description.trim().is_empty() {
            description.trim()
        } else {
            clean_description.trim()
        };
        let description = short_task_description(raw);
        let mut metadata = TaskStepMetadata {
            files: string_array(item.get("files")),
            outcome: item.get("outcome").and_then(Value::as_str).map(ToOwned::to_owned),
            verify: string_or_string_array(item.get("verify")),
        };
        if metadata.files.is_empty() {
            metadata.files = parsed_files;
        }
        if metadata.verify.is_empty() {
            metadata.verify = parsed_verify;
        }

        ordered_paths.push(index_path.clone());
        nodes_by_path.insert(
            index_path.clone(),
            TaskTreeNode {
                index_path,
                description,
                status,
                metadata,
                children: Vec::new(),
            },
        );
    }

    let mut attachment_order = ordered_paths.clone();
    attachment_order.sort_by_key(|path| std::cmp::Reverse(path.split('.').count()));
    for path in attachment_order {
        let Some(parent_path) = path.rsplit_once('.').map(|(parent, _)| parent.to_string()) else {
            continue;
        };
        let Some(child) = nodes_by_path.remove(&path) else {
            continue;
        };
        if let Some(parent) = nodes_by_path.get_mut(&parent_path) {
            parent.children.push(child);
        } else {
            nodes_by_path.insert(path, child);
        }
    }

    let roots = ordered_paths
        .into_iter()
        .filter_map(|path| nodes_by_path.remove(&path))
        .collect::<Vec<_>>();
    compact_task_tree_view(&roots)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect()
}

fn string_or_string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) => vec![value.clone()],
        Some(Value::Array(_)) => string_array(value),
        _ => Vec::new(),
    }
}

#[derive(Default)]
pub struct TaskCounts {
    pub total: usize,
    pub completed: usize,
    pub in_progress: usize,
    pub pending: usize,
    pub blocked: usize,
}

impl TaskCounts {
    pub fn add(&mut self, status: &TaskTrackingStatus) {
        self.total += 1;
        match status {
            TaskTrackingStatus::Pending => self.pending += 1,
            TaskTrackingStatus::InProgress => self.in_progress += 1,
            TaskTrackingStatus::Completed => self.completed += 1,
            TaskTrackingStatus::Blocked => self.blocked += 1,
        }
    }

    pub fn progress_percent(&self) -> usize {
        if self.total > 0 {
            #[allow(
                clippy::cast_sign_loss,
                clippy::let_and_return,
                reason = "Intentional compatibility, platform, or test-only suppression."
            )]
            let progress = ((self.completed as f64 / self.total as f64 * 100.0).round()).max(0.0) as usize;
            progress
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_marked_status_prefix_rejects_unmarked_text() {
        let parsed = parse_marked_status_prefix("plain text without marker");
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_status_prefix_defaults_to_pending_for_unmarked_text() {
        let (status, description) = parse_status_prefix("plain text without marker");
        assert_eq!(status, TaskTrackingStatus::Pending);
        assert_eq!(description, "plain text without marker");
    }

    #[test]
    fn parse_status_prefix_supports_both_in_progress_markers() {
        let (status_tilde, text_tilde) = parse_status_prefix("[~] do thing");
        let (status_slash, text_slash) = parse_status_prefix("[/] do thing");
        assert_eq!(status_tilde, TaskTrackingStatus::InProgress);
        assert_eq!(status_slash, TaskTrackingStatus::InProgress);
        assert_eq!(text_tilde, "do thing");
        assert_eq!(text_slash, "do thing");
    }

    #[test]
    fn append_notes_joins_with_single_newline() {
        let merged = append_notes(Some("left".to_string()), Some("right"));
        assert_eq!(merged, Some("left\nright".to_string()));
    }

    #[test]
    fn append_notes_section_ignores_blank_notes() {
        let mut markdown = "# Title\n".to_string();
        append_notes_section(&mut markdown, Some("   "));
        assert_eq!(markdown, "# Title\n");
    }

    #[test]
    fn is_bulk_sync_update_requires_items_and_missing_single_item_fields() {
        let items = vec!["Step".to_string()];
        let no_items: Option<&[String]> = None;
        assert!(is_bulk_sync_update(Some(&items), None, None, None));
        assert!(!is_bulk_sync_update(no_items, Some(1), None, Some("completed")));
    }

    #[test]
    fn is_bulk_sync_update_rejects_mixed_single_item_fields() {
        let items = vec!["Step".to_string()];
        let error = validate_update_shape(Some(&items), Some(1), None, Some("completed"))
            .expect_err("mixed bulk and single update fields must fail closed");

        assert!(error.to_string().contains("cannot combine 'items'"));
    }

    #[test]
    fn task_tracker_update_rejects_both_index_forms() {
        let error = validate_update_shape::<String>(None, Some(1), Some("1"), Some("completed"))
            .expect_err("an update must select exactly one index form");

        assert!(error.to_string().contains("cannot combine 'index' and 'index_path'"));
    }

    #[test]
    fn task_counts_tracks_progress() {
        let mut counts = TaskCounts::default();
        counts.add(&TaskTrackingStatus::Completed);
        counts.add(&TaskTrackingStatus::Pending);
        counts.add(&TaskTrackingStatus::Blocked);
        counts.add(&TaskTrackingStatus::InProgress);
        assert_eq!(counts.total, 4);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.pending, 1);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.in_progress, 1);
        assert_eq!(counts.progress_percent(), 25);
    }

    #[test]
    fn compact_task_tree_view_formats_flat_leaf_statuses_exactly() {
        let nodes = [
            TaskTreeNode {
                index_path: "1".to_string(),
                description: "Pending".to_string(),
                status: TaskTrackingStatus::Pending,
                metadata: TaskStepMetadata::default(),
                children: Vec::new(),
            },
            TaskTreeNode {
                index_path: "2".to_string(),
                description: "In progress".to_string(),
                status: TaskTrackingStatus::InProgress,
                metadata: TaskStepMetadata::default(),
                children: Vec::new(),
            },
            TaskTreeNode {
                index_path: "3".to_string(),
                description: "Completed".to_string(),
                status: TaskTrackingStatus::Completed,
                metadata: TaskStepMetadata::default(),
                children: Vec::new(),
            },
            TaskTreeNode {
                index_path: "4".to_string(),
                description: "Blocked".to_string(),
                status: TaskTrackingStatus::Blocked,
                metadata: TaskStepMetadata::default(),
                children: Vec::new(),
            },
        ];

        let displays = compact_task_tree_view(&nodes)
            .into_iter()
            .filter_map(|line| line.get("display").and_then(Value::as_str).map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(
            displays,
            vec![
                "  ├ □ Pending".to_string(),
                "  ├ [-] In progress".to_string(),
                "  ├ [x] Completed".to_string(),
                "  └ [!] Blocked".to_string(),
            ]
        );
    }

    #[test]
    fn compact_task_tree_view_omits_parent_status_but_keeps_metadata_fields() {
        let nodes = [TaskTreeNode {
            index_path: "1".to_string(),
            description: "Parent".to_string(),
            status: TaskTrackingStatus::InProgress,
            metadata: TaskStepMetadata {
                files: vec!["Cargo.toml".to_string()],
                outcome: Some("Ready".to_string()),
                verify: vec!["cargo check".to_string()],
            },
            children: vec![TaskTreeNode {
                index_path: "1.1".to_string(),
                description: "Child".to_string(),
                status: TaskTrackingStatus::Completed,
                metadata: TaskStepMetadata::default(),
                children: Vec::new(),
            }],
        }];

        let lines = compact_task_tree_view(&nodes);
        assert_eq!(lines[0]["display"], "  └ Parent");
        assert_eq!(lines[1]["display"], "    [x] Child");
        assert_eq!(lines[0]["files"], json!(["Cargo.toml"]));
        assert_eq!(lines[0]["outcome"], "Ready");
        assert_eq!(lines[0]["verify"], json!(["cargo check"]));
    }

    #[test]
    fn split_task_description_metadata_strips_files_and_verify_suffix() {
        let (clean, files, verify) =
            split_task_description_metadata("Emit summary -> files: [src/a.rs, src/b.rs] -> verify: [cargo check]");
        assert_eq!(clean, "Emit summary");
        assert_eq!(files, vec!["src/a.rs".to_string(), "src/b.rs".to_string()]);
        assert_eq!(verify, vec!["cargo check".to_string()]);
    }

    #[test]
    fn split_task_description_metadata_keeps_inline_code_arrows_intact() {
        let (clean, files, verify) =
            split_task_description_metadata("Use `a -> files: b` in code -> files: [src/a.rs]");
        assert_eq!(clean, "Use `a -> files: b` in code");
        assert_eq!(files, vec!["src/a.rs".to_string()]);
        assert!(verify.is_empty());
    }

    #[test]
    fn split_task_description_metadata_without_markers_returns_trimmed_description() {
        let (clean, files, verify) = split_task_description_metadata("  Just an action  ");
        assert_eq!(clean, "Just an action");
        assert!(files.is_empty());
        assert!(verify.is_empty());
    }

    #[test]
    fn split_task_description_metadata_preserves_quoted_commas_in_files_and_verify() {
        let (clean, files, verify) = split_task_description_metadata(
            "Inspect range -> files: ['src/a.rs, b.rs', src/c.rs] -> verify: [sed -n '/^## A/,/^## B/p']",
        );
        assert_eq!(clean, "Inspect range");
        assert_eq!(files, vec!["'src/a.rs, b.rs'".to_string(), "src/c.rs".to_string()]);
        assert_eq!(verify, vec!["sed -n '/^## A/,/^## B/p'".to_string()]);
    }

    #[test]
    fn compact_task_tree_view_from_items_strips_inline_metadata_into_fields() {
        let items = vec![json!({
            "index_path": "1",
            "description": "Update parser -> files: [src/a.rs] -> verify: [cargo check]",
            "status": "pending",
        })];
        let rows = compact_task_tree_view_from_items(&items);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["display"], "  └ □ Update parser");
        assert_eq!(rows[0]["text"], "Update parser");
        assert_eq!(rows[0]["files"], json!(["src/a.rs"]));
        assert_eq!(rows[0]["verify"], json!(["cargo check"]));
    }

    #[test]
    fn short_task_description_keeps_leading_clause_only() {
        assert_eq!(
            short_task_description(
                "Add vtcode exec resume to the Commands section – document the cross-turn contract, sourced from ExecSubcommand::Resume"
            ),
            "Add vtcode exec resume to the Commands section"
        );
        assert_eq!(
            short_task_description("Update the Everyday recipes block — add a headless example"),
            "Update the Everyday recipes block"
        );
        assert_eq!(
            short_task_description("Emit summary -> files: [src/a.rs] -> verify: [cargo check]"),
            "Emit summary"
        );
        assert_eq!(short_task_description("Verify with cargo check"), "Verify with cargo check");
        assert_eq!(short_task_description(""), "");
    }

    #[test]
    fn compact_task_tree_view_from_items_shortens_detail_tail() {
        let items = vec![json!({
            "index_path": "1",
            "description": "Add vtcode exec resume to the Commands section – document the cross-turn contract in the command table",
            "status": "pending",
        })];
        let rows = compact_task_tree_view_from_items(&items);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["display"], "  └ □ Add vtcode exec resume to the Commands section");
        assert_eq!(rows[0]["text"], "Add vtcode exec resume to the Commands section");
    }
}
