//! Query-only compatibility with the tokenizer in CASS's released v9 index.
//!
//! Based on the contributor's tokenizer reuse in Frankensearch PR #74. The
//! upstream fix cannot yet be selected through CASS's published dependencies:
//! current Frankensearch main also changes durable identity/schema. Keep the
//! registry source contract and existing indexes, and lower affected operands
//! through public Quill APIs until a compatible release carries the correction.
//!
//! Bare affected words use adjacent exact fragments in one field. Explicit
//! trailing prefixes still use the released bounded prefix columns, which do
//! not guarantee adjacency or complete deep/long-prefix recall. Mixed CJK
//! fragments use a same-field conjunction because bigrams share positions.
//! Canonically equivalent spellings can have different legacy indexed terms;
//! adjacent literal fragments remain indistinguishable from an accented word.

use frankensearch::quill::query::{
    BooleanClause, BooleanOperator, CassQueryFilters, CassQueryParser, Occur, PositionedTerm,
    Query, QueryField, QueryNode, QueryNodeId,
};
use frankensearch::quill::schema::Analyzer;
use frankensearch::quill::scribe::{CassAnalyzer, TokenAnalyzer};

use super::{
    CassBoolExpr, CassBoolParser, CassBoolToken, CassBoolUnsupported, FsCassQueryToken,
    FsCassWildcardPattern, cass_bool_groups_have_operands, cass_bool_nesting_is_bounded,
    cass_bool_tokens, fs_cass_sanitize_query,
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
    // An empty group must not disappear from AND/OR or become a complement.
    // Check before the released parser's ASCII/CJK delegation as well.
    if !cass_bool_nesting_is_bounded(&tokens) || !cass_bool_groups_have_operands(&tokens) {
        return Query::empty();
    }
    if !requires_fragments {
        // Keep the native tree/scoring for ordinary ASCII and CJK queries,
        // but never delegate an expression that would erase an operand.
        let mut has_operand = false;
        for token in &tokens {
            let (text, phrase) = match token {
                CassBoolToken::Token(FsCassQueryToken::Term(text)) => (text, false),
                CassBoolToken::Token(FsCassQueryToken::Phrase(text)) => (text, true),
                _ => continue,
            };
            if !phrase && text.trim().is_empty() {
                continue;
            }
            has_operand = true;
            if !operand_is_searchable(text, phrase) {
                return Query::empty();
            }
        }
        return if has_operand {
            parser.parse(raw, filters).query
        } else {
            Query::empty()
        };
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
        // A nonempty expression without an operand is never a browse query.
        Ok(None) => return Query::empty(),
        Err(_) => return Query::empty(),
    };
    let (query, negative) = lower_expression(expression);
    let query = if negative { complement(query) } else { query };
    apply_native_filters(parser, query, filters)
}

/// Substring retries cannot preserve the boundaries of analyzed Unicode
/// operands. Validate ordinary operands too, so punctuation or a dropped
/// required token cannot be retried as a match-all wildcard.
pub(super) fn allows_automatic_wildcard(raw: &str) -> bool {
    if raw.trim().is_empty()
        || raw.chars().nth(MAX_QUERY_SCALARS).is_some()
        || raw.chars().any(needs_indexed_fragments)
    {
        return false;
    }
    let tokens = cass_bool_tokens(raw);
    !tokens.is_empty()
        && tokens.iter().all(|token| {
            matches!(token, CassBoolToken::Token(FsCassQueryToken::Term(text))
                if !text.contains('*') && operand_is_searchable(text, false))
        })
}

fn operand_is_searchable(raw: &str, phrase: bool) -> bool {
    let mut found = 0;
    for part in operand_parts(raw) {
        let core = part.trim_matches('*');
        if core.is_empty() && phrase {
            continue;
        }
        if phrase && (core.contains('*') || found == MAX_INDEXED_FRAGMENTS) {
            return false;
        }
        // Every nonempty ASCII run in this bound is admitted by the index.
        // Avoid allocating token vectors on ordinary short ASCII searches.
        let admitted_ascii = core.is_ascii()
            && core.len() <= MAX_INDEXED_TERM_BYTES
            && core.bytes().any(|byte| byte.is_ascii_alphanumeric());
        if !admitted_ascii && analyzed_fragments(core).is_err() {
            return false;
        }
        found += 1;
    }
    found != 0
}

/// Preserve raw combining marks and case until the indexing analyzer runs.
/// ASCII punctuation separates independent bare operands; a single compound
/// hyphen is retained, while repeated hyphens remain separators.
fn operand_parts(raw: &str) -> Vec<String> {
    if !raw.chars().any(needs_indexed_fragments) {
        return fs_cass_sanitize_query(raw)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
    }
    raw.split("--")
        .flat_map(|part| {
            part.split(|ch: char| {
                ch.is_whitespace()
                    || (ch.is_ascii() && !ch.is_ascii_alphanumeric() && !matches!(ch, '*' | '-'))
            })
        })
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
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
    positions: Vec<u32>,
    ends_with_token: bool,
}

/// Analyze original case before any Unicode mapping. In particular, İ must
/// remain an index separator, rather than first becoming an indexed ASCII i.
fn analyzed_fragments(raw: &str) -> Result<IndexedFragments, CassBoolUnsupported> {
    let mut terms = Vec::new();
    let mut positions = Vec::new();
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
        if terms.len() == MAX_INDEXED_FRAGMENTS || token.text.len() > MAX_INDEXED_TERM_BYTES {
            rejected = true;
        } else {
            terms.push(token.text.clone());
            positions.push(token.position);
        }
    });
    rejected |= raw[covered_to..]
        .bytes()
        .any(|byte| byte.is_ascii_alphanumeric());
    if rejected || terms.is_empty() {
        return Err(CassBoolUnsupported);
    }
    Ok(IndexedFragments {
        terms,
        positions,
        ends_with_token: covered_to == raw.len(),
    })
}

fn lower_term(raw: &str) -> Result<Option<Query>, CassBoolUnsupported> {
    // The released lexer can retain Unicode whitespace as an unquoted token.
    // It is spacing, not a required empty quoted operand.
    if raw.trim().is_empty() {
        return Ok(None);
    }
    lower_compound(&operand_parts(raw))
}

fn lower_compound(parts: &[String]) -> Result<Option<Query>, CassBoolUnsupported> {
    let mut queries = Vec::new();
    for part in parts {
        if let Some(query) = lower_term_part(part)? {
            queries.push(query);
        }
    }
    if queries.is_empty() {
        return Err(CassBoolUnsupported);
    }
    Ok(Some(conjunction(queries)))
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
                return Err(CassBoolUnsupported);
            }
            let fragments = analyzed_fragments(raw.trim_matches('*'))?;
            if raw.chars().any(needs_indexed_fragments) {
                let ascii_tail = fragments.terms.last().is_some_and(|term| term.is_ascii());
                return Ok(Some(lower_fragments(
                    &fragments,
                    prefix && fragments.ends_with_token && ascii_tail,
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
        // A wildcard over one dictionary term cannot span the fragments of
        // an affected word. Refuse it before Boolean complementation.
        FsCassWildcardPattern::Suffix(_)
        | FsCassWildcardPattern::Substring(_)
        | FsCassWildcardPattern::Complex(_) => {
            if raw.chars().any(needs_indexed_fragments) {
                return Err(CassBoolUnsupported);
            }
            analyzed_fragments(raw.trim_matches('*'))?;
            let fields = QuillCassFields::compiled();
            Ok(Some(Query::glob(
                vec![fields.content, fields.title],
                raw.to_lowercase(),
            )))
        }
    }
}

fn lower_fragments(fragments: &IndexedFragments, prefix: bool) -> Query {
    let terms = &fragments.terms;
    let fields = QuillCassFields::compiled();
    if !prefix && !terms.iter().any(|term| term.chars().any(is_cass_cjk)) {
        let exact_fields = vec![
            QueryField::new(fields.title, 1.0),
            QueryField::new(fields.content, 1.0),
        ];
        if terms.len() == 1 {
            return Query::term(exact_fields, terms[0].clone());
        }
        return Query::phrase(
            exact_fields,
            terms
                .iter()
                .zip(&fragments.positions)
                .map(|(text, position)| PositionedTerm::new(*position, text.clone()))
                .collect(),
            0,
            false,
        );
    }
    // CJK bigrams share a position but are all required, not alternatives.
    // Explicit prefixes retain the released prefix-column behavior: this
    // bounded compatibility route still cannot enforce adjacency or find
    // every deep/long prefix. Do not substitute the released phrase-prefix
    // API, which silently truncates each segment's expansion to 50 terms.
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
    let parts = operand_parts(raw)
        .into_iter()
        .map(|part| part.trim_matches('*').to_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return Err(CassBoolUnsupported);
    }
    let affected = raw.chars().any(needs_indexed_fragments);
    for part in &parts {
        if part.contains('*') {
            return Err(CassBoolUnsupported);
        }
        // Validate each word independently. A standalone unsupported word
        // must not disappear between the searchable words of a phrase.
        analyzed_fragments(part)?;
    }
    if affected {
        let fragments = analyzed_fragments(&parts.join(" "))?;
        return Ok(Some(lower_fragments(&fragments, false)));
    }
    let terms = parts
        .iter()
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>();
    if terms.len() > MAX_INDEXED_FRAGMENTS {
        return Err(CassBoolUnsupported);
    }
    if terms.len() <= 1 || terms.iter().any(|term| term.chars().any(is_cass_cjk)) {
        return lower_compound(&parts);
    }
    let fields = QuillCassFields::compiled();
    Ok(Some(Query::phrase(
        vec![
            QueryField::new(fields.title, 1.0),
            QueryField::new(fields.content, 1.0),
        ],
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
            "\"*backup*\"",
            "backup \r",
            "ca*",
            "*fix",
            "m*d",
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
    fn tokenizer_runs_before_case_mapping_and_rejects_empty_results() {
        for (raw, expected) in [
            ("KÉRLEK", vec!["k", "rlek"]),
            ("İstanbul", vec!["stanbul"]),
            ("GRÖẞERE", vec!["gr", "ere"]),
            ("Straße", vec!["stra", "e"]),
            ("cafe\u{301}", vec!["cafe"]),
            ("ke\u{301}rlek", vec!["ke", "rlek"]),
            ("Key", vec!["ey"]),
            ("日本語é", vec!["日本", "本語"]),
            ("日é本", vec!["日", "本"]),
        ] {
            let fragments = analyzed_fragments(raw)
                .ok()
                .expect("searchable original spelling");
            assert_eq!(fragments.terms, expected, "{raw:?}");
        }
        assert_eq!(
            analyzed_fragments("日本語é")
                .ok()
                .expect("mixed CJK input")
                .positions,
            vec![0, 0],
            "all bigrams from one CJK run share an analyzer position"
        );
        for raw in ["éß", "K", "İ", "привет", "🚀", "*", ""] {
            assert!(analyzed_fragments(raw).is_err(), "{raw:?}");
        }
        assert!(analyzed_fragments(&format!("{}é", "a".repeat(257))).is_err());
        assert!(analyzed_fragments(&"aé".repeat(257)).is_err());
    }

    #[test]
    fn unsearchable_operands_fail_the_complete_expression() {
        let parser = parser();
        let filters = CassQueryFilters::default();
        for operand in [
            "é", "K", "İ", "привет", "שלום", "مرحبا", "🚀", "!!!", "*", "**",
            "()", "(NOT)", "(AND OR)", "(())", "( \t)", "(",
            "\"\"", "\"   \"", "\"***\"", "*érlek", "*kérlek*", "ké*rlek",
            "\"kérlek 🚀 nézd\"", "\"kérlek é nézd\"", "\"f*o\"", "\"foo f*o\"",
        ] {
            for raw in [
                operand.to_owned(),
                format!("NOT {operand}"),
                format!("backup OR {operand}"),
                format!("backup NOT {operand}"),
            ] {
                assert!(parse(&parser, &raw, &filters).is_empty(), "{raw:?}");
            }
        }
        for raw in ["AND", "OR", "NOT", "()", "(NOT)", "&& ||"] {
            assert!(parse(&parser, raw, &filters).is_empty(), "{raw:?}");
        }
        for raw in ["", " \t\n ", "\u{2003}"] {
            assert!(matches!(parse(&parser, raw, &filters).root(), QueryNode::All));
        }
    }

    #[test]
    fn retries_preserve_token_boundaries_and_require_searchable_operands() {
        for raw in ["apple", "pple", "日本語", "foo bar"] {
            assert!(allows_automatic_wildcard(raw), "{raw:?}");
        }
        for raw in [
            "", "!!!", "*", "AND", "backup OR é", "kérlek", "ke\u{301}rlek",
            "Key", "é", "🚀", "\"exact phrase\"", "foo*",
        ] {
            assert!(!allows_automatic_wildcard(raw), "{raw:?}");
        }
        assert!(!allows_automatic_wildcard(&"x".repeat(257)));
        assert!(!allows_automatic_wildcard(&"x ".repeat(MAX_QUERY_SCALARS)));
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
            format!("\"{}\"", "backup ".repeat(MAX_INDEXED_FRAGMENTS + 1)),
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
