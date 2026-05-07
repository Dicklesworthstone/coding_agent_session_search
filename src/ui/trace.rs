//! Render trace + time-travel capture for ftui TUI (bead 2noh9.4.3).
//!
//! Records frame snapshots, event streams, and render timing so that TUI bugs
//! can be reproduced from a trace bundle without rerunning on the original
//! machine.
//!
//! # Formats
//!
//! - **Render trace** (`.trace.jsonl`): one JSON object per frame with timing,
//!   size, message that triggered the render, and optional text snapshot.
//! - **Event stream** (`.events.jsonl`): one JSON object per `CassMsg` with
//!   timestamp and serialized variant tag.
//! - **Trace bundle** (directory): render trace + event stream + `tui_state.json`
//!   + `system_info.json`.

use std::io::{Error, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

// =========================================================================
// Trace record types
// =========================================================================

/// One frame's render metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameRecord {
    /// Monotonic frame index (0-based).
    pub frame_index: u64,
    /// Wall-clock timestamp (millis since Unix epoch).
    pub timestamp_ms: u64,
    /// Duration of the `view()` call in microseconds.
    pub render_us: u64,
    /// Terminal width at render time.
    pub width: u16,
    /// Terminal height at render time.
    pub height: u16,
    /// Human-readable label of the message that triggered this render, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// Plain-text snapshot of the buffer (optional, can be large).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_snapshot: Option<String>,
}

/// One event's metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    /// Wall-clock timestamp (millis since Unix epoch).
    pub timestamp_ms: u64,
    /// Monotonic event index (0-based).
    pub event_index: u64,
    /// CassMsg variant tag (e.g. "QueryChanged", "SearchRequested").
    pub msg_tag: String,
    /// Optional details (e.g. the query text for QueryChanged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// System information snapshot for trace bundles.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub arch: String,
    pub cass_version: String,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub terminal_size: Option<(u16, u16)>,
    pub timestamp: String,
}

impl SystemInfo {
    /// Capture current system info.
    pub fn capture() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cass_version: env!("CARGO_PKG_VERSION").to_string(),
            term: dotenvy::var("TERM").ok(),
            colorterm: dotenvy::var("COLORTERM").ok(),
            terminal_size: None, // filled by caller if available
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// =========================================================================
// Trace writer
// =========================================================================

/// Appends frame and event records to JSONL files.
pub struct TraceWriter {
    render_file: Option<std::io::BufWriter<std::fs::File>>,
    events_file: Option<std::io::BufWriter<std::fs::File>>,
    frame_count: u64,
    event_count: u64,
    _epoch: Instant,
}

impl TraceWriter {
    /// Open a trace writer.  Pass `None` for paths you don't want to record.
    pub fn open(render_path: Option<&Path>, events_path: Option<&Path>) -> std::io::Result<Self> {
        if let (Some(render_path), Some(events_path)) = (render_path, events_path)
            && render_path == events_path
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "render and event trace outputs must use distinct paths: {}",
                    render_path.display()
                ),
            ));
        }

        if let Some(path) = render_path {
            ensure_trace_output_available(path)?;
        }
        if let Some(path) = events_path {
            ensure_trace_output_available(path)?;
        }

        let render_file = render_path.map(create_trace_output).transpose()?;
        let events_file = events_path.map(create_trace_output).transpose()?;
        Ok(Self {
            render_file,
            events_file,
            frame_count: 0,
            event_count: 0,
            _epoch: Instant::now(),
        })
    }

    /// Record a rendered frame.
    pub fn record_frame(
        &mut self,
        render_duration: Duration,
        width: u16,
        height: u16,
        trigger: Option<&str>,
        text_snapshot: Option<String>,
    ) -> std::io::Result<()> {
        if let Some(ref mut f) = self.render_file {
            let record = FrameRecord {
                frame_index: self.frame_count,
                timestamp_ms: wall_millis(),
                render_us: render_duration.as_micros() as u64,
                width,
                height,
                trigger: trigger.map(|s| s.to_string()),
                text_snapshot,
            };
            serde_json::to_writer(&mut *f, &record)?;
            f.write_all(b"\n")?;
            self.frame_count += 1;
        }
        Ok(())
    }

    /// Record an event (message).
    pub fn record_event(&mut self, msg_tag: &str, detail: Option<&str>) -> std::io::Result<()> {
        if let Some(ref mut f) = self.events_file {
            let record = EventRecord {
                timestamp_ms: wall_millis(),
                event_index: self.event_count,
                msg_tag: msg_tag.to_string(),
                detail: detail.map(|s| s.to_string()),
            };
            serde_json::to_writer(&mut *f, &record)?;
            f.write_all(b"\n")?;
            self.event_count += 1;
        }
        Ok(())
    }

    /// Flush both files.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let Some(ref mut f) = self.render_file {
            f.flush()?;
        }
        if let Some(ref mut f) = self.events_file {
            f.flush()?;
        }
        Ok(())
    }

    /// Number of frames recorded.
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Number of events recorded.
    pub fn event_count(&self) -> u64 {
        self.event_count
    }

    /// Whether any recording is active.
    pub fn is_active(&self) -> bool {
        self.render_file.is_some() || self.events_file.is_some()
    }
}

fn ensure_trace_output_available(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                format!("trace output already exists: {}", path.display()),
            ));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    Ok(())
}

fn create_trace_output(path: &Path) -> std::io::Result<std::io::BufWriter<std::fs::File>> {
    ensure_trace_output_available(path)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    Ok(std::io::BufWriter::new(file))
}

impl Drop for TraceWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

// =========================================================================
// Trace bundle
// =========================================================================

/// Write a complete trace bundle directory containing:
/// - `render.trace.jsonl`  (if render_records is non-empty)
/// - `events.jsonl`        (if event_records is non-empty)
/// - `system_info.json`
/// - `tui_state.json`      (if state bytes are provided)
pub fn write_trace_bundle(
    bundle_dir: &Path,
    system_info: &SystemInfo,
    tui_state_json: Option<&str>,
) -> std::io::Result<()> {
    ensure_trace_bundle_dir(bundle_dir)?;

    let sys_path = bundle_dir.join("system_info.json");
    let state_path = tui_state_json.map(|_| bundle_dir.join("tui_state.json"));
    ensure_trace_output_available(&sys_path)?;
    if let Some(path) = &state_path {
        ensure_trace_output_available(path)?;
    }

    // System info
    let mut sys_file = create_trace_output(&sys_path)?;
    serde_json::to_writer_pretty(&mut sys_file, system_info)?;

    // TUI state
    if let (Some(state), Some(state_path)) = (tui_state_json, state_path) {
        let mut state_file = create_trace_output(&state_path)?;
        state_file.write_all(state.as_bytes())?;
    }

    Ok(())
}

fn ensure_trace_bundle_dir(bundle_dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(bundle_dir) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "trace bundle directory must not be a symlink: {}",
                        bundle_dir.display()
                    ),
                ));
            }
            if !file_type.is_dir() {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "trace bundle path must be a directory: {}",
                        bundle_dir.display()
                    ),
                ));
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(bundle_dir)?;
            ensure_trace_bundle_dir(bundle_dir)
        }
        Err(err) => Err(err),
    }
}

// =========================================================================
// Trace reader (for replay / inspection)
// =========================================================================

/// Read a JSONL render trace file and return parsed records.
pub fn read_render_trace(path: &Path) -> std::io::Result<Vec<FrameRecord>> {
    let content = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: FrameRecord = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid frame record: {e}"),
            )
        })?;
        records.push(record);
    }
    Ok(records)
}

/// Read a JSONL event stream file and return parsed records.
pub fn read_event_stream(path: &Path) -> std::io::Result<Vec<EventRecord>> {
    let content = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: EventRecord = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid event record: {e}"),
            )
        })?;
        records.push(record);
    }
    Ok(records)
}

// =========================================================================
// Trace options (parsed from CLI)
// =========================================================================

/// Options controlling trace capture, parsed from CLI flags.
#[derive(Clone, Debug, Default)]
pub struct TraceOptions {
    /// Path for render trace JSONL output.
    pub render_path: Option<PathBuf>,
    /// Path for event stream JSONL output.
    pub events_path: Option<PathBuf>,
    /// Path for a full trace bundle directory.
    pub bundle_dir: Option<PathBuf>,
    /// Whether to include text snapshots in render trace (large output).
    pub include_snapshots: bool,
}

impl TraceOptions {
    /// Whether any tracing is requested.
    pub fn is_active(&self) -> bool {
        self.render_path.is_some() || self.events_path.is_some() || self.bundle_dir.is_some()
    }

    /// Create a TraceWriter from these options.  If bundle_dir is set,
    /// render and event paths default to files inside the bundle dir.
    pub fn into_writer(&self) -> std::io::Result<TraceWriter> {
        let (render_path, events_path) = if let Some(ref dir) = self.bundle_dir {
            std::fs::create_dir_all(dir)?;
            (
                self.render_path
                    .clone()
                    .unwrap_or_else(|| dir.join("render.trace.jsonl")),
                self.events_path
                    .clone()
                    .unwrap_or_else(|| dir.join("events.jsonl")),
            )
        } else {
            (
                self.render_path.clone().unwrap_or_default(),
                self.events_path.clone().unwrap_or_default(),
            )
        };

        let render = if self.render_path.is_some() || self.bundle_dir.is_some() {
            Some(render_path.as_path())
        } else {
            None
        };
        let events = if self.events_path.is_some() || self.bundle_dir.is_some() {
            Some(events_path.as_path())
        } else {
            None
        };

        TraceWriter::open(render, events)
    }
}

// =========================================================================
// Helpers
// =========================================================================

fn wall_millis() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn trace_writer_records_frames_and_events() {
        let tmp = TempDir::new().unwrap();
        let render_path = tmp.path().join("render.trace.jsonl");
        let events_path = tmp.path().join("events.jsonl");

        let mut writer = TraceWriter::open(Some(&render_path), Some(&events_path)).unwrap();
        assert!(writer.is_active());

        writer
            .record_frame(Duration::from_micros(150), 80, 24, Some("init"), None)
            .unwrap();
        writer
            .record_frame(Duration::from_micros(200), 80, 24, Some("Tick"), None)
            .unwrap();
        writer.record_event("QueryChanged", Some("hello")).unwrap();
        writer.record_event("SearchRequested", None).unwrap();
        writer.flush().unwrap();

        assert_eq!(writer.frame_count(), 2);
        assert_eq!(writer.event_count(), 2);

        // Verify readback
        let frames = read_render_trace(&render_path).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame_index, 0);
        assert_eq!(frames[0].trigger.as_deref(), Some("init"));
        assert_eq!(frames[1].frame_index, 1);

        let events = read_event_stream(&events_path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].msg_tag, "QueryChanged");
        assert_eq!(events[0].detail.as_deref(), Some("hello"));
        assert_eq!(events[1].msg_tag, "SearchRequested");
    }

    #[test]
    fn trace_writer_noop_when_no_paths() {
        let mut writer = TraceWriter::open(None, None).unwrap();
        assert!(!writer.is_active());
        // Should silently no-op
        writer
            .record_frame(Duration::from_micros(100), 80, 24, None, None)
            .unwrap();
        writer.record_event("Tick", None).unwrap();
        assert_eq!(writer.frame_count(), 0);
        assert_eq!(writer.event_count(), 0);
    }

    #[test]
    fn trace_writer_with_text_snapshot() {
        let tmp = TempDir::new().unwrap();
        let render_path = tmp.path().join("render.trace.jsonl");

        let mut writer = TraceWriter::open(Some(&render_path), None).unwrap();
        writer
            .record_frame(
                Duration::from_micros(500),
                80,
                24,
                Some("SearchCompleted"),
                Some("╭─ results ─╮\n│ hit 1     │\n╰───────────╯".to_string()),
            )
            .unwrap();
        writer.flush().unwrap();

        let frames = read_render_trace(&render_path).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].text_snapshot.is_some());
        assert!(frames[0].text_snapshot.as_ref().unwrap().contains("hit 1"));
    }

    #[test]
    fn trace_writer_refuses_existing_output_path() {
        let tmp = TempDir::new().unwrap();
        let render_path = tmp.path().join("render.trace.jsonl");
        std::fs::write(&render_path, "existing trace").unwrap();

        let err = match TraceWriter::open(Some(&render_path), None) {
            Ok(_) => panic!("expected existing trace output to be rejected"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&render_path).unwrap(),
            "existing trace"
        );
    }

    #[test]
    fn trace_writer_refuses_existing_event_path_without_creating_render_output() {
        let tmp = TempDir::new().unwrap();
        let render_path = tmp.path().join("render.trace.jsonl");
        let events_path = tmp.path().join("events.trace.jsonl");
        std::fs::write(&events_path, "existing events").unwrap();

        let err = match TraceWriter::open(Some(&render_path), Some(&events_path)) {
            Ok(_) => panic!("expected existing event trace output to be rejected"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert!(
            !render_path.exists(),
            "render trace should not be created when event trace preflight fails"
        );
        assert_eq!(
            std::fs::read_to_string(&events_path).unwrap(),
            "existing events"
        );
    }

    #[test]
    fn trace_writer_refuses_shared_render_and_event_path() {
        let tmp = TempDir::new().unwrap();
        let trace_path = tmp.path().join("trace.jsonl");

        let err = match TraceWriter::open(Some(&trace_path), Some(&trace_path)) {
            Ok(_) => panic!("expected shared trace output path to be rejected"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            !trace_path.exists(),
            "shared-path validation should not create a partial trace file"
        );
    }

    #[test]
    #[cfg(unix)]
    fn trace_writer_refuses_symlinked_output_path() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let protected_path = tmp.path().join("protected.jsonl");
        let trace_path = tmp.path().join("render.trace.jsonl");
        std::fs::write(&protected_path, "do not overwrite").unwrap();
        symlink(&protected_path, &trace_path).unwrap();

        let err = match TraceWriter::open(Some(&trace_path), None) {
            Ok(_) => panic!("expected symlinked trace output to be rejected"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&protected_path).unwrap(),
            "do not overwrite"
        );
        assert!(
            std::fs::symlink_metadata(&trace_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected trace symlink should remain untouched"
        );
    }

    #[test]
    fn write_and_read_trace_bundle() {
        let tmp = TempDir::new().unwrap();
        let bundle_dir = tmp.path().join("bundle");

        let sys_info = SystemInfo::capture();
        write_trace_bundle(&bundle_dir, &sys_info, Some(r#"{"query":"test"}"#)).unwrap();

        assert!(bundle_dir.join("system_info.json").exists());
        assert!(bundle_dir.join("tui_state.json").exists());

        let state = std::fs::read_to_string(bundle_dir.join("tui_state.json")).unwrap();
        assert!(state.contains("test"));
    }

    #[test]
    #[cfg(unix)]
    fn write_trace_bundle_rejects_symlinked_bundle_dir() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside_dir = tmp.path().join("outside");
        let bundle_dir = tmp.path().join("bundle");
        std::fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, &bundle_dir).unwrap();

        let err = write_trace_bundle(&bundle_dir, &SystemInfo::capture(), Some("{}")).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            !outside_dir.join("system_info.json").exists(),
            "trace bundle writer must not follow a symlinked bundle directory"
        );
        assert!(
            std::fs::symlink_metadata(&bundle_dir)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected trace bundle symlink should remain untouched"
        );
    }

    #[test]
    fn write_trace_bundle_refuses_existing_state_without_creating_system_info() {
        let tmp = TempDir::new().unwrap();
        let bundle_dir = tmp.path().join("bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let state_path = bundle_dir.join("tui_state.json");
        std::fs::write(&state_path, "existing state").unwrap();

        let err = write_trace_bundle(&bundle_dir, &SystemInfo::capture(), Some("{}")).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert!(
            !bundle_dir.join("system_info.json").exists(),
            "system_info should not be created when state preflight fails"
        );
        assert_eq!(
            std::fs::read_to_string(&state_path).unwrap(),
            "existing state"
        );
    }

    #[test]
    fn system_info_captures_environment() {
        let info = SystemInfo::capture();
        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert!(!info.cass_version.is_empty());
        assert!(!info.timestamp.is_empty());
    }

    #[test]
    fn trace_options_active_detection() {
        let opts = TraceOptions::default();
        assert!(!opts.is_active());

        let opts = TraceOptions {
            render_path: Some(PathBuf::from("/tmp/test.jsonl")),
            ..Default::default()
        };
        assert!(opts.is_active());

        let opts = TraceOptions {
            bundle_dir: Some(PathBuf::from("/tmp/bundle")),
            ..Default::default()
        };
        assert!(opts.is_active());
    }

    #[test]
    fn trace_options_bundle_creates_default_paths() {
        let tmp = TempDir::new().unwrap();
        let bundle_dir = tmp.path().join("bundle");

        let opts = TraceOptions {
            bundle_dir: Some(bundle_dir.clone()),
            ..Default::default()
        };

        let mut writer = opts.into_writer().unwrap();
        assert!(writer.is_active());
        writer
            .record_frame(Duration::from_micros(100), 80, 24, None, None)
            .unwrap();
        writer.record_event("Tick", None).unwrap();
        writer.flush().unwrap();

        assert!(bundle_dir.join("render.trace.jsonl").exists());
        assert!(bundle_dir.join("events.jsonl").exists());
    }
}
