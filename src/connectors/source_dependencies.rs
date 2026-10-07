//! Source-ingest dependency capabilities at the CASS connector boundary.
//!
//! The pinned FAD trait does not expose this contract. Keep it in the runtime
//! registration, rather than teaching the indexer a list of provider names.

use std::collections::HashMap;
use std::sync::OnceLock;

pub(crate) mod observation;

use super::ConnectorFactory;

/// Whether discovery of a new sibling can change a source's parsed result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SourceDependencyPolicy {
    /// Conservative default, including unknown and newly registered connectors.
    /// A newly appearing WAL, metadata file, or other optional sidecar matters.
    #[default]
    ObserveParentDirectory,
    /// Parsing reads only the primary source. Explicit dependencies, should a
    /// connector supply any, are still validated; only the implicit parent is
    /// omitted. This is an opt-in parser contract, not an inference from an
    /// empty `required_sidecars` list on one particular scan.
    SelfContained,
}

impl SourceDependencyPolicy {
    pub(crate) fn observes_parent_directory(self) -> bool {
        self == Self::ObserveParentDirectory
    }
}

/// Explicit opt-in owned by the concrete connector adapter.
pub(crate) trait SourceDependencyCapability {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy;
}

// Both adapters delegate source boundaries to FAD's single-transcript readers.
// Revisit these declarations if either adapter starts consulting sibling files.
impl SourceDependencyCapability for super::claude_code::ClaudeCodeConnector {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy = SourceDependencyPolicy::SelfContained;
}

impl SourceDependencyCapability for super::codex::CodexConnector {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy = SourceDependencyPolicy::SelfContained;
}

pub(crate) struct ConnectorRegistration {
    pub(crate) name: &'static str,
    pub(crate) source_slug: &'static str,
    pub(crate) factory: ConnectorFactory,
    pub(crate) source_dependencies: SourceDependencyPolicy,
}

impl ConnectorRegistration {
    pub(crate) fn new(name: &'static str, factory: ConnectorFactory) -> Self {
        Self {
            name,
            source_slug: name,
            factory,
            source_dependencies: SourceDependencyPolicy::default(),
        }
    }

    pub(crate) fn with_source_capability<T: SourceDependencyCapability>(
        mut self,
        source_slug: &'static str,
    ) -> Self {
        self.source_slug = source_slug;
        self.source_dependencies = T::SOURCE_DEPENDENCIES;
        self
    }
}

/// Resolve source slugs from the same registrations that construct connectors.
/// Initialization happens once, not once per transcript on the reuse fast path.
pub(crate) fn source_dependency_policy(source_slug: &str) -> SourceDependencyPolicy {
    static POLICIES: OnceLock<HashMap<&'static str, SourceDependencyPolicy>> = OnceLock::new();
    POLICIES
        .get_or_init(|| {
            let mut policies = HashMap::new();
            for registration in super::get_connector_registrations() {
                policies.insert(registration.name, registration.source_dependencies);
                policies.insert(registration.source_slug, registration.source_dependencies);
            }
            policies
        })
        .get(source_slug)
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh512_capabilities_follow_the_runtime_registry_and_default_closed() {
        for slug in ["claude", "claude_code", "codex"] {
            assert_eq!(
                source_dependency_policy(slug),
                SourceDependencyPolicy::SelfContained
            );
        }
        for registration in super::super::get_connector_registrations() {
            if !matches!(registration.source_slug, "claude_code" | "codex") {
                assert_eq!(
                    registration.source_dependencies,
                    SourceDependencyPolicy::ObserveParentDirectory
                );
            }
            assert_eq!(
                source_dependency_policy(registration.source_slug),
                registration.source_dependencies
            );
        }
        assert_eq!(
            source_dependency_policy("future-connector"),
            SourceDependencyPolicy::ObserveParentDirectory
        );
    }
}
