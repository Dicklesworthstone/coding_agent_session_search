//! Metamorphic search-semantics oracle (bead coding_agent_session_search-2l1b0.68).
//!
//! A generated Codex corpus has a known term -> message map and known message
//! times. Every query runs through the real `cass` binary (lexical mode,
//! automatic wildcard fallback off) and its hit set must equal the one a
//! small set-algebra reference computes: OR is union, AND (explicit or
//! implicit) is intersection, NOT is difference, and a time window keeps
//! exactly the messages inside it. These relations do not depend on any one
//! expected answer, so a regression in the grammar, the filters or the
//! engine shows up as a set difference, printed per query.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Instant;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const TERMS: [&str; 5] = ["kiwiword", "limeword", "mangoword", "plumword", "pearword"];
const SESSIONS: usize = 18;
const MESSAGES_PER_SESSION: usize = 10;
/// 2026-08-01T00:00:00Z; session `s` happens on day `s`.
const DAY0_SECS: i64 = 1_785_542_400;
const DAY_SECS: i64 = 86_400;

/// One generated message: its id, UTC timestamp (seconds) and terms.
struct Message {
    id: usize,
    at_secs: i64,
    terms: BTreeSet<&'static str>,
}

struct Corpus {
    _root: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    messages: Vec<Message>,
}

impl Corpus {
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cass"));
        cmd.env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join(".codex"))
            .env("CASS_DATA_DIR", &self.data_dir)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_AUTOMATIC_WILDCARD_FALLBACK_MAX_DOCS", "0")
            .env("TZ", "UTC");
        cmd
    }

    /// Message ids a lexical search returns, after checking that no automatic
    /// wildcard fallback widened the result.
    fn search(&self, query: &str, extra: &[&str]) -> TestResult<BTreeSet<usize>> {
        let started = Instant::now();
        let mut cmd = self.cmd();
        cmd.args([
            "search",
            query,
            "--mode",
            "lexical",
            "--robot",
            "--robot-meta",
            "--limit",
            "1000",
            "--no-maintenance",
        ])
        .args(extra);
        let output = cmd.output()?;
        if !output.status.success() {
            return Err(format!(
                "search {query:?} {extra:?} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        let payload: Value = serde_json::from_slice(&output.stdout)?;
        if payload["_meta"]["wildcard_fallback"] == true {
            return Err(format!("search {query:?} used a wildcard fallback").into());
        }
        let mut ids = BTreeSet::new();
        for hit in payload["hits"]
            .as_array()
            .ok_or("search payload has no hits")?
        {
            let content = hit["content"].as_str().unwrap_or_default();
            let id = message_id(content)
                .ok_or_else(|| format!("hit without a msgid token: {content:?}"))?;
            ids.insert(id);
        }
        eprintln!(
            "[metamorphic] query={query:?} extra={extra:?} hits={} elapsed_ms={}",
            ids.len(),
            started.elapsed().as_millis()
        );
        Ok(ids)
    }

    fn with_term(&self, term: &str) -> BTreeSet<usize> {
        self.messages
            .iter()
            .filter(|message| message.terms.contains(term))
            .map(|message| message.id)
            .collect()
    }

    fn in_window(&self, from_secs: i64, to_secs: i64) -> BTreeSet<usize> {
        self.messages
            .iter()
            .filter(|message| (from_secs..=to_secs).contains(&message.at_secs))
            .map(|message| message.id)
            .collect()
    }
}

fn message_id(content: &str) -> Option<usize> {
    let start = content.find("msgid")? + "msgid".len();
    let digits: String = content[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Deterministic xorshift, so the corpus is identical on every run.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .expect("fixture timestamp")
        .format("%Y-%m-%dT%H:%M:%S.000Z")
        .to_string()
}

fn write_session(codex_home: &Path, session: usize, messages: &mut Vec<Message>) -> TestResult {
    let day = DAY0_SECS + session as i64 * DAY_SECS;
    let dir = codex_home
        .join("sessions/2026/08")
        .join(format!("{:02}", session + 1));
    fs::create_dir_all(&dir)?;
    let name = format!("meta{session:02}");
    let mut lines = vec![format!(
        r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{name}","cwd":"/work/metamorphic","cli_version":"0.42.0"}}}}"#,
        rfc3339(day)
    )];
    let mut rng =
        0x9E37_79B9_7F4A_7C15_u64 ^ (session as u64 + 1).wrapping_mul(0x2545_F491_4F6C_DD1D);
    for index in 0..MESSAGES_PER_SESSION {
        let id = session * 100 + index;
        // Messages sit at 00:10, 02:10, ... so each day's first and last
        // messages bracket the window boundaries tested below.
        let at_secs = day + 600 + index as i64 * 2 * 3_600;
        // The first message becomes the title, which is indexed with every
        // message of the conversation; keep terms out of it.
        let terms: BTreeSet<&'static str> = if index == 0 {
            BTreeSet::new()
        } else {
            TERMS
                .iter()
                .copied()
                .filter(|_| next_random(&mut rng) % 100 < 40)
                .collect()
        };
        let mut words = vec![format!("msgid{id}"), "note".to_string()];
        words.extend(terms.iter().map(|term| (*term).to_string()));
        let (kind, role) = if index % 2 == 0 {
            ("input_text", "user")
        } else {
            ("text", "assistant")
        };
        lines.push(format!(
            r#"{{"timestamp":"{}","type":"response_item","payload":{{"type":"message","role":"{role}","content":[{{"type":"{kind}","text":"{}"}}]}}}}"#,
            rfc3339(at_secs),
            words.join(" ")
        ));
        messages.push(Message { id, at_secs, terms });
    }
    fs::write(
        dir.join(format!(
            "rollout-2026-08-{:02}T00-00-00-{name}.jsonl",
            session + 1
        )),
        lines.join("\n") + "\n",
    )?;
    Ok(())
}

fn corpus() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| build_corpus().expect("build and index the metamorphic corpus"))
}

fn build_corpus() -> TestResult<Corpus> {
    let root = tempfile::TempDir::new()?;
    let home = root.path().join("home");
    let data_dir = root.path().join("cass-data");
    fs::create_dir_all(&data_dir)?;
    let mut messages = Vec::new();
    for session in 0..SESSIONS {
        write_session(&home.join(".codex"), session, &mut messages)?;
    }
    let corpus = Corpus {
        _root: root,
        home,
        data_dir,
        messages,
    };
    let started = Instant::now();
    let output = corpus
        .cmd()
        .args(["index", "--full", "--json", "--no-progress-events"])
        .output()?;
    eprintln!(
        "[metamorphic] index --full exit={:?} elapsed_ms={} messages={}",
        output.status.code(),
        started.elapsed().as_millis(),
        corpus.messages.len()
    );
    if !output.status.success() {
        return Err(format!("index failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(corpus)
}

fn assert_same(label: &str, got: &BTreeSet<usize>, expected: &BTreeSet<usize>) -> TestResult {
    if got == expected {
        return Ok(());
    }
    let missing: Vec<_> = expected.difference(got).collect();
    let extra: Vec<_> = got.difference(expected).collect();
    Err(format!("{label}: missing {missing:?}, unexpected {extra:?}").into())
}

/// The reference sets are non-trivial, so an engine that returned nothing or
/// everything cannot pass by accident.
#[test]
fn every_term_matches_exactly_its_messages() -> TestResult {
    let corpus = corpus();
    let all: BTreeSet<usize> = corpus.messages.iter().map(|message| message.id).collect();
    for term in TERMS {
        let expected = corpus.with_term(term);
        assert!(
            !expected.is_empty() && expected.len() < all.len() / 2,
            "fixture must give {term} a proper subset: {}",
            expected.len()
        );
        assert_same(term, &corpus.search(term, &[])?, &expected)?;
    }
    Ok(())
}

#[test]
fn or_is_union_and_and_is_intersection() -> TestResult {
    let corpus = corpus();
    let mut cases = BTreeMap::new();
    for (a, b) in [
        ("kiwiword", "limeword"),
        ("mangoword", "plumword"),
        ("pearword", "kiwiword"),
    ] {
        let (sa, sb) = (corpus.with_term(a), corpus.with_term(b));
        cases.insert(format!("{a} OR {b}"), sa.union(&sb).copied().collect());
        let both: BTreeSet<usize> = sa.intersection(&sb).copied().collect();
        cases.insert(format!("{a} AND {b}"), both.clone());
        cases.insert(format!("{a} {b}"), both);
        cases.insert(
            format!("{a} NOT {b}"),
            sa.difference(&sb).copied().collect(),
        );
    }
    for (query, expected) in &cases {
        assert_same(query, &corpus.search(query, &[])?, expected)?;
    }
    Ok(())
}

/// Narrowing the window never adds hits, and each window keeps exactly the
/// messages whose time falls inside it (date-only `--until` covers that day).
#[test]
fn time_windows_are_exact_and_nested() -> TestResult {
    let corpus = corpus();
    let term = "limeword";
    let with_term = corpus.with_term(term);
    let day = |n: i64| DAY0_SECS + n * DAY_SECS;
    let windows = [
        ("--since", "2026-08-05", day(4), i64::MAX),
        ("--since", "2026-08-12", day(11), i64::MAX),
        ("--until", "2026-08-06", i64::MIN, day(6) - 1),
    ];
    let mut previous_since: Option<BTreeSet<usize>> = None;
    for (flag, date, from, to) in windows {
        let expected: BTreeSet<usize> = with_term
            .intersection(&corpus.in_window(from, to))
            .copied()
            .collect();
        let got = corpus.search(term, &[flag, date])?;
        assert_same(&format!("{term} {flag} {date}"), &got, &expected)?;
        if flag == "--since" {
            if let Some(wider) = &previous_since {
                assert!(got.is_subset(wider), "a later --since must not add hits");
            }
            previous_since = Some(got);
        }
    }
    Ok(())
}

/// README documents NOT > AND > OR precedence and parentheses. The Quill CASS
/// grammar binds OR tighter than AND and ignores parentheses, so these fail
/// until bead 2l1b0.52 lands the documented grammar upstream.
#[test]
#[ignore = "fails until 2l1b0.52: Quill's CASS grammar binds OR tighter and ignores parentheses"]
fn precedence_and_parentheses_follow_the_documented_grammar() -> TestResult {
    let corpus = corpus();
    let (k, l, m) = (
        corpus.with_term("kiwiword"),
        corpus.with_term("limeword"),
        corpus.with_term("mangoword"),
    );
    let l_and_m: BTreeSet<usize> = l.intersection(&m).copied().collect();
    let k_or_l: BTreeSet<usize> = k.union(&l).copied().collect();
    assert_same(
        "kiwiword OR limeword AND mangoword",
        &corpus.search("kiwiword OR limeword AND mangoword", &[])?,
        &k.union(&l_and_m).copied().collect(),
    )?;
    assert_same(
        "(kiwiword OR limeword) AND mangoword",
        &corpus.search("(kiwiword OR limeword) AND mangoword", &[])?,
        &k_or_l.intersection(&m).copied().collect(),
    )
}
