//! Recovery testing module for encrypted pages archives.
//!
//! Tests for:
//! - Recovery key generation and unlock
//! - Multi-key-slot operations (add/remove)
//! - Disaster recovery scenarios
//! - Edge cases (typos, case sensitivity, unicode normalization)

mod disaster;
mod key_slots;
