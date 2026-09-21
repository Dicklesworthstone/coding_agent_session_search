//! Keep Codex discovery and both parser passes on the same path policy as
//! raw-mirror capture; excluded content must not re-enter through an alias.

pub(super) use super::super::path_policy::ScanExclusions;
