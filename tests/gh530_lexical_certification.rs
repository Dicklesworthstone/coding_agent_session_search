//! GH #530: generation-boundary regressions, not permission to relax certification.
//!
//! The native tests isolate committed visibility from input accounting. The CLI
//! test keeps twenty session files growing while a forced rebuild runs, then
//! checks that a follow-up ingest recovers every durable source message.
use coding_agent_search::search::quill_bridge::manifest_live_doc_count;
use coding_agent_search::search::tantivy::{
    TantivyIndex, index_dir, validated_searchable_index_summary,
};
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensearch::quill::cass::{CassDocument, CassDocumentRef};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn documents(start: usize, count: usize) -> Vec<CassDocument> {
    (start..start + count)
        .map(|id| CassDocument {
            agent: "claude".into(),
            workspace: Some("/work/gh530".into()),
            workspace_original: None,
            source_path: "/sessions/gh530.jsonl".into(),
            msg_idx: id as u64,
            created_at: Some(1_787_587_200_000),
            title: Some("GH530 committed snapshot".into()),
            content: format!("snapshotproof{id:06}z durable lexical message {id}"),
            conversation_id: Some(1),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
        })
        .collect()
}

fn borrowed(documents: &[CassDocument]) -> Vec<CassDocumentRef<'_>> {
    documents
        .iter()
        .map(|document| CassDocumentRef {
            agent: &document.agent,
            workspace: document.workspace.as_deref(),
            workspace_original: document.workspace_original.as_deref(),
            source_path: &document.source_path,
            msg_idx: document.msg_idx,
            created_at: document.created_at,
            title: document.title.as_deref(),
            content: &document.content,
            conversation_id: document.conversation_id,
            source_id: &document.source_id,
            origin_kind: &document.origin_kind,
            origin_host: document.origin_host.as_deref(),
        })
        .collect()
}

fn assert_committed_count(path: &Path, expected: usize) {
    let observed = validated_searchable_index_summary(path).unwrap();
    assert_eq!(
        observed.docs,
        expected,
        "fresh reader at {}",
        path.display()
    );
}

#[test]
fn gh530_twenty_producers_preserve_committed_counts_and_pinned_readers() {
    let directory = tempfile::tempdir().unwrap();
    let mut index = TantivyIndex::open_or_create(directory.path()).unwrap();
    let empty_reader = index.reader().unwrap();
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(4);
        for producer in 0..20 {
            let sender = sender.clone();
            scope.spawn(move || {
                for round in 0..4 {
                    let start = (producer * 4 + round) * 73;
                    if sender.send(documents(start, 73)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sender);
        let mut submitted = 0;
        for batch in receiver {
            submitted += index.add_prebuilt_documents_slice(&batch).unwrap();
            index.commit().unwrap();
            assert_eq!(index.doc_count().unwrap(), submitted as u64);
            assert_committed_count(directory.path(), submitted);
            assert_eq!(empty_reader.doc_count().unwrap(), 0);
        }
        assert_eq!(submitted, 20 * 4 * 73);
    });
    index.force_merge().unwrap();
    assert_eq!(empty_reader.doc_count().unwrap(), 0);
    drop(index);
    assert_committed_count(directory.path(), 20 * 4 * 73);
}

/// Kill/reap a child even when a test assertion or timeout unwinds.
struct Process {
    child: Child,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl Process {
    fn spawn(command: &mut Command, directory: &Path, name: &str) -> Self {
        let stdout = directory.join(format!("{name}.stdout"));
        let stderr = directory.join(format!("{name}.stderr"));
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn diagnostics(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&self.stdout).unwrap_or_default(),
            fs::read_to_string(&self.stderr).unwrap_or_default()
        )
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "child timed out: {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn success(&mut self) {
        let status = self.wait(Duration::from_secs(180));
        assert!(
            status.success(),
            "child exited {status}: {}",
            self.diagnostics()
        );
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "subprocess fixture; invoked explicitly by the crash-window regression"]
fn gh530_committed_writer_subprocess() {
    let path = PathBuf::from(std::env::var_os("CASS_GH530_CRASH_INDEX").expect("fixture path"));
    let ready = PathBuf::from(std::env::var_os("CASS_GH530_CRASH_READY").expect("ready path"));
    let mut index = TantivyIndex::open_or_create(&path).unwrap();
    assert_eq!(
        index
            .add_prebuilt_documents_slice(&documents(100, 73))
            .unwrap(),
        73
    );
    index.commit().unwrap();
    assert_committed_count(&path, 173);
    // The parent kills the process after publication but before any caller
    // checkpoint advance. Do not substitute dropping the writer for a crash.
    fs::write(ready, b"committed").unwrap();
    loop {
        thread::park_timeout(Duration::from_secs(1));
    }
}

#[test]
fn gh530_crash_after_commit_replays_surplus_without_losing_late_documents() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("candidate");
    let mut index = TantivyIndex::open_or_create(&path).unwrap();
    assert_eq!(
        index
            .add_prebuilt_documents_slice(&documents(0, 100))
            .unwrap(),
        100
    );
    index.commit().unwrap();
    drop(index);
    let checkpoint_docs = 100;
    let ready = directory.path().join("writer-ready");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "gh530_committed_writer_subprocess",
            "--ignored",
            "--nocapture",
        ])
        .env("CASS_GH530_CRASH_INDEX", &path)
        .env("CASS_GH530_CRASH_READY", &ready);
    let mut process = Process::spawn(&mut command, directory.path(), "crash-writer");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready.exists() {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "writer exited before committed barrier: {}",
            process.diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "commit barrier timed out: {}",
            process.diagnostics()
        );
        thread::sleep(Duration::from_millis(10));
    }
    process.child.kill().unwrap();
    assert!(!process.child.wait().unwrap().success());

    assert_committed_count(&path, checkpoint_docs + 73);
    let mut resumed = TantivyIndex::open_or_create(&path).unwrap();
    let pinned = resumed.reader().unwrap();
    resumed.arm_resume_reconcile(73);
    let replay = documents(checkpoint_docs, 73);
    assert_eq!(
        resumed
            .add_prebuilt_document_refs_slice(&borrowed(&replay))
            .unwrap(),
        73
    );
    assert_eq!(resumed.resume_reconcile_docs_remaining(), 0);
    let late = documents(173, 64);
    assert_eq!(
        resumed
            .add_prebuilt_document_refs_slice(&borrowed(&late))
            .unwrap(),
        64
    );
    resumed.commit().unwrap();
    resumed.force_merge().unwrap();
    assert_eq!(pinned.doc_count().unwrap(), 173);
    assert_eq!(resumed.doc_count().unwrap(), 237);
    drop(resumed);
    assert_committed_count(&path, checkpoint_docs + replay.len() + late.len());

    // A repeated identity-idempotent retry must not turn input operations
    // into new live documents, including after a fresh writer open.
    let mut retry = TantivyIndex::open_or_create(&path).unwrap();
    retry
        .upsert_prebuilt_documents_slice(&documents(0, 237))
        .unwrap();
    retry.commit().unwrap();
    drop(retry);
    assert_committed_count(&path, 237);
}

#[test]
fn gh530_replacements_and_merge_do_not_count_tombstones_as_live_documents() {
    let directory = tempfile::tempdir().unwrap();
    let mut index = TantivyIndex::open_or_create(directory.path()).unwrap();
    index
        .add_prebuilt_documents_slice(&documents(0, 100))
        .unwrap();
    index.commit().unwrap();
    let pinned = index.reader().unwrap();
    for revision in 0..3 {
        let mut revised = documents(0, 100);
        for document in &mut revised {
            document.content = format!("revision{revision} {}", document.content);
        }
        assert_eq!(
            index.upsert_prebuilt_documents_slice(&revised).unwrap(),
            100
        );
        index.commit().unwrap();
        assert_committed_count(directory.path(), 100);
        let manifest = manifest_live_doc_count(directory.path()).expect("published Quill manifest");
        assert_eq!(manifest.live_docs, 100);
    }
    index
        .add_prebuilt_documents_slice(&documents(100, 73))
        .unwrap();
    index.commit().unwrap();
    index.force_merge().unwrap();
    let manifest = manifest_live_doc_count(directory.path()).expect("merged Quill manifest");
    assert_eq!(manifest.live_docs, 173);
    assert_eq!(pinned.doc_count().unwrap(), 100);
    assert_eq!(index.doc_count().unwrap(), 173);
    drop(index);
    assert_committed_count(directory.path(), 173);
}

#[test]
fn gh530_unreadable_candidate_cannot_supply_count_authority() {
    let directory = tempfile::tempdir().unwrap();
    let live = directory.path().join("live");
    let candidate = directory.path().join("candidate");
    for (path, count) in [(&live, 100), (&candidate, 173)] {
        let mut index = TantivyIndex::open_or_create(path).unwrap();
        index
            .add_prebuilt_documents_slice(&documents(0, count))
            .unwrap();
        index.commit().unwrap();
    }
    // Only derived data in this test's candidate is damaged. In particular,
    // never manufacture an expected count from a missing published manifest.
    // Quill can recover from MANIFEST.prev when MANIFEST is absent.
    // Corrupt both slots to exercise a genuinely unreadable candidate,
    // rather than accidentally accepting the engine's valid fallback.
    fs::write(candidate.join("MANIFEST"), b"invalid-gh530-manifest").unwrap();
    fs::write(candidate.join("MANIFEST.prev"), b"invalid-gh530-previous").unwrap();
    assert!(validated_searchable_index_summary(&candidate).is_err());
    assert_committed_count(&live, 100);
}

const SESSIONS: usize = 20;
const INITIAL_MESSAGES: usize = 32;

fn session_path(home: &Path, session: usize) -> PathBuf {
    home.join(".claude/projects/gh530")
        .join(format!("session-{session}.jsonl"))
}

fn source_row(session: usize, message: usize) -> String {
    json!({
        "type": "user", "sessionId": format!("gh530-{session}"),
        "uuid": format!("gh530-{session}-{message}"),
        "timestamp": "2026-08-01T10:00:00Z", "cwd": "/work/gh530",
        "message": {"role": "user", "content": format!(
            "sessionproof{session:03}z gh530s{session:03}m{message:08}z durable source message"
        )}
    })
    .to_string()
        + "\n"
}

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CASS_DATA_DIR", home.join("data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_STREAMING_INDEX", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_ACTIVE_SESSION_RECENT_WRITE_WINDOW_SECS", "0")
        .env("RUST_MIN_STACK", "134217728")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .args(["--color", "never"]);
    command
}

struct GrowingSessions {
    stop: Arc<AtomicBool>,
    completed_rounds: Arc<AtomicUsize>,
    handle: Option<JoinHandle<io::Result<usize>>>,
}

impl GrowingSessions {
    fn start(home: &Path) -> Self {
        Self::start_at(home, INITIAL_MESSAGES)
    }

    fn start_at(home: &Path, first_message: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let completed_rounds = Arc::new(AtomicUsize::new(0));
        let worker_stop = stop.clone();
        let worker_rounds = completed_rounds.clone();
        let home = home.to_path_buf();
        let handle = thread::spawn(move || {
            thread::scope(|scope| {
                let (acknowledge, completed) = mpsc::channel();
                let mut producers = Vec::new();
                for session in 0..SESSIONS {
                    let (sender, requests) = mpsc::sync_channel(1);
                    producers.push(sender);
                    let acknowledge = acknowledge.clone();
                    let path = session_path(&home, session);
                    scope.spawn(move || {
                        for message in requests {
                            let result = (|| -> io::Result<()> {
                                let mut file = OpenOptions::new().append(true).open(&path)?;
                                file.write_all(source_row(session, message).as_bytes())?;
                                file.sync_data()
                            })();
                            if acknowledge.send(result).is_err() {
                                break;
                            }
                        }
                    });
                }
                drop(acknowledge);
                let mut next = first_message;
                while !worker_stop.load(Ordering::Acquire) {
                    for producer in &producers {
                        producer.send(next).map_err(io::Error::other)?;
                    }
                    for _ in 0..SESSIONS {
                        completed.recv().map_err(io::Error::other)??;
                    }
                    next += 1;
                    worker_rounds.fetch_add(1, Ordering::Release);
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(next)
            })
        });
        Self {
            stop,
            completed_rounds,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap().unwrap()
    }
}

impl Drop for GrowingSessions {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn gh530_cli_rebuild_with_growing_sessions_recovers_durable_tail_in_search() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".claude/projects/gh530")).unwrap();
    for session in 0..SESSIONS {
        let rows: String = (0..INITIAL_MESSAGES)
            .map(|message| source_row(session, message))
            .collect();
        fs::write(session_path(home.path(), session), rows).unwrap();
    }
    let mut initial = cass(home.path());
    initial.args(["index", "--full", "--json"]);
    Process::spawn(&mut initial, home.path(), "initial").success();

    let growers = GrowingSessions::start(home.path());
    let before = growers.completed_rounds.load(Ordering::Acquire);
    let mut rebuild = cass(home.path());
    rebuild.args(["index", "--force-rebuild", "--json"]);
    Process::spawn(&mut rebuild, home.path(), "growing-rebuild").success();
    assert!(
        growers.completed_rounds.load(Ordering::Acquire) > before,
        "no ingestion overlapped the CLI rebuild"
    );
    let per_session = growers.finish();
    assert!(per_session > INITIAL_MESSAGES);

    // Source writes after the rebuild's scan boundary are still durable but
    // need the next ingestion pass. A rebuild must not advance their ledger
    // past unread bytes or leave search permanently maintenance-required.
    let mut catch_up = cass(home.path());
    catch_up.args(["index", "--json"]);
    Process::spawn(&mut catch_up, home.path(), "catch-up").success();
    let storage = SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
    let conversations = storage.list_conversations(1000, 0).unwrap();
    assert_eq!(conversations.len(), SESSIONS);
    for conversation in conversations {
        assert_eq!(
            storage
                .fetch_messages(conversation.id.unwrap())
                .unwrap()
                .len(),
            per_session
        );
    }
    drop(storage);
    let live = index_dir(&home.path().join("data")).unwrap();
    assert_committed_count(&live, SESSIONS * per_session);
    for session in 0..SESSIONS {
        let term = format!("gh530s{session:03}m{:08}z", per_session - 1);
        let mut search = cass(home.path());
        search.args([
            "search",
            &term,
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--limit",
            "10",
        ]);
        let mut process = Process::spawn(&mut search, home.path(), &format!("search-{session}"));
        process.success();
        let result: Value = serde_json::from_slice(&fs::read(&process.stdout).unwrap()).unwrap();
        let hits = result["hits"]
            .as_array()
            .expect("successful lexical search has hits");
        assert_eq!(
            hits.len(),
            1,
            "missing or duplicated tail for session {session}: {result}"
        );
        assert!(
            hits[0]["content"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .any(|word| word == term)
        );
    }
}

fn wait_for_barrier(process: &mut Process, path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(bytes) = fs::read(path)
            && let Ok(value) = serde_json::from_slice(&bytes)
        {
            return value;
        }
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "rebuild exited before commit barrier: {}",
            process.diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "commit barrier timed out: {}",
            process.diagnostics()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn checkpoint_at(live: &Path) -> Value {
    serde_json::from_slice(&fs::read(live.join(".lexical-rebuild-state.json")).unwrap()).unwrap()
}

fn quarantines(data: &Path) -> Vec<PathBuf> {
    fs::read_dir(index_dir(data).unwrap().parent().unwrap())
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".lexical-rebuild-quarantine-")
        })
        .map(|entry| entry.path())
        .collect()
}

fn initialize_sessions(home: &Path) {
    initialize_sessions_with_count(home, INITIAL_MESSAGES);
}

fn initialize_sessions_with_count(home: &Path, messages: usize) {
    fs::create_dir_all(home.join(".claude/projects/gh530")).unwrap();
    for session in 0..SESSIONS {
        let rows: String = (0..messages)
            .map(|message| source_row(session, message))
            .collect();
        fs::write(session_path(home, session), rows).unwrap();
    }
    let mut command = cass(home);
    command.args(["index", "--full", "--json"]);
    Process::spawn(&mut command, home, "seed").success();
}

fn parked_rebuild(home: &Path, name: &str) -> (Process, Value) {
    let marker = home.join(format!("{name}-barrier.json"));
    let mut command = cass(home);
    command
        .args(["index", "--full", "--force-rebuild", "--json"])
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
            &marker,
        )
        .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "2")
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
            "180000",
        )
        .env("CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "1",
        )
        .env("CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS", "1")
        .env(
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            "1",
        );
    let mut process = Process::spawn(&mut command, home, name);
    let barrier = wait_for_barrier(&mut process, &marker);
    assert!(
        barrier["committed_indexed_docs"].as_u64().unwrap()
            > barrier["checkpoint_indexed_docs"].as_u64().unwrap()
    );
    (process, barrier)
}

#[test]
fn gh530_cli_rejects_noncanonical_surplus_before_replaying_a_non_eof_checkpoint() {
    let home = tempfile::tempdir().unwrap();
    initialize_sessions(home.path());
    let data = home.path().join("data");
    let live = index_dir(&data).unwrap();
    let prior_manifest = fs::read(live.join("MANIFEST")).unwrap();
    let (mut process, _) = parked_rebuild(home.path(), "non-eof");
    process.child.kill().unwrap();
    assert!(!process.child.wait().unwrap().success());
    assert_eq!(fs::read(live.join("MANIFEST")).unwrap(), prior_manifest);
    let mut interrupted = checkpoint_at(&live);
    let pending = interrupted["pending"].clone();
    assert!(
        pending.is_object(),
        "must interrupt an actual pending commit: {interrupted}"
    );
    // Account for the durable pending commit, then inject 73 identities that
    // cannot be removed by replaying any canonical conversation suffix.
    interrupted["indexed_docs"] = pending["indexed_docs"].clone();
    interrupted["processed_conversations"] = pending["processed_conversations"].clone();
    interrupted["committed_offset"] = pending["next_offset"].clone();
    interrupted["committed_conversation_id"] = pending["next_conversation_id"].clone();
    interrupted["pending"] = Value::Null;
    assert!(interrupted["processed_conversations"].as_u64().unwrap() < SESSIONS as u64);
    let scratch = live.with_file_name(format!(
        ".{}.rebuild-staging",
        live.file_name().unwrap().to_string_lossy()
    ));
    let mut index = TantivyIndex::open_or_create(&scratch).unwrap();
    index
        .add_prebuilt_documents_slice(&documents(1_000_000, 73))
        .unwrap();
    index.commit().unwrap();
    drop(index);
    let contaminated_manifest = fs::read(scratch.join("MANIFEST")).unwrap();
    fs::write(
        live.join(".lexical-rebuild-state.json"),
        serde_json::to_vec_pretty(&interrupted).unwrap(),
    )
    .unwrap();
    let mut resume = cass(home.path());
    resume.args(["index", "--json"]);
    Process::spawn(&mut resume, home.path(), "surplus-resume").success();
    let retained = quarantines(&data);
    assert_eq!(retained.len(), 1, "retain exactly one ambiguous candidate");
    let retained_checkpoint = checkpoint_at(&retained[0]);
    assert_eq!(
        retained_checkpoint["processed_conversations"], interrupted["processed_conversations"],
        "surplus must be rejected BEFORE replay, not after another full suffix pass"
    );
    assert_eq!(
        retained_checkpoint["indexed_docs"],
        interrupted["indexed_docs"]
    );
    assert_eq!(
        fs::read(retained[0].join("index/MANIFEST")).unwrap(),
        contaminated_manifest,
        "preserve the refused generation without writing into it"
    );
    assert_committed_count(&live, SESSIONS * INITIAL_MESSAGES);
    assert_eq!(checkpoint_at(&live)["completed"], true);
    let mut no_op = cass(home.path());
    no_op.args(["index", "--json"]);
    Process::spawn(&mut no_op, home.path(), "surplus-no-op").success();
    assert_committed_count(&live, SESSIONS * INITIAL_MESSAGES);
    assert_eq!(quarantines(&data).len(), 1);
}

#[test]
fn gh530_cli_killed_rebuild_finishes_while_sources_grow_and_incremental_catches_up() {
    let home = tempfile::tempdir().unwrap();
    initialize_sessions(home.path());
    let data = home.path().join("data");
    let live = index_dir(&data).unwrap();
    let (mut process, _) = parked_rebuild(home.path(), "growing-crash");
    let growers = GrowingSessions::start(home.path());
    // Create a NEW session while the old canonical generation is being rebuilt.
    fs::write(session_path(home.path(), SESSIONS), source_row(SESSIONS, 0)).unwrap();
    // The incomplete staging generation must not replace a certified live one.
    // Exercise the actual search admission path, not just an engine reader.
    for (term, count) in [("gh530s000m00000031z", 1), ("gh530s020m00000000z", 0)] {
        let mut search = cass(home.path());
        search.args([
            "search",
            term,
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
        ]);
        let mut result =
            Process::spawn(&mut search, home.path(), &format!("during-rebuild-{count}"));
        result.success();
        let response: Value = serde_json::from_slice(&fs::read(&result.stdout).unwrap()).unwrap();
        assert_eq!(
            response["hits"].as_array().unwrap().len(),
            count,
            "search must serve only the certified prior generation: {response}"
        );
    }
    let before = growers.completed_rounds.load(Ordering::Acquire);
    let deadline = Instant::now() + Duration::from_secs(10);
    while growers.completed_rounds.load(Ordering::Acquire) == before {
        assert!(Instant::now() < deadline, "source producer did not advance");
        thread::sleep(Duration::from_millis(10));
    }
    process.child.kill().unwrap();
    assert!(!process.child.wait().unwrap().success());
    let mut resume = cass(home.path());
    resume.args(["index", "--json"]);
    let started = Instant::now();
    Process::spawn(&mut resume, home.path(), "growing-resume").success();
    eprintln!(
        "GH530 crash-resume elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert!(growers.completed_rounds.load(Ordering::Acquire) > before);
    assert_eq!(
        checkpoint_at(&live)["completed"],
        true,
        "must certify without stopping source writers"
    );
    assert!(
        quarantines(&data).is_empty(),
        "an exact landed pending commit must resume, not discard its durable prefix"
    );
    // A subsequent incremental pass must also finish under live ingestion,
    // including a session created AFTER certification, not only a stopped tail.
    fs::write(
        session_path(home.path(), SESSIONS + 1),
        source_row(SESSIONS + 1, 0),
    )
    .unwrap();
    let durable_rounds = growers.completed_rounds.load(Ordering::Acquire);
    let durable_prefix = INITIAL_MESSAGES + durable_rounds;
    let mut live_catch_up = cass(home.path());
    live_catch_up.args(["index", "--json"]);
    Process::spawn(&mut live_catch_up, home.path(), "growing-live-catch-up").success();
    assert!(growers.completed_rounds.load(Ordering::Acquire) > durable_rounds);
    assert_eq!(checkpoint_at(&live)["completed"], true);
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    let conversations = storage.list_conversations(1000, 0).unwrap();
    assert_eq!(conversations.len(), SESSIONS + 2);
    for conversation in &conversations {
        let is_original = (0..SESSIONS)
            .any(|session| conversation.source_path == session_path(home.path(), session));
        let minimum = if is_original { durable_prefix } else { 1 };
        assert!(
            storage
                .fetch_messages(conversation.id.unwrap())
                .unwrap()
                .len()
                >= minimum
        );
    }
    drop(storage);
    let per_session = growers.finish();
    let mut catch_up = cass(home.path());
    catch_up.args(["index", "--json"]);
    Process::spawn(&mut catch_up, home.path(), "growing-catch-up").success();
    assert_committed_count(&live, SESSIONS * per_session + 2);
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    let conversations = storage.list_conversations(1000, 0).unwrap();
    assert_eq!(conversations.len(), SESSIONS + 2);
    assert_eq!(
        conversations
            .iter()
            .map(|conversation| storage
                .fetch_messages(conversation.id.unwrap())
                .unwrap()
                .len())
            .sum::<usize>(),
        SESSIONS * per_session + 2
    );
    drop(storage);
    for session in 0..SESSIONS + 2 {
        let message = if session >= SESSIONS {
            0
        } else {
            per_session - 1
        };
        let term = format!("gh530s{session:03}m{message:08}z");
        let mut command = cass(home.path());
        command.args([
            "search",
            &term,
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--limit",
            "10",
        ]);
        let mut result = Process::spawn(
            &mut command,
            home.path(),
            &format!("recovered-search-{session}"),
        );
        result.success();
        let hits: Value = serde_json::from_slice(&fs::read(&result.stdout).unwrap()).unwrap();
        assert_eq!(
            hits["hits"].as_array().unwrap().len(),
            1,
            "missing or duplicate recovered message {term}: {hits}"
        );
    }
}

#[test]
fn gh530_watch_once_then_plain_index_does_not_rebuild_a_certified_generation() {
    let home = tempfile::tempdir().unwrap();
    initialize_sessions(home.path());
    let source = session_path(home.path(), 0);
    let mut file = OpenOptions::new().append(true).open(&source).unwrap();
    file.write_all(source_row(0, INITIAL_MESSAGES).as_bytes())
        .unwrap();
    file.sync_data().unwrap();
    drop(file);
    let mut watch = cass(home.path());
    watch
        .args(["index", "--watch-once"])
        .arg(&source)
        .arg("--json");
    Process::spawn(&mut watch, home.path(), "watch-once").success();
    let unexpected_rebuild = home.path().join("unexpected-rebuild.json");
    let mut incremental = cass(home.path());
    incremental
        .args(["index", "--json"])
        .env(
            "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
            &unexpected_rebuild,
        )
        .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS", "1");
    let started = Instant::now();
    let mut process = Process::spawn(&mut incremental, home.path(), "after-watch-once");
    process.success();
    eprintln!(
        "GH530 post-watch incremental elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert!(
        !unexpected_rebuild.exists(),
        "ordinary incremental indexing unnecessarily rebuilt the archive: {}",
        process.diagnostics()
    );
    let live = index_dir(&home.path().join("data")).unwrap();
    assert_committed_count(&live, SESSIONS * INITIAL_MESSAGES + 1);
    assert_eq!(checkpoint_at(&live)["completed"], true);
}

#[test]
#[ignore = "larger performance fixture; run explicitly with --ignored --exact"]
fn gh530_cli_scale_rebuild_progresses_with_twenty_active_writers() {
    let home = tempfile::tempdir().unwrap();
    let dimension = |name: &str, default: usize| {
        std::env::var(name).map_or(default, |value| {
            value.parse::<usize>().expect("positive fixture dimension")
        })
    };
    let archive_sessions = dimension("CASS_GH530_SCALE_SESSIONS", SESSIONS);
    let initial_messages = dimension("CASS_GH530_SCALE_MESSAGES", 512);
    assert!(archive_sessions >= SESSIONS && initial_messages > 0);
    let initial_docs = archive_sessions.checked_mul(initial_messages).unwrap();
    fs::create_dir_all(home.path().join(".claude/projects/gh530")).unwrap();
    for session in 0..archive_sessions {
        let rows: String = (0..initial_messages)
            .map(|message| source_row(session, message))
            .collect();
        fs::write(session_path(home.path(), session), rows).unwrap();
    }
    let mut seed = cass(home.path());
    seed.args(["index", "--full", "--json"]);
    let started = Instant::now();
    Process::spawn(&mut seed, home.path(), "scale-seed").success();
    let seed_ms = started.elapsed().as_millis();
    let growers = GrowingSessions::start_at(home.path(), initial_messages);
    let before = growers.completed_rounds.load(Ordering::Acquire);
    let mut rebuild = cass(home.path());
    rebuild.args(["index", "--full", "--force-rebuild", "--json"]);
    let started = Instant::now();
    Process::spawn(&mut rebuild, home.path(), "scale-rebuild").success();
    let rebuild_ms = started.elapsed().as_millis();
    assert!(growers.completed_rounds.load(Ordering::Acquire) > before);
    let live = index_dir(&home.path().join("data")).unwrap();
    assert_eq!(checkpoint_at(&live)["completed"], true);
    assert_committed_count(&live, initial_docs);
    let per_session = growers.finish();
    let mut catch_up = cass(home.path());
    catch_up.args(["index", "--json"]);
    let started = Instant::now();
    Process::spawn(&mut catch_up, home.path(), "scale-catch-up").success();
    let catch_up_ms = started.elapsed().as_millis();
    let final_docs = initial_docs + SESSIONS * (per_session - initial_messages);
    assert_committed_count(&live, final_docs);
    eprintln!(
        "GH530 scale sessions={archive_sessions} initial_docs={initial_docs} final_docs={final_docs} seed_ms={seed_ms} rebuild_ms={rebuild_ms} catch_up_ms={catch_up_ms}"
    );
}

#[test]
fn gh530_cli_late_surplus_outside_the_upsert_window_does_not_abort_resume() {
    let home = tempfile::tempdir().unwrap();
    let messages = 256;
    initialize_sessions_with_count(home.path(), messages);
    let data = home.path().join("data");
    let live = index_dir(&data).unwrap();
    let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
    let conversation = storage
        .list_conversations(1000, 0)
        .unwrap()
        .into_iter()
        .max_by_key(|conversation| conversation.id)
        .unwrap();
    let message = storage
        .fetch_messages(conversation.id.unwrap())
        .unwrap()
        .into_iter()
        .max_by_key(|message| message.idx)
        .unwrap();
    let mut late = documents(0, 1).pop().unwrap();
    late.source_id = conversation.source_id;
    late.source_path = conversation.source_path.to_string_lossy().into_owned();
    late.conversation_id = conversation.id;
    late.msg_idx = u64::try_from(message.idx).unwrap();
    late.content = message.content;
    drop(storage);
    let (mut process, barrier) = parked_rebuild(home.path(), "late-surplus");
    process.child.kill().unwrap();
    assert!(!process.child.wait().unwrap().success());
    let committed = barrier["committed_indexed_docs"].as_u64().unwrap() as usize;
    assert!(
        SESSIONS * messages - committed > 4097,
        "the duplicate must be beyond the old gap-plus-4096 upsert budget"
    );
    let scratch = live.with_file_name(format!(
        ".{}.rebuild-staging",
        live.file_name().unwrap().to_string_lossy()
    ));
    let mut index = TantivyIndex::open_or_create(&scratch).unwrap();
    assert_eq!(index.doc_count().unwrap(), committed as u64);
    index.add_prebuilt_documents_slice(&[late]).unwrap();
    index.commit().unwrap();
    drop(index);
    let mut resume = cass(home.path());
    resume.args(["index", "--json"]);
    let started = Instant::now();
    Process::spawn(&mut resume, home.path(), "late-surplus-resume").success();
    eprintln!(
        "GH530 late-surplus recovery docs={} elapsed_ms={}",
        SESSIONS * messages,
        started.elapsed().as_millis()
    );
    assert_committed_count(&live, SESSIONS * messages);
    assert_eq!(checkpoint_at(&live)["completed"], true);
    assert_eq!(quarantines(&data).len(), 1);
}
