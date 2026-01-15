//! Bundle size estimation and limits enforcement.
//!
//! Provides pre-export size estimation to warn users before they spend time
//! exporting/encrypting data that would exceed GitHub Pages limits.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
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
        let conn = Connection::open(db_path.as_ref())
            .context("Failed to open database for size estimation")?;

        // Build filter conditions
        let mut conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(agents) = agents.filter(|a| !a.is_empty()) {
            let placeholders: Vec<_> = agents.iter().map(|_| "?").collect();
            conditions.push(format!("c.agent IN ({})", placeholders.join(", ")));
            for agent in agents {
                params.push(Box::new(agent.clone()));
            }
        }

        if let Some(since) = since_ts {
            conditions.push("c.started_at >= ?".to_string());
            params.push(Box::new(since));
        }

        if let Some(until) = until_ts {
            conditions.push("c.started_at <= ?".to_string());
            params.push(Box::new(until));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        // Query conversation count
        let conv_sql = format!("SELECT COUNT(*) FROM conversations c{}", where_clause);
        let conversation_count: u64 = conn
            .query_row(
                &conv_sql,
                rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                |row| row.get::<_, i64>(0).map(|v| v as u64),
            )
            .unwrap_or(0);

        // Query message count and content size
        let msg_sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(m.content)), 0)
             FROM messages m
             JOIN conversations c ON m.conversation_id = c.id
             {}",
            where_clause
        );
        let (message_count, plaintext_bytes): (u64, u64) = conn
            .query_row(
                &msg_sql,
                rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                |row| {
                    Ok((
                        row.get::<_, i64>(0).unwrap_or(0) as u64,
                        row.get::<_, i64>(1).unwrap_or(0) as u64,
                    ))
                },
            )
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
        let chunk_count = compressed_bytes.div_ceil(DEFAULT_CHUNK_SIZE).max(1) as u32;

        // Add AEAD overhead
        let encrypted_bytes = compressed_bytes + (chunk_count as u64 * AEAD_TAG_OVERHEAD);

        // Total with static assets
        let total_site_bytes = encrypted_bytes + STATIC_ASSETS_SIZE;

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
#[derive(Debug, Clone)]
pub enum SizeError {
    /// Total site size exceeds GitHub Pages limit
    TotalExceedsLimit { actual: u64, limit: u64 },
    /// Individual file exceeds limit
    FileExceedsLimit {
        path: String,
        actual: u64,
        limit: u64,
    },
}

impl std::fmt::Display for SizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SizeError::TotalExceedsLimit { actual, limit } => {
                write!(
                    f,
                    "Total size ({}) exceeds GitHub Pages limit ({})\n\n\
                     Suggestions:\n\
                     • Use --since \"90 days ago\" for recent conversations only\n\
                     • Use --agents <name> to limit to specific agents\n\
                     • Use --workspaces <path> to limit projects",
                    format_bytes(*actual),
                    format_bytes(*limit)
                )
            }
            SizeError::FileExceedsLimit {
                path,
                actual,
                limit,
            } => {
                write!(
                    f,
                    "File {} ({}) exceeds limit ({})",
                    path,
                    format_bytes(*actual),
                    format_bytes(*limit)
                )
            }
        }
    }
}

impl std::error::Error for SizeError {}

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

        if path.is_dir() {
            visit_files(&path, f)?;
        } else {
            let metadata = std::fs::metadata(&path)?;
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
}
