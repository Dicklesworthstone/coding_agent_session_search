//! FTUI application surface and supporting UI modules.
//!
//! [`app`] is the canonical runtime entrypoint. [`tui`] is a retained
//! comment-only legacy shell kept in-tree by policy until file deletion is
//! explicitly authorized.
pub mod analytics_charts;
pub mod app;
pub mod components;
pub mod data;
pub mod ftui_adapter;
pub mod ranking;
pub mod shortcuts;
pub mod style_system;
pub mod theme;
pub mod time_parser;
pub mod trace;
pub mod tui;

#[cfg(test)]
mod legacy_shell_tests {
    use super::app::CassApp;
    use super::ftui_adapter::Rect;
    use super::theme::CassTheme;

    #[test]
    fn canonical_ui_runtime_types_live_outside_legacy_tui_shell() {
        let _ = std::mem::size_of::<CassApp>();
        let _ = std::mem::size_of::<CassTheme>();
        let _ = std::mem::size_of::<Rect>();
    }
}

/// Structured test logging for unit/E2E scenario diagnostics (2dccg.11.6).
///
/// Provides a lightweight, in-crate test logger with JSON-structured events
/// so that any test failure includes enough context to diagnose without rerunning.
///
/// Schema version: 1 (stable, backwards-compatible additions only).
#[cfg(test)]
pub mod test_log {
    use std::cell::RefCell;
    use std::time::Instant;

    /// Schema version for structured test log events.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Category of test event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Category {
        Style,
        Render,
        Interaction,
        Degradation,
        Theme,
        Layout,
    }

    impl Category {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Style => "style",
                Self::Render => "render",
                Self::Interaction => "interaction",
                Self::Degradation => "degradation",
                Self::Theme => "theme",
                Self::Layout => "layout",
            }
        }
    }

    /// Kind of test event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Event {
        AssertPass,
        AssertFail,
        StepStart,
        StepEnd,
        StateSnapshot,
    }

    impl Event {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::AssertPass => "assert_pass",
                Self::AssertFail => "assert_fail",
                Self::StepStart => "step_start",
                Self::StepEnd => "step_end",
                Self::StateSnapshot => "state_snapshot",
            }
        }
    }

    /// A single structured test log entry.
    #[derive(Debug, Clone)]
    pub struct LogEntry {
        pub test_id: String,
        pub elapsed_us: u64,
        pub category: Category,
        pub event: Event,
        pub detail: String,
    }

    impl LogEntry {
        /// Serialize to a single-line JSON string.
        pub fn to_json(&self) -> String {
            format!(
                r#"{{"schema_v":{},"test_id":"{}","elapsed_us":{},"category":"{}","event":"{}","detail":{}}}"#,
                SCHEMA_VERSION,
                self.test_id.replace('"', r#"\""#),
                self.elapsed_us,
                self.category.as_str(),
                self.event.as_str(),
                self.detail,
            )
        }
    }

    /// Lightweight per-test structured logger.
    ///
    /// Collects events in memory; on failure, emits them as a diagnostic dump.
    /// Zero-cost when tests pass and output is not captured.
    pub struct TestLogger {
        test_id: String,
        start: Instant,
        entries: RefCell<Vec<LogEntry>>,
    }

    impl TestLogger {
        /// Create a new logger for the given test scenario.
        pub fn new(test_id: impl Into<String>) -> Self {
            Self {
                test_id: test_id.into(),
                start: Instant::now(),
                entries: RefCell::new(Vec::new()),
            }
        }

        /// Log a structured event.
        pub fn log(&self, category: Category, event: Event, detail: impl Into<String>) {
            let elapsed_us = self.start.elapsed().as_micros() as u64;
            self.entries.borrow_mut().push(LogEntry {
                test_id: self.test_id.clone(),
                elapsed_us,
                category,
                event,
                detail: detail.into(),
            });
        }

        /// Log an assertion pass.
        pub fn pass(&self, category: Category, detail: impl Into<String>) {
            self.log(category, Event::AssertPass, detail);
        }

        /// Log an assertion failure (call before the actual assert! so the log is captured).
        pub fn fail(&self, category: Category, detail: impl Into<String>) {
            self.log(category, Event::AssertFail, detail);
        }

        /// Log a step start.
        pub fn step_start(&self, category: Category, detail: impl Into<String>) {
            self.log(category, Event::StepStart, detail);
        }

        /// Log a step end.
        pub fn step_end(&self, category: Category, detail: impl Into<String>) {
            self.log(category, Event::StepEnd, detail);
        }

        /// Emit a state snapshot (theme, degradation, viewport, etc.).
        pub fn snapshot(&self, category: Category, detail: impl Into<String>) {
            self.log(category, Event::StateSnapshot, detail);
        }

        /// Return all entries as JSONL.
        pub fn to_jsonl(&self) -> String {
            self.entries
                .borrow()
                .iter()
                .map(|e| e.to_json())
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// Return pass/fail/total summary.
        pub fn summary(&self) -> (usize, usize, usize) {
            let entries = self.entries.borrow();
            let pass = entries
                .iter()
                .filter(|e| e.event == Event::AssertPass)
                .count();
            let fail = entries
                .iter()
                .filter(|e| e.event == Event::AssertFail)
                .count();
            (pass, fail, entries.len())
        }

        /// Dump all events to stderr (useful on test failure).
        pub fn dump_on_failure(&self) {
            let (pass, fail, total) = self.summary();
            if fail > 0 {
                eprintln!(
                    "--- TestLogger dump for '{}' ({} pass, {} fail, {} total) ---",
                    self.test_id, pass, fail, total
                );
                eprintln!("{}", self.to_jsonl());
                eprintln!("--- end dump ---");
            }
        }
    }

    impl Drop for TestLogger {
        fn drop(&mut self) {
            // Auto-dump on panic (test failure)
            if std::thread::panicking() {
                let (pass, fail, total) = self.summary();
                eprintln!(
                    "\n--- TestLogger auto-dump for '{}' ({} pass, {} fail, {} total) ---",
                    self.test_id, pass, fail, total
                );
                eprintln!("{}", self.to_jsonl());
                eprintln!("--- end auto-dump ---\n");
            }
        }
    }

    /// Assert two styles are equal, logging pass/fail with full context.
    #[macro_export]
    macro_rules! assert_style_eq {
        ($logger:expr, $left:expr, $right:expr, $category:expr, $msg:expr) => {{
            let left_val = &$left;
            let right_val = &$right;
            if left_val == right_val {
                $logger.pass($category, format!(r#""{}""#, $msg));
            } else {
                $logger.fail(
                    $category,
                    format!(
                        r#"{{"msg":"{}","left":"{:?}","right":"{:?}"}}"#,
                        $msg, left_val, right_val
                    ),
                );
                panic!(
                    "assert_style_eq failed: {}\n  left: {:?}\n  right: {:?}",
                    $msg, left_val, right_val
                );
            }
        }};
    }

    /// Assert a condition, logging pass/fail with context.
    #[macro_export]
    macro_rules! assert_logged {
        ($logger:expr, $cond:expr, $category:expr, $msg:expr) => {{
            if $cond {
                $logger.pass($category, format!(r#""{}""#, $msg));
            } else {
                $logger.fail(
                    $category,
                    format!(r#"{{"msg":"{}","condition":"false"}}"#, $msg),
                );
                panic!("assert_logged failed: {}", $msg);
            }
        }};
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_logger_basic_lifecycle() {
            let log = TestLogger::new("test_logger_basic");
            log.step_start(Category::Style, r#""begin style check""#.to_string());
            log.pass(Category::Style, r#""token resolved""#.to_string());
            log.step_end(Category::Style, r#""style check done""#.to_string());

            let (pass, fail, total) = log.summary();
            assert_eq!(pass, 1);
            assert_eq!(fail, 0);
            assert_eq!(total, 3);
        }

        #[test]
        fn test_logger_jsonl_output() {
            let log = TestLogger::new("jsonl_test");
            log.pass(Category::Render, r#""rendered ok""#.to_string());
            let jsonl = log.to_jsonl();
            assert!(jsonl.contains(r#""schema_v":1"#));
            assert!(jsonl.contains(r#""test_id":"jsonl_test""#));
            assert!(jsonl.contains(r#""category":"render""#));
            assert!(jsonl.contains(r#""event":"assert_pass""#));
        }

        #[test]
        fn test_logger_summary_counts_correctly() {
            let log = TestLogger::new("summary_test");
            log.pass(Category::Style, r#""a""#.to_string());
            log.pass(Category::Theme, r#""b""#.to_string());
            log.fail(Category::Degradation, r#""c""#.to_string());
            log.snapshot(Category::Layout, r#""d""#.to_string());

            let (pass, fail, total) = log.summary();
            assert_eq!(pass, 2);
            assert_eq!(fail, 1);
            assert_eq!(total, 4);
        }

        #[test]
        fn test_logger_schema_version_stable() {
            assert_eq!(
                SCHEMA_VERSION, 1,
                "schema version must not change without migration"
            );
        }

        #[test]
        fn category_all_variants_have_str() {
            let cats = [
                Category::Style,
                Category::Render,
                Category::Interaction,
                Category::Degradation,
                Category::Theme,
                Category::Layout,
            ];
            for cat in cats {
                assert!(!cat.as_str().is_empty());
            }
        }

        #[test]
        fn event_all_variants_have_str() {
            let events = [
                Event::AssertPass,
                Event::AssertFail,
                Event::StepStart,
                Event::StepEnd,
                Event::StateSnapshot,
            ];
            for ev in events {
                assert!(!ev.as_str().is_empty());
            }
        }

        #[test]
        fn assert_style_eq_macro_passes() {
            let log = TestLogger::new("macro_test");
            let a = 42u32;
            let b = 42u32;
            assert_style_eq!(log, a, b, Category::Style, "values should match");
            let (pass, _, _) = log.summary();
            assert_eq!(pass, 1);
        }

        #[test]
        fn assert_logged_macro_passes() {
            let log = TestLogger::new("logged_test");
            assert_logged!(log, true, Category::Render, "condition holds");
            let (pass, _, _) = log.summary();
            assert_eq!(pass, 1);
        }
    }
}
