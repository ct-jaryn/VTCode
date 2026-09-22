//! File input helpers for provider-specific inline file attachments.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::path::Path;

pub const MAX_INPUT_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// File data prepared for inline model input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInputData {
    pub base64_data: String,
    pub filename: String,
    file_path: String,
    size: u64,
}

/// Read a validated local file path for inline model input.
///
/// Callers must validate path scope and user intent before using this helper.
///
/// Stat, read, and base64 encoding all happen inside one `spawn_blocking`
/// segment. `tokio::fs` would run the stat and read as two separate hops onto
/// the shared blocking pool, and base64-encoding up to 50 MiB on a runtime
/// worker would be a long poll that stalls I/O — both patterns the fast-Tokio
/// guidance calls out ("batch a series of filesystem operations into the
/// largest sensible blocking segment").
pub async fn read_input_file_any_path<P: AsRef<Path>>(file_path: P) -> Result<FileInputData> {
    let path = file_path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || read_input_file_blocking(&path))
        .await
        .context("input file read task failed")?
}

fn read_input_file_blocking(path: &Path) -> Result<FileInputData> {
    let metadata = std::fs::metadata(path).with_context(|| format!("Failed to stat input file: {}", path.display()))?;

    if !metadata.is_file() {
        return Err(anyhow::anyhow!("Input path is not a file: {}", path.display()));
    }

    if metadata.len() > MAX_INPUT_FILE_BYTES {
        return Err(anyhow::anyhow!(
            "Input file too large: {} bytes (max {} bytes)",
            metadata.len(),
            MAX_INPUT_FILE_BYTES
        ));
    }

    let file_contents =
        std::fs::read(path).with_context(|| format!("Failed to read input file: {}", path.display()))?;

    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string());

    Ok(FileInputData {
        base64_data: base64::engine::general_purpose::STANDARD.encode(&file_contents),
        filename,
        file_path: path.display().to_string(),
        size: file_contents.len() as u64,
    })
}

pub fn decoded_base64_size(file_data: &str) -> Result<u64> {
    let payload = inline_base64_payload(file_data);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("Invalid base64 file_data payload")?;
    Ok(decoded.len() as u64)
}

fn inline_base64_payload(file_data: &str) -> &str {
    let trimmed = file_data.trim();
    if let Some((prefix, payload)) = trimmed.split_once(',')
        && prefix.contains(";base64")
    {
        payload.trim()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_INPUT_FILE_BYTES, decoded_base64_size};
    use base64::Engine as _;

    #[test]
    fn decoded_base64_size_supports_raw_base64() {
        assert_eq!(decoded_base64_size("aGVsbG8=").unwrap(), 5);
    }

    #[test]
    fn decoded_base64_size_supports_data_url_prefix() {
        assert_eq!(decoded_base64_size("data:application/pdf;base64,aGVsbG8=").unwrap(), 5);
    }

    #[test]
    fn max_input_file_bytes_matches_openai_limit() {
        assert_eq!(MAX_INPUT_FILE_BYTES, 50 * 1024 * 1024);
    }

    #[tokio::test]
    async fn read_input_file_any_path_round_trips_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("payload.bin");
        std::fs::write(&path, b"hello world").expect("write payload");

        let data = super::read_input_file_any_path(&path).await.expect("read payload");

        assert_eq!(data.filename, "payload.bin");
        assert_eq!(data.size, 11);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&data.base64_data)
            .expect("valid base64");
        assert_eq!(decoded, b"hello world");
    }

    #[tokio::test]
    async fn read_input_file_any_path_rejects_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let err = super::read_input_file_any_path(dir.path())
            .await
            .expect_err("directory must be rejected");
        assert!(err.to_string().contains("not a file"), "unexpected error: {err}");
    }
}
