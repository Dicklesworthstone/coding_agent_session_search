//! Shared analytics query library.
//!
//! Extracts SQL, bucketing, and derived-metric logic from CLI commands into a
//! reusable module consumed by both `cass analytics *` CLI commands and the
//! FrankenTUI analytics dashboards.
//!
//! # Module structure
//!
//! - [`types`] — filter, grouping, result structs, error type
//! - [`bucketing`] — day_id / hour_id ↔ ISO date conversions
//! - [`derive`] — safe derived-metric computation
//! - [`query`] — SQL query builders against rollup tables
//! - [`validate`] — rollup invariant checks, drift detection, perf guardrails

pub mod bucketing;
pub mod derive;
pub mod query;
pub mod types;
pub mod validate;

// Re-export the most commonly used items at the crate::analytics level.
pub use types::{
    AnalyticsError, AnalyticsFilter, AnalyticsResult, BreakdownResult, BreakdownRow, CoverageInfo,
    DerivedMetrics, Dim, DriftInfo, DriftSignal, GroupBy, Metric, SourceFilter, StatusResult,
    TableInfo, TimeseriesResult, ToolReport, ToolRow, UnpricedModel, UnpricedModelsReport,
    UsageBucket,
};
pub use validate::{ValidateConfig, ValidationReport};
