#![warn(missing_docs)]
#![expect(
    clippy::indexing_slicing,
    clippy::string_slice,
    reason = "validated diff indexes and UTF-8 token boundaries are structural invariants"
)]
//! Bounded structured text diffs and renderer-neutral preview rows.
//!
//! The presentation architecture and underlying ideas (unified and side-by-side
//! terminal previews with intraline emphasis) were informed by
//! [OpenAI Codex](https://github.com/openai/codex) (Apache-2.0). This crate is
//! an independent implementation for VT Code; no Codex source code was copied.

use similar::{ChangeTag, TextDiff};
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A contiguous borrowed character-level diff chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk<'a> {
    /// Text present on both sides.
    Equal(&'a str),
    /// Text present only on the old side.
    Delete(&'a str),
    /// Text present only on the new side.
    Insert(&'a str),
}

/// Algorithms suitable for interactive diff previews.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DiffAlgorithm {
    /// Practical, heuristic Myers diff.
    #[default]
    Myers,
    /// Patience diff, useful for source code with unique anchors.
    Patience,
    /// Histogram diff, useful for repeated source-code lines.
    Histogram,
}

impl DiffAlgorithm {
    fn similar(self) -> similar::Algorithm {
        match self {
            Self::Myers => similar::Algorithm::Myers,
            Self::Patience => similar::Algorithm::Patience,
            Self::Histogram => similar::Algorithm::Histogram,
        }
    }
}

/// Options controlling diff generation.
#[derive(Debug, Clone)]
pub struct DiffOptions<'a> {
    /// Number of unchanged lines retained around a change.
    pub context_lines: usize,
    /// Optional old-side label used by formatters.
    pub old_label: Option<&'a str>,
    /// Optional new-side label used by formatters.
    pub new_label: Option<&'a str>,
    /// Whether formatters should emit missing-final-newline markers.
    pub missing_newline_hint: bool,
    /// Line diff algorithm.
    pub algorithm: DiffAlgorithm,
    /// Maximum line-diff computation time.
    pub timeout: Duration,
    /// Maximum total time spent on intraline refinement.
    pub inline_timeout: Duration,
}

impl Default for DiffOptions<'_> {
    fn default() -> Self {
        Self {
            context_lines: 3,
            old_label: None,
            new_label: None,
            missing_newline_hint: true,
            algorithm: DiffAlgorithm::Myers,
            timeout: Duration::from_millis(200),
            inline_timeout: Duration::from_millis(40),
        }
    }
}

/// A diff hunk with old/new ranges and semantic lines.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiffHunk {
    /// One-based old-side start line.
    pub old_start: usize,
    /// Number of represented old-side lines.
    pub old_lines: usize,
    /// One-based new-side start line.
    pub new_start: usize,
    /// Number of represented new-side lines.
    pub new_lines: usize,
    /// Lines contained in the hunk.
    pub lines: Vec<DiffLine>,
}

/// The semantic role of a line inside a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DiffLineKind {
    /// Unchanged context.
    Context,
    /// New-side insertion.
    Addition,
    /// Old-side deletion.
    Deletion,
}

/// A source line and its old/new positions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiffLine {
    /// Semantic line role.
    pub kind: DiffLineKind,
    /// One-based old-side line number, when present.
    pub old_line: Option<u32>,
    /// One-based new-side line number, when present.
    pub new_line: Option<u32>,
    /// Source text, including its original line terminator when present.
    pub text: String,
}

/// Aggregate statistics for a complete document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiffStats {
    /// Added lines.
    pub additions: usize,
    /// Deleted lines.
    pub deletions: usize,
    /// Context lines represented by hunks.
    pub context: usize,
    /// Number of hunks.
    pub hunks: usize,
    /// Semantic rows omitted by a bounded layout.
    pub omitted_rows: usize,
}

/// A complete renderer-independent diff.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiffDocument {
    /// Structured hunks.
    pub hunks: Vec<DiffHunk>,
    /// Whole-document statistics.
    pub stats: DiffStats,
    #[cfg_attr(feature = "serde", serde(default = "default_inline_timeout"))]
    inline_timeout: Duration,
}

impl Default for DiffDocument {
    fn default() -> Self {
        Self {
            hunks: Vec::new(),
            stats: DiffStats::default(),
            inline_timeout: default_inline_timeout(),
        }
    }
}

impl DiffDocument {
    /// Computes a bounded diff from before/after text.
    #[must_use]
    pub fn between(old: &str, new: &str, options: DiffOptions<'_>) -> Self {
        let old_lines = split_lines_with_terminator(old);
        let new_lines = split_lines_with_terminator(new);
        let old_line_refs: Vec<&str> = old_lines.iter().map(String::as_str).collect();
        let new_line_refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
        let mut config = TextDiff::configure();
        let _ = config.algorithm(options.algorithm.similar()).timeout(options.timeout);
        let diff = config.diff_slices(&old_line_refs, &new_line_refs);
        let mut hunks = Vec::new();

        for group in diff.grouped_ops(options.context_lines) {
            let Some(_first) = group.first() else {
                continue;
            };
            let mut lines = Vec::new();
            for operation in &group {
                for change in diff.iter_changes(operation) {
                    let (kind, old_line, new_line) = match change.tag() {
                        ChangeTag::Equal => (
                            DiffLineKind::Context,
                            change.old_index().and_then(one_based_u32),
                            change.new_index().and_then(one_based_u32),
                        ),
                        ChangeTag::Delete => (DiffLineKind::Deletion, change.old_index().and_then(one_based_u32), None),
                        ChangeTag::Insert => (DiffLineKind::Addition, None, change.new_index().and_then(one_based_u32)),
                    };
                    lines.push(DiffLine {
                        kind,
                        old_line,
                        new_line,
                        text: change.value().to_owned(),
                    });
                }
            }
            let old_lines = lines.iter().filter(|line| line.kind != DiffLineKind::Addition).count();
            let new_lines = lines.iter().filter(|line| line.kind != DiffLineKind::Deletion).count();
            let old_start = hunk_start_from_lines(&lines, true);
            let new_start = hunk_start_from_lines(&lines, false);
            hunks.push(DiffHunk { old_start, old_lines, new_start, new_lines, lines });
        }
        let mut document = Self::from_hunks(hunks);
        document.inline_timeout = options.inline_timeout;
        document
    }

    /// Constructs a document from precomputed hunks.
    #[must_use]
    pub fn from_hunks(hunks: Vec<DiffHunk>) -> Self {
        let mut stats = DiffStats { hunks: hunks.len(), ..DiffStats::default() };
        for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                DiffLineKind::Context => stats.context += 1,
                DiffLineKind::Addition => stats.additions += 1,
                DiffLineKind::Deletion => stats.deletions += 1,
            }
        }
        Self {
            hunks,
            stats,
            inline_timeout: default_inline_timeout(),
        }
    }

    /// Parses unified-diff text into a structured document.
    ///
    /// File metadata is accepted and ignored. A body line before the first
    /// valid hunk header is rejected instead of receiving invented numbers.
    pub fn from_unified(input: &str) -> Result<Self, ParseDiffError> {
        let mut hunks = Vec::new();
        let mut current: Option<DiffHunk> = None;
        let mut old_line = 0u32;
        let mut new_line = 0u32;
        let mut expected_old_lines = 0usize;
        let mut expected_new_lines = 0usize;
        let mut omitted_in_current_hunk = false;
        let mut omitted_rows = 0usize;
        let mut omitted_tail = Vec::new();

        for raw_with_ending in split_line_slices(input) {
            let raw = trim_line_ending(raw_with_ending);
            if raw.starts_with("@@") {
                if let Some(mut hunk) = current.take() {
                    assign_omitted_hunk_tail_line_numbers(
                        &mut hunk,
                        &omitted_tail,
                        expected_old_lines,
                        expected_new_lines,
                    );
                    if !omitted_in_current_hunk {
                        validate_hunk_counts(&hunk, expected_old_lines, expected_new_lines)?;
                    }
                    hunks.push(hunk);
                }
                omitted_tail.clear();
                let (old_start, old_count, new_start, new_count) =
                    parse_hunk_range(raw).ok_or_else(|| ParseDiffError::new("invalid hunk header"))?;
                old_line = old_start;
                new_line = new_start;
                expected_old_lines = old_count;
                expected_new_lines = new_count;
                omitted_in_current_hunk = false;
                current = Some(DiffHunk {
                    old_start: old_start as usize,
                    old_lines: 0,
                    new_start: new_start as usize,
                    new_lines: 0,
                    lines: Vec::new(),
                });
                continue;
            }
            if raw.starts_with("diff ")
                || raw.starts_with("index ")
                || raw == r"\ No newline at end of file"
                || raw.is_empty()
            {
                continue;
            }
            // Git's file/mode headers precede the first hunk in ordinary
            // unified output. Keep them out of the body parser while still
            // rejecting an actual `-...`/`+...` body before any hunk.
            let hunk_complete = current
                .as_ref()
                .is_some_and(|hunk| hunk.old_lines >= expected_old_lines && hunk.new_lines >= expected_new_lines);
            if (current.is_none() || hunk_complete) && is_unified_metadata_line(raw) {
                continue;
            }
            if hunk_complete && (raw.starts_with("--- ") || raw.starts_with("+++ ")) {
                continue;
            }
            let Some(hunk) = current.as_mut() else {
                return Err(ParseDiffError::new("diff body appears before a hunk header"));
            };
            let is_body_line = matches!(raw.as_bytes().first().copied(), Some(b'-' | b'+' | b' '));
            if !is_body_line && let Some(omitted) = parse_omitted_line_count(raw) {
                omitted_in_current_hunk = true;
                let marker_rows = omitted;
                let advance = marker_rows
                    .min(expected_old_lines.saturating_sub(hunk.old_lines))
                    .min(expected_new_lines.saturating_sub(hunk.new_lines));
                old_line = old_line.saturating_add(u32::try_from(advance).unwrap_or(u32::MAX));
                new_line = new_line.saturating_add(u32::try_from(advance).unwrap_or(u32::MAX));
                hunk.old_lines = hunk.old_lines.saturating_add(advance);
                hunk.new_lines = hunk.new_lines.saturating_add(advance);
                omitted_rows = omitted_rows.saturating_add(marker_rows);
                continue;
            }
            let (kind, old_number, new_number) = match raw.as_bytes().first().copied() {
                Some(b'-') => {
                    if hunk.old_lines >= expected_old_lines {
                        return Err(ParseDiffError::new("too many old-side lines for hunk header"));
                    }
                    let number = old_line;
                    old_line = old_line.saturating_add(1);
                    hunk.old_lines += 1;
                    (DiffLineKind::Deletion, Some(number), None)
                }
                Some(b'+') => {
                    if hunk.new_lines >= expected_new_lines {
                        return Err(ParseDiffError::new("too many new-side lines for hunk header"));
                    }
                    let number = new_line;
                    new_line = new_line.saturating_add(1);
                    hunk.new_lines += 1;
                    (DiffLineKind::Addition, None, Some(number))
                }
                Some(b' ') => {
                    if hunk.old_lines >= expected_old_lines || hunk.new_lines >= expected_new_lines {
                        return Err(ParseDiffError::new("too many context lines for hunk header"));
                    }
                    let old_number = old_line;
                    let new_number = new_line;
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                    hunk.old_lines += 1;
                    hunk.new_lines += 1;
                    (DiffLineKind::Context, Some(old_number), Some(new_number))
                }
                _ => return Err(ParseDiffError::new("invalid unified diff body line")),
            };
            hunk.lines.push(DiffLine {
                kind,
                old_line: old_number,
                new_line: new_number,
                text: raw_with_ending[1..].to_owned(),
            });
            if omitted_in_current_hunk {
                omitted_tail.push(hunk.lines.len() - 1);
            }
        }
        if let Some(mut hunk) = current {
            assign_omitted_hunk_tail_line_numbers(&mut hunk, &omitted_tail, expected_old_lines, expected_new_lines);
            if !omitted_in_current_hunk {
                validate_hunk_counts(&hunk, expected_old_lines, expected_new_lines)?;
            }
            hunks.push(hunk);
        }
        let mut document = Self::from_hunks(hunks);
        document.stats.omitted_rows = omitted_rows;
        Ok(document)
    }

    /// Lays out this document with terminal display-width wrapping.
    #[must_use]
    pub fn layout(&self, options: LayoutOptions) -> Vec<DiffRow> {
        layout_document(self, options)
    }
}

fn default_inline_timeout() -> Duration {
    Duration::from_millis(40)
}

fn one_based_u32(index: usize) -> Option<u32> {
    u32::try_from(index).ok()?.checked_add(1)
}

fn hunk_start_from_lines(lines: &[DiffLine], old_side: bool) -> usize {
    let line_number = lines
        .iter()
        .find_map(|line| if old_side { line.old_line } else { line.new_line });
    if let Some(line_number) = line_number {
        return usize::try_from(line_number).unwrap_or(usize::MAX).max(1);
    }

    let counterpart = lines
        .iter()
        .find_map(|line| if old_side { line.new_line } else { line.old_line });
    usize::try_from(counterpart.unwrap_or(1)).unwrap_or(usize::MAX).max(1)
}

fn validate_hunk_counts(
    hunk: &DiffHunk,
    expected_old_lines: usize,
    expected_new_lines: usize,
) -> Result<(), ParseDiffError> {
    if hunk.old_lines == expected_old_lines && hunk.new_lines == expected_new_lines {
        Ok(())
    } else {
        Err(ParseDiffError::new("hunk line counts do not match header"))
    }
}

fn assign_omitted_hunk_tail_line_numbers(
    hunk: &mut DiffHunk,
    tail: &[usize],
    expected_old_lines: usize,
    expected_new_lines: usize,
) {
    if tail.is_empty() {
        return;
    }
    let old_tail_count = tail
        .iter()
        .filter(|&&index| hunk.lines[index].kind != DiffLineKind::Addition)
        .count();
    let new_tail_count = tail
        .iter()
        .filter(|&&index| hunk.lines[index].kind != DiffLineKind::Deletion)
        .count();
    let mut old_line = u32::try_from(hunk.old_start)
        .unwrap_or(u32::MAX)
        .saturating_add(u32::try_from(expected_old_lines).unwrap_or(u32::MAX))
        .saturating_sub(u32::try_from(old_tail_count).unwrap_or(u32::MAX));
    let mut new_line = u32::try_from(hunk.new_start)
        .unwrap_or(u32::MAX)
        .saturating_add(u32::try_from(expected_new_lines).unwrap_or(u32::MAX))
        .saturating_sub(u32::try_from(new_tail_count).unwrap_or(u32::MAX));
    for &index in tail {
        match hunk.lines[index].kind {
            DiffLineKind::Addition => {
                hunk.lines[index].new_line = Some(new_line);
                new_line = new_line.saturating_add(1);
            }
            DiffLineKind::Deletion => {
                hunk.lines[index].old_line = Some(old_line);
                old_line = old_line.saturating_add(1);
            }
            DiffLineKind::Context => {
                hunk.lines[index].old_line = Some(old_line);
                hunk.lines[index].new_line = Some(new_line);
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
        }
    }
}

/// Error returned for malformed unified diff input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiffError {
    message: &'static str,
}

impl ParseDiffError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }
}

impl fmt::Display for ParseDiffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ParseDiffError {}

/// A diff rendered with both structured hunks and formatted text.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiffBundle {
    /// Structured hunks.
    pub hunks: Vec<DiffHunk>,
    /// Caller-formatted representation.
    pub formatted: String,
    /// Whether both inputs were identical.
    pub is_empty: bool,
}

/// Computes a structured diff and passes non-empty hunks to a formatter.
pub fn compute_diff<F>(old: &str, new: &str, options: DiffOptions<'_>, formatter: F) -> DiffBundle
where
    F: FnOnce(&[DiffHunk], &DiffOptions<'_>) -> String,
{
    if old == new {
        return DiffBundle {
            hunks: Vec::new(),
            formatted: String::new(),
            is_empty: true,
        };
    }
    let document = DiffDocument::between(old, new, options.clone());
    let formatted = formatter(&document.hunks, &options);
    DiffBundle { hunks: document.hunks, formatted, is_empty: false }
}

/// Formats a structured diff as plain unified text.
///
/// Labels are emitted when both [`DiffOptions::old_label`] and
/// [`DiffOptions::new_label`] are present. Source line terminators are
/// normalized to `\n` in the formatted output while missing-final-newline
/// hints remain controlled by [`DiffOptions::missing_newline_hint`].
#[must_use]
pub fn format_unified_diff(old: &str, new: &str, options: DiffOptions<'_>) -> String {
    let document = DiffDocument::between(old, new, options.clone());
    format_unified_hunks(&document.hunks, &options)
}

/// Formats precomputed hunks as plain unified text.
#[must_use]
pub fn format_unified_hunks(hunks: &[DiffHunk], options: &DiffOptions<'_>) -> String {
    if hunks.is_empty() {
        return String::new();
    }

    let mut output = String::new();
    if let (Some(old_label), Some(new_label)) = (options.old_label, options.new_label) {
        output.push_str("--- ");
        output.push_str(old_label);
        output.push('\n');
        output.push_str("+++ ");
        output.push_str(new_label);
        output.push('\n');
    }

    for hunk in hunks {
        output.push_str("@@ -");
        output.push_str(&format_unified_range(hunk.old_start, hunk.old_lines));
        output.push_str(" +");
        output.push_str(&format_unified_range(hunk.new_start, hunk.new_lines));
        output.push_str(" @@\n");
        for line in &hunk.lines {
            let prefix = match line.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Addition => '+',
                DiffLineKind::Deletion => '-',
            };
            output.push(prefix);
            output.push_str(trim_line_ending(&line.text));
            output.push('\n');

            let has_line_terminator = line.text.ends_with('\n') || line.text.ends_with('\r');
            if options.missing_newline_hint && !has_line_terminator {
                output.push_str(r"\ No newline at end of file");
                output.push('\n');
            }
        }
    }
    output
}

fn format_unified_range(start: usize, count: usize) -> String {
    if count == 0 {
        return format!("{},0", start.saturating_sub(1));
    }
    if count == 1 {
        return start.to_string();
    }
    format!("{start},{count}")
}

/// Formats a git-compatible `@@ -old +new @@` hunk header.
///
/// Range counts are preserved rather than collapsed to start-only form: a
/// pure deletion such as `@@ -65,19 +65,0 @@` must not read as a one-line
/// change. Counts of one are elided (`@@ -1 +1 @@`) to match `git diff`.
#[must_use]
pub fn format_hunk_header(old_start: usize, old_count: usize, new_start: usize, new_count: usize) -> String {
    format!(
        "@@ -{} +{} @@",
        format_unified_range(old_start, old_count),
        format_unified_range(new_start, new_count)
    )
}

/// Computes a character-level diff with adjacent chunks coalesced.
#[must_use]
pub fn compute_diff_chunks<'a>(old: &'a str, new: &'a str) -> Vec<Chunk<'a>> {
    if old == new {
        return (!old.is_empty()).then_some(Chunk::Equal(old)).into_iter().collect();
    }
    let diff = TextDiff::configure()
        .algorithm(similar::Algorithm::Myers)
        .timeout(Duration::from_millis(200))
        .diff_chars(old, new);
    let mut chunks = Vec::new();
    let mut old_offset = 0usize;
    let mut new_offset = 0usize;
    let mut run: Option<(ChangeTag, usize, usize)> = None;

    for change in diff.iter_all_changes() {
        let value = change.value();
        let byte_len = value.len();
        let (start, end) = match change.tag() {
            ChangeTag::Equal | ChangeTag::Delete => (old_offset, old_offset.saturating_add(byte_len)),
            ChangeTag::Insert => (new_offset, new_offset.saturating_add(byte_len)),
        };
        if let Some((tag, run_start, run_end)) = run {
            if tag == change.tag() && run_end == start {
                run = Some((tag, run_start, end));
            } else {
                push_chunk(&mut chunks, tag, run_start, run_end, old, new);
                run = Some((change.tag(), start, end));
            }
        } else {
            run = Some((change.tag(), start, end));
        }
        match change.tag() {
            ChangeTag::Equal => {
                old_offset = old_offset.saturating_add(byte_len);
                new_offset = new_offset.saturating_add(byte_len);
            }
            ChangeTag::Delete => old_offset = old_offset.saturating_add(byte_len),
            ChangeTag::Insert => new_offset = new_offset.saturating_add(byte_len),
        }
    }
    if let Some((tag, start, end)) = run {
        push_chunk(&mut chunks, tag, start, end, old, new);
    }
    chunks
}

fn push_chunk<'a>(chunks: &mut Vec<Chunk<'a>>, tag: ChangeTag, start: usize, end: usize, old: &'a str, new: &'a str) {
    let chunk = match tag {
        ChangeTag::Equal => Chunk::Equal(&old[start..end]),
        ChangeTag::Delete => Chunk::Delete(&old[start..end]),
        ChangeTag::Insert => Chunk::Insert(&new[start..end]),
    };
    chunks.push(chunk);
}

fn split_lines_with_terminator(text: &str) -> Vec<String> {
    split_line_slices(text).into_iter().map(str::to_owned).collect()
}

fn split_line_slices(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'\n' && bytes[index] != b'\r' {
            index += 1;
            continue;
        }
        let end = if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            index + 2
        } else {
            index + 1
        };
        lines.push(&text[start..end]);
        start = end;
        index = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Intraline byte range, always aligned to UTF-8 boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct IntralineRange {
    /// Inclusive byte start.
    pub start: usize,
    /// Exclusive byte end.
    pub end: usize,
}

/// Intra-line highlight ranges retained for compatibility.
pub type WordChangedRanges = Vec<(usize, usize)>;

/// Aggregate addition/deletion counts retained for compatibility.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffChangeCounts {
    /// Added lines.
    pub additions: usize,
    /// Deleted lines.
    pub deletions: usize,
}

impl DiffChangeCounts {
    /// Total changed lines.
    #[must_use]
    pub const fn total(self) -> usize {
        self.additions + self.deletions
    }
}

/// Counts additions and deletions in structured hunks.
#[must_use]
pub fn count_diff_changes(hunks: &[DiffHunk]) -> DiffChangeCounts {
    let mut counts = DiffChangeCounts::default();
    for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
        match line.kind {
            DiffLineKind::Addition => counts.additions += 1,
            DiffLineKind::Deletion => counts.deletions += 1,
            DiffLineKind::Context => {}
        }
    }
    counts
}

/// Semantic role used by legacy preview consumers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DiffDisplayKind {
    /// File or parser metadata.
    Metadata,
    /// Hunk range header.
    HunkHeader,
    /// Unchanged context.
    Context,
    /// Added content.
    Addition,
    /// Deleted content.
    Deletion,
}

impl DiffDisplayKind {
    /// Whether the kind represents source content.
    #[must_use]
    pub const fn is_diff(self) -> bool {
        matches!(self, Self::Context | Self::Addition | Self::Deletion)
    }
}

/// A semantic legacy display line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffDisplayLine {
    /// Semantic role.
    pub kind: DiffDisplayKind,
    /// Old-side number.
    pub old_line: Option<u32>,
    /// New-side number.
    pub new_line: Option<u32>,
    /// Content without the diff marker or line ending.
    pub text: String,
    /// Intraline changed byte ranges.
    pub changed: WordChangedRanges,
}

impl DiffDisplayLine {
    /// Constructs a source body line.
    #[must_use]
    pub fn body(kind: DiffDisplayKind, old_line: Option<u32>, new_line: Option<u32>, text: String) -> Self {
        Self {
            kind,
            old_line,
            new_line,
            text,
            changed: Vec::new(),
        }
    }

    /// Whether this line represents source content.
    #[must_use]
    pub const fn is_diff(&self) -> bool {
        self.kind.is_diff()
    }

    /// Formats a single numbered gutter.
    #[must_use]
    pub fn numbered_text(&self, line_number_width: usize) -> String {
        match self.kind {
            DiffDisplayKind::Metadata | DiffDisplayKind::HunkHeader => self.text.clone(),
            DiffDisplayKind::Deletion => {
                format!("-{:>width$} │ {}", self.old_line.unwrap_or_default(), self.text, width = line_number_width)
            }
            DiffDisplayKind::Addition => {
                format!("+{:>width$} │ {}", self.new_line.unwrap_or_default(), self.text, width = line_number_width)
            }
            DiffDisplayKind::Context => format!(
                " {:>width$} │ {}",
                self.new_line.or(self.old_line).unwrap_or_default(),
                self.text,
                width = line_number_width
            ),
        }
    }
}

/// Converts hunks to semantic display lines and bounded intraline ranges.
#[must_use]
pub fn display_lines_from_hunks(hunks: &[DiffHunk]) -> Vec<DiffDisplayLine> {
    display_lines_from_hunks_with_timeout(hunks, default_inline_timeout())
}

fn display_lines_from_hunks_with_timeout(hunks: &[DiffHunk], inline_timeout: Duration) -> Vec<DiffDisplayLine> {
    let mut output = Vec::new();
    for hunk in hunks {
        output.push(DiffDisplayLine::body(
            DiffDisplayKind::HunkHeader,
            None,
            None,
            format_hunk_header(hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines),
        ));
        output.extend(hunk.lines.iter().map(|line| {
            let kind = match line.kind {
                DiffLineKind::Context => DiffDisplayKind::Context,
                DiffLineKind::Addition => DiffDisplayKind::Addition,
                DiffLineKind::Deletion => DiffDisplayKind::Deletion,
            };
            DiffDisplayLine::body(kind, line.old_line, line.new_line, trim_line_ending(&line.text).to_owned())
        }));
    }
    annotate_word_level_diffs_with_timeout(&mut output, inline_timeout);
    output
}

/// Parses unified text into display lines, retaining metadata lines.
#[must_use]
pub fn display_lines_from_unified_diff(input: &str) -> Vec<DiffDisplayLine> {
    let mut output = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut in_hunk = false;
    let mut remaining_old = 0usize;
    let mut remaining_new = 0usize;
    let mut omission_in_hunk = false;
    let mut hunk_old_start = 0u32;
    let mut hunk_new_start = 0u32;
    let mut hunk_old_count = 0usize;
    let mut hunk_new_count = 0usize;
    let mut omitted_tail = Vec::new();
    for raw in input.lines() {
        if let Some((old_start, new_start)) = parse_hunk_starts(raw) {
            assign_omitted_tail_line_numbers(
                &mut output,
                &omitted_tail,
                hunk_old_start,
                hunk_old_count,
                hunk_new_start,
                hunk_new_count,
            );
            omitted_tail.clear();
            old_line = old_start;
            new_line = new_start;
            if let Some((_, old_count, _, new_count)) = parse_hunk_range(raw) {
                remaining_old = old_count;
                remaining_new = new_count;
                hunk_old_count = old_count;
                hunk_new_count = new_count;
            }
            hunk_old_start = old_start;
            hunk_new_start = new_start;
            in_hunk = true;
            omission_in_hunk = false;
            // Preserve the authored header verbatim so range counts survive.
            // Collapsing to `@@ -65 +65 @@` made a pure deletion read as a
            // one-line change.
            output.push(DiffDisplayLine::body(DiffDisplayKind::HunkHeader, None, None, raw.to_owned()));
        } else if in_hunk && (omission_in_hunk || remaining_new > 0) && raw.starts_with('+') {
            let output_index = output.len();
            output.push(DiffDisplayLine::body(
                DiffDisplayKind::Addition,
                None,
                (!omission_in_hunk).then_some(new_line),
                raw[1..].to_owned(),
            ));
            if omission_in_hunk {
                omitted_tail.push(output_index);
            }
            new_line = new_line.saturating_add(1);
            remaining_new = remaining_new.saturating_sub(1);
        } else if in_hunk && (omission_in_hunk || remaining_old > 0) && raw.starts_with('-') {
            let output_index = output.len();
            output.push(DiffDisplayLine::body(
                DiffDisplayKind::Deletion,
                (!omission_in_hunk).then_some(old_line),
                None,
                raw[1..].to_owned(),
            ));
            if omission_in_hunk {
                omitted_tail.push(output_index);
            }
            old_line = old_line.saturating_add(1);
            remaining_old = remaining_old.saturating_sub(1);
        } else if in_hunk && (omission_in_hunk || (remaining_old > 0 && remaining_new > 0)) && raw.starts_with(' ') {
            let output_index = output.len();
            output.push(DiffDisplayLine::body(
                DiffDisplayKind::Context,
                (!omission_in_hunk).then_some(old_line),
                (!omission_in_hunk).then_some(new_line),
                raw[1..].to_owned(),
            ));
            if omission_in_hunk {
                omitted_tail.push(output_index);
            }
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
            remaining_old = remaining_old.saturating_sub(1);
            remaining_new = remaining_new.saturating_sub(1);
        } else if in_hunk && let Some(omitted) = parse_omitted_line_count(raw) {
            output.push(DiffDisplayLine::body(DiffDisplayKind::Metadata, None, None, raw.to_owned()));
            omission_in_hunk = true;
            let omitted = omitted.min(remaining_old).min(remaining_new);
            old_line = old_line.saturating_add(u32::try_from(omitted).unwrap_or(u32::MAX));
            new_line = new_line.saturating_add(u32::try_from(omitted).unwrap_or(u32::MAX));
            remaining_old = remaining_old.saturating_sub(omitted);
            remaining_new = remaining_new.saturating_sub(omitted);
        } else {
            output.push(DiffDisplayLine::body(DiffDisplayKind::Metadata, None, None, raw.to_owned()));
        }
    }
    assign_omitted_tail_line_numbers(
        &mut output,
        &omitted_tail,
        hunk_old_start,
        hunk_old_count,
        hunk_new_start,
        hunk_new_count,
    );
    annotate_word_level_diffs(&mut output);
    output
}

fn assign_omitted_tail_line_numbers(
    lines: &mut [DiffDisplayLine],
    tail: &[usize],
    old_start: u32,
    old_count: usize,
    new_start: u32,
    new_count: usize,
) {
    if tail.is_empty() {
        return;
    }
    let old_tail_count = tail
        .iter()
        .filter(|&&index| lines[index].kind != DiffDisplayKind::Addition)
        .count();
    let new_tail_count = tail
        .iter()
        .filter(|&&index| lines[index].kind != DiffDisplayKind::Deletion)
        .count();
    let mut old_line = old_start
        .saturating_add(u32::try_from(old_count).unwrap_or(u32::MAX))
        .saturating_sub(u32::try_from(old_tail_count).unwrap_or(u32::MAX));
    let mut new_line = new_start
        .saturating_add(u32::try_from(new_count).unwrap_or(u32::MAX))
        .saturating_sub(u32::try_from(new_tail_count).unwrap_or(u32::MAX));
    for &index in tail {
        match lines[index].kind {
            DiffDisplayKind::Addition => {
                lines[index].new_line = Some(new_line);
                new_line = new_line.saturating_add(1);
            }
            DiffDisplayKind::Deletion => {
                lines[index].old_line = Some(old_line);
                old_line = old_line.saturating_add(1);
            }
            DiffDisplayKind::Context => {
                lines[index].old_line = Some(old_line);
                lines[index].new_line = Some(new_line);
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            DiffDisplayKind::Metadata | DiffDisplayKind::HunkHeader => {}
        }
    }
}

/// Formats a numbered unified diff without ANSI color.
#[must_use]
pub fn format_numbered_unified_diff(input: &str) -> Vec<String> {
    let lines = display_lines_from_unified_diff(input);
    let width = diff_display_line_number_width(&lines);
    lines.iter().map(|line| line.numbered_text(width)).collect()
}

/// Computes the clamped line-number gutter width.
#[must_use]
pub fn diff_display_line_number_width(lines: &[DiffDisplayLine]) -> usize {
    let maximum = lines
        .iter()
        .flat_map(|line| [line.old_line, line.new_line])
        .flatten()
        .max()
        .unwrap_or_default();
    decimal_digits(maximum).clamp(5, 6)
}

/// Minimum content width for the paired old/new preview.
///
/// Below this width, two independently numbered panes leave too little room
/// for source text. Renderers should fall back to the unified presentation.
pub const DIFF_MIN_SIDE_BY_SIDE_WIDTH: usize = 60;

/// Whether a measured width can support the paired old/new preview.
///
/// An unknown width preserves the existing caller behavior; redirected
/// output and test sinks may not expose terminal sizing at all.
#[must_use]
pub const fn diff_side_by_side_fits(available_width: Option<usize>) -> bool {
    match available_width {
        Some(width) => width >= DIFF_MIN_SIDE_BY_SIDE_WIDTH,
        None => true,
    }
}

/// Minimum source width retained when the unified line gutter is visible.
///
/// This keeps the marker, line number, separator, and a useful amount of
/// source text together. Narrower layouts should hide the gutter and give its
/// columns back to the source body.
pub const DIFF_MIN_BODY_WIDTH_WITH_GUTTER: usize = 20;

/// Width consumed by a unified diff gutter after the line-number field.
///
/// The rendered shape is `+123 │ `: one marker plus the three-cell separator
/// around `│`. The line-number field is supplied by the caller because it is
/// derived from the visible diff excerpt.
#[must_use]
pub const fn diff_gutter_width(line_number_width: usize) -> usize {
    line_number_width.saturating_add(4)
}

/// Whether a unified diff can keep its marker, line number, and separator
/// without starving the source body.
#[must_use]
pub const fn diff_gutter_fits(available_width: usize, line_number_width: usize) -> bool {
    available_width >= diff_gutter_width(line_number_width).saturating_add(DIFF_MIN_BODY_WIDTH_WITH_GUTTER)
}

/// Width to pass to semantic unified layout when the renderer hides its
/// visible gutter.
///
/// `layout_display_lines` subtracts the normal gutter before wrapping source
/// text. Giving that width back keeps wrapping aligned with compact rendering
/// without adding another public layout option.
#[must_use]
pub const fn diff_layout_width(available_width: usize, line_number_width: usize, show_gutter: bool) -> usize {
    if show_gutter {
        available_width
    } else {
        available_width.saturating_add(diff_gutter_width(line_number_width))
    }
}

fn decimal_digits(mut number: u32) -> usize {
    let mut digits = 1usize;
    while number >= 10 {
        number /= 10;
        digits += 1;
    }
    digits
}

/// One paired row in the compatibility side-by-side model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SideBySideRow {
    /// Old-side cell.
    pub left: Option<DiffDisplayLine>,
    /// New-side cell.
    pub right: Option<DiffDisplayLine>,
}

impl SideBySideRow {
    /// Whether the left cell is a header spanning both panes.
    #[must_use]
    pub fn is_full_width(&self) -> bool {
        self.right.is_none()
            && self
                .left
                .as_ref()
                .is_some_and(|line| matches!(line.kind, DiffDisplayKind::HunkHeader | DiffDisplayKind::Metadata))
    }
}

/// Pairs deletions and additions for side-by-side presentation.
#[must_use]
pub fn side_by_side_rows(lines: &[DiffDisplayLine]) -> Vec<SideBySideRow> {
    let mut rows = Vec::new();
    for_each_side_by_side_pair(lines, |left, right| {
        rows.push(SideBySideRow { left: left.cloned(), right: right.cloned() });
    });
    rows
}

fn for_each_side_by_side_pair<'a, F>(lines: &'a [DiffDisplayLine], mut visit: F)
where
    F: FnMut(Option<&'a DiffDisplayLine>, Option<&'a DiffDisplayLine>),
{
    let mut index = 0usize;
    while index < lines.len() {
        match lines[index].kind {
            DiffDisplayKind::HunkHeader | DiffDisplayKind::Metadata => {
                visit(Some(&lines[index]), None);
                index += 1;
            }
            DiffDisplayKind::Context => {
                let line = &lines[index];
                visit(Some(line), Some(line));
                index += 1;
            }
            DiffDisplayKind::Deletion => {
                let delete_start = index;
                while index < lines.len() && lines[index].kind == DiffDisplayKind::Deletion {
                    index += 1;
                }
                let insert_start = index;
                while index < lines.len() && lines[index].kind == DiffDisplayKind::Addition {
                    index += 1;
                }
                let delete_count = insert_start - delete_start;
                let insert_count = index - insert_start;
                for offset in 0..delete_count.max(insert_count) {
                    visit(
                        (offset < delete_count).then(|| &lines[delete_start + offset]),
                        (offset < insert_count).then(|| &lines[insert_start + offset]),
                    );
                }
            }
            DiffDisplayKind::Addition => {
                visit(None, Some(&lines[index]));
                index += 1;
            }
        }
    }
}

/// Adds byte-safe intraline ranges to consecutive deletion/addition groups.
pub fn annotate_word_level_diffs(lines: &mut [DiffDisplayLine]) {
    annotate_word_level_diffs_with_timeout(lines, default_inline_timeout());
}

fn annotate_word_level_diffs_with_timeout(lines: &mut [DiffDisplayLine], timeout: Duration) {
    // Binary content (NUL bytes) makes word-level refinement meaningless and
    // expensive; skip it entirely so intraline work stays within budget.
    if lines.iter().any(|line| line.text.as_bytes().contains(&0)) {
        return;
    }
    let deadline = Instant::now().checked_add(timeout);
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].kind != DiffDisplayKind::Deletion {
            index += 1;
            continue;
        }
        let delete_start = index;
        while index < lines.len() && lines[index].kind == DiffDisplayKind::Deletion {
            index += 1;
        }
        let insert_start = index;
        while index < lines.len() && lines[index].kind == DiffDisplayKind::Addition {
            index += 1;
        }
        let pair_count = (insert_start - delete_start).min(index - insert_start);
        for offset in 0..pair_count {
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return;
            }
            let (old_ranges, new_ranges) =
                word_level_changed_ranges(&lines[delete_start + offset].text, &lines[insert_start + offset].text);
            lines[delete_start + offset].changed = old_ranges;
            lines[insert_start + offset].changed = new_ranges;
        }
    }
}

/// Computes byte-safe word-level changed ranges for a line pair.
#[must_use]
pub fn word_level_changed_ranges(old: &str, new: &str) -> (WordChangedRanges, WordChangedRanges) {
    if old.is_empty() || new.is_empty() || old.len().saturating_add(new.len()) > 16_384 {
        return (Vec::new(), Vec::new());
    }
    let diff = TextDiff::configure()
        .algorithm(similar::Algorithm::Myers)
        .timeout(Duration::from_millis(10))
        .diff_unicode_words(old, new);
    if diff.ratio() < 0.35 {
        return (Vec::new(), Vec::new());
    }
    let mut old_offset = 0usize;
    let mut new_offset = 0usize;
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    for change in diff.iter_all_changes() {
        let length = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_offset += length;
                new_offset += length;
            }
            ChangeTag::Delete => {
                push_range(&mut old_ranges, old_offset, old_offset + length);
                old_offset += length;
            }
            ChangeTag::Insert => {
                push_range(&mut new_ranges, new_offset, new_offset + length);
                new_offset += length;
            }
        }
    }
    (old_ranges, new_ranges)
}

fn push_range(ranges: &mut WordChangedRanges, start: usize, end: usize) {
    match ranges.last_mut() {
        Some((_, prior_end)) if *prior_end == start => *prior_end = end,
        _ => ranges.push((start, end)),
    }
}

/// Requested preview layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum DiffLayout {
    /// One stacked old/new column.
    #[default]
    Unified,
    /// Paired old/new panes.
    SideBySide,
}

/// Width and bounded-excerpt options for semantic layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOptions {
    /// Requested layout.
    pub layout: DiffLayout,
    /// Available terminal columns.
    pub width: usize,
    /// Maximum rows, including an omission marker.
    pub max_rows: usize,
    /// Whether source bodies hard-wrap at display width.
    pub wrap: bool,
    /// Side-by-side fallback threshold.
    pub min_side_by_side_width: usize,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            layout: DiffLayout::Unified,
            width: 80,
            max_rows: 2_000,
            wrap: true,
            min_side_by_side_width: DIFF_MIN_SIDE_BY_SIDE_WIDTH,
        }
    }
}

/// Semantic role for a renderer-neutral row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowKind {
    /// File metadata outside a hunk.
    Metadata,
    /// Hunk header.
    HunkHeader,
    /// Unchanged content.
    Context,
    /// Added content.
    Addition,
    /// Deleted content.
    Deletion,
    /// A bounded middle omission.
    Omission,
}

/// One independently styled content segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSegment {
    /// Segment text.
    pub text: String,
    /// Whether the segment receives intraline emphasis.
    pub emphasized: bool,
}

/// One side of a semantic preview row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCell {
    /// Old-side line number.
    pub old_line: Option<u32>,
    /// New-side line number.
    pub new_line: Option<u32>,
    /// Diff marker.
    pub marker: char,
    /// Styled content segments.
    pub segments: Vec<DiffSegment>,
}

/// A renderer-neutral row, optionally containing paired side-by-side cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// Semantic role.
    pub kind: DiffRowKind,
    /// Marker for unified renderers and simple inspection.
    pub marker: char,
    /// Stable zero-based hunk identity.
    pub hunk_index: Option<usize>,
    /// Whether this row continues a hard-wrapped source line.
    pub continuation: bool,
    /// Unified or old-side cell.
    pub left: Option<DiffCell>,
    /// New-side cell in side-by-side mode.
    pub right: Option<DiffCell>,
}

fn layout_document(document: &DiffDocument, options: LayoutOptions) -> Vec<DiffRow> {
    let display = display_lines_from_hunks_with_timeout(&document.hunks, document.inline_timeout);
    layout_display_lines(&display, options)
}

/// Lays out precomputed display lines without repeating intraline analysis.
///
/// Interactive applications can cache semantic lines when an overlay opens
/// and call this inexpensive step again after a resize.
#[must_use]
pub fn layout_display_lines(display: &[DiffDisplayLine], options: LayoutOptions) -> Vec<DiffRow> {
    let use_side_by_side = options.layout == DiffLayout::SideBySide && options.width >= options.min_side_by_side_width;
    let mut rows = RowCollector::new(options.max_rows);
    if use_side_by_side {
        layout_side_by_side(display, options, &mut rows);
    } else {
        layout_unified(display, options, &mut rows);
    }
    rows.finish()
}

/// Returns a bounded head/tail excerpt of semantic display lines.
///
/// The returned vector contains at most `max_rows` entries. When rows are
/// omitted, one metadata entry (`... N lines omitted ...`) is inserted between
/// the retained head and tail. Source line numbers and intraline ranges on
/// retained entries are preserved, so callers can render the excerpt without
/// reparsing or inventing positions.
#[must_use]
pub fn bounded_display_lines(lines: &[DiffDisplayLine], max_rows: usize) -> Vec<DiffDisplayLine> {
    if max_rows == 0 {
        return Vec::new();
    }
    if lines.len() <= max_rows {
        return lines.to_vec();
    }

    let retained = max_rows.saturating_sub(1);
    let head_count = retained.saturating_add(1) / 2;
    let tail_count = retained / 2;
    let omitted = lines.len().saturating_sub(head_count + tail_count);

    let mut bounded = Vec::with_capacity(max_rows);
    bounded.extend_from_slice(&lines[..head_count]);
    bounded.push(DiffDisplayLine::body(
        DiffDisplayKind::Metadata,
        None,
        None,
        format!("... {omitted} lines omitted ..."),
    ));
    if tail_count > 0 {
        bounded.extend_from_slice(&lines[lines.len() - tail_count..]);
    }
    bounded
}

fn layout_unified(lines: &[DiffDisplayLine], options: LayoutOptions, rows: &mut RowCollector) {
    let gutter_width = diff_display_line_number_width(lines).saturating_add(4);
    let content_width = options.width.saturating_sub(gutter_width).max(1);
    let mut hunk_index = None;
    for line in lines {
        if line.kind == DiffDisplayKind::Metadata {
            rows.push(metadata_row(&line.text, hunk_index));
            continue;
        }
        if line.kind == DiffDisplayKind::HunkHeader {
            hunk_index = Some(hunk_index.map_or(0, |index| index + 1));
            rows.push(header_row(&line.text, hunk_index));
            continue;
        }
        let marker = marker_for_kind(line.kind);
        for (continuation, segments) in wrap_segments(&line.text, &line.changed, content_width, options.wrap)
            .into_iter()
            .enumerate()
        {
            rows.push(DiffRow {
                kind: row_kind(line.kind),
                marker,
                hunk_index,
                continuation: continuation > 0,
                left: Some(DiffCell {
                    old_line: (continuation == 0).then_some(line.old_line).flatten(),
                    new_line: (continuation == 0).then_some(line.new_line).flatten(),
                    marker,
                    segments,
                }),
                right: None,
            });
        }
    }
}

fn layout_side_by_side(lines: &[DiffDisplayLine], options: LayoutOptions, rows: &mut RowCollector) {
    let pane_width = options.width.saturating_sub(1) / 2;
    let content_width = pane_width.saturating_sub(6).max(1);
    let mut hunk_index = None;
    for_each_side_by_side_pair(lines, |left, right| {
        let is_full_width = right.is_none()
            && left.is_some_and(|line| matches!(line.kind, DiffDisplayKind::HunkHeader | DiffDisplayKind::Metadata));
        if is_full_width {
            let text = left.map_or("", |line| line.text.as_str());
            if left.is_some_and(|line| line.kind == DiffDisplayKind::HunkHeader) {
                hunk_index = Some(hunk_index.map_or(0, |index| index + 1));
                rows.push(header_row(text, hunk_index));
            } else {
                rows.push(metadata_row(text, hunk_index));
            }
            return;
        }
        let left_parts = left.map_or_else(
            || vec![Vec::new()],
            |line| wrap_segments(&line.text, &line.changed, content_width, options.wrap),
        );
        let right_parts = right.map_or_else(
            || vec![Vec::new()],
            |line| wrap_segments(&line.text, &line.changed, content_width, options.wrap),
        );
        let count = left_parts.len().max(right_parts.len());
        for offset in 0..count {
            let left_cell = left.and_then(|line| {
                left_parts.get(offset).map(|segments| DiffCell {
                    old_line: (offset == 0).then_some(line.old_line).flatten(),
                    new_line: (offset == 0).then_some(line.new_line).flatten(),
                    marker: marker_for_kind(line.kind),
                    segments: segments.clone(),
                })
            });
            let right_cell = right.and_then(|line| {
                right_parts.get(offset).map(|segments| DiffCell {
                    old_line: (offset == 0).then_some(line.old_line).flatten(),
                    new_line: (offset == 0).then_some(line.new_line).flatten(),
                    marker: marker_for_kind(line.kind),
                    segments: segments.clone(),
                })
            });
            let kind = right.or(left).map_or(DiffRowKind::Context, |line| row_kind(line.kind));
            rows.push(DiffRow {
                kind,
                marker: right_cell.as_ref().or(left_cell.as_ref()).map_or(' ', |cell| cell.marker),
                hunk_index,
                continuation: offset > 0,
                left: left_cell,
                right: right_cell,
            });
        }
    });
}

fn header_row(text: &str, hunk_index: Option<usize>) -> DiffRow {
    DiffRow {
        kind: DiffRowKind::HunkHeader,
        marker: '@',
        hunk_index,
        continuation: false,
        left: Some(DiffCell {
            old_line: None,
            new_line: None,
            marker: '@',
            segments: vec![DiffSegment { text: text.to_owned(), emphasized: false }],
        }),
        right: None,
    }
}

fn metadata_row(text: &str, hunk_index: Option<usize>) -> DiffRow {
    DiffRow {
        kind: DiffRowKind::Metadata,
        marker: ' ',
        hunk_index,
        continuation: false,
        left: Some(DiffCell {
            old_line: None,
            new_line: None,
            marker: ' ',
            segments: vec![DiffSegment { text: text.to_owned(), emphasized: false }],
        }),
        right: None,
    }
}

struct RowCollector {
    head: Vec<DiffRow>,
    tail: VecDeque<DiffRow>,
    max_rows: usize,
    total_rows: usize,
    overflowed: bool,
}

impl RowCollector {
    fn new(max_rows: usize) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            max_rows,
            total_rows: 0,
            overflowed: false,
        }
    }

    fn push(&mut self, row: DiffRow) {
        self.total_rows = self.total_rows.saturating_add(1);
        if self.max_rows == 0 {
            return;
        }
        if !self.overflowed {
            self.head.push(row);
            if self.head.len() <= self.max_rows {
                return;
            }

            let retained = self.max_rows.saturating_sub(1);
            let head_count = retained.saturating_add(1) / 2;
            let tail_count = retained / 2;
            if tail_count > 0 {
                let tail_start = self.head.len() - tail_count;
                self.tail = self.head.split_off(tail_start).into();
            }
            self.head.truncate(head_count);
            self.overflowed = true;
            return;
        }

        let tail_count = self.max_rows.saturating_sub(1) / 2;
        if tail_count > 0 {
            if self.tail.len() == tail_count {
                let _ = self.tail.pop_front();
            }
            self.tail.push_back(row);
        }
    }

    fn finish(mut self) -> Vec<DiffRow> {
        if !self.overflowed {
            return self.head;
        }
        let omitted = self.total_rows.saturating_sub(self.head.len()).saturating_sub(self.tail.len());
        self.head.push(omission_row(omitted));
        self.head.extend(self.tail);
        self.head
    }
}

fn omission_row(omitted: usize) -> DiffRow {
    DiffRow {
        kind: DiffRowKind::Omission,
        marker: '…',
        hunk_index: None,
        continuation: false,
        left: Some(DiffCell {
            old_line: None,
            new_line: None,
            marker: '…',
            segments: vec![DiffSegment {
                text: format!("{omitted} rows omitted"),
                emphasized: false,
            }],
        }),
        right: None,
    }
}

fn wrap_segments(text: &str, changed: &[(usize, usize)], width: usize, wrap: bool) -> Vec<Vec<DiffSegment>> {
    if !wrap || UnicodeWidthStr::width(text) <= width {
        return vec![segment_slice(text, changed, 0, text.len())];
    }
    let mut rows = Vec::new();
    let mut byte_start = 0usize;
    let mut display_width = 0usize;
    for (byte, character) in text.char_indices() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or_default();
        if display_width > 0 && display_width.saturating_add(char_width) > width {
            rows.push(segment_slice(text, changed, byte_start, byte));
            byte_start = byte;
            display_width = 0;
        }
        display_width = display_width.saturating_add(char_width);
    }
    rows.push(segment_slice(text, changed, byte_start, text.len()));
    rows
}

fn segment_slice(text: &str, changed: &[(usize, usize)], start: usize, end: usize) -> Vec<DiffSegment> {
    if start == end {
        return vec![DiffSegment { text: String::new(), emphasized: false }];
    }
    let mut boundaries = vec![start, end];
    for &(range_start, range_end) in changed {
        if range_start < end && range_end > start {
            let bounded_start = range_start.max(start).min(end);
            let bounded_end = range_end.max(start).min(end);
            if text.is_char_boundary(bounded_start) && text.is_char_boundary(bounded_end) {
                boundaries.push(bounded_start);
                boundaries.push(bounded_end);
            }
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .filter_map(|pair| {
            let segment_start = pair[0];
            let segment_end = pair[1];
            (segment_start < segment_end).then(|| DiffSegment {
                text: text[segment_start..segment_end].to_owned(),
                emphasized: changed
                    .iter()
                    .any(|&(range_start, range_end)| segment_start >= range_start && segment_end <= range_end),
            })
        })
        .collect()
}

fn marker_for_kind(kind: DiffDisplayKind) -> char {
    match kind {
        DiffDisplayKind::Addition => '+',
        DiffDisplayKind::Deletion => '-',
        DiffDisplayKind::HunkHeader => '@',
        DiffDisplayKind::Metadata | DiffDisplayKind::Context => ' ',
    }
}

fn row_kind(kind: DiffDisplayKind) -> DiffRowKind {
    match kind {
        DiffDisplayKind::Addition => DiffRowKind::Addition,
        DiffDisplayKind::Deletion => DiffRowKind::Deletion,
        DiffDisplayKind::HunkHeader => DiffRowKind::HunkHeader,
        DiffDisplayKind::Metadata => DiffRowKind::Metadata,
        DiffDisplayKind::Context => DiffRowKind::Context,
    }
}

fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let (old, _, new, _) = parse_hunk_range(line)?;
    Some((old, new))
}

fn parse_hunk_range(line: &str) -> Option<(u32, usize, u32, usize)> {
    let body = line.strip_prefix("@@ ")?.split(" @@").next()?;
    let mut parts = body.split_whitespace();
    let (old_start, old_count) = parse_range(parts.next()?, '-')?;
    let (new_start, new_count) = parse_range(parts.next()?, '+')?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_range(value: &str, marker: char) -> Option<(u32, usize)> {
    let range = value.strip_prefix(marker)?;
    let mut parts = range.splitn(2, ',');
    let start = parts.next()?.parse().ok()?;
    let count = parts.next().map_or(Some(1), |count| count.parse().ok())?;
    Some((start, count))
}

fn parse_omitted_line_count(line: &str) -> Option<usize> {
    let line = line.trim();
    line.strip_prefix("... ")?.strip_suffix(" lines omitted ...")?.parse().ok()
}

fn is_unified_metadata_line(line: &str) -> bool {
    line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("copy from ")
        || line.starts_with("copy to ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("Binary files ")
        || line == "GIT binary patch"
        || line.starts_with("literal ")
        || line.starts_with("delta ")
}

fn trim_line_ending(text: &str) -> &str {
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .or_else(|| text.strip_suffix('\r'))
        .unwrap_or(text)
}

#[cfg(feature = "ansi")]
mod ansi_adapter {
    use super::DiffRow;
    use anstyle::{Reset, Style};

    /// Caller-supplied foreground styles for ANSI output.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct AnsiDiffPalette {
        /// Hunk and omission style.
        pub header: Style,
        /// Context style.
        pub context: Style,
        /// Addition style.
        pub addition: Style,
        /// Deletion style.
        pub deletion: Style,
        /// Extra intraline emphasis.
        pub emphasis: Style,
    }

    /// Renders semantic rows as ANSI lines.
    #[must_use]
    pub fn render_ansi_rows(rows: &[DiffRow], palette: AnsiDiffPalette, color: bool) -> Vec<String> {
        rows.iter()
            .map(|row| {
                let mut output = String::new();
                output.push(row.marker);
                output.push(' ');
                if let Some(cell) = &row.left {
                    render_cell(&mut output, cell, palette, color);
                }
                if let Some(cell) = &row.right {
                    output.push_str(" │ ");
                    render_cell(&mut output, cell, palette, color);
                }
                output
            })
            .collect()
    }

    fn render_cell(output: &mut String, cell: &super::DiffCell, palette: AnsiDiffPalette, color: bool) {
        let style = match cell.marker {
            '+' => palette.addition,
            '-' => palette.deletion,
            '@' | '…' => palette.header,
            _ => palette.context,
        };
        for segment in &cell.segments {
            if color {
                let selected = if segment.emphasized { palette.emphasis } else { style };
                output.push_str(&selected.render().to_string());
                output.push_str(&segment.text);
                output.push_str(&Reset.render().to_string());
            } else {
                output.push_str(&segment.text);
            }
        }
    }
}

#[cfg(feature = "ansi")]
pub use ansi_adapter::{AnsiDiffPalette, render_ansi_rows};

#[cfg(feature = "ratatui")]
mod ratatui_adapter {
    use super::DiffRow;
    use ratatui::text::{Line, Span};

    /// Converts semantic rows to unstyled Ratatui lines for caller styling.
    #[must_use]
    pub fn to_ratatui_lines(rows: &[DiffRow]) -> Vec<Line<'static>> {
        rows.iter()
            .map(|row| {
                let mut spans = vec![Span::raw(format!("{} ", row.marker))];
                if let Some(cell) = &row.left {
                    spans.extend(cell.segments.iter().map(|segment| Span::raw(segment.text.clone())));
                }
                if let Some(cell) = &row.right {
                    spans.push(Span::raw(" │ "));
                    spans.extend(cell.segments.iter().map(|segment| Span::raw(segment.text.clone())));
                }
                Line::from(spans)
            })
            .collect()
    }
}

#[cfg(feature = "ratatui")]
pub use ratatui_adapter::to_ratatui_lines;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asymmetric_replacement_has_correct_line_numbers() {
        let document = DiffDocument::between("a\nb\nc\n", "a\nx\ny\nc\n", DiffOptions::default());
        let lines: Vec<_> = document.hunks.iter().flat_map(|hunk| &hunk.lines).collect();
        assert!(lines.iter().any(|line| {
            line.kind == DiffLineKind::Deletion && line.old_line == Some(2) && line.new_line.is_none()
        }));
        assert!(lines.iter().any(|line| {
            line.kind == DiffLineKind::Addition && line.old_line.is_none() && line.new_line == Some(3)
        }));
        assert_eq!(document.stats.additions, 2);
        assert_eq!(document.stats.deletions, 1);
    }

    #[test]
    fn repeated_lines_keep_the_changed_anchor() {
        let document = DiffDocument::between("same\nold\nsame\n", "same\nnew\nsame\n", DiffOptions::default());
        assert_eq!(document.stats.additions, 1);
        assert_eq!(document.stats.deletions, 1);
        assert_eq!(document.hunks[0].old_start, 1);
    }

    #[test]
    fn zero_context_hunks_preserve_empty_side_anchors() {
        let options = DiffOptions { context_lines: 0, ..DiffOptions::default() };
        let insertion = DiffDocument::between("a\nc\n", "a\nb\nc\n", options.clone());
        assert_eq!(insertion.hunks[0].old_start, 2);
        assert_eq!(insertion.hunks[0].new_start, 2);

        let deletion = DiffDocument::between("a\nb\nc\n", "a\nc\n", options);
        assert_eq!(deletion.hunks[0].old_start, 2);
        assert_eq!(deletion.hunks[0].new_start, 2);
    }

    #[test]
    fn preserves_crlf_cr_and_missing_final_newline() {
        let crlf = DiffDocument::between("a\r\nb\r\n", "a\r\nx\r\n", DiffOptions::default());
        assert!(crlf.hunks[0].lines.iter().any(|line| line.text == "a\r\n"));
        let cr = DiffDocument::between("a\rb\r", "a\rx\r", DiffOptions::default());
        assert!(cr.hunks[0].lines.iter().any(|line| line.text == "a\r"));
        let eof = DiffDocument::between("a\n", "a", DiffOptions::default());
        assert_eq!(eof.stats.additions, 1);
        assert_eq!(eof.stats.deletions, 1);
    }

    #[test]
    fn parser_rejects_body_before_hunk() {
        let error = DiffDocument::from_unified("-old\n+new\n").expect_err("body must need a hunk");
        assert_eq!(error.to_string(), "diff body appears before a hunk header");
    }

    #[test]
    fn parser_accepts_standard_git_metadata_before_hunk() {
        let document = DiffDocument::from_unified(
            "diff --git a/file.txt b/file.txt\nindex 1111111..2222222 100644\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
        )
        .expect("standard git metadata is not a body");
        assert_eq!(document.stats.deletions, 1);
        assert_eq!(document.stats.additions, 1);
    }

    #[test]
    fn parser_accepts_metadata_between_multiple_file_hunks() {
        let document = DiffDocument::from_unified(
            "diff --git a/one.txt b/one.txt\nnew file mode 100644\n--- /dev/null\n+++ b/one.txt\n@@ -0,0 +1 @@\n+one\ndiff --git a/two.txt b/two.txt\nnew file mode 100644\n--- /dev/null\n+++ b/two.txt\n@@ -0,0 +1 @@\n+two\n",
        )
        .expect("metadata between files is not a hunk body");
        assert_eq!(document.hunks.len(), 2);
        assert_eq!(document.stats.additions, 2);
    }

    #[test]
    fn parsed_body_lines_preserve_original_terminators() {
        let document = DiffDocument::from_unified("@@ -1 +1 @@\r\n-old\r\n+new\r\n").expect("valid CRLF diff");
        assert_eq!(document.hunks[0].lines[0].text, "old\r\n");
        assert_eq!(document.hunks[0].lines[1].text, "new\r\n");

        let document = DiffDocument::from_unified("@@ -1 +1 @@\r-old\r+new\r").expect("valid CR diff");
        assert_eq!(document.hunks[0].lines[0].text, "old\r");
        assert_eq!(document.hunks[0].lines[1].text, "new\r");
    }

    #[test]
    fn parser_rejects_incomplete_hunk_body() {
        let error = DiffDocument::from_unified("@@ -1,2 +1,2 @@\n-old\n+new\n")
            .expect_err("hunk body must satisfy the declared ranges");
        assert_eq!(error.to_string(), "hunk line counts do not match header");
    }

    #[test]
    fn parser_rejects_unknown_backslash_metadata_inside_hunk() {
        let error = DiffDocument::from_unified("@@ -1 +1 @@\n-old\n\\ unexpected marker\n+new\n")
            .expect_err("only Git's no-newline marker is valid inside a hunk");
        assert_eq!(error.to_string(), "invalid unified diff body line");
    }

    #[test]
    fn parser_tracks_asymmetric_hunk_numbers() {
        let document = DiffDocument::from_unified("@@ -4,1 +8,2 @@\n-old\n+new\n+extra\n").expect("valid diff");
        assert_eq!(document.hunks[0].old_start, 4);
        assert_eq!(document.hunks[0].new_start, 8);
        assert_eq!(document.hunks[0].lines[2].new_line, Some(9));
    }

    #[test]
    fn parser_keeps_context_lines_that_look_like_omission_markers() {
        let document =
            DiffDocument::from_unified("@@ -1 +1 @@\n ... 4 lines omitted ...\n").expect("valid context line");
        assert_eq!(document.hunks[0].lines[0].kind, DiffLineKind::Context);
        assert_eq!(document.stats.omitted_rows, 0);
    }

    #[test]
    fn parsed_omission_advances_both_line_counters() {
        let lines = display_lines_from_unified_diff("@@ -10,6 +20,6 @@\n same\n... 4 lines omitted ...\n-old\n+new\n");
        let deletion = lines
            .iter()
            .find(|line| line.kind == DiffDisplayKind::Deletion)
            .expect("deletion after omission");
        let addition = lines
            .iter()
            .find(|line| line.kind == DiffDisplayKind::Addition)
            .expect("addition after omission");
        assert_eq!(deletion.old_line, Some(15));
        assert_eq!(addition.new_line, Some(25));
    }

    #[test]
    fn truncated_hunk_keeps_tail_additions_as_diff_lines() {
        let mut input = String::from("@@ -1,201 +1,201 @@\n");
        for index in 0..95 {
            input.push_str(&format!("-old-{index}\n"));
        }
        input.push_str("... 243 lines omitted ...\n");
        for index in 137..201 {
            input.push_str(&format!("+new-{index}\n"));
        }

        let lines = display_lines_from_unified_diff(&input);
        let additions: Vec<_> = lines.iter().filter(|line| line.kind == DiffDisplayKind::Addition).collect();
        assert_eq!(additions.len(), 64);
        assert_eq!(additions.last().map(|line| line.text.as_str()), Some("new-200"));
    }

    #[test]
    fn truncated_hunk_numbers_tail_from_declared_end() {
        let mut input = String::from("@@ -1,201 +1,201 @@\n");
        for index in 0..93 {
            input.push_str(&format!("-old-{index}\n"));
        }
        input.push_str("... 277 lines omitted ...\n");
        for index in 169..201 {
            input.push_str(&format!("+new-{index}\n"));
        }

        let lines = display_lines_from_unified_diff(&input);
        let additions: Vec<_> = lines.iter().filter(|line| line.kind == DiffDisplayKind::Addition).collect();
        assert_eq!(additions.first().and_then(|line| line.new_line), Some(170));
        assert_eq!(additions.last().and_then(|line| line.new_line), Some(201));
    }

    #[test]
    fn parsed_metadata_stays_outside_hunk_semantics() {
        let display = display_lines_from_unified_diff("--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n");
        let rows = layout_display_lines(&display, LayoutOptions::default());
        assert_eq!(rows[0].kind, DiffRowKind::Metadata);
        assert_eq!(rows[1].kind, DiffRowKind::Metadata);
        assert_eq!(rows[2].kind, DiffRowKind::HunkHeader);
        assert_eq!(rows[2].hunk_index, Some(0));
    }

    #[test]
    fn unified_parser_accepts_omission_and_advances_numbers() {
        let document = DiffDocument::from_unified("@@ -10,6 +20,6 @@\n same\n... 4 lines omitted ...\n-old\n+new\n")
            .expect("omission marker is valid bounded preview metadata");
        assert_eq!(document.hunks[0].lines[1].old_line, Some(15));
        assert_eq!(document.hunks[0].lines[2].new_line, Some(25));
        assert_eq!(document.hunks[0].old_lines, 6);
        assert_eq!(document.hunks[0].new_lines, 6);
        assert_eq!(document.stats.omitted_rows, 4);
    }

    #[test]
    fn unified_parser_accepts_asymmetric_bounded_omission() {
        let document =
            DiffDocument::from_unified("@@ -1,4 +1,8 @@\n-old-1\n... 4 lines omitted ...\n+new-6\n+new-7\n+new-8\n")
                .expect("bounded omission may hide different old/new line counts");
        assert_eq!(document.stats.omitted_rows, 4);
        assert_eq!(document.hunks[0].old_lines, 4);
        assert_eq!(document.hunks[0].new_lines, 6);
    }

    #[test]
    fn unified_parser_numbers_one_sided_omitted_tails_from_hunk_end() {
        let added = DiffDocument::from_unified("@@ -0,0 +1,5 @@\n+one\n... 3 lines omitted ...\n+five\n")
            .expect("bounded addition is valid");
        assert_eq!(added.hunks[0].lines[1].new_line, Some(5));

        let deleted = DiffDocument::from_unified("@@ -1,5 +0,0 @@\n-one\n... 3 lines omitted ...\n-five\n")
            .expect("bounded deletion is valid");
        assert_eq!(deleted.hunks[0].lines[1].old_line, Some(5));
    }

    #[test]
    fn intraline_ranges_are_utf8_boundaries() {
        let (old, new) = word_level_changed_ranges("café rouge", "café bleu");
        for (start, end) in old {
            assert!("café rouge".is_char_boundary(start));
            assert!("café rouge".is_char_boundary(end));
        }
        for (start, end) in new {
            assert!("café bleu".is_char_boundary(start));
            assert!("café bleu".is_char_boundary(end));
        }
    }

    #[test]
    fn unicode_width_wrap_keeps_numbers_only_on_first_row() {
        let document = DiffDocument::between("", "界界界a\n", DiffOptions::default());
        let rows = document.layout(LayoutOptions { width: 12, ..LayoutOptions::default() });
        let additions: Vec<_> = rows.iter().filter(|row| row.kind == DiffRowKind::Addition).collect();
        assert!(additions.len() >= 2);
        assert_eq!(additions[0].left.as_ref().and_then(|cell| cell.new_line), Some(1));
        assert!(additions[1].continuation);
        assert_eq!(additions[1].left.as_ref().and_then(|cell| cell.new_line), None);
        assert_eq!(additions[1].marker, '+');
    }

    #[test]
    fn layout_ignores_invalid_intraline_boundaries() {
        let display = [DiffDisplayLine {
            kind: DiffDisplayKind::Addition,
            old_line: None,
            new_line: Some(1),
            text: "café".to_owned(),
            changed: vec![(2, 4)],
        }];
        let rows = layout_display_lines(&display, LayoutOptions { width: 20, ..LayoutOptions::default() });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left.as_ref().expect("cell").segments[0].text, "café");
    }

    #[test]
    fn narrow_side_by_side_falls_back_to_unified() {
        let document = DiffDocument::between("old\n", "new\n", DiffOptions::default());
        let rows = document.layout(LayoutOptions {
            layout: DiffLayout::SideBySide,
            width: 40,
            ..LayoutOptions::default()
        });
        assert!(
            rows.iter()
                .filter(|row| row.kind != DiffRowKind::HunkHeader)
                .all(|row| row.right.is_none())
        );
    }

    #[test]
    fn responsive_gutter_policy_preserves_source_room_at_the_boundary() {
        assert_eq!(diff_gutter_width(5), 9);
        assert!(diff_gutter_fits(29, 5));
        assert!(!diff_gutter_fits(28, 5));
        assert_eq!(diff_layout_width(29, 5, true), 29);
        assert_eq!(diff_layout_width(28, 5, false), 37);
    }

    #[test]
    fn side_by_side_policy_has_a_stable_resize_boundary() {
        assert!(!diff_side_by_side_fits(Some(59)));
        assert!(diff_side_by_side_fits(Some(DIFF_MIN_SIDE_BY_SIDE_WIDTH)));
        assert!(diff_side_by_side_fits(None));
    }

    #[test]
    fn side_by_side_wrapping_leaves_shorter_side_empty() {
        let document =
            DiffDocument::between("short\n", "this is a much longer replacement line\n", DiffOptions::default());
        let rows = document.layout(LayoutOptions {
            layout: DiffLayout::SideBySide,
            width: 80,
            ..LayoutOptions::default()
        });
        let continuation = rows.iter().find(|row| row.continuation).expect("long replacement should wrap");
        assert!(continuation.left.is_none());
        assert!(continuation.right.is_some());
    }

    #[test]
    fn bounded_rows_keep_head_tail_and_exact_omission() {
        let old = (0..20).map(|index| format!("old-{index}\n")).collect::<String>();
        let new = (0..20).map(|index| format!("new-{index}\n")).collect::<String>();
        let rows = DiffDocument::between(&old, &new, DiffOptions::default())
            .layout(LayoutOptions { max_rows: 5, ..LayoutOptions::default() });
        assert_eq!(rows.len(), 5);
        let omission = rows.iter().find(|row| row.kind == DiffRowKind::Omission).expect("omission row");
        let text = &omission.left.as_ref().expect("cell").segments[0].text;
        assert!(text.ends_with(" rows omitted"));
        assert!(rows.last().and_then(|row| row.left.as_ref()).is_some());
    }

    #[test]
    fn bounded_display_lines_keep_asymmetric_head_and_tail() {
        let lines =
            display_lines_from_unified_diff("@@ -1,4 +1,4 @@\n-old-head\n+new-head\n context\n-old-tail\n+new-tail\n");
        let bounded = bounded_display_lines(&lines, 4);

        assert_eq!(bounded.len(), 4);
        assert_eq!(bounded[0].text, "@@ -1,4 +1,4 @@");
        assert_eq!(bounded[1].text, "old-head");
        assert_eq!(bounded[2].kind, DiffDisplayKind::Metadata);
        assert!(bounded[2].text.contains("lines omitted"));
        assert_eq!(bounded[3].text, "new-tail");
    }

    #[test]
    fn display_lines_preserve_hunk_range_counts() {
        // Regression: `@@ -65,19 +64,0 @@` must not collapse to
        // `@@ -65 +64 @@`, which reads as a one-line change.
        let lines = display_lines_from_unified_diff("@@ -65,19 +64,0 @@\n-old\n");
        assert_eq!(lines[0].kind, DiffDisplayKind::HunkHeader);
        assert_eq!(lines[0].text, "@@ -65,19 +64,0 @@");
    }

    #[test]
    fn hunk_header_formatter_elides_single_counts_and_keeps_ranges() {
        assert_eq!(format_hunk_header(1, 1, 1, 1), "@@ -1 +1 @@");
        assert_eq!(format_hunk_header(65, 19, 65, 0), "@@ -65,19 +64,0 @@");
        assert_eq!(format_hunk_header(0, 0, 1, 5), "@@ -0,0 +1,5 @@");
    }

    #[test]
    fn display_lines_from_hunks_keep_range_counts() {
        let document = DiffDocument::between("a\nb\nc\n", "a\nx\ny\nc\n", DiffOptions::default());
        let lines = display_lines_from_hunks(&document.hunks);
        let hunk = &document.hunks[0];
        assert_eq!(lines[0].text, format_hunk_header(hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines));
        assert!(lines[0].text.contains(','), "hunk header must keep range counts: {:?}", lines[0].text);
    }

    #[test]
    fn plain_unified_formatter_preserves_labels_and_newline_hints() {
        let output = format_unified_diff(
            "old\n",
            "new",
            DiffOptions {
                old_label: Some("a/file.txt"),
                new_label: Some("b/file.txt"),
                ..DiffOptions::default()
            },
        );

        assert!(output.starts_with("--- a/file.txt\n+++ b/file.txt\n@@"));
        assert!(output.contains("@@ -1 +1 @@"));
        assert!(output.contains("-old\n"));
        assert!(output.contains("+new\n\\ No newline at end of file\n"));
        assert!(!output.contains('\r'));
    }

    #[test]
    fn character_chunks_coalesce_and_reconstruct_both_sides() {
        let chunks = compute_diff_chunks("abc", "axc");
        assert_eq!(chunks.len(), 4);
        let old = chunks
            .iter()
            .filter_map(|chunk| match chunk {
                Chunk::Equal(text) | Chunk::Delete(text) => Some(*text),
                Chunk::Insert(_) => None,
            })
            .collect::<String>();
        let new = chunks
            .iter()
            .filter_map(|chunk| match chunk {
                Chunk::Equal(text) | Chunk::Insert(text) => Some(*text),
                Chunk::Delete(_) => None,
            })
            .collect::<String>();
        assert_eq!(old, "abc");
        assert_eq!(new, "axc");
    }

    #[test]
    fn disjoint_large_input_respects_small_timeout_and_remains_readable() {
        let old = (0..10_000).map(|index| format!("old-{index}\n")).collect::<String>();
        let new = (0..10_000).rev().map(|index| format!("new-{index}\n")).collect::<String>();
        let started = Instant::now();
        let document = DiffDocument::between(
            &old,
            &new,
            DiffOptions {
                timeout: Duration::from_millis(5),
                ..DiffOptions::default()
            },
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(document.stats.additions > 0);
        assert!(document.stats.deletions > 0);
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let document = DiffDocument::between("a\r\nb\r\n", "a\r\nc\r\n", DiffOptions::default());
        assert_eq!(document.hunks.len(), 1);
        let texts: Vec<&str> = document.hunks[0].lines.iter().map(|line| line.text.as_str()).collect();
        assert!(texts.contains(&"a\r\n"));
        assert!(texts.contains(&"b\r\n"));
        assert!(texts.contains(&"c\r\n"));
    }

    #[test]
    fn missing_final_newline_emits_hint() {
        let document = DiffDocument::between("a\nb", "a\nb\n", DiffOptions::default());
        let formatted = format_unified_hunks(&document.hunks, &DiffOptions::default());
        assert!(formatted.contains("\\ No newline at end of file"));
    }

    #[test]
    fn zero_context_insert_hunk_header_is_git_compatible() {
        let options = DiffOptions { context_lines: 0, ..DiffOptions::default() };
        let document = DiffDocument::between("", "x\ny\n", options.clone());
        let formatted = format_unified_hunks(&document.hunks, &options);
        assert!(formatted.contains("@@ -0,0 +1,2 @@"));
    }

    #[test]
    fn binary_content_skips_intraline_annotation() {
        let binary_old = "data\u{0}one\nshared\n";
        let binary_new = "data\u{0}two\nshared\n";
        let lines =
            display_lines_from_hunks(&DiffDocument::between(binary_old, binary_new, DiffOptions::default()).hunks);
        let annotated: Vec<_> = lines.iter().filter(|line| !line.changed.is_empty()).collect();
        assert!(annotated.is_empty(), "binary lines must not receive intraline ranges");

        // Sanity: text content still gets intraline ranges.
        let text_lines = display_lines_from_hunks(
            &DiffDocument::between("alpha beta\n", "alpha gamma\n", DiffOptions::default()).hunks,
        );
        assert!(text_lines.iter().any(|line| !line.changed.is_empty()));
    }

    #[cfg(feature = "ansi")]
    #[test]
    fn ansi_adapter_can_render_without_color() {
        let document = DiffDocument::between("old\n", "new\n", DiffOptions::default());
        let rows = document.layout(LayoutOptions::default());
        let rendered = render_ansi_rows(&rows, AnsiDiffPalette::default(), false);
        assert!(rendered.iter().any(|line| line.starts_with("- old")));
        assert!(rendered.iter().any(|line| line.starts_with("+ new")));
        assert!(rendered.iter().all(|line| !line.contains('\u{1b}')));
    }

    #[cfg(feature = "ratatui")]
    #[test]
    fn ratatui_adapter_preserves_semantic_markers() {
        let document = DiffDocument::between("old\n", "new\n", DiffOptions::default());
        let rows = document.layout(LayoutOptions::default());
        let lines = to_ratatui_lines(&rows);
        let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert!(rendered.iter().any(|line| line.starts_with("- old")));
        assert!(rendered.iter().any(|line| line.starts_with("+ new")));
    }

    #[test]
    fn generative_small_docs_round_trip_across_algorithms_and_unified() {
        // Matklad-style oracle fuzzing in the small: a humble deterministic
        // PRNG is enough to shake out tricky interactions. Generate tiny docs
        // from a swarmed line alphabet (small, overlapping inputs beat huge
        // uniform ones) and cross-check Myers vs Patience vs Histogram plus a
        // unified format/parse round-trip.
        const LINE_ALPHABET: [&str; 6] = ["a\n", "b\n", "c\n", "alpha\n", "beta\n", "x\n"];
        let mut rng = SwarmRng::new(0x9E37_79B9_7F4A_7C15);
        // Re-use buffers across iterations (static allocation in the small).
        let mut alphabet: Vec<&str> = Vec::with_capacity(LINE_ALPHABET.len());
        let mut old_text = String::with_capacity(64);
        let mut new_text = String::with_capacity(64);

        // Hand-picked asymmetric boundaries first: order swaps and empty sides
        // catch what symmetric fixtures miss.
        let seeds: [(&str, &str); 5] = [
            ("a\n", "b\n"),
            ("a\nb\n", "b\na\n"),
            ("", "x\n"),
            ("x\n", ""),
            ("a\na\n", "a\n"),
        ];
        for (old, new) in seeds {
            assert_generative_oracles(old, new);
        }

        for _ in 0..1024 {
            alphabet.clear();
            alphabet.extend(LINE_ALPHABET);
            rng.shuffle_str(&mut alphabet);
            let alphabet_len = rng.range(1, alphabet.len() + 1);
            alphabet.truncate(alphabet_len);

            gen_doc(&mut rng, &alphabet, &mut old_text);
            gen_doc(&mut rng, &alphabet, &mut new_text);
            assert_generative_oracles(&old_text, &new_text);
        }
    }

    fn assert_generative_oracles(old: &str, new: &str) {
        let options_for = |algorithm: DiffAlgorithm| DiffOptions {
            context_lines: 100,
            algorithm,
            ..DiffOptions::default()
        };
        let myers = DiffDocument::between(old, new, options_for(DiffAlgorithm::Myers));
        let patience = DiffDocument::between(old, new, options_for(DiffAlgorithm::Patience));
        let histogram = DiffDocument::between(old, new, options_for(DiffAlgorithm::Histogram));

        if old == new {
            assert!(myers.hunks.is_empty(), "identical docs must yield no hunks: {old:?}");
            assert!(patience.hunks.is_empty(), "identical docs must yield no hunks: {old:?}");
            assert!(histogram.hunks.is_empty(), "identical docs must yield no hunks: {old:?}");
            return;
        }

        // Oracle 1: applying hunks to the old side must reconstruct both sides.
        let (myers_old, myers_new) = apply_hunks(&myers.hunks);
        assert_eq!(myers_old, old, "Myers hunks do not reconstruct old side for {old:?} -> {new:?}");
        assert_eq!(myers_new, new, "Myers hunks do not reconstruct new side for {old:?} -> {new:?}");

        // Oracle 2 (`regex` vs `regex_lite`): all algorithms must agree on the
        // applied result even when hunk splitting differs.
        let (patience_old, patience_new) = apply_hunks(&patience.hunks);
        let (histogram_old, histogram_new) = apply_hunks(&histogram.hunks);
        assert_eq!(
            (patience_old.as_str(), patience_new.as_str()),
            (old, new),
            "Patience mis-reconstructs {old:?} -> {new:?}"
        );
        assert_eq!(
            (histogram_old.as_str(), histogram_new.as_str()),
            (old, new),
            "Histogram mis-reconstructs {old:?} -> {new:?}"
        );

        // Oracle 3: unified format/parse round-trip preserves the applied result.
        let formatted = format_unified_hunks(&myers.hunks, &options_for(DiffAlgorithm::Myers));
        let reparsed = DiffDocument::from_unified(&formatted).expect("formatted hunks must parse");
        let (re_old, re_new) = apply_hunks(&reparsed.hunks);
        assert_eq!(
            (re_old.as_str(), re_new.as_str()),
            (old, new),
            "unified round-trip diverges for {old:?} -> {new:?}"
        );
        assert_eq!(
            (reparsed.stats.additions, reparsed.stats.deletions),
            (myers.stats.additions, myers.stats.deletions),
            "unified round-trip changed change counts for {old:?} -> {new:?}"
        );
    }

    fn apply_hunks(hunks: &[DiffHunk]) -> (String, String) {
        let mut old = String::new();
        let mut new = String::new();
        for line in hunks.iter().flat_map(|hunk| &hunk.lines) {
            match line.kind {
                DiffLineKind::Context => {
                    old.push_str(&line.text);
                    new.push_str(&line.text);
                }
                DiffLineKind::Deletion => old.push_str(&line.text),
                DiffLineKind::Addition => new.push_str(&line.text),
            }
        }
        (old, new)
    }

    fn gen_doc(rng: &mut SwarmRng, alphabet: &[&str], result: &mut String) {
        result.clear();
        let count = rng.range(0, 8);
        for _ in 0..count {
            let line = alphabet[rng.range(0, alphabet.len())];
            result.push_str(line);
        }
    }

    /// Minimal deterministic PRNG (splitmix64): no new dependencies, stable CI.
    struct SwarmRng {
        state: u64,
    }

    impl SwarmRng {
        const fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut value = self.state;
            value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            value ^ (value >> 31)
        }

        fn range(&mut self, low: usize, high: usize) -> usize {
            assert!(low < high, "empty range");
            let span = high - low;
            low + (self.next_u64() as usize % span)
        }

        fn shuffle_str(&mut self, items: &mut [&str]) {
            for index in (1..items.len()).rev() {
                let other = self.range(0, index + 1);
                items.swap(index, other);
            }
        }
    }
}
