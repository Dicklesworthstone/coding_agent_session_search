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

    /// Exclusions are applied to the entire source BEFORE its admission hook,
    /// never to messages or reconstruction inputs within an admitted source.
    /// This is independent of self-containment: a single-file container could
    /// still apply exclusions to only some of the sessions stored inside it.
    const WHOLE_SOURCE_EXCLUSIONS: bool = false;
}

// Both adapters delegate source boundaries to FAD's single-transcript readers.
// Revisit these declarations if either adapter starts consulting sibling files.
impl SourceDependencyCapability for super::claude_code::ClaudeCodeConnector {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy = SourceDependencyPolicy::SelfContained;
    // FAD rejects the session path before should_scan, then parses it whole.
    const WHOLE_SOURCE_EXCLUSIONS: bool = true;
}

impl SourceDependencyCapability for super::codex::CodexConnector {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy = SourceDependencyPolicy::SelfContained;
    // The CASS budget/exclusion adapter rejects the rollout before either
    // parser pass or the host hook. Archived rollouts use the same admission.
    const WHOLE_SOURCE_EXCLUSIONS: bool = true;
}

// Codebuff excludes the entire chat if either its transcript or consulted
// run-state path is excluded, before calling the host's admission predicate.
// The optional sidecar still affects reconstruction: keep parent observation.
impl SourceDependencyCapability for super::codebuff::CodebuffConnector {
    const SOURCE_DEPENDENCIES: SourceDependencyPolicy =
        SourceDependencyPolicy::ObserveParentDirectory;
    const WHOLE_SOURCE_EXCLUSIONS: bool = true;
}

pub(crate) struct ConnectorRegistration {
    pub(crate) name: &'static str,
    pub(crate) source_slug: &'static str,
    pub(crate) factory: ConnectorFactory,
    pub(crate) source_dependencies: SourceDependencyPolicy,
    whole_source_exclusions: bool,
}

impl ConnectorRegistration {
    pub(crate) fn new(name: &'static str, factory: ConnectorFactory) -> Self {
        Self {
            name,
            source_slug: name,
            factory,
            source_dependencies: SourceDependencyPolicy::default(),
            whole_source_exclusions: false,
        }
    }

    pub(crate) fn with_source_capability<T: SourceDependencyCapability>(
        mut self,
        source_slug: &'static str,
    ) -> Self {
        self.source_slug = source_slug;
        self.source_dependencies = T::SOURCE_DEPENDENCIES;
        self.whole_source_exclusions = T::WHOLE_SOURCE_EXCLUSIONS;
        self
    }
}

#[derive(Clone, Copy, Default)]
struct SourceCapabilities {
    dependencies: SourceDependencyPolicy,
    whole_source_exclusions: bool,
}

/// Resolve source slugs from the same registrations that construct connectors.
/// Initialization happens once, not once per transcript on the reuse fast path.
fn source_capabilities(source_slug: &str) -> SourceCapabilities {
    static CAPABILITIES: OnceLock<HashMap<&'static str, SourceCapabilities>> = OnceLock::new();
    CAPABILITIES
        .get_or_init(|| {
            let mut policies = HashMap::new();
            for registration in super::get_connector_registrations() {
                let capabilities = SourceCapabilities {
                    dependencies: registration.source_dependencies,
                    whole_source_exclusions: registration.whole_source_exclusions,
                };
                policies.insert(registration.name, capabilities);
                policies.insert(registration.source_slug, capabilities);
            }
            policies
        })
        .get(source_slug)
        .copied()
        .unwrap_or_default()
}

pub(crate) fn source_dependency_policy(source_slug: &str) -> SourceDependencyPolicy {
    source_capabilities(source_slug).dependencies
}

/// An unrelated exclusion must not force every admitted transcript to reparse.
/// Require an explicit declaration from both the running registration and its
/// source identity. Unknown adapters cannot borrow a known provider's promise.
/// The indexer still withholds completion if its own prepare callback filters
/// any conversation; aggregate scan watermarks must also remain conservative.
pub(crate) fn can_reuse_with_path_exclusions(connector_name: &str, source_slug: &str) -> bool {
    source_capabilities(connector_name).whole_source_exclusions
        && source_capabilities(source_slug).whole_source_exclusions
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

    #[test]
    fn gh512_whole_source_exclusions_require_explicit_runtime_capabilities() {
        for (registration, source) in [
            ("claude", "claude_code"),
            ("codex", "codex"),
            ("codebuff", "codebuff"),
        ] {
            assert!(can_reuse_with_path_exclusions(registration, source));
            assert!(!can_reuse_with_path_exclusions("unknown", source));
            assert!(!can_reuse_with_path_exclusions(registration, "unknown"));
        }
        for registration in super::super::get_connector_registrations() {
            assert_eq!(
                can_reuse_with_path_exclusions(registration.name, registration.source_slug),
                registration.whole_source_exclusions,
            );
            if !matches!(registration.source_slug, "claude_code" | "codex" | "codebuff") {
                assert!(!registration.whole_source_exclusions);
            }
        }
    }

    #[test]
    fn gh511_whole_chat_exclusions_do_not_make_codebuff_self_contained() {
        assert!(can_reuse_with_path_exclusions("codebuff", "codebuff"));
        assert_eq!(
            source_dependency_policy("codebuff"),
            SourceDependencyPolicy::ObserveParentDirectory
        );
        assert!(!can_reuse_with_path_exclusions("codebuff", "unknown"));
        assert!(!can_reuse_with_path_exclusions("unknown", "codebuff"));
    }

    #[test]
    fn gh512_self_containment_does_not_imply_whole_source_filtering() {
        struct PartiallyFilteredContainer;
        impl SourceDependencyCapability for PartiallyFilteredContainer {
            const SOURCE_DEPENDENCIES: SourceDependencyPolicy =
                SourceDependencyPolicy::SelfContained;
        }
        let factory = super::super::get_connector_factories()[0].1;
        let registration = ConnectorRegistration::new("fixture", factory)
            .with_source_capability::<PartiallyFilteredContainer>("fixture");
        assert_eq!(
            registration.source_dependencies,
            SourceDependencyPolicy::SelfContained
        );
        assert!(!registration.whole_source_exclusions);
    }
}
