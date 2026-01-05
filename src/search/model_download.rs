//! Model download and management system.
//!
//! This module handles:
//! - Model manifest with SHA256 checksums
//! - State machine for download lifecycle
//! - Resumable downloads with progress reporting
//! - SHA256 verification
//! - Atomic installation (temp dir -> rename)
//! - Model version upgrade detection
//!
//! **Network Policy**: No network calls occur without explicit user consent.
//! The download system is consent-gated via [`ModelState::NeedsConsent`].

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// Model state machine for download lifecycle.
///
/// State transitions:
/// ```text
/// NotInstalled ──> NeedsConsent ──> Downloading ──> Verifying ──> Ready
///                       │                │              │
///                       │                │              └──> VerificationFailed
///                       │                └──────────────────> Cancelled
///                       └────────────────────────────────────> Disabled
///
/// Ready ──> UpdateAvailable ──> Downloading (upgrade) ──> Verifying ──> Ready
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ModelState {
    /// Model not installed on disk.
    NotInstalled,
    /// User consent required before download.
    NeedsConsent,
    /// Download in progress.
    Downloading {
        /// Progress percentage (0-100).
        progress_pct: u8,
        /// Bytes downloaded so far.
        bytes_downloaded: u64,
        /// Total bytes to download.
        total_bytes: u64,
    },
    /// Verifying downloaded files.
    Verifying,
    /// Model ready for use.
    Ready,
    /// Model disabled by user or policy.
    Disabled { reason: String },
    /// Verification failed after download.
    VerificationFailed { reason: String },
    /// New model version available.
    UpdateAvailable {
        /// Currently installed revision.
        current_revision: String,
        /// Latest available revision.
        latest_revision: String,
    },
    /// Download was cancelled.
    Cancelled,
}

impl ModelState {
    /// Whether the model is ready for use.
    pub fn is_ready(&self) -> bool {
        matches!(self, ModelState::Ready)
    }

    /// Whether a download is in progress.
    pub fn is_downloading(&self) -> bool {
        matches!(self, ModelState::Downloading { .. })
    }

    /// Whether user consent is needed.
    pub fn needs_consent(&self) -> bool {
        matches!(self, ModelState::NeedsConsent)
    }

    /// Human-readable summary of the state.
    pub fn summary(&self) -> String {
        match self {
            ModelState::NotInstalled => "not installed".into(),
            ModelState::NeedsConsent => "needs consent".into(),
            ModelState::Downloading { progress_pct, .. } => {
                format!("downloading ({progress_pct}%)")
            }
            ModelState::Verifying => "verifying".into(),
            ModelState::Ready => "ready".into(),
            ModelState::Disabled { reason } => format!("disabled: {reason}"),
            ModelState::VerificationFailed { reason } => format!("verification failed: {reason}"),
            ModelState::UpdateAvailable {
                current_revision,
                latest_revision,
            } => {
                format!("update available: {current_revision} -> {latest_revision}")
            }
            ModelState::Cancelled => "cancelled".into(),
        }
    }
}

/// A file in the model manifest.
#[derive(Debug, Clone)]
pub struct ModelFile {
    /// File name (e.g., "model.onnx").
    pub name: String,
    /// Expected SHA256 hash (hex string).
    pub sha256: String,
    /// Expected file size in bytes.
    pub size: u64,
}

/// Model manifest describing a downloadable model.
#[derive(Debug, Clone)]
pub struct ModelManifest {
    /// Model identifier (e.g., "all-minilm-l6-v2").
    pub id: String,
    /// HuggingFace repository.
    pub repo: String,
    /// Pinned revision (commit SHA).
    pub revision: String,
    /// Files to download.
    pub files: Vec<ModelFile>,
    /// License identifier.
    pub license: String,
}

impl ModelManifest {
    /// Get the default MiniLM model manifest.
    ///
    /// The revision and checksums are pinned for reproducibility.
    pub fn minilm_v2() -> Self {
        Self {
            id: "all-minilm-l6-v2".into(),
            repo: "sentence-transformers/all-MiniLM-L6-v2".into(),
            // Pinned revision for reproducibility
            revision: "e4ce9877abf3edfe10b0d82785e83bdcb973e22e".into(),
            files: vec![
                ModelFile {
                    name: "model.onnx".into(),
                    sha256: "af9eceaf5d8a75d882c9cb8ba36c693a36bd41cf57ffe0adac38daa59bdf4bca"
                        .into(),
                    size: 22713856,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "eb1de459c8d47e0fb1bd6ef7e98d9cfcd7a50a8b1bca8f631b21f0ed7c5b2bde"
                        .into(),
                    size: 711396,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "89d6e23cd85b1d8cbc63c7a5cee4eb7b2df8e09dcf89eed39b0d6b84bd8dfe88"
                        .into(),
                    size: 612,
                },
                // Note: special_tokens_map.json and tokenizer_config.json are optional
                // metadata files that FastEmbed can work without. We only require the
                // three core files: model.onnx, tokenizer.json, and config.json.
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// Total size of all files in bytes.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// HuggingFace download URL for a file.
    pub fn download_url(&self, file: &ModelFile) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, file.name
        )
    }
}

/// Progress callback for downloads.
pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync>;

/// Download progress information.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    /// Current file being downloaded.
    pub current_file: String,
    /// File index (1-based).
    pub file_index: usize,
    /// Total number of files.
    pub total_files: usize,
    /// Bytes downloaded for current file.
    pub file_bytes: u64,
    /// Total bytes for current file.
    pub file_total: u64,
    /// Total bytes downloaded across all files.
    pub total_bytes: u64,
    /// Total bytes to download across all files.
    pub grand_total: u64,
    /// Overall progress percentage (0-100).
    pub progress_pct: u8,
}

/// Download error types.
#[derive(Debug)]
pub enum DownloadError {
    /// Network error during download.
    NetworkError(String),
    /// File I/O error.
    IoError(std::io::Error),
    /// SHA256 verification failed.
    VerificationFailed {
        file: String,
        expected: String,
        actual: String,
    },
    /// Download was cancelled.
    Cancelled,
    /// Timeout during download.
    Timeout,
    /// HTTP error response.
    HttpError { status: u16, message: String },
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DownloadError::NetworkError(msg) => write!(f, "network error: {msg}"),
            DownloadError::IoError(err) => write!(f, "I/O error: {err}"),
            DownloadError::VerificationFailed {
                file,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "verification failed for {file}: expected {expected}, got {actual}"
                )
            }
            DownloadError::Cancelled => write!(f, "download cancelled"),
            DownloadError::Timeout => write!(f, "download timed out"),
            DownloadError::HttpError { status, message } => {
                write!(f, "HTTP error {status}: {message}")
            }
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DownloadError::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(err: std::io::Error) -> Self {
        DownloadError::IoError(err)
    }
}

/// Model downloader with resumption and verification.
pub struct ModelDownloader {
    /// Target directory for model files.
    target_dir: PathBuf,
    /// Temporary download directory.
    temp_dir: PathBuf,
    /// Cancellation flag.
    cancelled: Arc<AtomicBool>,
    /// Connection timeout.
    connect_timeout: Duration,
    /// Per-file timeout.
    file_timeout: Duration,
    /// Maximum retries per file.
    max_retries: u32,
}

impl ModelDownloader {
    /// Create a new model downloader.
    pub fn new(target_dir: PathBuf) -> Self {
        let temp_dir = target_dir.with_extension("downloading");
        Self {
            target_dir,
            temp_dir,
            cancelled: Arc::new(AtomicBool::new(false)),
            connect_timeout: Duration::from_secs(30),
            file_timeout: Duration::from_secs(300), // 5 minutes per file
            max_retries: 3,
        }
    }

    /// Get a cancellation handle.
    pub fn cancellation_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// Cancel the download.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check if download was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Download and install a model.
    ///
    /// This function:
    /// 1. Creates a temporary download directory
    /// 2. Downloads each file with resume support
    /// 3. Verifies SHA256 checksums
    /// 4. Atomically moves to target directory
    ///
    /// # Arguments
    ///
    /// * `manifest` - Model manifest with file checksums
    /// * `on_progress` - Progress callback (called frequently)
    ///
    /// # Errors
    ///
    /// Returns `DownloadError` if download fails.
    pub fn download(
        &self,
        manifest: &ModelManifest,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        // Reset cancellation flag
        self.cancelled.store(false, Ordering::SeqCst);

        // Create temp directory
        fs::create_dir_all(&self.temp_dir)?;

        let grand_total = manifest.total_size();
        let total_files = manifest.files.len();
        let bytes_downloaded = Arc::new(AtomicU64::new(0));

        for (idx, file) in manifest.files.iter().enumerate() {
            if self.is_cancelled() {
                self.cleanup_temp();
                return Err(DownloadError::Cancelled);
            }

            let file_path = self.temp_dir.join(&file.name);
            let url = manifest.download_url(file);

            // Track bytes_downloaded at start of this file to reset on retry
            let bytes_before_file = bytes_downloaded.load(Ordering::SeqCst);

            // Download with retries
            let mut last_error = None;
            for attempt in 0..self.max_retries {
                if self.is_cancelled() {
                    self.cleanup_temp();
                    return Err(DownloadError::Cancelled);
                }

                // Reset byte counter to before this file on retry (avoid double-counting)
                if attempt > 0 {
                    bytes_downloaded.store(bytes_before_file, Ordering::SeqCst);
                }

                // Exponential backoff delay (except first attempt)
                if attempt > 0 {
                    let delay = Duration::from_secs(5 * (1 << (attempt - 1)));
                    std::thread::sleep(delay);
                }

                match self.download_file(
                    &url,
                    &file_path,
                    file.size,
                    idx,
                    total_files,
                    &bytes_downloaded,
                    grand_total,
                    on_progress.as_ref(),
                ) {
                    Ok(()) => {
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                    }
                }
            }

            if let Some(err) = last_error {
                self.cleanup_temp();
                return Err(err);
            }

            // Verify SHA256
            if self.is_cancelled() {
                self.cleanup_temp();
                return Err(DownloadError::Cancelled);
            }

            let actual_hash = compute_sha256(&file_path)?;
            if actual_hash != file.sha256 {
                self.cleanup_temp();
                return Err(DownloadError::VerificationFailed {
                    file: file.name.clone(),
                    expected: file.sha256.clone(),
                    actual: actual_hash,
                });
            }
        }

        // Atomic install: rename temp -> target
        self.atomic_install()?;

        // Write verified marker
        self.write_verified_marker(manifest)?;

        Ok(())
    }

    /// Download a single file with resume support.
    #[allow(clippy::too_many_arguments)]
    fn download_file(
        &self,
        url: &str,
        path: &Path,
        expected_size: u64,
        file_idx: usize,
        total_files: usize,
        bytes_downloaded: &Arc<AtomicU64>,
        grand_total: u64,
        on_progress: Option<&ProgressCallback>,
    ) -> Result<(), DownloadError> {
        // Check for existing partial download
        let existing_size = if path.exists() {
            fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        // If already complete, skip download
        if existing_size == expected_size {
            bytes_downloaded.fetch_add(expected_size, Ordering::SeqCst);
            return Ok(());
        }

        // Build request with Range header for resume
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(self.connect_timeout)
            .timeout(self.file_timeout)
            .build()
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

        let mut request = client.get(url);

        // Resume from existing size
        if existing_size > 0 {
            request = request.header("Range", format!("bytes={}-", existing_size));
            bytes_downloaded.fetch_add(existing_size, Ordering::SeqCst);
        }

        let response = request
            .send()
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

        let status = response.status().as_u16();
        if status >= 400 {
            return Err(DownloadError::HttpError {
                status,
                message: response.status().to_string(),
            });
        }

        // Open file in append or create mode
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(existing_size > 0)
            .write(true)
            .truncate(existing_size == 0)
            .open(path)?;

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Stream download with progress updates
        let mut reader = BufReader::new(response);
        let mut buffer = [0u8; 8192];
        let start = Instant::now();

        loop {
            if self.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }

            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }

            file.write_all(&buffer[..n])?;
            bytes_downloaded.fetch_add(n as u64, Ordering::SeqCst);

            // Report progress
            if let Some(callback) = on_progress {
                let total_downloaded = bytes_downloaded.load(Ordering::SeqCst);
                let file_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

                let progress_pct = if grand_total > 0 {
                    ((total_downloaded as f64 / grand_total as f64) * 100.0) as u8
                } else {
                    0
                };

                callback(DownloadProgress {
                    current_file: file_name.clone(),
                    file_index: file_idx + 1,
                    total_files,
                    file_bytes,
                    file_total: expected_size,
                    total_bytes: total_downloaded,
                    grand_total,
                    progress_pct,
                });
            }

            // Check timeout
            if start.elapsed() > self.file_timeout {
                return Err(DownloadError::Timeout);
            }
        }

        file.sync_all()?;
        Ok(())
    }

    /// Atomically install downloaded files.
    fn atomic_install(&self) -> Result<(), DownloadError> {
        // Remove existing target if present
        if self.target_dir.exists() {
            fs::remove_dir_all(&self.target_dir)?;
        }

        // Atomic rename
        fs::rename(&self.temp_dir, &self.target_dir)?;

        // Sync directory
        if let Some(parent) = self.target_dir.parent()
            && let Ok(dir) = File::open(parent)
        {
            let _ = dir.sync_all();
        }

        Ok(())
    }

    /// Write .verified marker file.
    fn write_verified_marker(&self, manifest: &ModelManifest) -> Result<(), DownloadError> {
        let marker_path = self.target_dir.join(".verified");
        let content = format!(
            "revision={}\nverified_at={}\n",
            manifest.revision,
            chrono::Utc::now().to_rfc3339()
        );
        fs::write(marker_path, content)?;
        Ok(())
    }

    /// Clean up temporary download directory.
    fn cleanup_temp(&self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

/// Compute SHA256 hash of a file.
pub fn compute_sha256(path: &Path) -> Result<String, DownloadError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();

    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Check if a model is installed and verified.
pub fn check_model_installed(model_dir: &Path) -> ModelState {
    if !model_dir.is_dir() {
        return ModelState::NotInstalled;
    }

    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return ModelState::NotInstalled;
    }

    // Check if all required files exist
    let required = ["model.onnx", "tokenizer.json", "config.json"];
    for file in required {
        if !model_dir.join(file).is_file() {
            return ModelState::NotInstalled;
        }
    }

    ModelState::Ready
}

/// Check for model version mismatch.
pub fn check_version_mismatch(model_dir: &Path, manifest: &ModelManifest) -> Option<ModelState> {
    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return None;
    }

    // Read installed revision
    let content = fs::read_to_string(&verified_marker).ok()?;
    let installed_revision = content
        .lines()
        .find(|l| l.starts_with("revision="))
        .map(|l| l.trim_start_matches("revision=").to_string())?;

    if installed_revision != manifest.revision {
        Some(ModelState::UpdateAvailable {
            current_revision: installed_revision,
            latest_revision: manifest.revision.clone(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_state_summary() {
        assert_eq!(ModelState::NotInstalled.summary(), "not installed");
        assert_eq!(ModelState::NeedsConsent.summary(), "needs consent");
        assert_eq!(ModelState::Ready.summary(), "ready");
        assert_eq!(
            ModelState::Downloading {
                progress_pct: 50,
                bytes_downloaded: 1000,
                total_bytes: 2000
            }
            .summary(),
            "downloading (50%)"
        );
    }

    #[test]
    fn test_model_state_is_ready() {
        assert!(ModelState::Ready.is_ready());
        assert!(!ModelState::NotInstalled.is_ready());
        assert!(!ModelState::NeedsConsent.is_ready());
        assert!(!ModelState::Downloading {
            progress_pct: 0,
            bytes_downloaded: 0,
            total_bytes: 0
        }
        .is_ready());
    }

    #[test]
    fn test_model_manifest_total_size() {
        let manifest = ModelManifest::minilm_v2();
        assert!(manifest.total_size() > 20_000_000); // > 20MB
    }

    #[test]
    fn test_model_manifest_download_url() {
        let manifest = ModelManifest::minilm_v2();
        let url = manifest.download_url(&manifest.files[0]);
        assert!(url.contains("huggingface.co"));
        assert!(url.contains("sentence-transformers/all-MiniLM-L6-v2"));
        assert!(url.contains("model.onnx"));
    }

    #[test]
    fn test_check_model_installed_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("nonexistent");
        assert_eq!(check_model_installed(&model_dir), ModelState::NotInstalled);
    }

    #[test]
    fn test_check_model_installed_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"fake").unwrap();
        assert_eq!(check_model_installed(&model_dir), ModelState::NotInstalled);
    }

    #[test]
    fn test_check_model_installed_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"fake").unwrap();
        fs::write(model_dir.join("tokenizer.json"), b"{}").unwrap();
        fs::write(model_dir.join("config.json"), b"{}").unwrap();
        fs::write(model_dir.join(".verified"), "revision=test\n").unwrap();
        assert_eq!(check_model_installed(&model_dir), ModelState::Ready);
    }

    #[test]
    fn test_compute_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, b"hello world").unwrap();
        let hash = compute_sha256(&file_path).unwrap();
        // SHA256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_check_version_mismatch_none() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(
            model_dir.join(".verified"),
            "revision=e4ce9877abf3edfe10b0d82785e83bdcb973e22e\n",
        )
        .unwrap();

        let manifest = ModelManifest::minilm_v2();
        let result = check_version_mismatch(&model_dir, &manifest);
        assert!(result.is_none());
    }

    #[test]
    fn test_check_version_mismatch_found() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join(".verified"), "revision=old_version\n").unwrap();

        let manifest = ModelManifest::minilm_v2();
        let result = check_version_mismatch(&model_dir, &manifest);
        assert!(matches!(result, Some(ModelState::UpdateAvailable { .. })));
    }

    #[test]
    fn test_download_error_display() {
        let err = DownloadError::NetworkError("connection refused".into());
        assert!(err.to_string().contains("network error"));

        let err = DownloadError::VerificationFailed {
            file: "test.onnx".into(),
            expected: "abc".into(),
            actual: "def".into(),
        };
        assert!(err.to_string().contains("verification failed"));
        assert!(err.to_string().contains("test.onnx"));
    }

    #[test]
    fn test_downloader_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));

        assert!(!downloader.is_cancelled());
        downloader.cancel();
        assert!(downloader.is_cancelled());
    }
}
