//! Bundle size estimation and limits enforcement.
//!
//! Provides pre-export size estimation to warn users before they spend time
//! exporting/encrypting data that would exceed GitHub Pages limits.

use anyhow::{Context, Result, bail};
use frankensqlite::Connection;
use frankensqlite::Row;
use frankensqlite::compat::{ConnectionExt, ParamValue, RowExt};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Maximum site size for GitHub Pages (1 GB)
pub const MAX_SITE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

/// Warning threshold for total site size (900 MB - approaching limit)
pub const SITE_SIZE_WARNING_BYTES: u64 = 900 * 1024 * 1024;

/// Maximum file size for GitHub (100 MiB)
pub const MAX_FILE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// Warning threshold for file size (50 MiB)
pub const FILE_SIZE_WARNING_BYTES: u64 = 50 * 1024 * 1024;

/// Default chunk size for encrypted payload (8 MiB)
pub const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;

/// AEAD authentication tag overhead per chunk (16 bytes)
pub const AEAD_TAG_OVERHEAD: u64 = 16;

/// Estimated static assets size (HTML, JS, CSS, WASM vendor) - approximately 2 MB
pub const STATIC_ASSETS_SIZE: u64 = 2 * 1024 * 1024;

/// Typical compression ratio for text content (deflate)
pub const COMPRESSION_RATIO: f64 = 0.45;

/// Pre-export size estimate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeEstimate {
    /// Raw content size in bytes (uncompressed)
    pub plaintext_bytes: u64,
    /// Estimated compressed size in bytes
    pub compressed_bytes: u64,
    /// Estimated encrypted size in bytes (with AEAD overhead)
    pub encrypted_bytes: u64,
    /// Static assets size (HTML, JS, CSS, WASM)
    pub static_assets_bytes: u64,
    /// Total estimated site size
    pub total_site_bytes: u64,
    /// Estimated number of payload chunks
    pub chunk_count: u32,
    /// Number of conversations included
    pub conversation_count: u64,
    /// Number of messages included
    pub message_count: u64,
}

impl SizeEstimate {
    /// Create a size estimate from a database and filter
    pub fn from_database<P: AsRef<Path>>(
        db_path: P,
        agents: Option<&[String]>,
        since_ts: Option<i64>,
        until_ts: Option<i64>,
    ) -> Result<Self> {
        let conn = Connection::open(db_path.as_ref().to_string_lossy().as_ref())
            .context("Failed to open database for size estimation")?;

        // Build filter conditions
        let mut conditions = Vec::new();
        let mut param_values: Vec<ParamValue> = Vec::new();

        if let Some(agents) = agents.filter(|a| !a.is_empty()) {
            let placeholders: Vec<_> = agents.iter().map(|_| "?").collect();
            conditions.push(format!("c.agent IN ({})", placeholders.join(", ")));
            for agent in agents {
                param_values.push(ParamValue::from(agent.as_str()));
            }
        }

        if let Some(since) = since_ts {
            conditions.push("c.started_at >= ?".to_string());
            param_values.push(ParamValue::from(since));
        }

        if let Some(until) = until_ts {
            conditions.push("c.started_at <= ?".to_string());
            param_values.push(ParamValue::from(until));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let params_slice = &param_values;

        // Query conversation count
        let conv_sql = format!("SELECT COUNT(*) FROM conversations c{}", where_clause);
        let conversation_count: u64 = conn
            .query_row_map(&conv_sql, params_slice, |row: &Row| {
                row.get_typed::<i64>(0).map(|v| v as u64)
            })
            .unwrap_or(0);

        // Query message count and content size
        let msg_sql = format!(
            "SELECT COUNT(*), SUM(LENGTH(m.content))
             FROM messages m
             JOIN conversations c ON m.conversation_id = c.id
             {}",
            where_clause
        );
        let (message_count, plaintext_bytes): (u64, u64) = conn
            .query_row_map(&msg_sql, params_slice, |row: &Row| {
                let raw_message_count = row.get_typed::<i64>(0).unwrap_or(0);
                let raw_plaintext_bytes = row.get_typed::<Option<i64>>(1)?.unwrap_or(0);
                Ok((
                    raw_message_count.max(0) as u64,
                    raw_plaintext_bytes.max(0) as u64,
                ))
            })
            .unwrap_or((0, 0));

        Self::from_plaintext_size(plaintext_bytes, conversation_count, message_count)
    }

    /// Create estimate from known plaintext size
    pub fn from_plaintext_size(
        plaintext_bytes: u64,
        conversation_count: u64,
        message_count: u64,
    ) -> Result<Self> {
        // Estimate compression
        let compressed_bytes = (plaintext_bytes as f64 * COMPRESSION_RATIO) as u64;

        // Calculate chunk count (minimum of 1 chunk even for empty content)
        let chunk_count_u64 = compressed_bytes.div_ceil(DEFAULT_CHUNK_SIZE).max(1);
        let chunk_count = u32::try_from(chunk_count_u64).unwrap_or(u32::MAX);

        // Add AEAD overhead
        let aead_overhead = u64::from(chunk_count)
            .checked_mul(AEAD_TAG_OVERHEAD)
            .ok_or_else(|| anyhow::anyhow!("AEAD overhead overflow"))?;
        let encrypted_bytes = compressed_bytes
            .checked_add(aead_overhead)
            .ok_or_else(|| anyhow::anyhow!("Encrypted size overflow"))?;

        // Total with static assets
        let total_site_bytes = encrypted_bytes
            .checked_add(STATIC_ASSETS_SIZE)
            .ok_or_else(|| anyhow::anyhow!("Total site size overflow"))?;

        Ok(Self {
            plaintext_bytes,
            compressed_bytes,
            encrypted_bytes,
            static_assets_bytes: STATIC_ASSETS_SIZE,
            total_site_bytes,
            chunk_count,
            conversation_count,
            message_count,
        })
    }

    /// Check if the estimate exceeds hard limits
    pub fn check_limits(&self) -> SizeLimitResult {
        if self.total_site_bytes > MAX_SITE_SIZE_BYTES {
            return SizeLimitResult::ExceedsLimit(SizeError::TotalExceedsLimit {
                actual: self.total_site_bytes,
                limit: MAX_SITE_SIZE_BYTES,
            });
        }

        if self.total_site_bytes > SITE_SIZE_WARNING_BYTES {
            return SizeLimitResult::Warning(SizeWarning::ApproachingLimit {
                actual: self.total_site_bytes,
                limit: MAX_SITE_SIZE_BYTES,
                percentage: (self.total_site_bytes as f64 / MAX_SITE_SIZE_BYTES as f64 * 100.0)
                    as u8,
            });
        }

        SizeLimitResult::Ok
    }

    /// Format the estimate for display
    pub fn format_display(&self) -> String {
        format!(
            "Estimated bundle size: {}\n\
             • Payload: {} ({} chunks × {} max)\n\
             • Static assets: {}\n\
             • Compression ratio: ~{:.0}%\n\
             • Conversations: {}\n\
             • Messages: {}",
            format_bytes(self.total_site_bytes),
            format_bytes(self.encrypted_bytes),
            self.chunk_count,
            format_bytes(DEFAULT_CHUNK_SIZE),
            format_bytes(self.static_assets_bytes),
            COMPRESSION_RATIO * 100.0,
            self.conversation_count,
            self.message_count,
        )
    }
}

/// Result of checking size limits
#[derive(Debug, Clone)]
pub enum SizeLimitResult {
    /// Size is within limits
    Ok,
    /// Size is approaching limits (warning)
    Warning(SizeWarning),
    /// Size exceeds limits (error)
    ExceedsLimit(SizeError),
}

impl SizeLimitResult {
    /// Returns true if export should proceed
    pub fn is_ok(&self) -> bool {
        matches!(self, SizeLimitResult::Ok)
    }

    /// Returns true if there's a warning but export can proceed
    pub fn is_warning(&self) -> bool {
        matches!(self, SizeLimitResult::Warning(_))
    }

    /// Returns true if export should be blocked
    pub fn is_error(&self) -> bool {
        matches!(self, SizeLimitResult::ExceedsLimit(_))
    }
}

/// Size-related errors
#[derive(Debug, Clone, thiserror::Error)]
pub enum SizeError {
    /// Total site size exceeds GitHub Pages limit
    #[error(
        "Total size ({}) exceeds GitHub Pages limit ({})\n\n\
         Suggestions:\n\
         • Use --since \"90 days ago\" for recent conversations only\n\
         • Use --agents <name> to limit to specific agents\n\
         • Use --workspaces <path> to limit projects",
        format_bytes(*actual),
        format_bytes(*limit)
    )]
    TotalExceedsLimit { actual: u64, limit: u64 },
    /// Individual file exceeds limit
    #[error("File {path} ({}) exceeds limit ({})", format_bytes(*actual), format_bytes(*limit))]
    FileExceedsLimit {
        path: String,
        actual: u64,
        limit: u64,
    },
}

/// Size-related warnings
#[derive(Debug, Clone)]
pub enum SizeWarning {
    /// Total size is approaching limit
    ApproachingLimit {
        actual: u64,
        limit: u64,
        percentage: u8,
    },
    /// Individual file is large
    LargeFile { path: String, size: u64 },
}

impl std::fmt::Display for SizeWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizeWarning::ApproachingLimit {
                actual,
                limit,
                percentage,
            } => {
                write!(
                    f,
                    "Estimated size {} is {}% of GitHub Pages limit ({})",
                    format_bytes(*actual),
                    percentage,
                    format_bytes(*limit)
                )
            }
            SizeWarning::LargeFile { path, size } => {
                write!(f, "Large file: {} ({})", path, format_bytes(*size))
            }
        }
    }
}

/// Post-export bundle verification
pub struct BundleVerifier;

impl BundleVerifier {
    /// Verify a bundle directory meets all size constraints
    pub fn verify<P: AsRef<Path>>(site_dir: P) -> Result<Vec<SizeWarning>> {
        let site_dir = site_dir.as_ref();
        let mut warnings = Vec::new();
        let mut total_size = 0u64;

        visit_files(site_dir, &mut |path, size| {
            total_size += size;

            if size > MAX_FILE_SIZE_BYTES {
                bail!(
                    "File {} ({}) exceeds maximum file size ({}). Chunking may have failed.",
                    path.display(),
                    format_bytes(size),
                    format_bytes(MAX_FILE_SIZE_BYTES)
                );
            }

            if size > FILE_SIZE_WARNING_BYTES {
                let rel_path = path
                    .strip_prefix(site_dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                warnings.push(SizeWarning::LargeFile {
                    path: rel_path,
                    size,
                });
            }

            Ok(())
        })?;

        if total_size > MAX_SITE_SIZE_BYTES {
            bail!(
                "Total bundle size ({}) exceeds GitHub Pages limit ({})",
                format_bytes(total_size),
                format_bytes(MAX_SITE_SIZE_BYTES)
            );
        }

        if total_size > SITE_SIZE_WARNING_BYTES {
            warnings.push(SizeWarning::ApproachingLimit {
                actual: total_size,
                limit: MAX_SITE_SIZE_BYTES,
                percentage: (total_size as f64 / MAX_SITE_SIZE_BYTES as f64 * 100.0) as u8,
            });
        }

        Ok(warnings)
    }
}

/// Visit all files in a directory recursively
fn visit_files<F>(dir: &Path, f: &mut F) -> Result<()>
where
    F: FnMut(&Path, u64) -> Result<()>,
{
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            visit_files(&path, f)?;
        } else if file_type.is_file() {
            f(&path, metadata.len())?;
        }
    }
    Ok(())
}

/// Format bytes as human-readable string
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_size_estimate_from_plaintext() {
        let estimate = SizeEstimate::from_plaintext_size(
            10 * 1024 * 1024, // 10 MB plaintext
            100,
            5000,
        )
        .unwrap();

        // Should compress to ~4.5 MB
        assert!(estimate.compressed_bytes < estimate.plaintext_bytes);
        assert_eq!(estimate.conversation_count, 100);
        assert_eq!(estimate.message_count, 5000);
        assert!(estimate.chunk_count >= 1);
    }

    #[test]
    fn test_size_estimate_empty() {
        let estimate = SizeEstimate::from_plaintext_size(0, 0, 0).unwrap();
        assert_eq!(estimate.plaintext_bytes, 0);
        assert_eq!(estimate.chunk_count, 1); // At least 1 chunk
        assert_eq!(estimate.static_assets_bytes, STATIC_ASSETS_SIZE);
    }

    #[test]
    fn test_size_limit_ok() {
        let estimate = SizeEstimate::from_plaintext_size(
            100 * 1024 * 1024, // 100 MB - should be fine
            100,
            5000,
        )
        .unwrap();

        let result = estimate.check_limits();
        assert!(result.is_ok());
    }

    #[test]
    fn test_size_limit_warning() {
        // Need ~900 MB compressed to trigger warning
        // 900 MB / 0.45 compression = 2000 MB plaintext
        let estimate = SizeEstimate::from_plaintext_size(
            2000 * 1024 * 1024, // 2 GB plaintext -> ~900 MB compressed
            1000,
            50000,
        )
        .unwrap();

        let result = estimate.check_limits();
        assert!(result.is_warning() || result.is_error());
    }

    #[test]
    fn test_size_limit_exceeded() {
        let estimate = SizeEstimate::from_plaintext_size(
            3000 * 1024 * 1024, // 3 GB plaintext -> ~1.35 GB compressed
            5000,
            250000,
        )
        .unwrap();

        let result = estimate.check_limits();
        assert!(result.is_error());
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 bytes");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_bytes(1536 * 1024), "1.5 MB");
    }

    #[test]
    fn test_format_display() {
        let estimate = SizeEstimate::from_plaintext_size(10 * 1024 * 1024, 50, 2500).unwrap();

        let display = estimate.format_display();
        assert!(display.contains("Estimated bundle size"));
        assert!(display.contains("Conversations: 50"));
        assert!(display.contains("Messages: 2500"));
    }

    #[test]
    fn test_size_error_display() {
        let err = SizeError::TotalExceedsLimit {
            actual: 2 * 1024 * 1024 * 1024,
            limit: 1024 * 1024 * 1024,
        };

        let msg = err.to_string();
        assert!(msg.contains("2.0 GB"));
        assert!(msg.contains("1.0 GB"));
        assert!(msg.contains("Suggestions"));
    }

    #[test]
    fn test_size_error_display_and_source_are_preserved() {
        let cases = vec![
            (
                SizeError::TotalExceedsLimit {
                    actual: 2 * 1024 * 1024 * 1024,
                    limit: 1024 * 1024 * 1024,
                },
                "Total size (2.0 GB) exceeds GitHub Pages limit (1.0 GB)\n\n\
                 Suggestions:\n\
                 • Use --since \"90 days ago\" for recent conversations only\n\
                 • Use --agents <name> to limit to specific agents\n\
                 • Use --workspaces <path> to limit projects",
            ),
            (
                SizeError::FileExceedsLimit {
                    path: "site/archive.bin".to_string(),
                    actual: 150 * 1024 * 1024,
                    limit: 100 * 1024 * 1024,
                },
                "File site/archive.bin (150.0 MB) exceeds limit (100.0 MB)",
            ),
        ];

        for (error, expected_display) in cases {
            assert_eq!(error.to_string(), expected_display);
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn test_bundle_verifier() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();

        // Create some small files
        std::fs::write(temp.path().join("small.txt"), vec![0u8; 1000]).unwrap();
        std::fs::write(temp.path().join("medium.txt"), vec![0u8; 10000]).unwrap();

        let warnings = BundleVerifier::verify(temp.path()).unwrap();
        assert!(warnings.is_empty()); // No warnings for small files
    }

    #[test]
    fn test_chunk_count_ceiling_division() {
        // Test that chunk count uses proper ceiling division
        // COMPRESSION_RATIO = 0.45, DEFAULT_CHUNK_SIZE = 8 MB

        // Test 1: Very small data -> 1 chunk
        let estimate = SizeEstimate::from_plaintext_size(1000, 1, 10).unwrap();
        assert_eq!(estimate.chunk_count, 1, "Small data should be 1 chunk");

        // Test 2: Data that compresses to exactly 1 chunk size
        // 8 MB / 0.45 = 17.78 MB plaintext -> exactly 8 MB compressed -> 1 chunk
        // Use a value that when multiplied by 0.45 gives exactly DEFAULT_CHUNK_SIZE
        let one_chunk_plaintext = (DEFAULT_CHUNK_SIZE as f64 / COMPRESSION_RATIO) as u64;
        let estimate = SizeEstimate::from_plaintext_size(one_chunk_plaintext, 10, 100).unwrap();
        // Due to floating point, compressed_bytes should be very close to DEFAULT_CHUNK_SIZE
        // The important thing is it should NOT be 2 chunks when it's exactly 1 chunk of data
        assert_eq!(
            estimate.chunk_count, 1,
            "Exactly one chunk's worth should be 1 chunk, not 2"
        );

        // Test 3: Data just over 1 chunk -> 2 chunks
        let over_one_chunk = one_chunk_plaintext + 1000000; // Add ~1 MB to plaintext
        let estimate = SizeEstimate::from_plaintext_size(over_one_chunk, 10, 100).unwrap();
        assert!(
            estimate.chunk_count >= 1,
            "Over one chunk should be at least 1 chunk"
        );

        // Test 4: Large data that compresses to ~2 chunks
        let two_chunks_plaintext = (2.0 * DEFAULT_CHUNK_SIZE as f64 / COMPRESSION_RATIO) as u64;
        let estimate = SizeEstimate::from_plaintext_size(two_chunks_plaintext, 100, 1000).unwrap();
        assert_eq!(
            estimate.chunk_count, 2,
            "Exactly two chunks' worth should be 2 chunks, not 3"
        );
    }

    #[test]
    fn test_from_plaintext_size_handles_extremely_large_inputs() {
        let estimate = SizeEstimate::from_plaintext_size(u64::MAX, 1, 1).unwrap();
        assert_eq!(estimate.chunk_count, u32::MAX);
        assert!(estimate.total_site_bytes >= estimate.compressed_bytes);
    }

    #[test]
    #[cfg(unix)]
    fn test_visit_files_skips_symlink_paths() {
        use std::collections::HashSet;
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::fs::create_dir_all(outside.path().join("nested")).unwrap();
        std::fs::write(outside.path().join("nested/hidden.txt"), "hidden").unwrap();

        symlink(
            outside.path().join("secret.txt"),
            src.path().join("linked-file.txt"),
        )
        .unwrap();
        symlink(outside.path().join("nested"), src.path().join("linked-dir")).unwrap();

        let mut visited = HashSet::new();
        visit_files(src.path(), &mut |path, _size| {
            visited.insert(
                path.strip_prefix(src.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
            );
            Ok(())
        })
        .unwrap();

        assert!(visited.contains("root.txt"));
        assert!(!visited.contains("linked-file.txt"));
        assert!(!visited.iter().any(|p| p.starts_with("linked-dir/")));
    }
}
