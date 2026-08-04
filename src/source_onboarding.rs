//! First-run source onboarding and readiness recommendations.
//!
//! Bead: coding_agent_session_search-guided-ops-repro-trust-5u82n.6
//! ("Build first-run source onboarding and readiness wizard").
//!
//! First-run / new-machine users should get to useful search quickly while
//! understanding privacy and resource implications. This module is the
//! **pure, scriptable decision core** behind the onboarding surface: it turns a
//! read-only [`OnboardingObservation`] (what CASS found — gathered by the
//! command layer from triage/discovery/models/health) into a deterministic
//! [`OnboardingReport`] that explains what will be indexed, what is missing, and
//! the single safest next command.
//!
//! It is `--json`-first and never launches a bare TUI: the interactive wizard is
//! a thin shell over this core. The diagnosis is **mutation-free** — the
//! recommended command is the explicit step the *user* runs, and it is never a
//! destructive operation.

use serde::{Deserialize, Serialize};

/// Stable schema version for the onboarding wire format.
pub const ONBOARDING_SCHEMA_VERSION: u32 = 1;

/// Readiness of a single detected provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderReadiness {
    /// Sessions found and the root is readable; ready to index.
    ReadyToIndex,
    /// Provider is configured-excluded; will not be indexed.
    Excluded,
    /// The session root exists but could not be read (permissions).
    RootUnreadable,
}

impl ProviderReadiness {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderReadiness::ReadyToIndex => "ready_to_index",
            ProviderReadiness::Excluded => "excluded",
            ProviderReadiness::RootUnreadable => "root_unreadable",
        }
    }
}

/// The single recommended next action for the machine's overall state. Ordered
/// by first-run precedence (most blocking first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingAction {
    /// No providers found: discover/add sources first.
    DiscoverSources,
    /// A session root is unreadable: fix permissions before indexing.
    FixSourcePermissions,
    /// Providers found but nothing indexed yet: run the first index.
    RunFirstIndex,
    /// An index already exists: ready to search.
    ReadyToSearch,
}

impl OnboardingAction {
    /// Stable snake_case wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            OnboardingAction::DiscoverSources => "discover_sources",
            OnboardingAction::FixSourcePermissions => "fix_source_permissions",
            OnboardingAction::RunFirstIndex => "run_first_index",
            OnboardingAction::ReadyToSearch => "ready_to_search",
        }
    }
}

/// A detected provider in the observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderObservation {
    /// Provider name (e.g. "claude_code", "codex").
    pub name: String,
    /// Whether the provider is configured-excluded.
    pub excluded: bool,
    /// Whether its session root path was readable.
    pub root_readable: bool,
    /// Estimated session count (0 when unknown/empty).
    pub estimated_sessions: u64,
}

/// Read-only snapshot of the machine's onboarding-relevant state, gathered by
/// the command layer (no mutation).
#[derive(Debug, Clone, Default)]
pub struct OnboardingObservation {
    /// Detected providers.
    pub providers: Vec<ProviderObservation>,
    /// Whether a semantic embedding model is present locally.
    pub semantic_model_present: bool,
    /// Whether any remote sources are configured.
    pub remote_sources_configured: bool,
    /// Whether an index/archive DB already exists with content.
    pub existing_indexed_db: bool,
    /// Conversations already indexed (0 when none).
    pub indexed_conversation_count: u64,
}

/// Per-provider readiness line in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReadinessEntry {
    /// Provider name.
    pub name: String,
    /// Readiness state.
    pub readiness: ProviderReadiness,
    /// Estimated sessions (a rough indexing-cost signal).
    pub estimated_sessions: u64,
}

/// The deterministic onboarding report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingReport {
    /// Mirrors [`ONBOARDING_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Per-provider readiness, sorted deterministically (name asc).
    pub providers: Vec<ProviderReadinessEntry>,
    /// Names of excluded providers (sorted).
    pub excluded_providers: Vec<String>,
    /// Estimated total sessions to index (sum over indexable providers).
    pub estimated_index_sessions: u64,
    /// Whether the semantic model is ready (else search is lexical-only).
    pub semantic_ready: bool,
    /// Advisory hint about remote sources, when relevant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_hint: Option<String>,
    /// The single recommended action.
    pub recommended_action: OnboardingAction,
    /// The concrete, safe (additive, non-destructive) next command for the user.
    pub recommended_command: String,
    /// Rollback/undo note for the recommended command.
    pub rollback_note: String,
    /// Always true: producing this report performs no mutation.
    pub mutation_free: bool,
}

/// Compute the deterministic onboarding report from a read-only observation.
/// Pure: no I/O, no mutation. Recommendation precedence is fixed (most blocking
/// first) so the output ordering is stable.
pub fn recommend(observation: &OnboardingObservation) -> OnboardingReport {
    // Build per-provider readiness, sorted by name for deterministic output.
    let mut providers: Vec<ProviderReadinessEntry> = observation
        .providers
        .iter()
        .map(|p| ProviderReadinessEntry {
            name: p.name.clone(),
            readiness: if p.excluded {
                ProviderReadiness::Excluded
            } else if !p.root_readable {
                ProviderReadiness::RootUnreadable
            } else {
                ProviderReadiness::ReadyToIndex
            },
            estimated_sessions: p.estimated_sessions,
        })
        .collect();
    providers.sort_by(|a, b| a.name.cmp(&b.name));

    let mut excluded_providers: Vec<String> = providers
        .iter()
        .filter(|p| p.readiness == ProviderReadiness::Excluded)
        .map(|p| p.name.clone())
        .collect();
    excluded_providers.sort();

    let indexable: Vec<&ProviderReadinessEntry> = providers
        .iter()
        .filter(|p| p.readiness == ProviderReadiness::ReadyToIndex)
        .collect();
    let estimated_index_sessions: u64 = indexable.iter().map(|p| p.estimated_sessions).sum();

    let any_unreadable = providers
        .iter()
        .any(|p| p.readiness == ProviderReadiness::RootUnreadable);

    // Precedence (most blocking first):
    // 1) no indexable/excluded providers at all -> discover
    // 2) an unreadable root -> fix permissions (preserve; no mutation)
    // 3) providers ready but nothing indexed -> first index
    // 4) an index already exists -> ready to search
    let recommended_action = if providers.is_empty() {
        OnboardingAction::DiscoverSources
    } else if any_unreadable && !observation.existing_indexed_db {
        OnboardingAction::FixSourcePermissions
    } else if observation.existing_indexed_db {
        OnboardingAction::ReadyToSearch
    } else if !indexable.is_empty() {
        OnboardingAction::RunFirstIndex
    } else if any_unreadable {
        OnboardingAction::FixSourcePermissions
    } else {
        // Providers exist but all excluded and nothing indexed.
        OnboardingAction::DiscoverSources
    };

    let (recommended_command, rollback_note) = match recommended_action {
        OnboardingAction::DiscoverSources => (
            "cass sources discover --json".to_string(),
            "read-only discovery; nothing is written until you run an explicit add/index"
                .to_string(),
        ),
        OnboardingAction::FixSourcePermissions => (
            "fix read permissions on the unreadable session root, then re-run cass triage --json"
                .to_string(),
            "no cass state changes; this only adjusts host filesystem permissions you control"
                .to_string(),
        ),
        OnboardingAction::RunFirstIndex => (
            "cass index --json".to_string(),
            "indexing is additive; remove the data dir to fully undo, or re-run safely".to_string(),
        ),
        OnboardingAction::ReadyToSearch => (
            "cass search <query> --json".to_string(),
            "search is read-only; nothing to undo".to_string(),
        ),
    };

    let remote_hint = if observation.remote_sources_configured {
        None
    } else {
        Some(
            "no remote sources configured; run cass sources add <ssh-host> to include other machines (additive)".to_string(),
        )
    };

    OnboardingReport {
        schema_version: ONBOARDING_SCHEMA_VERSION,
        providers,
        excluded_providers,
        estimated_index_sessions,
        semantic_ready: observation.semantic_model_present,
        remote_hint,
        recommended_action,
        recommended_command,
        rollback_note,
        mutation_free: true,
    }
}

impl OnboardingReport {
    /// Whether the machine is ready to search right now.
    pub fn is_ready_to_search(&self) -> bool {
        self.recommended_action == OnboardingAction::ReadyToSearch
    }

    /// A one-line advisory about semantic readiness for the human surface.
    pub fn semantic_note(&self) -> &'static str {
        if self.semantic_ready {
            "semantic search ready"
        } else {
            "semantic model absent; search will use lexical-only ranking until a model is downloaded"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, sessions: u64) -> ProviderObservation {
        ProviderObservation {
            name: name.to_string(),
            excluded: false,
            root_readable: true,
            estimated_sessions: sessions,
        }
    }

    fn base() -> OnboardingObservation {
        OnboardingObservation {
            semantic_model_present: true,
            remote_sources_configured: true,
            ..Default::default()
        }
    }

    #[test]
    fn empty_machine_recommends_discovery() {
        let r = recommend(&base());
        assert_eq!(r.recommended_action, OnboardingAction::DiscoverSources);
        assert_eq!(r.recommended_command, "cass sources discover --json");
        assert!(r.providers.is_empty());
        assert!(r.mutation_free);
    }

    #[test]
    fn one_provider_recommends_first_index() {
        let mut o = base();
        o.providers = vec![provider("codex", 12)];
        let r = recommend(&o);
        assert_eq!(r.recommended_action, OnboardingAction::RunFirstIndex);
        assert_eq!(r.recommended_command, "cass index --json");
        assert_eq!(r.estimated_index_sessions, 12);
        assert_eq!(r.providers[0].readiness, ProviderReadiness::ReadyToIndex);
    }

    #[test]
    fn many_providers_are_ordered_deterministically_and_summed() {
        let mut o = base();
        o.providers = vec![
            provider("codex", 5),
            provider("claude_code", 10),
            provider("cursor", 3),
        ];
        let r = recommend(&o);
        let names: Vec<&str> = r.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["claude_code", "codex", "cursor"],
            "providers sorted by name"
        );
        assert_eq!(r.estimated_index_sessions, 18);
        assert_eq!(r.recommended_action, OnboardingAction::RunFirstIndex);
    }

    #[test]
    fn missing_remote_config_yields_hint() {
        let mut o = base();
        o.providers = vec![provider("codex", 1)];
        o.remote_sources_configured = false;
        let r = recommend(&o);
        assert!(
            r.remote_hint
                .as_deref()
                .unwrap()
                .contains("cass sources add")
        );
        // Configured => no hint.
        let mut o2 = o.clone();
        o2.remote_sources_configured = true;
        assert!(recommend(&o2).remote_hint.is_none());
    }

    #[test]
    fn semantic_model_absent_reports_lexical_fallback() {
        let mut o = base();
        o.providers = vec![provider("codex", 1)];
        o.semantic_model_present = false;
        let r = recommend(&o);
        assert!(!r.semantic_ready);
        assert!(r.semantic_note().contains("lexical-only"));
    }

    #[test]
    fn excluded_provider_is_listed_and_not_counted() {
        let mut o = base();
        let mut excluded = provider("aider", 99);
        excluded.excluded = true;
        o.providers = vec![provider("codex", 4), excluded];
        let r = recommend(&o);
        assert_eq!(r.excluded_providers, vec!["aider".to_string()]);
        // Excluded sessions are not in the index estimate.
        assert_eq!(r.estimated_index_sessions, 4);
        assert_eq!(r.recommended_action, OnboardingAction::RunFirstIndex);
    }

    #[test]
    fn unreadable_root_recommends_fix_permissions_without_mutation() {
        let mut o = base();
        let mut bad = provider("codex", 0);
        bad.root_readable = false;
        o.providers = vec![bad];
        let r = recommend(&o);
        assert_eq!(r.recommended_action, OnboardingAction::FixSourcePermissions);
        assert_eq!(r.providers[0].readiness, ProviderReadiness::RootUnreadable);
        assert!(r.mutation_free);
    }

    #[test]
    fn existing_indexed_db_is_ready_to_search() {
        let mut o = base();
        o.providers = vec![provider("codex", 100)];
        o.existing_indexed_db = true;
        o.indexed_conversation_count = 100;
        let r = recommend(&o);
        assert_eq!(r.recommended_action, OnboardingAction::ReadyToSearch);
        assert_eq!(r.recommended_command, "cass search <query> --json");
        assert!(r.is_ready_to_search());
    }

    #[test]
    fn recommended_command_is_never_destructive() {
        // Sweep representative observations; no recommendation may be destructive.
        let observations = [
            base(),
            {
                let mut o = base();
                o.providers = vec![provider("codex", 1)];
                o
            },
            {
                let mut o = base();
                let mut bad = provider("codex", 0);
                bad.root_readable = false;
                o.providers = vec![bad];
                o
            },
            {
                let mut o = base();
                o.providers = vec![provider("codex", 1)];
                o.existing_indexed_db = true;
                o
            },
        ];
        for o in observations {
            let cmd = recommend(&o).recommended_command.to_ascii_lowercase();
            for needle in [
                "--delete",
                "rm -rf",
                "rm -r ",
                "--remove-source-files",
                "prune",
                "shred",
                "drop ",
            ] {
                assert!(
                    !cmd.contains(needle),
                    "destructive recommended command: {cmd:?}"
                );
            }
        }
    }

    #[test]
    fn report_json_contract_is_stable_and_round_trips() {
        let mut o = base();
        o.providers = vec![provider("codex", 7)];
        let r = recommend(&o);
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(value["schema_version"], ONBOARDING_SCHEMA_VERSION);
        assert_eq!(value["mutation_free"], true);
        assert_eq!(value["recommended_action"], "run_first_index");
        assert_eq!(value["providers"][0]["readiness"], "ready_to_index");
        let back: OnboardingReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, r);
    }
}
