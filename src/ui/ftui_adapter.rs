//! Thin FrankenTUI adapter for cass UI migration.
//!
//! Centralizes high-frequency imports so the migration can switch internals
//! without touching every call site repeatedly.

pub use ftui::core::geometry::{Rect, Sides, Size};
pub use ftui::layout::{Alignment, Constraint, Direction, Flex, Grid, LayoutSizeHint};
pub use ftui::render::budget::{DegradationLevel, FrameBudgetConfig};
pub use ftui::widgets::{StatefulWidget, Widget};
pub use ftui::{
    App, Cmd, Event, Frame, KeyCode, KeyEvent, Model, Modifiers, Program, RuntimeDiffConfig,
    ScreenMode, SessionOptions, Style, TerminalSession, TerminalWriter, Theme, UiAnchor,
};
