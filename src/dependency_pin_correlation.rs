//! Dependency-pin and upstream-fix correlation for CASS diagnostics.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.9.4
//! ("Track dependency pins and upstream fix correlation in diagnostics").
//!
//! CASS pins sibling crates (frankensqlite, frankensearch, asupersync, …) by git
//! revision. When a symptom shows up — e.g. a frankensqlite FTS/`OpenRead`
//! failure — the right diagnostic answer is often "this is already fixed upstream
//! in rev X; your pin is behind" or "your local checkout is dirty/patched, so the
//! pinned rev is not what is actually running". This module makes that correlation
//! explicit instead of leaving an operator to guess from CASS source symptoms.
//!
//! It is pure logic over inputs a probe gathers ([`PinObservation`]): the pinned
//! rev from `Cargo.toml`/lock, the observed local rev (when a sibling checkout is
//! present and readable), and whether the working tree is dirty. No network and
//! no filesystem access happen here — `no-network mode` is simply the case where
//! `observed_local_rev` is `None`. It composes with the root-cause taxonomy
//! (`9.1`) via [`RootCauseFamily`] so a correlated known issue can be attributed.

use crate::root_cause_taxonomy::RootCauseFamily;
use serde::{Deserialize, Serialize};

/// Stable schema version for the pin-correlation wire format.
pub const DEPENDENCY_PIN_SCHEMA_VERSION: u32 = 1;

/// A known upstream issue and the dependency revision that fixes it. Static
/// catalog entries let diagnostics say "your symptom matches issue Y, fixed in
/// rev Z" without a network call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownIssue {
    /// Stable issue identifier (e.g. `"fsqlite-openread-001"`).
    pub id: &'static str,
    /// The dependency package the issue lives in.
    pub package: &'static str,
    /// Short, prose-free symptom signature.
    pub symptom: &'static str,
    /// The root-cause family this issue manifests as.
    pub family: RootCauseFamily,
    /// First dependency revision/version known to contain the fix.
    pub fixed_in_rev: &'static str,
}

/// The built-in catalog of known upstream fixes worth correlating. Kept small and
/// curated; the report specifically called out frankensqlite FTS/`OpenRead`
/// fixes. Extend as fixes land.
pub const KNOWN_ISSUES: &[KnownIssue] = &[
    KnownIssue {
        id: "fsqlite-openread-001",
        package: "frankensqlite",
        symptom: "OpenRead error opening main DB",
        family: RootCauseFamily::FrankensqliteStorage,
        fixed_in_rev: "a4923d4",
    },
    KnownIssue {
        id: "fsqlite-fts-002",
        package: "frankensqlite",
        symptom: "FTS query fails while plain reads succeed",
        family: RootCauseFamily::FrankensqliteStorage,
        fixed_in_rev: "a4923d4",
    },
    KnownIssue {
        id: "frankensearch-tantivy-001",
        package: "frankensearch",
        symptom: "missing Tantivy metadata / segment error",
        family: RootCauseFamily::FrankensearchSearch,
        fixed_in_rev: "be455cc",
    },
];

/// What a probe observed about a single dependency pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinObservation {
    /// Dependency package name.
    pub package: String,
    /// The pinned rev/version from `Cargo.toml`/lock (the intended pin).
    pub pinned_rev: String,
    /// The rev actually present in the local sibling checkout, if one is present
    /// and readable. `None` means no checkout was available (e.g. no-network /
    /// no-sibling mode) — the pin cannot be confirmed against reality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_local_rev: Option<String>,
    /// Whether the local checkout's working tree is dirty (uncommitted edits) —
    /// the running code may differ from any committed rev.
    pub local_dirty: bool,
    /// Whether a local sibling checkout exists at all.
    pub checkout_present: bool,
}

/// The reconciled state of a dependency pin vs. what is actually present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinState {
    /// Observed local rev matches the pin and the tree is clean.
    Current,
    /// Observed local rev differs from the pin (checkout behind/ahead of pin).
    Stale,
    /// Local checkout is dirty: the running code is not any committed rev, so the
    /// pin is unverifiable and may be locally patched.
    DirtyLocalPatch,
    /// No sibling checkout present — pin cannot be confirmed against reality.
    MissingCheckout,
    /// No observed rev available (no-network / unreadable), but a checkout exists.
    Unverified,
}

/// The correlated assessment for one dependency pin: its state, any matching
/// known issues, whether an upstream fix is likely already (or not yet) applied,
/// and the recommended validation command. Stable snake_case JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinAssessment {
    /// Mirrors [`DEPENDENCY_PIN_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Dependency package.
    pub package: String,
    /// Intended pinned rev.
    pub pinned_rev: String,
    /// Observed local rev, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_local_rev: Option<String>,
    /// Reconciled pin state.
    pub pin_state: PinState,
    /// IDs of known issues that affect this package and are relevant to the pin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_issue_ids: Vec<String>,
    /// `true` when at least one known issue's fix is likely NOT present (pin or
    /// local rev does not match the fixed-in rev) and so a symptom may be the
    /// known upstream bug rather than a CASS defect.
    pub upstream_fix_possibly_missing: bool,
    /// The single recommended command to validate/resolve the pin.
    pub recommended_validation: String,
}

/// Reconcile a [`PinObservation`] into a [`PinState`].
fn reconcile_state(obs: &PinObservation) -> PinState {
    if !obs.checkout_present {
        return PinState::MissingCheckout;
    }
    if obs.local_dirty {
        return PinState::DirtyLocalPatch;
    }
    match &obs.observed_local_rev {
        None => PinState::Unverified,
        Some(local) if revs_match(local, &obs.pinned_rev) => PinState::Current,
        Some(_) => PinState::Stale,
    }
}

/// Compare two revs allowing for short/long git hash prefixes (either may be a
/// prefix of the other). Exact match for non-hash version strings.
fn revs_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return true;
    }
    // Git short-hash tolerance: treat as matching if one is a prefix of the other
    // and the shorter is a plausible hash prefix (>= 7 hex chars).
    let looks_hashy = |s: &str| s.len() >= 7 && s.chars().all(|c| c.is_ascii_hexdigit());
    if looks_hashy(a) && looks_hashy(b) {
        let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        return long.starts_with(short);
    }
    false
}

/// Known issues whose `package` matches (case-insensitively).
fn issues_for_package(package: &str) -> impl Iterator<Item = &'static KnownIssue> {
    let pkg = package.to_ascii_lowercase();
    KNOWN_ISSUES
        .iter()
        .filter(move |issue| issue.package.eq_ignore_ascii_case(&pkg))
}

/// Correlate a single pin observation against the known-issue catalog.
pub fn assess_pin(obs: &PinObservation) -> PinAssessment {
    let pin_state = reconcile_state(obs);
    let issues: Vec<&KnownIssue> = issues_for_package(&obs.package).collect();
    let known_issue_ids: Vec<String> = issues.iter().map(|i| i.id.to_string()).collect();

    // The fix is "possibly missing" when we cannot confirm the running rev carries
    // it: the relevant rev (observed if present, else the pin) does not match the
    // fixed-in rev, OR the tree is dirty/unverified/missing so we can't be sure.
    let effective_rev = obs.observed_local_rev.as_deref().unwrap_or(&obs.pinned_rev);
    let any_fix_unconfirmed = issues.iter().any(|issue| {
        !revs_match(effective_rev, issue.fixed_in_rev)
            || matches!(
                pin_state,
                PinState::DirtyLocalPatch | PinState::MissingCheckout | PinState::Unverified
            )
    });
    let upstream_fix_possibly_missing = !issues.is_empty() && any_fix_unconfirmed;

    let recommended_validation = recommend(obs, pin_state);

    PinAssessment {
        schema_version: DEPENDENCY_PIN_SCHEMA_VERSION,
        package: obs.package.clone(),
        pinned_rev: obs.pinned_rev.clone(),
        observed_local_rev: obs.observed_local_rev.clone(),
        pin_state,
        known_issue_ids,
        upstream_fix_possibly_missing,
        recommended_validation,
    }
}

/// Correlate every observed pin.
pub fn assess_pins(observations: &[PinObservation]) -> Vec<PinAssessment> {
    observations.iter().map(assess_pin).collect()
}

fn recommend(obs: &PinObservation, state: PinState) -> String {
    match state {
        PinState::Current => format!(
            "pin confirmed at {}; no action (re-run `cargo update -p {}` only to advance)",
            obs.pinned_rev, obs.package
        ),
        PinState::Stale => format!(
            "local {} differs from pin {}; run `cargo update -p {} --precise <rev>` or rebuild against the pinned rev",
            obs.observed_local_rev.as_deref().unwrap_or("rev"),
            obs.pinned_rev,
            obs.package
        ),
        PinState::DirtyLocalPatch => format!(
            "local checkout of {} is dirty; `git -C ../{} status` and commit/stash so the running code matches a known rev",
            obs.package, obs.package
        ),
        PinState::MissingCheckout => format!(
            "no local checkout of {}; building from the pinned git rev {} (clone the sibling to verify against source)",
            obs.package, obs.pinned_rev
        ),
        PinState::Unverified => format!(
            "cannot read local rev for {} (no-network/unreadable); verify with `git -C ../{} rev-parse HEAD`",
            obs.package, obs.package
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(
        pkg: &str,
        pinned: &str,
        local: Option<&str>,
        dirty: bool,
        present: bool,
    ) -> PinObservation {
        PinObservation {
            package: pkg.to_string(),
            pinned_rev: pinned.to_string(),
            observed_local_rev: local.map(str::to_string),
            local_dirty: dirty,
            checkout_present: present,
        }
    }

    #[test]
    fn current_pin_matches_and_needs_no_action() {
        let a = assess_pin(&obs(
            "frankensqlite",
            "a4923d4",
            Some("a4923d4"),
            false,
            true,
        ));
        assert_eq!(a.pin_state, PinState::Current);
        // Pin is at the fixed rev, so the known fixes are present.
        assert!(!a.upstream_fix_possibly_missing);
        assert!(!a.known_issue_ids.is_empty());
    }

    #[test]
    fn old_pin_is_stale_and_flags_missing_fix() {
        let a = assess_pin(&obs(
            "frankensqlite",
            "a4923d4",
            Some("0000abc"),
            false,
            true,
        ));
        assert_eq!(a.pin_state, PinState::Stale);
        // Local rev does not carry the fix.
        assert!(a.upstream_fix_possibly_missing);
        assert!(
            a.known_issue_ids
                .contains(&"fsqlite-openread-001".to_string())
        );
        assert!(a.recommended_validation.contains("cargo update"));
    }

    #[test]
    fn dirty_local_patch_is_unverifiable() {
        let a = assess_pin(&obs(
            "frankensqlite",
            "a4923d4",
            Some("a4923d4"),
            true,
            true,
        ));
        assert_eq!(a.pin_state, PinState::DirtyLocalPatch);
        // Even though the rev matches, dirtiness means the fix presence is unconfirmed.
        assert!(a.upstream_fix_possibly_missing);
        assert!(a.recommended_validation.contains("dirty"));
    }

    #[test]
    fn missing_sibling_checkout_is_flagged() {
        let a = assess_pin(&obs("frankensqlite", "a4923d4", None, false, false));
        assert_eq!(a.pin_state, PinState::MissingCheckout);
        assert!(a.upstream_fix_possibly_missing);
        assert!(a.recommended_validation.contains("no local checkout"));
    }

    #[test]
    fn no_network_mode_is_unverified_when_checkout_present_but_rev_unknown() {
        let a = assess_pin(&obs("frankensearch", "be455cc", None, false, true));
        assert_eq!(a.pin_state, PinState::Unverified);
        // Pin is at the fixed rev, but we can't observe local rev → unconfirmed.
        assert!(a.upstream_fix_possibly_missing);
        assert!(a.recommended_validation.contains("rev-parse"));
    }

    #[test]
    fn unverified_with_pin_at_fixed_rev_still_lists_issues() {
        let a = assess_pin(&obs("frankensearch", "be455cc", None, false, true));
        assert!(
            a.known_issue_ids
                .contains(&"frankensearch-tantivy-001".to_string())
        );
    }

    #[test]
    fn package_without_known_issues_has_no_fix_flag() {
        let a = assess_pin(&obs(
            "asupersync",
            "deadbeef0",
            Some("cafef00d1"),
            false,
            true,
        ));
        assert_eq!(a.pin_state, PinState::Stale); // revs differ
        assert!(a.known_issue_ids.is_empty());
        // No known issues for this package → nothing to flag as possibly-missing.
        assert!(!a.upstream_fix_possibly_missing);
    }

    #[test]
    fn rev_match_tolerates_short_long_hash_prefixes() {
        assert!(revs_match("a4923d4", "a4923d4097899e6e"));
        assert!(revs_match("a4923d4097899e6e", "a4923d4"));
        assert!(!revs_match("a4923d4", "b0000004"));
        assert!(revs_match("1.0.28", "1.0.28"));
        assert!(!revs_match("1.0.28", "1.0.29"));
        // Too-short to be a confident hash prefix → exact only.
        assert!(!revs_match("a492", "a4923d4097"));
    }

    #[test]
    fn current_pin_at_fixed_rev_confirms_fix_present() {
        // Long pinned rev whose prefix matches the catalog short rev.
        let a = assess_pin(&obs(
            "frankensqlite",
            "a4923d4097899e6e9805cefe67bce70e1b04a289",
            Some("a4923d4097899e6e9805cefe67bce70e1b04a289"),
            false,
            true,
        ));
        assert_eq!(a.pin_state, PinState::Current);
        assert!(
            !a.upstream_fix_possibly_missing,
            "fix rev prefix-matches the pin"
        );
    }

    #[test]
    fn assessment_serializes_with_stable_fields_and_round_trips() {
        let a = assess_pin(&obs(
            "frankensqlite",
            "a4923d4",
            Some("0000abc"),
            false,
            true,
        ));
        let value = serde_json::to_value(&a).unwrap();
        assert_eq!(value["schema_version"], DEPENDENCY_PIN_SCHEMA_VERSION);
        assert_eq!(value["package"], "frankensqlite");
        assert_eq!(value["pin_state"], "stale");
        assert_eq!(value["observed_local_rev"], "0000abc");
        assert_eq!(value["upstream_fix_possibly_missing"], true);
        assert!(!value["known_issue_ids"].as_array().unwrap().is_empty());
        let back: PinAssessment = serde_json::from_value(value).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn pin_state_wire_values_are_kebab_case() {
        for (state, wire) in [
            (PinState::Current, "current"),
            (PinState::Stale, "stale"),
            (PinState::DirtyLocalPatch, "dirty-local-patch"),
            (PinState::MissingCheckout, "missing-checkout"),
            (PinState::Unverified, "unverified"),
        ] {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{wire}\"")
            );
        }
    }

    #[test]
    fn known_issues_reference_real_root_cause_families() {
        for issue in KNOWN_ISSUES {
            assert!(
                issue.family.is_external_to_cass(),
                "known dependency issue {} should attribute outside CASS",
                issue.id
            );
            assert!(!issue.fixed_in_rev.is_empty());
            assert!(!issue.id.is_empty());
        }
    }

    #[test]
    fn assess_pins_maps_all_observations() {
        let observations = vec![
            obs("frankensqlite", "a4923d4", Some("a4923d4"), false, true),
            obs("frankensearch", "be455cc", None, false, false),
        ];
        let assessments = assess_pins(&observations);
        assert_eq!(assessments.len(), 2);
        assert_eq!(assessments[0].pin_state, PinState::Current);
        assert_eq!(assessments[1].pin_state, PinState::MissingCheckout);
    }
}
