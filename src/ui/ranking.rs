//! Result ordering for the TUI ranking modes (F12 / Alt+R).
//!
//! The engine returns hits in its own relevance order. Each mode re-orders
//! the hits the TUI has loaded (the first page plus any further pages the
//! user loads) with the formulas below, which README "Ranking & Scoring
//! Explained" describes exactly. Before this module every mode except the
//! empty-query date browse left engine order untouched
//! (bead coding_agent_session_search-2l1b0.53).
//!
//! - relevance: the engine score min-max normalized over the loaded hits
//!   (1.0 = best loaded hit; every hit gets 1.0 when all scores tie);
//! - recency: `0.5 ^ (age_days / RECENCY_HALF_LIFE_DAYS)`, so 1.0 now, about
//!   0.71 after a week, about 0.23 after a month; undated hits get 0.0;
//! - Recent Heavy `0.3·relevance + 0.7·recency`, Balanced `0.5/0.5`,
//!   Relevance Heavy `0.8/0.2`;
//! - Match Quality orders by match class first (exact, prefix, suffix,
//!   substring, wildcard, implicit fallback) and by the Relevance Heavy
//!   blend within a class;
//! - Date Newest / Date Oldest sort by `created_at`, undated hits last.
//!
//! Sorting is stable, so equal keys keep engine order.

use crate::search::query::SearchHit;

use super::app::{RankingMode, ts_to_secs};

/// Age at which the recency factor is 0.5.
pub const RECENCY_HALF_LIFE_DAYS: f64 = 14.0;

/// Recency in `[0, 1]`: 1.0 for a hit from now (or the future), halving every
/// [`RECENCY_HALF_LIFE_DAYS`]; 0.0 for an undated hit.
pub fn recency_factor(created_at: Option<i64>, now_secs: i64) -> f64 {
    let Some(created_at) = created_at else {
        return 0.0;
    };
    let age_secs = now_secs.saturating_sub(ts_to_secs(created_at)).max(0);
    0.5_f64.powf(age_secs as f64 / 86_400.0 / RECENCY_HALF_LIFE_DAYS)
}

/// `(relevance, recency)` weights of a blended mode; `None` for the date modes.
pub fn blend_weights(mode: RankingMode) -> Option<(f64, f64)> {
    match mode {
        RankingMode::RecentHeavy => Some((0.3, 0.7)),
        RankingMode::Balanced => Some((0.5, 0.5)),
        RankingMode::RelevanceHeavy | RankingMode::MatchQualityHeavy => Some((0.8, 0.2)),
        RankingMode::DateNewest | RankingMode::DateOldest => None,
    }
}

/// Re-order `hits` for `mode` as of `now_secs` (Unix seconds).
pub fn rank_hits(hits: &mut Vec<SearchHit>, mode: RankingMode, now_secs: i64) {
    let Some((relevance_weight, recency_weight)) = blend_weights(mode) else {
        let newest_first = mode == RankingMode::DateNewest;
        hits.sort_by(
            |a, b| match (a.created_at.map(ts_to_secs), b.created_at.map(ts_to_secs)) {
                (Some(a), Some(b)) if newest_first => b.cmp(&a),
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        );
        return;
    };

    let finite_scores = hits
        .iter()
        .map(|hit| f64::from(hit.score))
        .filter(|score| score.is_finite());
    let (min, max) = finite_scores.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| {
        (lo.min(s), hi.max(s))
    });
    let relevance = |score: f32| {
        let score = f64::from(score);
        if !score.is_finite() {
            0.0
        } else if max > min {
            (score - min) / (max - min)
        } else {
            1.0
        }
    };

    let mut keyed: Vec<(f64, f64, SearchHit)> = std::mem::take(hits)
        .into_iter()
        .map(|hit| {
            let class = if mode == RankingMode::MatchQualityHeavy {
                f64::from(hit.match_type.quality_factor())
            } else {
                0.0
            };
            let blend = relevance_weight * relevance(hit.score)
                + recency_weight * recency_factor(hit.created_at, now_secs);
            (class, blend, hit)
        })
        .collect();
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1)));
    hits.extend(keyed.into_iter().map(|(_, _, hit)| hit));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::MatchType;

    const NOW: i64 = 1_790_000_000;
    const DAY: i64 = 86_400;

    fn hit(title: &str, score: f32, age_days: Option<i64>, match_type: MatchType) -> SearchHit {
        SearchHit {
            title: title.to_string(),
            snippet: String::new(),
            content: String::new(),
            content_hash: 0,
            conversation_id: None,
            score,
            source_path: format!("/sessions/{title}.jsonl"),
            agent: "codex".to_string(),
            workspace: "/w".to_string(),
            workspace_original: None,
            // Milliseconds, as connectors store them.
            created_at: age_days.map(|days| (NOW - days * DAY) * 1_000),
            line_number: Some(1),
            match_type,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        }
    }

    /// Engine order: an old strong match, a fresh weaker match, and an undated
    /// weakest one. Relevance and recency disagree on purpose. Normalized
    /// relevance is 1.0 / 0.33 / 0.0; recency is ~0.003 / 1.0 / 0.0.
    fn disagreeing_hits() -> Vec<SearchHit> {
        vec![
            hit("old-strong", 10.0, Some(120), MatchType::Exact),
            hit("new-weak", 4.0, Some(0), MatchType::Exact),
            hit("undated", 1.0, None, MatchType::Exact),
        ]
    }

    fn ranked(mut hits: Vec<SearchHit>, mode: RankingMode) -> Vec<String> {
        rank_hits(&mut hits, mode, NOW);
        hits.into_iter().map(|hit| hit.title).collect()
    }

    #[test]
    fn recency_halves_every_half_life_and_ignores_the_timestamp_unit() {
        assert_eq!(recency_factor(Some(NOW * 1_000), NOW), 1.0);
        let half_life_ms = (NOW - 14 * DAY) * 1_000;
        assert!((recency_factor(Some(half_life_ms), NOW) - 0.5).abs() < 1e-9);
        let half_life_secs = NOW - 14 * DAY;
        assert!((recency_factor(Some(half_life_secs), NOW) - 0.5).abs() < 1e-9);
        assert_eq!(recency_factor(None, NOW), 0.0);
        assert_eq!(recency_factor(Some((NOW + DAY) * 1_000), NOW), 1.0);
    }

    /// The bead's negative case: the old code returned engine order in every
    /// mode, so these differing orders are exactly what it failed to produce.
    #[test]
    fn modes_disagree_where_relevance_and_recency_disagree() {
        // Relevance Heavy 0.80 / 0.47 / 0.00, Recent Heavy 0.30 / 0.80 / 0.00,
        // Balanced 0.50 / 0.67 / 0.00.
        assert_eq!(
            ranked(disagreeing_hits(), RankingMode::RelevanceHeavy),
            ["old-strong", "new-weak", "undated"]
        );
        assert_eq!(
            ranked(disagreeing_hits(), RankingMode::RecentHeavy),
            ["new-weak", "old-strong", "undated"]
        );
        assert_eq!(
            ranked(disagreeing_hits(), RankingMode::Balanced),
            ["new-weak", "old-strong", "undated"]
        );
    }

    #[test]
    fn date_modes_sort_by_created_at_with_undated_last() {
        let hits = vec![
            hit("middle", 1.0, Some(10), MatchType::Exact),
            hit("undated", 9.0, None, MatchType::Exact),
            hit("newest", 1.0, Some(1), MatchType::Exact),
            hit("oldest", 5.0, Some(400), MatchType::Exact),
        ];
        assert_eq!(
            ranked(hits.clone(), RankingMode::DateNewest),
            ["newest", "middle", "oldest", "undated"]
        );
        assert_eq!(
            ranked(hits, RankingMode::DateOldest),
            ["oldest", "middle", "newest", "undated"]
        );
    }

    #[test]
    fn match_quality_puts_every_exact_hit_above_a_stronger_fallback_hit() {
        let hits = vec![
            hit(
                "fallback-strong",
                50.0,
                Some(0),
                MatchType::ImplicitWildcard,
            ),
            hit("prefix", 5.0, Some(3), MatchType::Prefix),
            hit("exact-weak", 1.0, Some(30), MatchType::Exact),
        ];
        assert_eq!(
            ranked(hits.clone(), RankingMode::MatchQualityHeavy),
            ["exact-weak", "prefix", "fallback-strong"]
        );
        // Relevance Heavy has the same blend but no class order, so the
        // strong fallback hit stays on top there.
        assert_eq!(
            ranked(hits, RankingMode::RelevanceHeavy)[0],
            "fallback-strong"
        );
    }

    #[test]
    fn tied_keys_keep_engine_order_and_non_finite_scores_sink() {
        let tied = vec![
            hit("first", 3.0, Some(2), MatchType::Exact),
            hit("second", 3.0, Some(2), MatchType::Exact),
            hit("nan", f32::NAN, Some(2), MatchType::Exact),
        ];
        assert_eq!(
            ranked(tied, RankingMode::RelevanceHeavy),
            ["first", "second", "nan"]
        );
        // All scores equal (e.g. an empty-query date browse): relevance is 1.0
        // for every hit and recency decides.
        let browse = vec![
            hit("older", 0.0, Some(5), MatchType::Exact),
            hit("newer", 0.0, Some(1), MatchType::Exact),
        ];
        assert_eq!(ranked(browse, RankingMode::Balanced), ["newer", "older"]);
    }
}
