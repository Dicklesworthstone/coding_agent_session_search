//! Opt-in conversation-title policy and deterministic title/body score fusion.
//!
//! This module owns ranking policy only. Title representation and vector-space
//! versions belong to the title artifact, so changing a weight never requires
//! re-embedding. Shipping callers bypass fusion entirely when the feature is off.

use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use serde::Serialize;

pub const MAX_TITLE_CHANNEL_CANDIDATES: usize = 1_000;
pub const MAX_TITLE_TOTAL_CANDIDATES: usize = 2_000;
pub const TITLE_RRF_K: f32 = 60.0;
pub const DEFAULT_TITLE_ADMISSION_LIMIT: usize = 100_000;
pub const MAX_TITLE_ADMISSION_LIMIT: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleFusion {
    /// Weighted reciprocal ranks; equal scores share their competition rank.
    Rrf,
    /// Weighted signed cosine scores, with missing evidence contributing zero.
    Weighted,
    /// Maximum over present, active channels. At weight .5 this has the
    /// reporter's unweighted maximum ranking, including for negative scores.
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TitleSearchConfig {
    pub enabled: bool,
    pub fusion: TitleFusion,
    pub title_weight: f32,
    pub body_candidates: usize,
    pub title_candidates: usize,
    pub total_candidates: usize,
    /// Bound cold canonical admission work independently of returned results.
    pub max_admission_titles: usize,
}

impl Default for TitleSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fusion: TitleFusion::Weighted,
            title_weight: 0.25,
            body_candidates: MAX_TITLE_CHANNEL_CANDIDATES,
            title_candidates: MAX_TITLE_CHANNEL_CANDIDATES,
            total_candidates: MAX_TITLE_TOTAL_CANDIDATES,
            max_admission_titles: DEFAULT_TITLE_ADMISSION_LIMIT,
        }
    }
}

impl TitleSearchConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_values(|name| dotenvy::var(name).ok())
    }

    /// Parse without modifying process-global environment, including in tests.
    pub fn from_values(mut value: impl FnMut(&str) -> Option<String>) -> Result<Self> {
        let enabled = match value("CASS_SEMANTIC_TITLES") {
            None => false,
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "no" | "off" => false,
                "1" | "true" | "yes" | "on" => true,
                _ => bail!("CASS_SEMANTIC_TITLES must be 0 or 1"),
            },
        };
        let mut config = Self {
            enabled,
            ..Self::default()
        };
        if !enabled {
            // An inactive feature neither interprets its policy knobs nor
            // changes incumbent candidate windows and body score semantics.
            return Ok(config);
        }
        if let Some(raw) = value("CASS_TITLE_FUSION") {
            config.fusion = match raw.trim().to_ascii_lowercase().as_str() {
                "rrf" => TitleFusion::Rrf,
                "weighted" => TitleFusion::Weighted,
                "max" => TitleFusion::Max,
                _ => bail!("CASS_TITLE_FUSION must be rrf, weighted, or max"),
            };
        }
        if let Some(raw) = value("CASS_TITLE_WEIGHT") {
            config.title_weight = raw
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("CASS_TITLE_WEIGHT must be a number in [0,1]"))?;
        }
        for (name, destination) in [
            ("CASS_TITLE_BODY_CANDIDATES", &mut config.body_candidates),
            ("CASS_TITLE_CANDIDATES", &mut config.title_candidates),
            ("CASS_TITLE_TOTAL_CANDIDATES", &mut config.total_candidates),
            (
                "CASS_TITLE_MAX_ADMISSION_TITLES",
                &mut config.max_admission_titles,
            ),
        ] {
            if let Some(raw) = value(name) {
                *destination = raw
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("{name} must be a positive integer"))?;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.title_weight.is_finite() && (0.0..=1.0).contains(&self.title_weight),
            "CASS_TITLE_WEIGHT must be finite and in [0,1]"
        );
        ensure!(
            (1..=MAX_TITLE_CHANNEL_CANDIDATES).contains(&self.body_candidates),
            "CASS_TITLE_BODY_CANDIDATES must be in 1..=1000"
        );
        ensure!(
            (1..=MAX_TITLE_CHANNEL_CANDIDATES).contains(&self.title_candidates),
            "CASS_TITLE_CANDIDATES must be in 1..=1000"
        );
        ensure!(
            (1..=MAX_TITLE_TOTAL_CANDIDATES).contains(&self.total_candidates),
            "CASS_TITLE_TOTAL_CANDIDATES must be in 1..=2000"
        );
        ensure!(
            (1..=MAX_TITLE_ADMISSION_LIMIT).contains(&self.max_admission_titles),
            "CASS_TITLE_MAX_ADMISSION_TITLES must be in 1..=1000000"
        );
        Ok(())
    }
}

/// Request-owned evidence: body certificates do not certify the optional title
/// artifact. Cache and candidate bounds are disclosed independently.
#[derive(Debug, Clone, Serialize)]
pub struct TitleSearchMetadata {
    /// Candidate validity is checked against canonical rows. This is never a
    /// claim that every current conversation has an indexed title.
    pub coverage: TitleCoverage,
    pub policy: TitleSearchConfig,
    pub title_generation: Option<String>,
    pub archive_mutation_seq: u64,
    pub title_mutation_seq: Option<u64>,
    pub indexed_titles: usize,
    pub eligible_titles: usize,
    pub stale_titles_excluded: usize,
    pub body_conversations: usize,
    pub title_conversations: usize,
    pub candidate_window_limited: bool,
    pub admission_cache_hit: bool,
    pub admission_titles_examined: usize,
    pub admission_bitmap_bytes: usize,
    pub admission_ms: u64,
    pub title_search_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleCoverage {
    ValidatedCandidates,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConversationScore {
    pub conversation_id: i64,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusedConversationScore {
    pub conversation_id: i64,
    pub score: f32,
    pub body_score: Option<f32>,
    pub title_score: Option<f32>,
}

#[derive(Default)]
struct Evidence {
    body: Option<(f32, usize)>,
    title: Option<(f32, usize)>,
}

/// One conversation gets at most one vote per channel. A bounded input may
/// include several body passages; only its strongest passage supplies evidence.
fn channel_ranking(values: &[ConversationScore], maximum: usize) -> Result<Vec<ConversationScore>> {
    ensure!(
        values.len() <= maximum,
        "title fusion input exceeds its channel candidate budget"
    );
    let mut best = BTreeMap::<i64, f32>::new();
    for value in values {
        ensure!(
            value.conversation_id > 0,
            "title fusion requires a canonical conversation ID"
        );
        ensure!(
            value.score.is_finite(),
            "title fusion requires finite relevance scores"
        );
        // Signed zero is equal evidence and must not acquire a separate rank.
        let score = if value.score == 0.0 { 0.0 } else { value.score };
        best.entry(value.conversation_id)
            .and_modify(|previous| *previous = previous.max(score))
            .or_insert(score);
    }
    let mut ranked = best
        .into_iter()
        .map(|(conversation_id, score)| ConversationScore {
            conversation_id,
            score,
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.conversation_id.cmp(&right.conversation_id))
    });
    Ok(ranked)
}

/// Same-model fusion over real conversation identities. Scores retain their
/// sign and finite raw f32 magnitude; there is no clipping or per-query scaling.
/// Weighted sum uses missing=0. Max considers only present active channels.
/// RRF uses competition ranks, so tied bodies cannot invent title-independent
/// relevance from archive ID order. Oversized channel inputs are refused.
pub fn fuse_title_body(
    body: &[ConversationScore],
    titles: &[ConversationScore],
    config: TitleSearchConfig,
) -> Result<Vec<FusedConversationScore>> {
    config.validate()?;
    if !config.enabled {
        let mut ranked = channel_ranking(body, config.body_candidates)?
            .into_iter()
            .map(|value| FusedConversationScore {
                conversation_id: value.conversation_id,
                score: value.score,
                body_score: Some(value.score),
                title_score: None,
            })
            .collect::<Vec<_>>();
        ranked.truncate(config.total_candidates);
        return Ok(ranked);
    }
    let mut evidence = BTreeMap::<i64, Evidence>::new();
    for (is_title, values, maximum, active) in [
        (
            false,
            body,
            config.body_candidates,
            config.title_weight < 1.0,
        ),
        (
            true,
            titles,
            config.title_candidates,
            config.title_weight > 0.0,
        ),
    ] {
        if !active {
            continue;
        }
        let mut previous_score = None;
        let mut tied_rank = 0;
        for (position, candidate) in channel_ranking(values, maximum)?.into_iter().enumerate() {
            if previous_score != Some(candidate.score) {
                tied_rank = position + 1;
                previous_score = Some(candidate.score);
            }
            let entry = evidence.entry(candidate.conversation_id).or_default();
            let destination = if is_title {
                &mut entry.title
            } else {
                &mut entry.body
            };
            *destination = Some((candidate.score, tied_rank));
        }
    }
    let mut ranked = Vec::with_capacity(evidence.len());
    for (conversation_id, evidence) in evidence {
        let body_score = evidence.body.map(|(score, _)| score);
        let title_score = evidence.title.map(|(score, _)| score);
        let body_weight = 1.0 - config.title_weight;
        let score = match config.fusion {
            TitleFusion::Rrf => {
                evidence
                    .body
                    .map_or(0.0, |(_, rank)| body_weight / (TITLE_RRF_K + rank as f32))
                    + evidence.title.map_or(0.0, |(_, rank)| {
                        config.title_weight / (TITLE_RRF_K + rank as f32)
                    })
            }
            TitleFusion::Weighted => {
                body_weight * body_score.unwrap_or(0.0)
                    + config.title_weight * title_score.unwrap_or(0.0)
            }
            TitleFusion::Max => match (body_score, title_score) {
                (Some(body), Some(title)) => (body_weight * body).max(config.title_weight * title),
                (Some(body), None) => body_weight * body,
                (None, Some(title)) => config.title_weight * title,
                (None, None) => bail!("title fusion candidate has no active evidence"),
            },
        };
        ensure!(
            score.is_finite(),
            "title fusion produced a non-finite relevance score"
        );
        ranked.push(FusedConversationScore {
            conversation_id,
            score,
            body_score,
            title_score,
        });
    }
    ranked.sort_unstable_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.conversation_id.cmp(&right.conversation_id))
    });
    ranked.truncate(config.total_candidates);
    Ok(ranked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(values: &[(i64, f32)]) -> Vec<ConversationScore> {
        values
            .iter()
            .map(|&(conversation_id, score)| ConversationScore {
                conversation_id,
                score,
            })
            .collect()
    }

    fn policy(fusion: TitleFusion, title_weight: f32) -> TitleSearchConfig {
        TitleSearchConfig {
            enabled: true,
            fusion,
            title_weight,
            ..TitleSearchConfig::default()
        }
    }

    fn ids(values: &[FusedConversationScore]) -> Vec<i64> {
        values.iter().map(|value| value.conversation_id).collect()
    }

    #[test]
    fn title_policy_is_opt_in_and_disabled_knobs_are_inactive() -> Result<()> {
        let default = TitleSearchConfig::from_values(|_| None)?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert!(!default.enabled);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(default.fusion, TitleFusion::Weighted);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(default.title_weight, 0.25);
        let disabled = TitleSearchConfig::from_values(|key| {
            Some(
                if key == "CASS_SEMANTIC_TITLES" {
                    "off"
                } else {
                    "invalid"
                }
                .into(),
            )
        })?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(disabled, default);
        let fused = fuse_title_body(
            &scores(&[(1, -0.2), (2, 0.6)]),
            &scores(&[(0, f32::NAN)]),
            disabled,
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ids(&fused), [2, 1]);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(fused[0].score, 0.6);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert!(fused.iter().all(|hit| hit.title_score.is_none()));
        Ok(())
    }

    #[test]
    fn enabled_policy_rejects_invalid_weights_strategies_and_budgets() {
        for (key, value) in [
            ("CASS_SEMANTIC_TITLES", "maybe"),
            ("CASS_TITLE_FUSION", "surprise"),
            ("CASS_TITLE_WEIGHT", "NaN"),
            ("CASS_TITLE_WEIGHT", "inf"),
            ("CASS_TITLE_WEIGHT", "-0.01"),
            ("CASS_TITLE_WEIGHT", "1.01"),
            ("CASS_TITLE_BODY_CANDIDATES", "0"),
            ("CASS_TITLE_CANDIDATES", "1001"),
            ("CASS_TITLE_TOTAL_CANDIDATES", "2001"),
            ("CASS_TITLE_MAX_ADMISSION_TITLES", "0"),
            ("CASS_TITLE_MAX_ADMISSION_TITLES", "1000001"),
        ] {
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert!(
                TitleSearchConfig::from_values(|name| {
                    if name == key {
                        Some(value.into())
                    } else if name == "CASS_SEMANTIC_TITLES" {
                        Some("1".into())
                    } else {
                        None
                    }
                })
                .is_err(),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn identical_bodies_allow_titles_to_distinguish_conversations_with_tied_rrf() -> Result<()> {
        let body = scores(&[(1, 0.4), (2, 0.4), (3, 0.4), (4, 0.4)]);
        let titles = scores(&[(4, 0.9), (3, 0.7), (2, 0.5), (1, 0.3)]);
        for fusion in [TitleFusion::Weighted, TitleFusion::Rrf] {
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert_eq!(
                ids(&fuse_title_body(&body, &titles, policy(fusion, 0.25))?),
                [4, 3, 2, 1]
            );
        }
        Ok(())
    }

    #[test]
    fn misleading_title_does_not_erase_stronger_body_evidence_at_fixed_weight() -> Result<()> {
        for fusion in [TitleFusion::Weighted, TitleFusion::Rrf, TitleFusion::Max] {
            let ranked = fuse_title_body(
                &scores(&[(1, 0.95), (2, 0.15)]),
                &scores(&[(2, 0.99), (1, 0.05)]),
                policy(fusion, 0.25),
            )?;
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert_eq!(ranked[0].conversation_id, 1);
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert_eq!(ranked[0].body_score, Some(0.95));
        }
        Ok(())
    }

    #[test]
    fn title_only_candidate_has_no_fabricated_body_score() -> Result<()> {
        let ranked = fuse_title_body(
            &scores(&[(1, 0.1)]),
            &scores(&[(2, 0.95)]),
            policy(TitleFusion::Weighted, 0.25),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ranked[0].conversation_id, 2);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ranked[0].body_score, None);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ranked[0].title_score, Some(0.95));
        Ok(())
    }

    #[test]
    fn duplicate_passages_and_titles_do_not_multiply_rrf_votes() -> Result<()> {
        let once = fuse_title_body(
            &scores(&[(1, 0.8), (2, 0.7)]),
            &scores(&[(2, 0.9)]),
            policy(TitleFusion::Rrf, 0.25),
        )?;
        let repeated = fuse_title_body(
            &scores(&[(1, 0.8), (1, 0.7), (1, 0.6), (2, 0.7)]),
            &scores(&[(2, 0.9), (2, 0.8)]),
            policy(TitleFusion::Rrf, 0.25),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(once, repeated);
        Ok(())
    }

    #[test]
    fn signed_scores_are_not_clipped_and_max_missing_channel_is_absent() -> Result<()> {
        let ranked = fuse_title_body(
            &scores(&[(1, -0.4)]),
            &scores(&[(1, 0.8)]),
            policy(TitleFusion::Weighted, 0.25),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert!((ranked[0].score + 0.1).abs() < 1e-6);
        let maximum = fuse_title_body(
            &scores(&[(1, -0.4), (2, -0.2)]),
            &[],
            policy(TitleFusion::Max, 0.5),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ids(&maximum), [2, 1]);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(maximum[0].score, -0.1);
        // Preserve a finite score slightly beyond one from upstream rounding.
        let outside = fuse_title_body(
            &scores(&[(1, 1.000_001)]),
            &[],
            policy(TitleFusion::Weighted, 0.0),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(outside[0].score, 1.000_001);
        Ok(())
    }

    #[test]
    fn max_at_equal_weight_matches_reporter_order_even_for_negative_scores() -> Result<()> {
        let ranked = fuse_title_body(
            &scores(&[(1, -0.5), (2, 0.8), (3, 0.1)]),
            &scores(&[(1, -0.3), (2, 0.1), (3, 0.95)]),
            policy(TitleFusion::Max, 0.5),
        )?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ids(&ranked), [3, 2, 1]);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ranked[2].score, -0.15);
        Ok(())
    }

    #[test]
    fn endpoints_are_true_channel_ablations_including_negative_evidence() -> Result<()> {
        for fusion in [TitleFusion::Weighted, TitleFusion::Rrf, TitleFusion::Max] {
            let body = scores(&[(1, -0.8), (2, -0.2)]);
            let titles = scores(&[(3, -0.1), (4, -0.7)]);
            let body_only = fuse_title_body(&body, &titles, policy(fusion, 0.0))?;
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert_eq!(ids(&body_only), [2, 1]);
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert!(body_only.iter().all(|hit| hit.title_score.is_none()));
            let title_only = fuse_title_body(&body, &titles, policy(fusion, 1.0))?;
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert_eq!(ids(&title_only), [3, 4]);
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert!(title_only.iter().all(|hit| hit.body_score.is_none()));
        }
        Ok(())
    }

    #[test]
    fn budgets_and_invalid_active_evidence_fail_explicitly() -> Result<()> {
        let config = TitleSearchConfig {
            body_candidates: 1,
            total_candidates: 1,
            ..policy(TitleFusion::Weighted, 0.25)
        };
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert!(fuse_title_body(&scores(&[(1, 0.1), (2, 0.2)]), &[], config).is_err());
        for score in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
            assert!(fuse_title_body(&scores(&[(1, score)]), &[], config).is_err());
        }
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert!(fuse_title_body(&scores(&[(0, 0.1)]), &[], config).is_err());
        let limited = fuse_title_body(&scores(&[(1, 0.8)]), &scores(&[(2, 0.5)]), config)?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(limited.len(), 1);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(limited[0].conversation_id, 1);
        Ok(())
    }

    #[test]
    fn signed_zero_and_input_order_do_not_change_tie_ranks() -> Result<()> {
        let policy = policy(TitleFusion::Rrf, 0.25);
        let titles = scores(&[(1, 0.2), (2, 0.8)]);
        let first = fuse_title_body(&scores(&[(1, -0.0), (2, 0.0)]), &titles, policy)?;
        let reversed = fuse_title_body(&scores(&[(2, -0.0), (1, 0.0)]), &titles, policy)?;
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(first, reversed);
        // ubs:ignore[rust.panic.assert-macros] Intentional #[cfg(test)] assertion.
        assert_eq!(ids(&first), [2, 1]);
        Ok(())
    }
}
