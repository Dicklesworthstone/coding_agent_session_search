use coding_agent_search::search::query::{MatchType, SearchHit};

// Utility: reproduce ranking blend used in the TUI without touching tui.rs
fn blended_score(hit: &SearchHit, max_created: i64, alpha: f32) -> f32 {
    let recency = if max_created > 0 {
        hit.created_at.unwrap_or(0) as f32 / max_created as f32
    } else {
        0.0
    };
    hit.score * hit.match_type.quality_factor() + alpha * recency
}

#[test]
fn exact_hits_rank_above_wildcards_at_equal_recency_and_score() {
    let max_created = 2_000_000;
    let alpha = 0.4; // Balanced mode in TUI

    let exact = SearchHit {
        title: "t".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(max_created),
        line_number: None,
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let prefix = SearchHit {
        match_type: MatchType::Prefix,
        ..exact.clone()
    };
    let suffix = SearchHit {
        match_type: MatchType::Suffix,
        ..exact.clone()
    };
    let substring = SearchHit {
        match_type: MatchType::Substring,
        ..exact.clone()
    };
    let implicit = SearchHit {
        match_type: MatchType::ImplicitWildcard,
        ..exact.clone()
    };

    let exact_score = blended_score(&exact, max_created, alpha);
    let prefix_score = blended_score(&prefix, max_created, alpha);
    let suffix_score = blended_score(&suffix, max_created, alpha);
    let substring_score = blended_score(&substring, max_created, alpha);
    let implicit_score = blended_score(&implicit, max_created, alpha);

    assert!(exact_score > prefix_score);
    assert!(prefix_score > suffix_score);
    assert!(suffix_score > substring_score);
    assert!(substring_score > implicit_score);
}

#[test]
fn recency_boost_can_outweigh_quality_when_far_newer() {
    // Two hits: older exact vs newer suffix wildcard.
    // Using RecentHeavy alpha so recency clearly outranks quality penalty.
    let alpha = 1.0; // RecentHeavy mode

    let older_exact = SearchHit {
        title: "old".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p1".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(1_000_000),
        line_number: None,
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let newer_suffix = SearchHit {
        title: "new".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p2".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(2_000_000),
        line_number: None,
        match_type: MatchType::Suffix, // quality factor 0.8 vs 1.0
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let max_created = newer_suffix.created_at.unwrap();
    let older_score = blended_score(&older_exact, max_created, alpha);
    let newer_score = blended_score(&newer_suffix, max_created, alpha);

    assert!(
        newer_score > older_score,
        "recency boost should let much newer suffix beat older exact: {newer_score} > {older_score}"
    );
}

#[test]
fn relevance_heavy_mode_prefers_quality_over_recency() {
    // With RelevanceHeavy alpha (0.1), quality factor matters more than recency.
    let alpha = 0.1; // RelevanceHeavy mode
    let max_created = 2_000_000;

    let older_exact = SearchHit {
        title: "old_exact".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p1".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(500_000), // Much older
        line_number: None,
        match_type: MatchType::Exact, // quality factor 1.0
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let newer_substring = SearchHit {
        title: "new_substring".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p2".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(max_created), // Most recent
        line_number: None,
        match_type: MatchType::Substring, // quality factor 0.7
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let older_score = blended_score(&older_exact, max_created, alpha);
    let newer_score = blended_score(&newer_substring, max_created, alpha);

    // With low alpha, exact match (1.0 * 1.0 = 1.0) + small recency should beat
    // substring (1.0 * 0.7 = 0.7) + full recency
    assert!(
        older_score > newer_score,
        "relevance-heavy: older exact ({older_score}) should beat newer substring ({newer_score})"
    );
}

#[test]
fn match_quality_heavy_mode_balances_quality_and_recency() {
    // MatchQualityHeavy uses alpha=0.2, moderate recency influence.
    let alpha = 0.2;
    let max_created = 2_000_000;

    let exact = SearchHit {
        title: "exact".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(max_created),
        line_number: None,
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let implicit = SearchHit {
        match_type: MatchType::ImplicitWildcard, // quality factor 0.6
        ..exact.clone()
    };

    let exact_score = blended_score(&exact, max_created, alpha);
    let implicit_score = blended_score(&implicit, max_created, alpha);

    // Quality difference: 1.0 - 0.6 = 0.4
    // Both have same recency, so exact should clearly win
    assert!(
        exact_score > implicit_score,
        "match-quality: exact ({exact_score}) should beat implicit ({implicit_score})"
    );

    // The gap should be roughly 0.4 (quality difference) at same recency
    let gap = exact_score - implicit_score;
    assert!(
        gap > 0.3 && gap < 0.5,
        "quality gap should be ~0.4, got {gap}"
    );
}

#[test]
fn ranking_handles_missing_created_at() {
    // Hits without created_at should still rank based on score * quality_factor
    let max_created = 2_000_000;
    let alpha = 0.4;

    let hit_with_date = SearchHit {
        title: "with_date".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p1".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(max_created),
        line_number: None,
        match_type: MatchType::Prefix, // quality factor 0.9
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let hit_without_date = SearchHit {
        title: "no_date".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 1.0,
        source_path: "p2".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: None, // Missing date
        line_number: None,
        match_type: MatchType::Exact, // quality factor 1.0
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let with_date_score = blended_score(&hit_with_date, max_created, alpha);
    let no_date_score = blended_score(&hit_without_date, max_created, alpha);

    // No date means recency = 0, so score = 1.0 * 1.0 + 0 = 1.0
    // With date at max: score = 1.0 * 0.9 + 0.4 * 1.0 = 1.3
    // The hit with date + recency should win despite lower quality
    assert!(
        with_date_score > no_date_score,
        "hit with date ({with_date_score}) should beat hit without ({no_date_score})"
    );
}

#[test]
fn ranking_handles_zero_max_created() {
    // Edge case: when max_created is 0, recency should be 0
    let max_created = 0;
    let alpha = 0.4;

    let hit = SearchHit {
        title: "t".into(),
        snippet: "s".into(),
        content: "c".into(),
        content_hash: 0,
        score: 2.0,
        source_path: "p".into(),
        agent: "a".into(),
        workspace: "w".into(),
        workspace_original: None,
        created_at: Some(1_000_000),
        line_number: None,
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: None,
    };

    let score = blended_score(&hit, max_created, alpha);
    // recency = 0 (because max_created=0), so score = 2.0 * 1.0 + 0 = 2.0
    assert!(
        (score - 2.0).abs() < 0.001,
        "score with max_created=0 should be just score*quality: {score}"
    );
}

#[test]
fn all_ranking_modes_maintain_quality_ordering_at_equal_inputs() {
    // At equal recency and Tantivy score, all modes should preserve quality ordering:
    // Exact > Prefix > Suffix > Substring > ImplicitWildcard
    let max_created = 1_000_000;
    let alphas = [1.0, 0.4, 0.2, 0.1]; // RecentHeavy, Balanced, MatchQuality, Relevance

    for alpha in alphas {
        let base = SearchHit {
            title: "t".into(),
            snippet: "s".into(),
            content: "c".into(),
            content_hash: 0,
            score: 1.0,
            source_path: "p".into(),
            agent: "a".into(),
            workspace: "w".into(),
            workspace_original: None,
            created_at: Some(max_created),
            line_number: None,
            match_type: MatchType::Exact,
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: None,
        };

        let exact_score = blended_score(&base, max_created, alpha);
        let prefix_score = blended_score(
            &SearchHit {
                match_type: MatchType::Prefix,
                ..base.clone()
            },
            max_created,
            alpha,
        );
        let suffix_score = blended_score(
            &SearchHit {
                match_type: MatchType::Suffix,
                ..base.clone()
            },
            max_created,
            alpha,
        );
        let substring_score = blended_score(
            &SearchHit {
                match_type: MatchType::Substring,
                ..base.clone()
            },
            max_created,
            alpha,
        );
        let implicit_score = blended_score(
            &SearchHit {
                match_type: MatchType::ImplicitWildcard,
                ..base.clone()
            },
            max_created,
            alpha,
        );

        assert!(exact_score > prefix_score, "alpha={alpha}: exact > prefix");
        assert!(
            prefix_score > suffix_score,
            "alpha={alpha}: prefix > suffix"
        );
        assert!(
            suffix_score > substring_score,
            "alpha={alpha}: suffix > substring"
        );
        assert!(
            substring_score > implicit_score,
            "alpha={alpha}: substring > implicit"
        );
    }
}
