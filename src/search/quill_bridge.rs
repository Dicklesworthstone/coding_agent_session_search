//! Synchronous facade over the async Quill lexical engine.
//!
//! CASS's lexical layer is synchronous because Tantivy is synchronous. Quill is
//! `asupersync`-async. Rather than colour 54k lines of indexer call sites with
//! `async fn`, this module drives each Quill future to completion on the calling
//! thread — the same bridge pattern [`crate::franken_sync`] already uses for the
//! async FrankenSQLite engine, for exactly the same reason.
//!
//! The runtime lives in a thread-local slot and is *taken out* while a future is
//! being driven, so a reentrant bridge call finds the slot empty and builds a
//! fresh runtime instead of re-entering `block_on` on the same instance.
//! `Runtime::block_on` has no `Send` bound and saves/restores the ambient
//! runtime handle, so nesting inside a consumer's own `block_on` is safe.
//!
//! Every future is created, polled, and dropped entirely within one bridge
//! call, so engine state never crosses a thread boundary between poll steps.

use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use asupersync::runtime::{Runtime, RuntimeBuilder};
use frankensearch::Cx;
use frankensearch::quill::cass::{
    CASS_MERGE_COOLDOWN_MS, CASS_MERGE_SEGMENT_THRESHOLD, CassDocument as QuillCassDocument,
    CassMergeStatus,
};
use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;
use frankensearch::quill::{QuillConfig, QuillIndex, QuillSearchIndex, SchemaDocument};

/// Filename that marks a directory as a published Quill index.
///
/// Verified empirically rather than assumed: a freshly created CASS index
/// contains exactly `MANIFEST` and `LOCK`. The manifest is the published
/// authority, so its presence is what makes a directory readable.
pub const QUILL_INDEX_MARKER: &str = "MANIFEST";

/// Operator override for Quill's deterministic per-query work budget
/// (`QuillConfig::query_fuel_budget`, default 10,000,000 units). The budget is
/// what turns a pathological query into a fast typed refusal instead of an
/// unbounded scan, so raising it is a diagnostic/escape hatch, not a tuning
/// knob; the durable fix for fuel exhaustion is a compacted index (#441).
pub const CASS_QUILL_QUERY_FUEL_BUDGET_ENV: &str = "CASS_QUILL_QUERY_FUEL_BUDGET";

/// Whether `error` is Quill's typed query-fuel refusal (#441), anywhere in
/// its context chain. The engine reports it as `... query fuel exhausted after
/// N/M units ...`; callers use this to tell "the lexical engine refused the
/// work" apart from an index fault.
#[must_use]
pub fn is_query_fuel_exhausted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.to_string().contains("query fuel exhausted"))
}

/// Engine configuration every CASS reader and writer opens with.
///
/// Two deliberate departures from `QuillConfig::default()`:
///
/// * `max_visibility_lag_ms` is effectively infinite. Quill's default (1 s)
///   is a cross-process visibility contract: once unpublished changes are a
///   second old, the *next ingest call* seals every shard and publishes a new
///   MANIFEST on its own. CASS never relies on that — every reader opens the
///   snapshot CASS itself publishes through an explicit `commit()`, and the
///   rebuild checkpoint (`.lexical-rebuild-state.json`) is written around that
///   same commit. Letting the engine publish underneath the checkpoint is
///   exactly what #440 reported: an interrupted `--force-rebuild` left a
///   staging MANIFEST several conversations ahead of the durable cursor, so
///   the resumed run re-inserted already-live identities and Quill refused
///   the duplicate. It is also why #441's archives grew hundreds of tiny
///   segments: a seal per shard per second, each in its own sparse docid
///   lease, which the hole-ratio-gated tier merge can never fold. With
///   explicit publication only, a segment is sealed on the accumulation
///   budget or at commit, and the MANIFEST moves only when CASS says so.
/// * `query_fuel_budget` honours [`CASS_QUILL_QUERY_FUEL_BUDGET_ENV`].
///
/// Everything else keeps the engine default so the on-disk format and the
/// admission/merge semantics stay exactly what the pinned engine documents.
#[must_use]
pub fn cass_quill_config() -> QuillConfig {
    let mut config = QuillConfig {
        max_visibility_lag_ms: u64::MAX,
        ..QuillConfig::default()
    };
    if let Some(budget) = query_fuel_budget_override(
        dotenvy::var(CASS_QUILL_QUERY_FUEL_BUDGET_ENV)
            .ok()
            .as_deref(),
    ) {
        config.query_fuel_budget = budget;
    }
    config
}

/// Parse the fuel-budget override. Zero, garbage, and absent all mean "keep
/// the engine default" — a zero budget would refuse every query, which is
/// never what an operator setting this variable wants.
fn query_fuel_budget_override(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|value| value.trim().replace('_', "").parse::<u64>().ok())
        .filter(|budget| *budget > 0)
}

thread_local! {
    static DRIVER: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

pub(crate) fn shutdown_driver() -> bool {
    DRIVER
        .with(|slot| slot.borrow_mut().take())
        .is_none_or(|runtime| runtime.shutdown_timeout(std::time::Duration::from_secs(30)))
}

/// Drive one Quill future to completion on the calling thread.
///
/// The closure receives a per-call [`Cx`]. A fresh `Cx` per bridge call is
/// deliberate: a `Cx` carries cancellation and deadline state scoped to one
/// request, and reusing one across independent engine calls would let an
/// earlier cancellation silently poison a later unrelated call.
fn drive<T, F>(call: impl FnOnce(Cx) -> F) -> T
where
    F: Future<Output = T>,
{
    let runtime = DRIVER
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| {
            RuntimeBuilder::current_thread()
                .build()
                .expect("failed to build Quill sync-bridge runtime")
        });
    // Restore the runtime on the way out even if the driven future panics.
    //
    // The slot is TAKEN for the duration of the call so a reentrant bridge call
    // finds it empty and builds its own runtime instead of re-entering
    // `block_on` on this one. That take must be paired with a put-back on every
    // exit path: with a plain sequential restore, a panic escaping `block_on`
    // would unwind past it and leave the slot permanently empty, so every later
    // call on this thread would build a fresh runtime. Not a correctness bug —
    // a fresh runtime is always valid — but it silently converts a cached
    // runtime into a per-call allocation for the rest of the thread's life.
    struct RestoreOnDrop(Option<Runtime>);
    impl Drop for RestoreOnDrop {
        fn drop(&mut self) {
            if let Some(runtime) = self.0.take() {
                DRIVER.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    // Only reclaim the slot if it is still empty: a reentrant
                    // call may have built and parked its own runtime while this
                    // one was driving, and clobbering it would drop a live
                    // runtime that an outer frame is still using.
                    if slot.is_none() {
                        *slot = Some(runtime);
                    }
                });
            }
        }
    }

    // The guard OWNS the runtime across the call, so an unwind runs its Drop
    // with the runtime still in hand. Setting the guard after `block_on`
    // returns would be pointless: a panic unwinds before that assignment and
    // drops the runtime instead of parking it.
    let guard = RestoreOnDrop(Some(runtime));
    let output = guard
        .0
        .as_ref()
        .expect("sync-bridge runtime is present for the duration of the call")
        .block_on(async {
            let cx = Cx::for_request();
            call(cx).await
        });
    drop(guard);
    output
}

/// Refresh a reader to the latest published snapshot.
///
/// The Tantivy incumbent called `IndexReader::reload`, which re-mmaps the
/// directory. A Quill reader is bound to the snapshot it opened, and `refresh`
/// rebinds it to the current publication; it reports whether the binding
/// actually moved.
///
/// # Errors
///
/// Returns an error when the current publication cannot be opened.
pub fn refresh_reader(reader: &QuillSearchIndex) -> Result<bool> {
    drive(|cx| async move { reader.refresh(&cx).await })
        .map_err(|error| anyhow!("refreshing the Quill CASS reader: {error}"))
}

/// One ranked hit from a Quill lexical search.
///
/// Replaces the incumbent's `LexicalDocHit`, whose `doc_address` was a Tantivy
/// `(segment_ord, segment_doc_id)` pair. Quill addresses a document by its
/// snapshot-global id, and additionally carries the external document id, so a
/// consumer that only needs identity never has to touch stored fields at all.
#[derive(Debug, Clone, PartialEq)]
pub struct QuillLexicalDocHit {
    /// BM25 relevance score.
    pub bm25_score: f32,
    /// Zero-based rank within the returned page.
    pub rank: usize,
    /// Snapshot-global document id, used to read stored columns back.
    pub global_docid: u32,
    /// External document identity, as minted by
    /// [`frankensearch::quill::cass::cass_document_identity`].
    ///
    /// Deliberately NOT documented as a literal format string: the shape is the
    /// engine's to define and it has already changed once (it was
    /// `"{source_id}#{msg_idx}"` until that proved non-unique — one `source_id`
    /// covers every locally discovered conversation, so message 0 of each
    /// collided). Treat it as an opaque identity; if you need the parts, read
    /// the stored columns rather than parsing this.
    pub document_id: String,
}

/// One page of Quill lexical results.
#[derive(Debug, Clone, PartialEq)]
pub struct QuillLexicalPage {
    /// Ranked hits for the requested page.
    pub hits: Vec<QuillLexicalDocHit>,
    /// Exact match count when it was requested and computed.
    pub total_count: Option<usize>,
    /// Live document count in the searched snapshot.
    pub doc_count: usize,
}

/// Execute one already-parsed query against `reader`.
///
/// `exact_count` is the caller's decision, not the engine's: computing an exact
/// total over a large index costs a full scan, so the caller decides when that
/// is worth paying for and this reports `None` otherwise.
///
/// # Errors
///
/// Returns an error when execution, scoring, or collection fails.
pub fn search_paginated(
    reader: &QuillSearchIndex,
    query: &frankensearch::quill::query::Query,
    limit: usize,
    offset: usize,
    exact_count: bool,
) -> Result<QuillLexicalPage> {
    let result = drive(|cx| async move {
        reader.search_preparsed_paginated(&cx, query, limit, offset, exact_count)
    })
    .map_err(|error| anyhow!("executing a Quill lexical query: {error}"))?;
    Ok(QuillLexicalPage {
        hits: result
            .hits
            .iter()
            .enumerate()
            .map(|(rank, hit)| QuillLexicalDocHit {
                bm25_score: hit.score,
                rank,
                global_docid: hit.global_docid,
                document_id: hit.document_id.clone(),
            })
            .collect(),
        total_count: result
            .total_count
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX)),
        doc_count: usize::try_from(result.doc_count).unwrap_or(usize::MAX),
    })
}

/// Read one stored text column for a hit, if the column holds anything.
///
/// # Errors
///
/// Returns an error when the snapshot cannot be proven readable.
pub fn stored_text(
    reader: &QuillSearchIndex,
    field_ord: u16,
    global_docid: u32,
) -> Result<Option<String>> {
    let Some(bytes) = reader.stored_field_value(field_ord, global_docid)? else {
        return Ok(None);
    };
    // A stored text column holds its source UTF-8 bytes. Invalid UTF-8 here
    // would mean the column was written by something other than this schema's
    // ingest, so report it rather than lossily substituting replacement chars.
    Ok(Some(String::from_utf8(bytes).map_err(|error| {
        anyhow!("stored column {field_ord} for doc {global_docid} is not UTF-8: {error}")
    })?))
}

/// Read one stored numeric column as `i64`.
///
/// Scribe writes a stored numeric column as exactly eight little-endian bytes,
/// so anything else means the column was written by a different schema — that
/// is reported rather than silently truncated.
///
/// # Errors
///
/// Returns an error when the snapshot cannot be proven readable or the column
/// is not eight bytes wide.
pub fn stored_i64(
    reader: &QuillSearchIndex,
    field_ord: u16,
    global_docid: u32,
) -> Result<Option<i64>> {
    Ok(stored_numeric_bytes(reader, field_ord, global_docid)?.map(i64::from_le_bytes))
}

/// Read one stored numeric column as `u64`.
///
/// # Errors
///
/// Identical to [`stored_i64`].
pub fn stored_u64(
    reader: &QuillSearchIndex,
    field_ord: u16,
    global_docid: u32,
) -> Result<Option<u64>> {
    Ok(stored_numeric_bytes(reader, field_ord, global_docid)?.map(u64::from_le_bytes))
}

fn stored_numeric_bytes(
    reader: &QuillSearchIndex,
    field_ord: u16,
    global_docid: u32,
) -> Result<Option<[u8; 8]>> {
    let Some(bytes) = reader.stored_field_value(field_ord, global_docid)? else {
        return Ok(None);
    };
    let width = bytes.len();
    Ok(Some(<[u8; 8]>::try_from(bytes.as_slice()).map_err(
        |_| {
            anyhow!(
                "stored numeric column {field_ord} for doc {global_docid} is {width} bytes, not 8"
            )
        },
    )?))
}

/// Build a snippet generator for the content field from a query's terms.
///
/// The incumbent built its generator from a Tantivy query and rendered against
/// a retrieved `TantivyDocument`. Quill's generator is compiled from terms and
/// renders against source text, which suits this caller better: it already
/// hydrates content per hit, so no second document fetch is needed.
///
/// Term weights are derived from document frequency, and this supplies `1` for
/// every term rather than a real per-term count. Frequency only steers *which*
/// window is chosen among candidates — a uniform weight can pick a different
/// but still valid window, and never changes which terms are highlighted.
/// Passing `0` would silently drop the term entirely, so `1` is the correct
/// floor here.
#[must_use]
pub fn content_snippet_generator(
    terms: &[String],
    config: frankensearch::quill::SnippetConfig,
) -> frankensearch::quill::SnippetGenerator {
    use frankensearch::quill::{SnippetGenerator, SnippetTerm, schema::Analyzer};
    SnippetGenerator::new(
        Analyzer::CassHyphenNormalize,
        terms
            .iter()
            .filter(|term| !term.is_empty())
            .map(|term| SnippetTerm::new(term.clone(), 1)),
        config,
    )
}

/// Open a read-only CASS-schema reader on `path`.
///
/// The Tantivy incumbent took a `ReloadPolicy` here because a Tantivy reader
/// watches its directory and reloads. A Quill reader is bound to the published
/// snapshot it opened, so freshness is a matter of reopening rather than of
/// policy — there is no equivalent knob, and callers that want newer data open
/// again.
///
/// # Errors
///
/// Returns an error when the published snapshot cannot be opened.
pub fn open_cass_reader(path: &Path) -> Result<QuillSearchIndex> {
    drive(|cx| {
        let path = path.to_path_buf();
        async move {
            QuillSearchIndex::open_with_schema(&cx, path, CASS_SEMANTIC_SCHEMA, cass_quill_config())
                .await
        }
    })
    .map_err(|error| anyhow!("opening Quill CASS reader at {}: {error}", path.display()))
}

/// Field handles for the compiled CASS schema.
///
/// The Tantivy incumbent resolved these from a runtime schema read, because a
/// Tantivy `Field` is an opaque handle minted when the schema is built. Quill
/// field ordinals are fixed by the compiled `CASS_SEMANTIC_SCHEMA`, so this is
/// a constant table rather than a lookup — there is no failure mode where a
/// field is missing, and `conversation_id` is no longer `Option` because the
/// compiled schema always carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuillCassFields {
    pub agent: u16,
    pub workspace: u16,
    pub workspace_original: u16,
    pub source_path: u16,
    pub msg_idx: u16,
    pub created_at: u16,
    pub title: u16,
    pub content: u16,
    pub title_prefix: u16,
    pub content_prefix: u16,
    pub preview: u16,
    pub source_id: u16,
    pub origin_kind: u16,
    pub origin_host: u16,
    pub conversation_id: u16,
}

impl QuillCassFields {
    /// The pinned ordinals of the compiled CASS schema.
    #[must_use]
    pub const fn compiled() -> Self {
        use frankensearch::quill::cass::field;
        Self {
            agent: field::AGENT,
            workspace: field::WORKSPACE,
            workspace_original: field::WORKSPACE_ORIGINAL,
            source_path: field::SOURCE_PATH,
            msg_idx: field::MSG_IDX,
            created_at: field::CREATED_AT,
            title: field::TITLE,
            content: field::CONTENT,
            title_prefix: field::TITLE_PREFIX,
            content_prefix: field::CONTENT_PREFIX,
            preview: field::PREVIEW,
            source_id: field::SOURCE_ID,
            origin_kind: field::ORIGIN_KIND,
            origin_host: field::ORIGIN_HOST,
            conversation_id: field::CONVERSATION_ID,
        }
    }
}

impl Default for QuillCassFields {
    fn default() -> Self {
        Self::compiled()
    }
}

/// A Quill-backed CASS lexical index with a synchronous API.
///
/// Mirrors the surface `CassTantivyIndex` exposed so the calling layer keeps its
/// existing shape across the engine swap.
pub struct QuillCassIndex {
    index: QuillIndex,
    directory: PathBuf,
    /// Epoch milliseconds of the last compaction, 0 when never compacted.
    last_merge_ts: i64,
}

impl QuillCassIndex {
    /// Open an existing CASS-schema index, creating it when absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be created or the index
    /// cannot be opened under the CASS schema.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let directory = path.to_path_buf();
        // Create ONLY when the directory holds no published manifest.
        //
        // The obvious spelling — try `open_with_schema`, fall back to
        // `create_with_schema` on any error — is wrong: it converts every open
        // failure into "make a new index", so a corrupt manifest, a permission
        // error, or a schema mismatch would all silently produce an empty index
        // in place of the real one. Deciding on the marker's presence keeps
        // "absent" separate from "broken", and lets a genuine open failure
        // propagate.
        //
        // `create_with_schema` is create-or-open-compatible, so two processes
        // racing on a fresh directory both end up with the same index rather
        // than one failing.
        let index_exists = path.join(QUILL_INDEX_MARKER).exists();
        let index = drive(|cx| {
            let directory = directory.clone();
            async move {
                if index_exists {
                    QuillIndex::open_with_schema(
                        &cx,
                        directory,
                        CASS_SEMANTIC_SCHEMA,
                        cass_quill_config(),
                    )
                    .await
                } else {
                    QuillIndex::create_with_schema(
                        &cx,
                        directory,
                        CASS_SEMANTIC_SCHEMA,
                        cass_quill_config(),
                    )
                    .await
                }
            }
        })
        .map_err(|error| {
            anyhow!(
                "{} Quill CASS index at {}: {error}",
                if index_exists { "opening" } else { "creating" },
                path.display()
            )
        })?;
        let mut index = Self {
            index,
            directory: path.to_path_buf(),
            last_merge_ts: 0,
        };
        // A freshly created index has a writer but no published manifest, so
        // nothing on disk yet announces it as an index and no reader can open
        // it. Publish the empty snapshot immediately: a caller that indexes
        // zero documents (an empty corpus, or a rebuild that finds nothing)
        // must still leave behind a readable, contract-valid index rather than
        // a directory that later reads as "no index here".
        if !index_exists {
            index.commit()?;
        }
        Ok(index)
    }

    /// Index one batch of CASS documents.
    ///
    /// # Errors
    ///
    /// Returns an error when admission or accumulation refuses the batch.
    pub fn add_cass_documents(&mut self, documents: &[QuillCassDocument]) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let projected: Vec<SchemaDocument> = documents
            .iter()
            .map(QuillCassDocument::to_schema_document)
            .collect();
        drive(|cx| {
            let projected = &projected;
            let index = &self.index;
            async move { index.index_schema_documents(&cx, projected).await }
        })
        .map_err(|error| anyhow!("indexing CASS documents into Quill: {error}"))
    }

    /// Upsert one batch of CASS documents under their stable identities
    /// (source id + source path + canonical conversation id + message idx).
    ///
    /// Unlike [`Self::add_cass_documents`], a document whose identity is
    /// already live REPLACES it instead of appending a duplicate — the
    /// primitive the qhiv2 / gh#382 targeted partial-prefix reconcile needs:
    /// retrying the same source set converges to exactly one live document
    /// per identity.
    ///
    /// # Errors
    ///
    /// Returns an error when admission, identity resolution, or accumulation
    /// refuses the batch.
    pub fn upsert_cass_documents(&mut self, documents: &[QuillCassDocument]) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let projected: Vec<SchemaDocument> = documents
            .iter()
            .map(QuillCassDocument::to_schema_document)
            .collect();
        drive(|cx| {
            let projected = &projected;
            let index = &self.index;
            async move { index.upsert_schema_documents(&cx, projected).await }
        })
        .map_err(|error| anyhow!("upserting CASS documents into Quill: {error}"))
    }

    /// Publish everything staged since the last commit.
    ///
    /// # Errors
    ///
    /// Returns an error when publication fails.
    pub fn commit(&mut self) -> Result<()> {
        drive(|cx| {
            let index = &self.index;
            async move { index.commit(&cx).await }
        })
        .map(|_| ())
        .map_err(|error| anyhow!("committing the Quill CASS index: {error}"))
    }

    /// Delete every live document and publish the empty successor.
    ///
    /// # Errors
    ///
    /// Returns an error when the successor cannot be published.
    pub fn delete_all(&mut self) -> Result<()> {
        drive(|cx| {
            let index = &self.index;
            async move { index.delete_all(&cx).await }
        })
        .map_err(|error| anyhow!("clearing the Quill CASS index: {error}"))
    }

    /// Open a read handle on the published snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the published snapshot cannot be opened.
    pub fn reader(&self) -> Result<QuillSearchIndex> {
        drive(|cx| {
            let directory = self.directory.clone();
            async move {
                QuillSearchIndex::open_with_schema(
                    &cx,
                    directory,
                    CASS_SEMANTIC_SCHEMA,
                    cass_quill_config(),
                )
                .await
            }
        })
        .map_err(|error| anyhow!("opening the Quill CASS reader: {error}"))
    }

    /// Live document count in the published snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when publication authority cannot prove the snapshot
    /// readable.
    pub fn doc_count(&self) -> Result<u64> {
        Ok(self.reader()?.doc_count()?)
    }

    /// Durable directory backing this index.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.directory
    }

    /// Whether compaction is currently wanted, and why.
    #[must_use]
    pub fn merge_status(&self, segment_count: usize, now_ms: i64) -> CassMergeStatus {
        CassMergeStatus {
            segment_count,
            last_merge_ts: self.last_merge_ts,
            ms_since_last_merge: if self.last_merge_ts > 0 {
                now_ms - self.last_merge_ts
            } else {
                -1
            },
            merge_threshold: CASS_MERGE_SEGMENT_THRESHOLD,
            cooldown_ms: CASS_MERGE_COOLDOWN_MS,
        }
    }

    /// Record that a compaction completed at `now_ms`.
    pub fn note_merged(&mut self, now_ms: i64) {
        self.last_merge_ts = now_ms;
    }

    /// Index one batch of borrowed CASS documents.
    ///
    /// Quill's ingest owns its column values, so the borrowed form is
    /// materialized here. This keeps the caller's streaming shape — it never
    /// has to clone message bodies just to hold a batch — while paying the
    /// copy once, at the engine boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when admission or accumulation refuses the batch.
    pub fn add_cass_document_refs(
        &mut self,
        documents: &[frankensearch::quill::cass::CassDocumentRef<'_>],
    ) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let owned: Vec<QuillCassDocument> = documents
            .iter()
            .map(|document| QuillCassDocument {
                agent: document.agent.to_owned(),
                workspace: document.workspace.map(str::to_owned),
                workspace_original: document.workspace_original.map(str::to_owned),
                source_path: document.source_path.to_owned(),
                msg_idx: document.msg_idx,
                created_at: document.created_at,
                title: document.title.map(str::to_owned),
                content: document.content.to_owned(),
                source_id: document.source_id.to_owned(),
                origin_kind: document.origin_kind.to_owned(),
                origin_host: document.origin_host.map(str::to_owned),
                conversation_id: document.conversation_id,
            })
            .collect();
        self.add_cass_documents(&owned)
    }

    /// Upsert one batch of borrowed CASS documents under their stable
    /// identities (see [`Self::upsert_cass_documents`]).
    ///
    /// # Errors
    ///
    /// Returns an error when admission, identity resolution, or accumulation
    /// refuses the batch.
    pub fn upsert_cass_document_refs(
        &mut self,
        documents: &[frankensearch::quill::cass::CassDocumentRef<'_>],
    ) -> Result<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let owned: Vec<QuillCassDocument> = documents
            .iter()
            .map(|document| QuillCassDocument {
                agent: document.agent.to_owned(),
                workspace: document.workspace.map(str::to_owned),
                workspace_original: document.workspace_original.map(str::to_owned),
                source_path: document.source_path.to_owned(),
                msg_idx: document.msg_idx,
                created_at: document.created_at,
                title: document.title.map(str::to_owned),
                content: document.content.to_owned(),
                source_id: document.source_id.to_owned(),
                origin_kind: document.origin_kind.to_owned(),
                origin_host: document.origin_host.map(str::to_owned),
                conversation_id: document.conversation_id,
            })
            .collect();
        self.upsert_cass_documents(&owned)
    }

    /// Live segment count in the published snapshot.
    ///
    /// Returns 0 when nothing is published yet, which is what the caller's
    /// merge policy treats as "nothing to merge".
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.reader()
            .ok()
            .and_then(|reader| reader.segment_count().ok())
            .unwrap_or(0)
    }

    /// Compact when the segment count and cooldown say it is worth doing.
    ///
    /// Runs the bounded policy in [`plan_bounded_segment_merge`]: every run of
    /// two or more adjacent *small* segments is folded into one, so an
    /// append-only archive converges to a handful of large segments plus one
    /// growing tail instead of accumulating a segment per session (#441).
    /// Tombstone-density compaction runs afterwards so replaced/deleted rows
    /// are reclaimed on the same cadence.
    ///
    /// Returns whether any merge or compaction actually ran.
    ///
    /// # Errors
    ///
    /// Returns an error when a merge or compaction itself fails.
    pub fn optimize_if_idle(&mut self, now_ms: i64) -> Result<bool> {
        let segments = self.segment_count();
        if !self.merge_status(segments, now_ms).should_merge() {
            return Ok(false);
        }
        let merged = self.merge_small_segment_runs()?;
        let compacted = self.compact_tombstones()?;
        if merged || compacted {
            self.note_merged(now_ms);
        }
        Ok(merged || compacted)
    }

    /// Fold every published segment into one, regardless of policy, then
    /// reclaim tombstones.
    ///
    /// This is the end-of-rebuild step: a from-scratch build seals one segment
    /// per ingest shard per accumulation budget, and the query planner's
    /// per-segment dictionary probes make segment count the dominant query
    /// cost (#441), so a freshly published generation should start at one.
    ///
    /// # Errors
    ///
    /// Returns an error when the merge or compaction fails. Requires a fully
    /// committed index (call [`Self::commit`] first).
    pub fn force_merge(&mut self) -> Result<()> {
        let segments = self.published_segment_profile()?;
        if segments.len() >= 2 {
            let source_ids: Vec<u64> = segments.iter().map(|segment| segment.0).collect();
            self.concat_merge_run(&source_ids)?;
        }
        self.compact_tombstones()?;
        Ok(())
    }

    /// `(segment_id, live_doc_count)` for every published segment, in manifest
    /// (docid) order — the order [`QuillIndex::concat_merge`] requires a source
    /// run to be consecutive in.
    fn published_segment_profile(&self) -> Result<Vec<(u64, u64)>> {
        let snapshot = self
            .index
            .snapshot()
            .map_err(|error| anyhow!("reading the Quill CASS manifest: {error}"))?;
        Ok(snapshot
            .segments()
            .iter()
            .map(|segment| {
                (
                    segment.manifest().segment_id,
                    u64::from(segment.manifest().live_doc_count()),
                )
            })
            .collect())
    }

    /// Apply [`plan_bounded_segment_merge`] until it has nothing left to fold.
    /// Returns whether at least one merge ran.
    fn merge_small_segment_runs(&mut self) -> Result<bool> {
        let mut merged_any = false;
        // Each iteration strictly reduces the segment count, so this loop is
        // bounded by the initial count; the cap is a belt-and-braces guard.
        for _ in 0..MAX_BOUNDED_MERGE_PASSES {
            let profile = self.published_segment_profile()?;
            let Some(run) = plan_bounded_segment_merge(&profile) else {
                break;
            };
            self.concat_merge_run(&run)?;
            merged_any = true;
        }
        Ok(merged_any)
    }

    /// Q1-preserving concat merge of one consecutive manifest run into a
    /// fresh segment. The output id is random and collision-checked against
    /// the live manifest, matching the engine's own id discipline.
    fn concat_merge_run(&mut self, source_segment_ids: &[u64]) -> Result<()> {
        let output_segment_id = fresh_segment_id(&self.published_segment_profile()?);
        let created_unix_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        drive(|cx| {
            let index = &self.index;
            async move {
                index
                    .concat_merge(&cx, source_segment_ids, output_segment_id, created_unix_s)
                    .await
            }
        })
        .map(|_| ())
        .map_err(|error| {
            anyhow!(
                "concat-merging {} Quill CASS segments: {error}",
                source_segment_ids.len()
            )
        })
    }

    /// Density-driven tombstone compaction; a no-op on an append-only index.
    fn compact_tombstones(&mut self) -> Result<bool> {
        drive(|cx| {
            let index = &self.index;
            async move {
                index
                    .compact(&cx, frankensearch::quill::CompactionPolicy::default())
                    .await
            }
        })
        .map(|report| report.changed())
        .map_err(|error| anyhow!("compacting the Quill CASS index: {error}"))
    }

    /// Bulk-load merge policy hook.
    ///
    /// Tantivy needed its merge policy relaxed during a bulk load so the writer
    /// did not merge continuously while ingesting. Quill seals segments on
    /// budget and lease boundaries and compacts only when asked, so there is no
    /// equivalent knob and nothing to relax. Retained as a no-op so the
    /// caller's bulk-load sequence keeps its shape.
    pub const fn configure_bulk_load_merge_policy(&self) {}
}

/// Upper bound on bounded-merge passes per `optimize_if_idle` call. Each pass
/// removes at least one segment, so this can only bind on a manifest with
/// thousands of segments — and then it merely defers the remainder to the next
/// maintenance window instead of holding the writer for the whole backlog.
const MAX_BOUNDED_MERGE_PASSES: usize = 256;

/// Indexes at or below this many live documents are simply merged into one
/// segment: the rewrite is cheap and the tiering below would only add
/// bookkeeping. One Q1 lease (65,536 docids) is the natural boundary.
const BOUNDED_MERGE_SMALL_INDEX_DOCS: u64 = 1 << 16;

/// A segment holding at least `total / BOUNDED_MERGE_BIG_DIVISOR` live docs is
/// "big" and never rewritten by the bounded policy; only runs of small
/// segments are folded. Eight bounds the big-segment count at eight (each is
/// at least an eighth of the corpus) plus at most one small run between any
/// two, and keeps every incremental merge to at most an eighth of the index.
const BOUNDED_MERGE_BIG_DIVISOR: u64 = 8;

/// Pick the next run of adjacent segments to concat-merge, or `None` when the
/// manifest is already converged.
///
/// `profile` is `(segment_id, live_doc_count)` in manifest order. The policy
/// is size-tiered by *document count*, deliberately not by docid width: the
/// engine's own tier merge classifies by docid-range width and refuses runs
/// whose hull is more than half holes, and a CASS archive built session by
/// session is nothing but holes (every writer session leases fresh 65,536-wide
/// docid blocks per shard and burns the unused tail). A Q1 concat merge
/// preserves ids and tolerates those gaps, so counting live rows is the
/// measure that actually predicts merge cost and query cost here.
///
/// Returns the longest run of two or more adjacent small segments. Ties go to
/// the earliest run so the result is deterministic for a given manifest.
fn plan_bounded_segment_merge(profile: &[(u64, u64)]) -> Option<Vec<u64>> {
    if profile.len() < 2 {
        return None;
    }
    let total: u64 = profile
        .iter()
        .map(|(_, live)| *live)
        .fold(0, u64::saturating_add);
    if total <= BOUNDED_MERGE_SMALL_INDEX_DOCS {
        return Some(profile.iter().map(|(id, _)| *id).collect());
    }
    let big_threshold = (total / BOUNDED_MERGE_BIG_DIVISOR).max(1);
    let mut best: Option<(usize, usize)> = None;
    let mut run_start: Option<usize> = None;
    for (index, (_, live)) in profile.iter().enumerate() {
        let small = *live < big_threshold;
        match (small, run_start) {
            (true, None) => run_start = Some(index),
            (false, Some(start)) => {
                let len = index - start;
                if len >= 2 && best.is_none_or(|(_, best_len)| len > best_len) {
                    best = Some((start, len));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run_start {
        let len = profile.len() - start;
        if len >= 2 && best.is_none_or(|(_, best_len)| len > best_len) {
            best = Some((start, len));
        }
    }
    best.map(|(start, len)| {
        profile[start..start + len]
            .iter()
            .map(|(id, _)| *id)
            .collect()
    })
}

/// A random, nonzero segment id absent from `profile`.
fn fresh_segment_id(profile: &[(u64, u64)]) -> u64 {
    loop {
        let candidate: u64 = rand::random();
        if candidate != 0 && profile.iter().all(|(id, _)| *id != candidate) {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Segments referenced by the published MANIFEST, read through the same
    /// reader the merge policy consults.
    fn published_segment_count(index: &QuillCassIndex) -> usize {
        index.segment_count()
    }

    #[test]
    fn fuel_budget_override_parses_positive_integers_only() {
        assert_eq!(query_fuel_budget_override(None), None);
        assert_eq!(query_fuel_budget_override(Some("")), None);
        assert_eq!(query_fuel_budget_override(Some("0")), None);
        assert_eq!(query_fuel_budget_override(Some("lots")), None);
        assert_eq!(
            query_fuel_budget_override(Some(" 25000000 ")),
            Some(25_000_000)
        );
        assert_eq!(
            query_fuel_budget_override(Some("25_000_000")),
            Some(25_000_000)
        );
    }

    #[test]
    fn cass_config_publishes_only_on_explicit_commit() {
        let config = cass_quill_config();
        assert_eq!(
            config.max_visibility_lag_ms,
            u64::MAX,
            "#440/#441: the engine must never publish a MANIFEST underneath the rebuild checkpoint"
        );
        assert!(
            config.validate().is_ok(),
            "the CASS config must pass engine validation"
        );
        let default = QuillConfig::default();
        assert_eq!(config.tier_fanout, default.tier_fanout);
        assert_eq!(
            config.scribe_shard_budget_bytes,
            default.scribe_shard_budget_bytes
        );
        assert_eq!(config.query_fuel_budget, default.query_fuel_budget);
    }

    #[test]
    fn bounded_merge_plan_folds_small_runs_and_leaves_big_segments_alone() {
        // Tiny index: everything merges.
        assert_eq!(
            plan_bounded_segment_merge(&[(1, 10), (2, 20), (3, 30)]),
            Some(vec![1, 2, 3])
        );
        // Nothing to do with fewer than two segments.
        assert_eq!(plan_bounded_segment_merge(&[(1, 10)]), None);
        assert_eq!(plan_bounded_segment_merge(&[]), None);

        // Large index (total 800k, big threshold 100k): one big head, then a
        // run of small tail segments -> the tail run is the plan and the big
        // segment is untouched.
        let profile = [(7, 700_000), (8, 40_000), (9, 30_000), (10, 30_000)];
        assert_eq!(plan_bounded_segment_merge(&profile), Some(vec![8, 9, 10]));

        // Two runs: the longest wins; ties go to the earliest.
        let profile = [
            (1, 500_000),
            (2, 10),
            (3, 10),
            (4, 500_000),
            (5, 10),
            (6, 10),
            (7, 10),
        ];
        assert_eq!(plan_bounded_segment_merge(&profile), Some(vec![5, 6, 7]));
        let profile = [
            (1, 500_000),
            (2, 10),
            (3, 10),
            (4, 500_000),
            (5, 10),
            (6, 10),
        ];
        assert_eq!(plan_bounded_segment_merge(&profile), Some(vec![2, 3]));

        // Converged: only big segments, or isolated smalls between bigs.
        let profile = [(1, 300_000), (2, 300_000), (3, 300_000)];
        assert_eq!(plan_bounded_segment_merge(&profile), None);
        let profile = [(1, 300_000), (2, 5), (3, 300_000), (4, 5)];
        assert_eq!(plan_bounded_segment_merge(&profile), None);
    }

    #[test]
    fn fresh_segment_id_avoids_live_ids_and_zero() {
        let profile = [(1, 1), (2, 1)];
        for _ in 0..64 {
            let id = fresh_segment_id(&profile);
            assert!(id != 0 && id != 1 && id != 2);
        }
    }

    fn sample(source_id: &str, msg_idx: u64, content: &str) -> QuillCassDocument {
        QuillCassDocument {
            agent: "claude".to_owned(),
            workspace: Some("cass".to_owned()),
            workspace_original: Some("cass".to_owned()),
            source_path: format!("/transcripts/{source_id}.jsonl"),
            msg_idx,
            created_at: Some(1_700_000_000),
            title: Some("bridge session".to_owned()),
            content: content.to_owned(),
            source_id: source_id.to_owned(),
            origin_kind: "local".to_owned(),
            origin_host: None,
            conversation_id: Some(1),
        }
    }

    /// The bridge must drive a full write/commit/read cycle from sync code.
    ///
    /// This is the claim the whole module exists to support: cass's
    /// synchronous lexical layer can operate an async engine without any
    /// caller becoming `async`.
    #[test]
    fn bridge_round_trips_documents_without_an_async_caller() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let mut index = QuillCassIndex::open_or_create(directory.path()).expect("open or create");
        index
            .add_cass_documents(&[
                sample("alpha", 0, "the borrow checker rejected this lifetime"),
                sample("beta", 1, "tokenizer throughput regressed"),
            ])
            .expect("index documents");
        index.commit().expect("commit");
        assert_eq!(index.doc_count().expect("doc count"), 2);
    }

    /// A reentrant bridge call must not re-enter `block_on` on one runtime.
    ///
    /// The thread-local slot is emptied while a future is in flight precisely
    /// so this nests instead of panicking; without that, any Quill call made
    /// from inside another Quill call would abort.
    #[test]
    fn bridge_nests_without_reentering_one_runtime() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let index = QuillCassIndex::open_or_create(directory.path()).expect("open or create");
        let nested = drive(|_cx| async {
            // A second bridge call while the first future is being driven.
            index.doc_count()
        });
        assert_eq!(nested.expect("nested doc count"), 0);
    }

    /// #440: the engine must not publish a MANIFEST on its own visibility
    /// cadence. With the engine default (1 s) a second ingest call after a
    /// one-second pause seals and publishes everything staged so far, which
    /// is precisely how a staging index ran ahead of the rebuild checkpoint.
    #[test]
    fn engine_publishes_only_on_explicit_commit() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let mut index = QuillCassIndex::open_or_create(directory.path()).expect("open or create");
        index
            .add_cass_documents(&[sample("alpha", 0, "first staged batch")])
            .expect("stage first batch");
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        index
            .add_cass_documents(&[sample("alpha", 1, "second staged batch")])
            .expect("stage second batch");
        assert_eq!(
            index.doc_count().expect("published doc count"),
            0,
            "nothing may be published before CASS commits"
        );
        index.commit().expect("commit");
        assert_eq!(index.doc_count().expect("published doc count"), 2);
    }

    /// #440: a resumed rebuild re-adds documents the published authority may
    /// already hold. The upsert path must converge to one live document per
    /// identity where a plain add is (correctly) refused as a duplicate.
    #[test]
    fn upsert_refs_converge_where_add_refuses_duplicates() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let mut index = QuillCassIndex::open_or_create(directory.path()).expect("open or create");
        let published = [
            sample("alpha", 0, "published before the interruption"),
            sample("alpha", 1, "also published before the interruption"),
        ];
        index
            .add_cass_documents(&published)
            .expect("index published documents");
        index.commit().expect("publish");

        let duplicate = index.add_cass_documents(&published[..1]);
        assert!(
            duplicate
                .as_ref()
                .is_err_and(|error| error.to_string().contains("duplicate live document id")),
            "a plain re-add of a live identity must be refused: {duplicate:?}"
        );

        let replay = [
            published[0].clone(),
            published[1].clone(),
            sample("alpha", 2, "new after the interruption"),
        ];
        let refs: Vec<_> = replay.iter().map(QuillCassDocument::as_ref).collect();
        index
            .upsert_cass_document_refs(&refs)
            .expect("upsert converges on the published identities");
        index.commit().expect("publish replay");
        assert_eq!(index.doc_count().expect("doc count"), 3);
    }

    /// #441: one writer session per open leases fresh docid blocks, so every
    /// session leaves at least one segment the engine's width-tiered merge
    /// will never fold. The bounded policy must fold them and keep the corpus
    /// searchable.
    #[test]
    fn bounded_merge_folds_session_segments_into_one() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let sessions = 5_u64;
        for session in 0..sessions {
            let mut index = QuillCassIndex::open_or_create(directory.path()).expect("open session");
            index
                .add_cass_documents(&[
                    sample(&format!("s{session}"), 0, "session zero message alpha"),
                    sample(&format!("s{session}"), 1, "session one message beta"),
                ])
                .expect("index session documents");
            index.commit().expect("commit session");
        }
        let mut index = QuillCassIndex::open_or_create(directory.path()).expect("reopen");
        let before = published_segment_count(&index);
        assert!(
            before >= usize::try_from(sessions).expect("small count"),
            "each session must have left at least one segment, got {before}"
        );
        assert_eq!(index.doc_count().expect("doc count"), sessions * 2);

        // Cooldown never elapsed and nothing merged yet -> policy fires.
        let merged = index.optimize_if_idle(1_700_000_000_000).expect("optimize");
        assert!(merged, "five session segments exceed the merge threshold");
        assert_eq!(published_segment_count(&index), 1);
        assert_eq!(index.doc_count().expect("doc count"), sessions * 2);

        // A reader opened on the merged generation still finds every session.
        // The CASS-schema reader is preparsed-only, so go through the CASS
        // parser exactly as the search layer does.
        let reader = index.reader().expect("reader");
        let parser = frankensearch::quill::query::CassQueryParser::new(CASS_SEMANTIC_SCHEMA)
            .expect("CASS query parser");
        let parsed = parser.parse(
            "alpha",
            &frankensearch::quill::query::CassQueryFilters::default(),
        );
        let page = search_paginated(&reader, &parsed.query, 100, 0, false)
            .expect("search merged generation");
        assert_eq!(
            page.hits.len(),
            usize::try_from(sessions).expect("small count")
        );

        // Converged: another pass inside the cooldown is a no-op.
        assert!(!index.optimize_if_idle(1_700_000_000_001).expect("optimize"));

        // A further session appends a segment; force_merge folds it back.
        index
            .add_cass_documents(&[sample("late", 0, "late session alpha")])
            .expect("index late session");
        index.commit().expect("commit late session");
        assert!(published_segment_count(&index) >= 2);
        index.force_merge().expect("force merge");
        assert_eq!(published_segment_count(&index), 1);
        assert_eq!(index.doc_count().expect("doc count"), sessions * 2 + 1);
    }

    /// Fuel exhaustion is recognised by the engine's own error text, anywhere
    /// in the cause chain, and only that text (GH #441 degrade path).
    #[test]
    fn query_fuel_exhaustion_is_recognised_through_the_cause_chain() {
        let inner = anyhow!("scalar Quill query fuel exhausted after 10000000/10000000 units");
        let wrapped = inner.context("executing a Quill lexical query");
        assert!(is_query_fuel_exhausted(&wrapped));
        assert!(!is_query_fuel_exhausted(&anyhow!(
            "opening the Quill CASS reader: manifest missing"
        )));
    }

    /// GH #441: an archive only ever appends, so tombstone-driven compaction
    /// never fires and every commit seals a segment the engine's own tier
    /// policy does not fold (measured here: 40 commits → 40 segments with the
    /// production configuration alone). What bounds growth in production is
    /// the post-run maintenance step `cass index` calls after every
    /// incremental run (`optimize_if_idle`) — so that is the invariant this
    /// pins: after forty append-only commits, one maintenance pass folds the
    /// generation well below the merge threshold and keeps every document.
    /// No-claim: this proves consolidation happens, not its cost.
    #[test]
    fn post_run_maintenance_bounds_segment_growth_on_append_only_commits() {
        let directory = tempfile::tempdir().expect("bridge index directory");
        let mut index = QuillCassIndex::open_or_create(directory.path()).expect("open or create");
        let commits = 40_u64;
        for commit in 0..commits {
            let batch: Vec<QuillCassDocument> = (0..8)
                .map(|slot| {
                    let source = format!("session-{commit}");
                    sample(
                        &source,
                        slot,
                        &format!("commit {commit} message {slot} about borrow checker lifetimes"),
                    )
                })
                .collect();
            index.add_cass_documents(&batch).expect("index batch");
            index.commit().expect("commit batch");
        }
        let before = published_segment_count(&index);
        assert!(
            before >= CASS_MERGE_SEGMENT_THRESHOLD,
            "precondition: append-only commits must leave unfolded segments (got {before})"
        );

        let merged = index.optimize_if_idle(1_700_000_000_000).expect("optimize");
        assert!(
            merged,
            "the post-run maintenance pass must fold {before} segments"
        );
        let after = published_segment_count(&index);
        assert!(
            after < CASS_MERGE_SEGMENT_THRESHOLD,
            "maintenance left {after} segments (threshold {CASS_MERGE_SEGMENT_THRESHOLD})"
        );
        assert_eq!(index.doc_count().expect("doc count"), commits * 8);
    }
}
