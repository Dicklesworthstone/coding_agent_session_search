//! TUI ranking modes through the real `ui::ranking::rank_hits`.
//!
//! This file used to re-implement a blend formula locally and assert on that
//! copy; the TUI never ran it, so every mode displayed engine order for any
//! non-empty query while these tests passed (bead
//! coding_agent_session_search-2l1b0.53). Every assertion now goes through
//! the function the TUI calls when results arrive and on F12.

use coding_agent_search::search::query::{MatchType, SearchHit};
use coding_agent_search::ui::app::RankingMode;
use coding_agent_search::ui::ranking::{RECENCY_HALF_LIFE_DAYS, rank_hits, recency_factor};

const NOW: i64 = 1_790_000_000;
const DAY: i64 = 86_400;

const ALL_MODES: [RankingMode; 6] = [
    RankingMode::RecentHeavy,
    RankingMode::Balanced,
    RankingMode::RelevanceHeavy,
    RankingMode::MatchQualityHeavy,
    RankingMode::DateNewest,
    RankingMode::DateOldest,
];

fn hit(title: &str, score: f32, age_days: Option<i64>, match_type: MatchType) -> SearchHit {
    SearchHit {
        title: title.into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score,
        source_path: format!("/sessions/{title}.jsonl"),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: age_days.map(|days| (NOW - days * DAY) * 1_000),
        line_number: None,
        match_type,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    }
}

fn order(mut hits: Vec<SearchHit>, mode: RankingMode) -> Vec<String> {
    rank_hits(&mut hits, mode, NOW);
    hits.into_iter().map(|hit| hit.title).collect()
}

#[test]
fn match_quality_orders_match_classes_at_equal_score_and_age() {
    let hits = vec![
        hit("implicit", 1.0, Some(1), MatchType::ImplicitWildcard),
        hit("substring", 1.0, Some(1), MatchType::Substring),
        hit("wildcard", 1.0, Some(1), MatchType::Wildcard),
        hit("suffix", 1.0, Some(1), MatchType::Suffix),
        hit("prefix", 1.0, Some(1), MatchType::Prefix),
        hit("exact", 1.0, Some(1), MatchType::Exact),
    ];
    assert_eq!(
        order(hits, RankingMode::MatchQualityHeavy),
        [
            "exact",
            "prefix",
            "suffix",
            "substring",
            "wildcard",
            "implicit"
        ]
    );
}

#[test]
fn recent_heavy_lets_a_far_newer_weaker_match_beat_an_older_stronger_one() {
    let hits = vec![
        hit("old-strong", 1.0, Some(90), MatchType::Exact),
        hit("new-weaker", 0.6, Some(0), MatchType::Suffix),
        hit("floor", 0.2, Some(365), MatchType::Exact),
    ];
    assert_eq!(
        order(hits.clone(), RankingMode::RecentHeavy)[0],
        "new-weaker"
    );
    // The same hits under Relevance Heavy keep the stronger match on top:
    // the mode choice, not the input, decides.
    assert_eq!(order(hits, RankingMode::RelevanceHeavy)[0], "old-strong");
}

#[test]
fn match_quality_keeps_an_exact_hit_above_a_newer_stronger_fallback_hit() {
    let hits = vec![
        hit("fallback", 5.0, Some(0), MatchType::ImplicitWildcard),
        hit("exact", 1.0, Some(60), MatchType::Exact),
    ];
    assert_eq!(
        order(hits, RankingMode::MatchQualityHeavy),
        ["exact", "fallback"]
    );
}

#[test]
fn undated_hits_score_no_recency_and_sort_last_by_date() {
    assert_eq!(recency_factor(None, NOW), 0.0);
    let hits = vec![
        hit("undated", 3.0, None, MatchType::Exact),
        hit("dated", 3.0, Some(2), MatchType::Exact),
    ];
    for mode in [
        RankingMode::RecentHeavy,
        RankingMode::DateNewest,
        RankingMode::DateOldest,
    ] {
        assert_eq!(order(hits.clone(), mode), ["dated", "undated"], "{mode:?}");
    }
}

#[test]
fn recency_follows_the_documented_half_life() {
    let at_half_life = (NOW - (RECENCY_HALF_LIFE_DAYS as i64) * DAY) * 1_000;
    assert!((recency_factor(Some(at_half_life), NOW) - 0.5).abs() < 1e-9);
    let week = recency_factor(Some((NOW - 7 * DAY) * 1_000), NOW);
    let month = recency_factor(Some((NOW - 30 * DAY) * 1_000), NOW);
    assert!((0.70..0.72).contains(&week), "week {week}");
    assert!((0.22..0.24).contains(&month), "month {month}");
}

#[test]
fn every_mode_is_a_permutation_of_the_loaded_hits() {
    let hits: Vec<SearchHit> = (0..40_i32)
        .map(|i| {
            hit(
                &format!("h{i:02}"),
                (i * 37 % 11) as f32,
                (i % 5 != 0).then_some(i64::from(i) * 3),
                if i % 3 == 0 {
                    MatchType::Prefix
                } else {
                    MatchType::Exact
                },
            )
        })
        .collect();
    let mut expected: Vec<String> = hits.iter().map(|hit| hit.title.clone()).collect();
    expected.sort();
    for mode in ALL_MODES {
        let mut got = order(hits.clone(), mode);
        got.sort();
        assert_eq!(got, expected, "{mode:?} lost or duplicated a hit");
    }
}
