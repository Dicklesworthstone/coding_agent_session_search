//! Toast notification component for transient user feedback.
//!
//! Provides non-blocking notifications that auto-dismiss after a configurable duration.
//! Supports coalescing of similar messages to prevent notification spam.

use ftui::core::geometry::Rect;
use ftui::render::cell::PackedRgba;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::theme::ThemePalette;

/// Type of toast notification, determines styling and icon
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastType {
    /// Informational message
    Info,
    /// Success/completion message
    Success,
    /// Warning that doesn't block operation
    Warning,
    /// Error that needs attention
    Error,
}

impl ToastType {
    /// Get the icon/prefix for this toast type
    pub fn icon(self) -> &'static str {
        match self {
            Self::Info => "i",
            Self::Success => "*",
            Self::Warning => "!",
            Self::Error => "x",
        }
    }

    /// Get the color for this toast type as a PackedRgba.
    pub fn color(self, palette: &ThemePalette) -> PackedRgba {
        match self {
            Self::Info => palette.accent,
            Self::Success => palette.user,
            Self::Warning => palette.system,
            Self::Error => PackedRgba::rgb(247, 118, 142),
        }
    }

    /// Get default duration for this toast type
    pub fn default_duration(self) -> Duration {
        match self {
            Self::Info => Duration::from_secs(3),
            Self::Success => Duration::from_secs(2),
            Self::Warning => Duration::from_secs(4),
            Self::Error => Duration::from_secs(6), // Errors stay longer
        }
    }
}

/// Position where toasts appear on screen
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastPosition {
    /// Top-right corner (default)
    #[default]
    TopRight,
    /// Top-left corner
    TopLeft,
    /// Bottom-right corner
    BottomRight,
    /// Bottom-left corner
    BottomLeft,
    /// Top-center
    TopCenter,
    /// Bottom-center
    BottomCenter,
}

/// A single toast notification
#[derive(Debug, Clone)]
pub struct Toast {
    /// Unique identifier for coalescing
    pub id: String,
    /// The message to display
    pub message: String,
    /// Type of toast (determines styling)
    pub toast_type: ToastType,
    /// When the toast was created
    pub created_at: Instant,
    /// How long until auto-dismiss
    pub duration: Duration,
    /// Number of coalesced messages (for "x5" badge)
    pub count: usize,
}

impl Toast {
    /// Create a new toast with default duration
    pub fn new(message: impl Into<String>, toast_type: ToastType) -> Self {
        let message = message.into();
        let id = format!("{:?}:{}", toast_type, message);
        Self {
            id,
            message,
            toast_type,
            created_at: Instant::now(),
            duration: toast_type.default_duration(),
            count: 1,
        }
    }

    /// Create a toast with custom duration
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Create a toast with custom ID (for coalescing control)
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Check if this toast has expired
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.duration
    }

    /// Get remaining time as a fraction (0.0 = expired, 1.0 = just created)
    pub fn remaining_fraction(&self) -> f32 {
        let total = self.duration.as_secs_f32();
        if total <= 0.0 {
            return 0.0; // Treat zero/negative duration as immediately expired
        }
        let elapsed = self.created_at.elapsed().as_secs_f32();
        (1.0 - elapsed / total).clamp(0.0, 1.0)
    }

    /// Convenience constructors
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, ToastType::Info)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(message, ToastType::Success)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(message, ToastType::Warning)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, ToastType::Error)
    }
}

/// Manages a collection of toast notifications
#[derive(Debug)]
pub struct ToastManager {
    /// Active toasts (newest first for top-down rendering)
    toasts: VecDeque<Toast>,
    /// Maximum number of visible toasts
    max_visible: usize,
    /// Position on screen
    position: ToastPosition,
    /// Whether to coalesce similar toasts
    coalesce: bool,
}

impl Default for ToastManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastManager {
    /// Create a new toast manager with defaults
    pub fn new() -> Self {
        Self {
            toasts: VecDeque::new(),
            max_visible: 5,
            position: ToastPosition::TopRight,
            coalesce: true,
        }
    }

    /// Set maximum visible toasts
    pub fn with_max_visible(mut self, max: usize) -> Self {
        self.max_visible = max;
        self
    }

    /// Set toast position
    pub fn with_position(mut self, position: ToastPosition) -> Self {
        self.position = position;
        self
    }

    /// Enable/disable coalescing
    pub fn with_coalesce(mut self, coalesce: bool) -> Self {
        self.coalesce = coalesce;
        self
    }

    /// Add a new toast
    pub fn push(&mut self, toast: Toast) {
        // Try to coalesce with existing toast
        if self.coalesce
            && let Some(existing) = self.toasts.iter_mut().find(|t| t.id == toast.id)
        {
            existing.count = existing.count.saturating_add(1);
            existing.created_at = Instant::now(); // Reset timer
            return;
        }

        // Add new toast at front
        self.toasts.push_front(toast);

        // Trim excess
        let retention_limit = self.max_visible.saturating_mul(2);
        while self.toasts.len() > retention_limit {
            self.toasts.pop_back();
        }
    }

    /// Remove expired toasts
    pub fn tick(&mut self) {
        self.toasts.retain(|t| !t.is_expired());
    }

    /// Clear all toasts
    pub fn clear(&mut self) {
        self.toasts.clear();
    }

    /// Dismiss the oldest toast
    pub fn dismiss_oldest(&mut self) {
        self.toasts.pop_back();
    }

    /// Dismiss all toasts of a specific type
    pub fn dismiss_type(&mut self, toast_type: ToastType) {
        self.toasts.retain(|t| t.toast_type != toast_type);
    }

    /// Get visible toasts (limited by `max_visible`)
    pub fn visible(&self) -> impl Iterator<Item = &Toast> {
        self.toasts.iter().take(self.max_visible)
    }

    /// Check if there are any active toasts
    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    /// Get count of active toasts
    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    /// Get the position setting
    pub fn position(&self) -> ToastPosition {
        self.position
    }

    /// Calculate the render area for toasts given the full terminal area
    pub fn render_area(&self, full_area: Rect) -> Rect {
        const HORIZONTAL_MARGIN: u16 = 2;
        const TOAST_ROW_HEIGHT: usize = 3;
        const VERTICAL_MARGIN: u16 = 1;

        let toast_width = 40.min(full_area.width.saturating_sub(4));
        let visible_count = self.visible().count();
        let max_height = full_area.height.saturating_sub(VERTICAL_MARGIN * 2);
        let toast_height = visible_count
            .saturating_mul(TOAST_ROW_HEIGHT)
            .min(usize::from(max_height))
            .try_into()
            .unwrap_or(max_height);

        if toast_width == 0 || toast_height == 0 {
            return Rect::new(full_area.x, full_area.y, 0, 0);
        }

        let x = match self.position {
            ToastPosition::TopLeft | ToastPosition::BottomLeft => {
                full_area.x.saturating_add(HORIZONTAL_MARGIN)
            }
            ToastPosition::TopRight | ToastPosition::BottomRight => full_area
                .right()
                .saturating_sub(toast_width.saturating_add(HORIZONTAL_MARGIN)),
            ToastPosition::TopCenter | ToastPosition::BottomCenter => full_area
                .x
                .saturating_add(full_area.width.saturating_sub(toast_width) / 2),
        };

        let y = match self.position {
            ToastPosition::TopLeft | ToastPosition::TopRight | ToastPosition::TopCenter => {
                full_area.y.saturating_add(VERTICAL_MARGIN)
            }
            ToastPosition::BottomLeft
            | ToastPosition::BottomRight
            | ToastPosition::BottomCenter => full_area
                .bottom()
                .saturating_sub(toast_height.saturating_add(VERTICAL_MARGIN)),
        };

        Rect::new(x, y, toast_width, toast_height)
    }
}

#[cfg(test)]
mod tests {
    use super::{Toast, ToastManager, ToastPosition, ToastType};
    use ftui::core::geometry::Rect;
    use std::collections::VecDeque;
    use std::time::Duration;

    #[test]
    fn test_toast_creation() {
        let toast = Toast::info("Test message");
        assert_eq!(toast.message, "Test message");
        assert_eq!(toast.toast_type, ToastType::Info);
        assert_eq!(toast.count, 1);
    }

    #[test]
    fn test_toast_type_defaults() {
        assert_eq!(ToastType::Info.default_duration(), Duration::from_secs(3));
        assert_eq!(ToastType::Error.default_duration(), Duration::from_secs(6));
    }

    #[test]
    fn test_toast_manager_push() {
        let mut manager = ToastManager::new();
        manager.push(Toast::info("First"));
        manager.push(Toast::success("Second"));
        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn test_toast_coalescing() {
        let mut manager = ToastManager::new().with_coalesce(true);
        manager.push(Toast::info("Same message"));
        manager.push(Toast::info("Same message"));
        manager.push(Toast::info("Same message"));

        assert_eq!(manager.len(), 1);
        assert_eq!(manager.visible().next().unwrap().count, 3);
    }

    #[test]
    fn test_toast_coalesced_count_saturates() {
        let mut manager = ToastManager::new().with_coalesce(true);
        manager.push(Toast::info("Same message"));
        manager.toasts.front_mut().unwrap().count = usize::MAX;

        manager.push(Toast::info("Same message"));

        assert_eq!(manager.len(), 1);
        assert_eq!(manager.visible().next().unwrap().count, usize::MAX);
    }

    #[test]
    fn test_toast_coalescing_disabled() {
        let mut manager = ToastManager::new().with_coalesce(false);
        manager.push(Toast::info("Same message"));
        manager.push(Toast::info("Same message"));

        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn test_toast_position() {
        let manager = ToastManager::new().with_position(ToastPosition::BottomLeft);
        assert_eq!(manager.position(), ToastPosition::BottomLeft);
    }

    #[test]
    fn test_toast_retention_limit_saturates() {
        let mut manager = ToastManager::new()
            .with_max_visible(usize::MAX)
            .with_coalesce(false);

        manager.push(Toast::info("First"));
        manager.push(Toast::info("Second"));

        assert_eq!(manager.len(), 2);
    }

    #[test]
    fn test_render_area_respects_full_area_origin() {
        let mut manager = ToastManager::new().with_position(ToastPosition::TopLeft);
        manager.push(Toast::info("Origin-aware"));

        let area = manager.render_area(Rect::new(10, 5, 80, 20));

        assert_eq!(area.x, 12);
        assert_eq!(area.y, 6);
        assert_eq!(area.width, 40);
        assert_eq!(area.height, 3);
    }

    #[test]
    fn test_render_area_caps_large_visible_count_without_truncation() {
        let manager = ToastManager {
            toasts: (0..21_846)
                .map(|idx| Toast::info(format!("Toast {idx}")))
                .collect::<VecDeque<_>>(),
            max_visible: usize::MAX,
            position: ToastPosition::TopRight,
            coalesce: false,
        };

        let area = manager.render_area(Rect::new(0, 0, 80, u16::MAX));

        assert_eq!(area.height, u16::MAX - 2);
    }

    #[test]
    fn test_dismiss_type() {
        let mut manager = ToastManager::new();
        manager.push(Toast::info("Info 1"));
        manager.push(Toast::error("Error 1"));
        manager.push(Toast::info("Info 2"));

        manager.dismiss_type(ToastType::Info);
        assert_eq!(manager.len(), 1);
        assert_eq!(
            manager.visible().next().unwrap().toast_type,
            ToastType::Error
        );
    }
}
