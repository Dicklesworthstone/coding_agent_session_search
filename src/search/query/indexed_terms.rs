//! Query-only compatibility with the tokenizer in CASS's released v9 index.
//!
//! Based on the contributor's tokenizer reuse in Frankensearch PR #74. The
//! upstream fix cannot yet be selected through CASS's published dependencies:
//! current Frankensearch main also changes durable identity/schema. Keep the
//! registry source contract and existing indexes, and lower affected operands
//! through public Quill APIs until a compatible release carries the correction.

use frankensearch::quill::query::{
    BooleanClause, BooleanOperator, CassQueryFilters, CassQueryParser, Occur, PositionedTerm,
    Query, QueryField, QueryNode, QueryNodeId,
};
use frankensearch::quill::schema::Analyzer;
use frankensearch::quill::scribe::{CassAnalyzer, TokenAnalyzer};

use super::{
    CassBoolExpr, CassBoolParser, CassBoolToken, CassBoolUnsupported, FsCassQueryToken,
    FsCassWildcardPattern, cass_bool_nesting_is_bounded, cass_bool_tokens, fs_cass_sanitize_query,
};
use crate::search::quill_bridge::QuillCassFields;

const MAX_QUERY_SCALARS: usize = 10_000;
const MAX_INDEXED_FRAGMENTS: usize = 256;
const MAX_INDEXED_TERM_BYTES: usize = 256;

/// Lower the same query for ranked pages, exact counts, retries, and warming.
pub(super) fn parse(parser: &CassQueryParser, raw: &str, filters: &CassQueryFilters) -> Query {
    if raw.trim().is_empty() {
        return parser.parse(raw, filters).query;
    }
    let requires_fragments = raw.chars().any(needs_indexed_fragments);
    let cutoff = raw
        .char_indices()
        .nth(MAX_QUERY_SCALARS)
        .map(|(offset, _)| offset);
    if requires_fragments && cutoff.is_some() {
        return Query::empty();
    }
    // Preserve the released parser's ordinary ASCII truncation. Tokenization
    // and the recursive compatibility parser never see an unbounded input.
    let admitted = cutoff.map_or(raw, |offset| &raw[..offset]);
    let tokens = cass_bool_tokens(admitted);
    if !cass_bool_nesting_is_bounded(&tokens) {
        return Query::empty();
    }
    let has_empty_operand = tokens.iter().any(|token| match token {
        CassBoolToken::Token(FsCassQueryToken::Term(text) | FsCassQueryToken::Phrase(text)) => {
            !text.chars().any(char::is_alphanumeric) && !only_stars_and_whitespace(text)
        }
        _ => false,
    });
    if !requires_fragments && !has_empty_operand && !tokens.is_empty() {
        return parser.parse(raw, filters).query;
    }
    if cutoff.is_some() {
        return Query::empty();
    }
    let lowered = CassBoolParser::parse(&tokens, |token| match token {
        FsCassQueryToken::Term(text) => lower_term(text),
        FsCassQueryToken::Phrase(text) => lower_phrase(text),
        _ => Ok(None),
    });
    let expression = match lowered {
        Ok(Some(expression)) => expression,
        // Explicit unindexable operands have already become contradictions.
        // Only deliberately skipped stars/recovered empty groups remain.
        Ok(None) => return parser.parse("", filters).query,
        Err(_) => return Query::empty(),
    };
    let (query, negative) = lower_expression(expression);
    let query = if negative { complement(query) } else { query };
    apply_native_filters(parser, query, filters)
}

fn only_stars_and_whitespace(raw: &str) -> bool {
    raw.contains('*') && raw.chars().all(|ch| ch == '*' || ch.is_whitespace())
}

// These are the exact ranges in the released CASS tokenizer. Broadening them
// would change the query/index language contract rather than repair it.
fn is_cass_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{4E00}'..='\u{9FFF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{3100}'..='\u{312F}'
            | '\u{3300}'..='\u{33FF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
    )
}

fn needs_indexed_fragments(ch: char) -> bool {
    !ch.is_ascii() && !is_cass_cjk(ch)
}

struct IndexedFragments {
    terms: Vec<String>,
    ends_with_token: bool,
}

/// Analyze original case before any Unicode mapping. In particular, İ must
/// remain an index separator, rather than first becoming an indexed ASCII i.
fn indexed_fragments(raw: &str) -> Result<Option<IndexedFragments>, CassBoolUnsupported> {
    if !raw.chars().any(needs_indexed_fragments) || raw.chars().any(is_cass_cjk) {
        return Ok(None);
    }
    let mut terms = Vec::new();
    let mut covered_to = 0;
    let mut rejected = false;
    CassAnalyzer::default().analyze(Analyzer::CassPrefixNormalize, raw, &mut |token| {
        // The released analyzer silently omits >256-byte tokens. Detect any
        // such required ASCII run in the gaps instead of weakening AND/NOT.
        if token.offset_from > covered_to {
            rejected |= raw[covered_to..token.offset_from]
                .bytes()
                .any(|byte| byte.is_ascii_alphanumeric());
        }
        covered_to = covered_to.max(token.offset_to);
        if terms.len() == MAX_INDEXED_FRAGMENTS {
            rejected = true;
        } else {
            terms.push(token.text.clone());
        }
    });
    rejected |= raw[covered_to..]
        .bytes()
        .any(|byte| byte.is_ascii_alphanumeric());
    if rejected {
        return Err(CassBoolUnsupported);
    }
    Ok(Some(IndexedFragments {
        terms,
        ends_with_token: covered_to == raw.len(),
    }))
}

fn lower_term(raw: &str) -> Result<Option<Query>, CassBoolUnsupported> {
    let parts = fs_cass_sanitize_query(raw)
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let query = lower_compound(&parts)?;
    if query.is_none() && !raw.trim().is_empty() && !only_stars_and_whitespace(raw) {
        return Ok(Some(impossible()));
    }
    Ok(query)
}

fn lower_compound(parts: &[String]) -> Result<Option<Query>, CassBoolUnsupported> {
    let mut queries = Vec::new();
    for part in parts {
        if let Some(query) = lower_term_part(part)? {
            queries.push(query);
        }
    }
    Ok((!queries.is_empty()).then(|| conjunction(queries)))
}

fn searchable_fields() -> Vec<QueryField> {
    let fields = QuillCassFields::compiled();
    [
        fields.title,
        fields.content,
        fields.title_prefix,
        fields.content_prefix,
    ]
    .into_iter()
    .map(|field| QueryField::new(field, 1.0))
    .collect()
}

fn lower_term_part(raw: &str) -> Result<Option<Query>, CassBoolUnsupported> {
    let pattern = FsCassWildcardPattern::parse(raw);
    let prefix = matches!(pattern, FsCassWildcardPattern::Prefix(_));
    match pattern {
        FsCassWildcardPattern::Exact(term) | FsCassWildcardPattern::Prefix(term) => {
            if term.is_empty() {
                return Ok(None);
            }
            if let Some(fragments) = indexed_fragments(raw.trim_matches('*'))? {
                return Ok(Some(lower_fragments(
                    &fragments.terms,
                    prefix && fragments.ends_with_token,
                )));
            }
            if term.chars().any(is_cass_cjk) {
                let chars = term
                    .chars()
                    .filter(|ch| is_cass_cjk(*ch))
                    .collect::<Vec<_>>();
                let terms = if chars.len() <= 1 {
                    chars
                        .into_iter()
                        .map(|ch| ch.to_string())
                        .collect::<Vec<_>>()
                } else {
                    chars.windows(2).map(|pair| pair.iter().collect()).collect()
                };
                return Ok(Some(conjunction(
                    terms
                        .into_iter()
                        .map(|text| Query::term(searchable_fields(), text))
                        .collect(),
                )));
            }
            Ok(Some(Query::term(searchable_fields(), term)))
        }
        // Existing indexes cannot express these wildcard shapes for accented
        // words. Keep the native behavior; never expand dictionary terms.
        FsCassWildcardPattern::Suffix(_)
        | FsCassWildcardPattern::Substring(_)
        | FsCassWildcardPattern::Complex(_) => {
            let fields = QuillCassFields::compiled();
            Ok(Some(Query::glob(
                vec![fields.content, fields.title],
                raw.to_lowercase(),
            )))
        }
    }
}

fn lower_fragments(terms: &[String], prefix: bool) -> Query {
    if terms.is_empty() {
        return impossible();
    }
    let fields = QuillCassFields::compiled();
    let alternatives = [
        (fields.title, fields.title_prefix),
        (fields.content, fields.content_prefix),
    ]
    .into_iter()
    .map(|(field, prefix_field)| {
        let required = terms
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let mut fields = vec![QueryField::new(field, 1.0)];
                if prefix && index + 1 == terms.len() {
                    fields.push(QueryField::new(prefix_field, 1.0));
                }
                Query::term(fields, text.clone())
            })
            .collect();
        BooleanClause::new(Occur::Should, conjunction(required))
    })
    .collect();
    Query::boolean(alternatives, Some(BooleanOperator::Or))
}

fn lower_phrase(raw: &str) -> Result<Option<Query>, CassBoolUnsupported> {
    let mut terms = Vec::new();
    let mut original_terms = Vec::new();
    let mut normalized = false;
    let mut unindexable = false;
    for part in fs_cass_sanitize_query(raw).split_whitespace() {
        let part = part.trim_matches('*');
        if part.is_empty() {
            continue;
        }
        original_terms.push(part.to_owned());
        match if part.contains('*') {
            // Quoted interior stars remain literal/legacy wildcard syntax;
            // tokenizing them as separators would invent new matches.
            None
        } else {
            indexed_fragments(part)?
        } {
            Some(fragments) => {
                normalized = true;
                unindexable |= fragments.terms.is_empty();
                terms.extend(fragments.terms);
            }
            None => terms.push(part.to_lowercase()),
        }
        if normalized
            && (terms.len() > MAX_INDEXED_FRAGMENTS
                || terms
                    .iter()
                    .any(|term| term.is_ascii() && term.len() > MAX_INDEXED_TERM_BYTES))
        {
            return Err(CassBoolUnsupported);
        }
    }
    if unindexable {
        return Ok(Some(impossible()));
    }
    if terms.is_empty() {
        return Ok(if only_stars_and_whitespace(raw) {
            None
        } else {
            Some(impossible())
        });
    }
    let fields = QuillCassFields::compiled();
    let exact_fields = vec![
        QueryField::new(fields.title, 1.0),
        QueryField::new(fields.content, 1.0),
    ];
    if normalized && terms.len() == 1 {
        return Ok(Some(lower_fragments(&terms, false)));
    }
    if terms.iter().any(|term| term.chars().any(is_cass_cjk)) {
        // CJK phrases keep their legacy non-positional AND. Preserve each
        // original affected word's exact same-field group: flattening it into
        // separate legacy terms would reintroduce prefix/cross-field matches.
        return lower_compound(&original_terms);
    }
    if terms.len() <= 1 {
        // Unchanged ASCII single-word phrases keep existing prefix fields.
        return lower_compound(&terms);
    }
    Ok(Some(Query::phrase(
        exact_fields,
        terms
            .into_iter()
            .zip(0_u32..)
            .map(|(text, position)| PositionedTerm::new(position, text))
            .collect(),
        0,
        false,
    )))
}

fn conjunction(mut queries: Vec<Query>) -> Query {
    if queries.len() == 1 {
        return queries.pop().unwrap_or_else(Query::empty);
    }
    Query::boolean(
        queries
            .into_iter()
            .map(|query| BooleanClause::new(Occur::Must, query))
            .collect(),
        Some(BooleanOperator::And),
    )
}

fn complement(query: Query) -> Query {
    Query::boolean(
        vec![
            BooleanClause::new(Occur::Must, Query::all()),
            BooleanClause::new(Occur::MustNot, query),
        ],
        None,
    )
}

fn impossible() -> Query {
    complement(Query::all())
}

/// Match the native parser's negative-clause scoring: direct exclusions do
/// not score, while root/OR complements need an explicit universe.
fn lower_expression(expression: CassBoolExpr<Query>) -> (Query, bool) {
    match expression {
        CassBoolExpr::Operand(query) => (query, false),
        CassBoolExpr::Not(inner) => {
            let (query, negative) = lower_expression(*inner);
            (query, !negative)
        }
        CassBoolExpr::And(operands) => {
            let mut clauses: Vec<_> = operands
                .into_iter()
                .map(|operand| {
                    let (query, negative) = lower_expression(operand);
                    BooleanClause::new(
                        if negative {
                            Occur::MustNot
                        } else {
                            Occur::Must
                        },
                        query,
                    )
                })
                .collect();
            if clauses.iter().all(|clause| clause.occur == Occur::MustNot) {
                clauses.insert(0, BooleanClause::new(Occur::Must, Query::all()));
            }
            (Query::boolean(clauses, Some(BooleanOperator::And)), false)
        }
        CassBoolExpr::Or(operands) => (
            Query::boolean(
                operands
                    .into_iter()
                    .map(|operand| {
                        let (query, negative) = lower_expression(operand);
                        BooleanClause::new(
                            Occur::Should,
                            if negative { complement(query) } else { query },
                        )
                    })
                    .collect(),
                Some(BooleanOperator::Or),
            ),
            false,
        ),
    }
}

fn apply_native_filters(
    parser: &CassQueryParser,
    query: Query,
    filters: &CassQueryFilters,
) -> Query {
    let native = parser.parse("", filters).query;
    if native.allocation_failure().is_some() {
        return Query::empty();
    }
    let mut replacement = Some(query);
    let filtered = replace_filter_root(&native, native.root_id(), &mut replacement);
    if replacement.is_some() {
        // The native filter tree must retain its one match-all root. Fail
        // closed if a later dependency changes that public-tree contract.
        Query::empty()
    } else {
        filtered
    }
}

/// The native filter-only tree is shallow (root, OR sets, leaves), regardless
/// of user query nesting. Replacing its All leaf preserves filter scoring and
/// source/workspace/time semantics without interpreting filter strings.
fn replace_filter_root(native: &Query, id: QueryNodeId, replacement: &mut Option<Query>) -> Query {
    match native.node(id) {
        QueryNode::All => replacement.take().unwrap_or_else(Query::all),
        QueryNode::Empty | QueryNode::AllocationFailure { .. } => Query::empty(),
        QueryNode::Term { fields, text } => Query::term(fields.clone(), text.clone()),
        QueryNode::Phrase {
            fields,
            terms,
            slop,
            prefix,
        } => Query::phrase(fields.clone(), terms.clone(), *slop, *prefix),
        QueryNode::Boolean { clauses, operator } => Query::boolean(
            clauses
                .iter()
                .map(|clause| {
                    BooleanClause::new(
                        clause.occur,
                        replace_filter_root(native, clause.query, replacement),
                    )
                })
                .collect(),
            *operator,
        ),
        QueryNode::Range {
            field_id,
            lower,
            upper,
        } => Query::range(*field_id, lower.clone(), upper.clone()),
        QueryNode::Set { field_id, values } => Query::set(*field_id, values.clone()),
        QueryNode::Glob { field_ids, pattern } => Query::glob(field_ids.clone(), pattern.clone()),
        QueryNode::Boost { query, factor } => {
            Query::boost(replace_filter_root(native, *query, replacement), *factor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankensearch::quill::query::CassSourceFilter;
    use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;

    fn parser() -> CassQueryParser {
        CassQueryParser::new(CASS_SEMANTIC_SCHEMA).expect("released CASS schema")
    }

    #[test]
    fn unchanged_queries_and_native_filters_keep_their_query_trees() {
        let parser = parser();
        let filters = CassQueryFilters {
            agents: vec!["ÁGENT OR *".to_owned()],
            workspaces: vec!["/work/árvíz".to_owned()],
            created_from: Some(123),
            created_to: Some(456),
            source_filter: CassSourceFilter::SourceId("source:É/NOT".to_owned()),
        };
        for raw in [
            "",
            "backup",
            "backup OR test AND -debug",
            "NOT backup AND NOT debug",
            "\"hello world\"",
            "\"AND\"",
            "ca*",
            "*fix",
            "m*d",
            "*",
            "***\u{2003}",
            "\"*** \u{2003}\"",
            "日本語",
            "\"日本語 検索\"",
        ] {
            assert_eq!(
                parse(&parser, raw, &filters),
                parser.parse(raw, &filters).query,
                "unaffected query {raw:?}"
            );
            let unfiltered = parser.parse(raw, &CassQueryFilters::default()).query;
            assert_eq!(
                apply_native_filters(&parser, unfiltered, &filters),
                parser.parse(raw, &filters).query,
                "native filter shape/scoring for {raw:?}"
            );
        }
    }

    #[test]
    fn tokenizer_runs_before_case_mapping_and_preserves_empty_results() {
        for (raw, expected) in [
            ("KÉRLEK", vec!["k", "rlek"]),
            ("İstanbul", vec!["stanbul"]),
            ("GRÖẞERE", vec!["gr", "ere"]),
            ("Straße", vec!["stra", "e"]),
            ("cafe\u{301}", vec!["cafe"]),
            ("éß", vec![]),
        ] {
            let fragments = indexed_fragments(raw)
                .ok()
                .flatten()
                .expect("affected operand");
            assert_eq!(fragments.terms, expected, "{raw:?}");
        }
        assert!(indexed_fragments("日本語é").is_ok_and(|value| value.is_none()));
        assert!(indexed_fragments(&format!("{}é", "a".repeat(257))).is_err());
        assert!(indexed_fragments(&"aé".repeat(257)).is_err());
    }

    #[test]
    fn required_empty_quotes_survive_the_compatibility_lexer() {
        assert_eq!(
            cass_bool_tokens("kérlek AND \"\""),
            vec![
                CassBoolToken::Token(FsCassQueryToken::Term("kérlek".to_owned())),
                CassBoolToken::Token(FsCassQueryToken::And),
                CassBoolToken::Token(FsCassQueryToken::Phrase(String::new())),
            ]
        );
        assert_eq!(
            cass_bool_tokens("-\"\""),
            vec![
                CassBoolToken::Token(FsCassQueryToken::Not),
                CassBoolToken::Token(FsCassQueryToken::Phrase(String::new())),
            ]
        );
    }

    #[test]
    fn rejected_expansion_cannot_be_negated_or_disjoined_into_matches() {
        let parser = parser();
        let filters = CassQueryFilters::default();
        for oversized in [
            "aé".repeat(257),
            format!("{}é", "a".repeat(257)),
            "é".repeat(MAX_QUERY_SCALARS + 1),
            format!("{}kérlek{}", "(".repeat(129), ")".repeat(129)),
            format!("\"é {}\"", "a".repeat(257)),
            format!("\"{} café\"", "a".repeat(257)),
        ] {
            for raw in [
                format!("NOT ({oversized})"),
                format!("backup OR {oversized}"),
            ] {
                assert!(parse(&parser, &raw, &filters).is_empty(), "{raw:?}");
            }
        }
        let literal_parens = format!("\"kérlek {} nézd\"", "(".repeat(200));
        assert!(!parse(&parser, &literal_parens, &filters).is_empty());
        let accepted_boundary = format!("{}kérlek{}", "(".repeat(128), ")".repeat(128));
        assert!(!parse(&parser, &accepted_boundary, &filters).is_empty());
        let code_word_parens = format!("fn{}kérlek", "(".repeat(200));
        assert!(!parse(&parser, &code_word_parens, &filters).is_empty());
        let cjk_bigrams = format!("\"café {}\"", "日本語".repeat(40));
        assert!(!parse(&parser, &cjk_bigrams, &filters).is_empty());
    }

    #[test]
    fn exclusion_nodes_do_not_introduce_positive_match_all_scores() {
        fn all_nodes(query: &Query, id: QueryNodeId) -> usize {
            match query.node(id) {
                QueryNode::All => 1,
                QueryNode::Boolean { clauses, .. } => clauses
                    .iter()
                    .map(|clause| all_nodes(query, clause.query))
                    .sum(),
                QueryNode::Boost { query: child, .. } => all_nodes(query, *child),
                _ => 0,
            }
        }
        let parser = parser();
        for (raw, expected) in [
            ("kérlek AND NOT backup", 0),
            ("NOT kérlek", 1),
            ("NOT kérlek AND NOT backup", 1),
            ("NOT kérlek OR NOT backup", 2),
        ] {
            let query = parse(&parser, raw, &CassQueryFilters::default());
            assert_eq!(all_nodes(&query, query.root_id()), expected, "{raw:?}");
        }
    }
}
