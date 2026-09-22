#![expect(
    clippy::indexing_slicing,
    reason = "Image signatures are checked for minimum length before fixed-format byte access."
)]

//! Image processing utilities

use anyhow::{Context, Result};
use base64::Engine;
use std::path::Path;

/// Represents the data from an image file ready for LLM consumption
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImageData {
    /// Base64-encoded image data
    pub base64_data: String,

    /// MIME type of the image (e.g., "image/png", "image/jpeg")
    pub mime_type: String,

    /// Original file path or URL
    pub file_path: String,

    /// File size in bytes
    pub size: u64,
}

/// Detects MIME type from Content-Type header
pub fn detect_mime_type_from_content_type(content_type: &str) -> Option<String> {
    let content_type = content_type.to_lowercase();
    if content_type.starts_with("image/png") {
        Some("image/png".to_string())
    } else if content_type.starts_with("image/jpeg") || content_type.starts_with("image/jpg") {
        Some("image/jpeg".to_string())
    } else if content_type.starts_with("image/gif") {
        Some("image/gif".to_string())
    } else if content_type.starts_with("image/webp") {
        Some("image/webp".to_string())
    } else if content_type.starts_with("image/bmp") {
        Some("image/bmp".to_string())
    } else if content_type.starts_with("image/tiff") || content_type.starts_with("image/tif") {
        Some("image/tiff".to_string())
    } else if content_type.starts_with("image/svg") {
        Some("image/svg+xml".to_string())
    } else {
        None
    }
}

/// Detects MIME type from file data (magic bytes)
pub fn detect_mime_type_from_data(data: &[u8]) -> String {
    // JPEG magic bytes: starts with FF D8
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return "image/jpeg".to_string();
    }

    // Need at least 8 bytes for other formats
    if data.len() < 8 {
        return "image/png".to_string();
    }

    match &data[..8] {
        [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] => "image/png".to_string(),
        [0x47, 0x49, 0x46, 0x38, _, _, _, _] => {
            if data.len() >= 12 && &data[8..12] == b"WEBP" {
                "image/webp".to_string()
            } else {
                "image/gif".to_string()
            }
        }
        [0x52, 0x49, 0x46, 0x46, _, _, _, _] => {
            if data.len() >= 12 && &data[8..12] == b"WEBP" {
                "image/webp".to_string()
            } else {
                "image/png".to_string()
            }
        }
        [0x42, 0x4D, _, _] => "image/bmp".to_string(),
        _ => "image/png".to_string(),
    }
}

/// Detects the MIME type based on file extension
fn detect_mime_type_from_extension(path: &Path) -> Result<String> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_lowercase();

    let mime_type = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => return Err(anyhow::anyhow!("Unsupported image format: {extension}")),
    };

    Ok(mime_type.to_string())
}

/// Validates that the image file path has a supported extension
pub fn has_supported_image_extension(path: &Path) -> bool {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_lowercase();

    const VALID_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "svg"];
    VALID_EXTENSIONS.contains(&extension.as_str())
}

/// Encodes binary data to base64
pub fn encode_to_base64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Maximum accepted image file size (20 MB).
pub const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;

fn image_too_large_error(len: u64) -> anyhow::Error {
    anyhow::anyhow!("Image file too large: {len} bytes (max {}MB)", MAX_IMAGE_FILE_BYTES / (1024 * 1024))
}

/// Read an image file into base64 form inside one blocking segment.
///
/// Stat, size check, read, and base64 encoding all run in a single
/// `spawn_blocking` hop instead of chained `tokio::fs` calls plus an on-worker
/// encode of up to 20 MB. Batching here keeps the shared blocking pool from
/// seeing two round-trips per file and keeps the base64 encode off the runtime
/// workers (fast-Tokio: batch blocking work, avoid long polls).
async fn read_image_file_blocking(path: &Path) -> Result<ImageData> {
    let owned_path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_image_file_blocking_inner(&owned_path))
        .await
        .context("image read task failed")?
}

fn read_image_file_blocking_inner(path: &Path) -> Result<ImageData> {
    // Fail fast on oversized regular files before reading them into memory.
    // `metadata.len()` is 0 for pipes/character devices, so those still fall
    // through to the read + post-read size check below.
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.is_file()
        && metadata.len() > MAX_IMAGE_FILE_BYTES
    {
        return Err(image_too_large_error(metadata.len()));
    }

    let file_contents =
        std::fs::read(path).with_context(|| format!("Failed to read image file: {}", path.display()))?;

    if file_contents.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Err(image_too_large_error(file_contents.len() as u64));
    }

    let mime_type = detect_mime_type_from_extension(path)?;
    Ok(ImageData {
        base64_data: encode_to_base64(&file_contents),
        mime_type,
        file_path: path.display().to_string(),
        size: file_contents.len() as u64,
    })
}

/// Reads an image file from the local filesystem and converts it to base64 format.
///
/// Validates the path for traversal attacks and checks the file extension
/// against a supported set. Max file size is 20 MB.
pub async fn read_image_file<P: AsRef<Path>>(file_path: P) -> Result<ImageData> {
    use crate::paths::is_safe_relative_path;

    let path = file_path.as_ref();

    if !is_safe_relative_path(&path.to_string_lossy()) {
        return Err(anyhow::anyhow!("Unsafe or traversal detected in image path: {}", path.display()));
    }

    if !has_supported_image_extension(path) {
        return Err(anyhow::anyhow!("Unsupported image extension for path: {}", path.display()));
    }

    read_image_file_blocking(path).await
}

/// Reads an image file from an absolute path (or already validated path) and
/// converts it to base64.
///
/// This skips relative-path safety checks and should only be used when the
/// caller has already validated the path scope and intent.
pub async fn read_image_file_any_path<P: AsRef<Path>>(file_path: P) -> Result<ImageData> {
    let path = file_path.as_ref();

    if !has_supported_image_extension(path) {
        return Err(anyhow::anyhow!("Unsupported image extension for path: {}", path.display()));
    }

    read_image_file_blocking(path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    const PNG_MAGIC: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    #[tokio::test]
    async fn read_image_file_returns_encoded_bytes_and_mime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pixel.png");
        std::fs::write(&path, PNG_MAGIC).expect("write image");

        let data = read_image_file_any_path(&path).await.expect("read image");

        assert_eq!(data.mime_type, "image/png");
        assert_eq!(data.size, PNG_MAGIC.len() as u64);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&data.base64_data)
            .expect("valid base64");
        assert_eq!(decoded, PNG_MAGIC);
    }

    #[tokio::test]
    async fn read_image_file_rejects_oversized_file_before_encoding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.png");
        // Sparse file just over the cap: rejected by the metadata guard so the
        // contents are never read or encoded.
        let file = std::fs::File::create(&path).expect("create image");
        file.set_len(MAX_IMAGE_FILE_BYTES + 1).expect("set length");
        drop(file);

        let err = read_image_file_any_path(&path)
            .await
            .expect_err("oversized image must be rejected");
        assert!(err.to_string().contains("too large"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn read_image_file_any_path_rejects_unsupported_extension() {
        let err = read_image_file_any_path("notes.txt")
            .await
            .expect_err("unsupported extension must be rejected");
        assert!(err.to_string().contains("Unsupported image extension"));
    }

    #[test]
    fn detect_mime_type_from_data_recognizes_png_magic() {
        assert_eq!(detect_mime_type_from_data(PNG_MAGIC), "image/png");
    }
}
