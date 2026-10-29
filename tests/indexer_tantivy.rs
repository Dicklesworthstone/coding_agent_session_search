use coding_agent_search::connectors::{NormalizedConversation, NormalizedMessage};
use coding_agent_search::search::tantivy::{SCHEMA_HASH, TantivyIndex};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

fn read_schema_hash(dir: &TempDir) -> String {
    let path = dir.path().join("schema_hash.json");
    fs::read_to_string(path).expect("schema_hash.json should exist")
}

#[test]
fn open_or_create_writes_schema_hash() {
    let dir = TempDir::new().unwrap();

    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");
    index.commit().unwrap();

    let schema_file = read_schema_hash(&dir);
    assert!(
        schema_file.contains(SCHEMA_HASH),
        "schema_hash.json should contain current schema hash"
    );

    // The published manifest is what marks the directory as a real index;
    // `meta.json` was the Tantivy-era equivalent.
    assert!(
        dir.path()
            .join(coding_agent_search::search::quill_bridge::QUILL_INDEX_MARKER)
            .exists(),
        "the published Quill manifest should be present"
    );
}

#[test]
fn open_or_create_reuses_when_hash_matches() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");
    index.commit().unwrap();

    let sentinel = dir.path().join("sentinel.txt");
    fs::write(&sentinel, b"keep").unwrap();
    drop(index); // release writer lock before reopening

    // Second open with matching schema hash should not delete existing files.
    let mut index_again = TantivyIndex::open_or_create(dir.path()).expect("reopen index");
    index_again.commit().unwrap();

    assert!(
        sentinel.exists(),
        "sentinel file should remain when schema hash matches (no rebuild)"
    );
}

#[test]
fn open_or_create_rebuilds_on_schema_mismatch() {
    let dir = TempDir::new().unwrap();

    // Seed directory with mismatched schema hash and a sentinel.
    fs::write(
        dir.path().join("schema_hash.json"),
        r#"{"schema_hash":"old-hash"}"#,
    )
    .unwrap();
    let sentinel = dir.path().join("sentinel.txt");
    fs::write(&sentinel, b"remove-me").unwrap();

    let mut index = TantivyIndex::open_or_create(dir.path()).expect("recreate index");
    index.commit().unwrap();

    // Directory was rebuilt, so sentinel should be gone and schema hash should be updated.
    assert!(
        !sentinel.exists(),
        "sentinel should be removed when index is rebuilt due to hash mismatch"
    );
    let schema_file = read_schema_hash(&dir);
    assert!(
        schema_file.contains(SCHEMA_HASH),
        "schema hash should be refreshed after rebuild"
    );
}

#[test]
fn optimize_if_idle_triggers_when_segment_threshold_met() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");

    // Create multiple tiny conversations and commit after each to generate multiple segments.
    for i in 0..5 {
        let conv = NormalizedConversation {
            agent_slug: "codex".into(),
            external_id: Some(format!("conv-{i}")),
            title: Some(format!("Conv {i}")),
            workspace: None,
            source_path: dir.path().join(format!("conv-{i}.jsonl")),
            started_at: Some(i),
            ended_at: Some(i),
            metadata: json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".into(),
                author: Some("user".into()),
                created_at: Some(i),
                content: format!("hello-{i}"),
                extra: json!({}),
                snippets: Vec::new(),
                invocations: Vec::new(),
            }],
        };
        index.add_conversation(&conv).expect("add conv");
        index.commit().expect("commit");
    }

    let pre_segments = index.segment_count();

    // Tantivy's merge policy may eagerly merge; only require trigger when threshold is met.
    let merged = index.optimize_if_idle().expect("optimize_if_idle");
    if pre_segments >= 4 {
        assert!(
            merged,
            "optimize_if_idle should trigger when segment threshold is met"
        );
    } else {
        assert!(
            !merged,
            "optimize_if_idle may skip when segments below threshold; got {pre_segments}"
        );
    }
}

#[test]
fn incremental_commit_preserves_existing_docs() {
    let dir = TempDir::new().unwrap();
    let mut index = TantivyIndex::open_or_create(dir.path()).expect("create index");

    // First conversation
    let conv_a = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("a".into()),
        title: Some("First".into()),
        workspace: None,
        source_path: dir.path().join("a.jsonl"),
        started_at: Some(1),
        ended_at: Some(1),
        metadata: json!({}),
        messages: vec![NormalizedMessage {
            idx: 0,
            role: "user".into(),
            author: Some("u".into()),
            created_at: Some(1),
            content: "first message".into(),
            extra: json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        }],
    };
    index.add_conversation(&conv_a).expect("add conv a");
    index.commit().expect("commit a");

    let reader = index.reader().expect("reader");
    let initial_docs = reader.doc_count().expect("doc count");
    assert_eq!(initial_docs, 1, "one doc after first commit");

    // Second conversation: incremental add, new commit should preserve prior doc
    let conv_b = NormalizedConversation {
        agent_slug: "codex".into(),
        external_id: Some("b".into()),
        title: Some("Second".into()),
        workspace: None,
        source_path: dir.path().join("b.jsonl"),
        started_at: Some(2),
        ended_at: Some(2),
        metadata: json!({}),
        messages: vec![NormalizedMessage {
            idx: 0,
            role: "assistant".into(),
            author: Some("u".into()),
            created_at: Some(2),
            content: "second message".into(),
            extra: json!({}),
            snippets: Vec::new(),
            invocations: Vec::new(),
        }],
    };
    index.add_conversation(&conv_b).expect("add conv b");
    index.commit().expect("commit b");

    let reader2 = index.reader().expect("reader2");
    let docs_after = reader2.doc_count().expect("doc count");
    assert_eq!(
        docs_after, 2,
        "incremental commit should retain existing docs and add new ones"
    );
}

mod quill_producer_metadata {
    use super::*;
    use anyhow::{Context, Result};
    use coding_agent_search::search::quill_bridge::{
        QuillCassFields, QuillLexicalPage, open_cass_reader, search_paginated, stored_text,
    };
    use frankensearch::quill::keeper::{Manifest, load_manifest_pair, pack_engine_version};
    use frankensearch::quill::query::{CassQueryFilters, CassQueryParser};
    use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;
    use frankensearch::quill::segment::{
        EncodedSegment, SectionInput, SectionKind, SegmentHeaderInput, SegmentReader,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    fn conversation(root: &Path, name: &str, agent: &str, content: &str) -> NormalizedConversation {
        NormalizedConversation {
            agent_slug: agent.to_owned(),
            external_id: Some(name.to_owned()),
            title: Some(name.to_owned()),
            workspace: Some(root.join("workspace")),
            source_path: root.join(format!("{name}.jsonl")),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_000_000),
            metadata: json!({}),
            messages: vec![NormalizedMessage {
                idx: 0,
                role: "user".to_owned(),
                author: None,
                created_at: Some(1_700_000_000_000),
                content: content.to_owned(),
                extra: json!({}),
                snippets: Vec::new(),
                invocations: Vec::new(),
            }],
        }
    }

    fn search(
        directory: &Path,
        query: &str,
        filters: &CassQueryFilters,
    ) -> Result<(QuillLexicalPage, Vec<Option<String>>)> {
        let reader = open_cass_reader(directory)?;
        let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)?;
        let parsed = parser.parse(query, filters);
        let page = search_paginated(&reader, &parsed.query, 10, 0, true)?;
        let titles = page
            .hits
            .iter()
            .map(|hit| stored_text(&reader, QuillCassFields::compiled().title, hit.global_docid))
            .collect::<Result<Vec<_>>>()?;
        Ok((page, titles))
    }

    fn manifest(directory: &Path, slot: &str) -> Result<Manifest> {
        Ok(Manifest::from_bytes(&fs::read(directory.join(slot))?)?)
    }

    fn segment_name(id: u64) -> String {
        format!("seg-{id:016x}.fslx")
    }

    /// Change only producer metadata in a private, closed copy of a real CASS
    /// index. The engine codec rebuilds checksums; payload sections, document
    /// identities, schema, generations and tombstones stay unchanged.
    ///
    /// These are synthetic version stamps, not artifacts from a 0.4.2 binary.
    /// The test is a compatibility regression, not release qualification.
    fn copy_with_mixed_producers(source: &Path, target: &Path) -> Result<()> {
        fs::copy(
            source.join("schema_hash.json"),
            target.join("schema_hash.json"),
        )?;
        let current = manifest(source, "MANIFEST")?;
        assert_eq!(
            current.segments.len(),
            2,
            "fixture needs two independent seals"
        );
        let producers = [pack_engine_version(0, 4, 0), pack_engine_version(0, 4, 2)];
        let mut witnesses = BTreeMap::new();
        for (segment, producer) in current.segments.iter().zip(producers) {
            let name = segment_name(segment.segment_id);
            let bytes = fs::read(source.join(&name))?;
            let reader = SegmentReader::from_bytes(&bytes, CASS_SEMANTIC_SCHEMA)?;
            let header = reader.header();
            let mut sections = Vec::new();
            for entry in reader.section_entries() {
                sections.push(SectionInput {
                    kind: entry.kind,
                    flags: entry.flags,
                    bytes: reader
                        .section(entry.kind)?
                        .context("missing fixture section")?,
                });
            }
            let encoded = EncodedSegment::encode(
                SegmentHeaderInput {
                    segment_id: header.segment_id,
                    schema: CASS_SEMANTIC_SCHEMA,
                    docid_lo: header.docid_lo,
                    docid_hi: header.docid_hi,
                    doc_count: header.doc_count,
                    created_unix_s: header.created_unix_s,
                    engine_version: producer,
                },
                &sections,
            )?;
            fs::write(target.join(name), encoded.as_bytes())?;
            witnesses.insert(
                segment.segment_id,
                (encoded.file_len(), encoded.file_xxh3()),
            );
        }
        for slot in ["MANIFEST", "MANIFEST.prev"] {
            let mut rewritten = manifest(source, slot)?;
            rewritten.engine_version = pack_engine_version(0, 4, 2);
            for segment in &mut rewritten.segments {
                let &(file_len, file_xxh3) = witnesses
                    .get(&segment.segment_id)
                    .context("previous generation references an unexpected segment")?;
                segment.file_len = file_len;
                segment.file_xxh3 = file_xxh3;
            }
            fs::write(target.join(slot), rewritten.to_bytes()?)?;
        }
        Ok(())
    }

    #[test]
    fn compatible_producers_preserve_indexing_search_and_recovery() -> Result<()> {
        let original = TempDir::new()?;
        let mixed = TempDir::new()?;
        let mut index = TantivyIndex::open_or_create(original.path())?;
        for document in [
            conversation(
                original.path(),
                "Memory fix",
                "codex",
                "migration memory memory sealing",
            ),
            conversation(
                original.path(),
                "Search baseline",
                "claude",
                "migration query compatibility",
            ),
        ] {
            index.add_conversation(&document)?;
            index.commit()?;
        }
        drop(index);

        let filters = CassQueryFilters::default();
        let baseline = search(original.path(), "migration", &filters)?;
        assert_eq!(baseline.0.doc_count, 2);
        assert_eq!(baseline.0.total_count, Some(2));
        assert_eq!(baseline.0.hits.len(), 2);
        let agent_filter = CassQueryFilters {
            agents: vec!["codex".to_owned()],
            ..CassQueryFilters::default()
        };
        let filtered = search(original.path(), "migration", &agent_filter)?;
        assert_eq!(filtered.0.total_count, Some(1));
        assert_eq!(filtered.1, vec![Some("Memory fix".to_owned())]);

        // All original readers and writers have been dropped before creating
        // the separate fixture. Never mutate a mapped published segment.
        copy_with_mixed_producers(original.path(), mixed.path())?;
        let schema_bytes = fs::read(mixed.path().join("schema_hash.json"))?;
        assert_eq!(
            schema_bytes,
            fs::read(original.path().join("schema_hash.json"))?
        );
        let mixed_manifest = manifest(mixed.path(), "MANIFEST")?;
        assert_eq!(mixed_manifest.schema_id, CASS_SEMANTIC_SCHEMA.schema_id()?);
        // Equality covers scores, ranks, opaque identities, global docids,
        // counts and stored-title hydration, not just the number of hits.
        assert_eq!(search(mixed.path(), "migration", &filters)?, baseline);
        assert_eq!(search(mixed.path(), "migration", &agent_filter)?, filtered);

        let appended = conversation(
            original.path(),
            "After reopen",
            "codex",
            "migration recovered append",
        );
        let mut writer = TantivyIndex::open_or_create(mixed.path())?;
        writer.add_conversation(&appended)?;
        writer.commit()?;
        drop(writer);
        let published = search(mixed.path(), "migration", &filters)?;
        assert_eq!(published.0.doc_count, 3);
        assert_eq!(published.0.total_count, Some(3));
        assert_eq!(published.0.hits.len(), 3);
        assert_eq!(
            search(mixed.path(), "recovered", &filters)?.1,
            vec![Some("After reopen".to_owned())]
        );
        assert_eq!(
            fs::read(mixed.path().join("schema_hash.json"))?,
            schema_bytes
        );

        // A torn current manifest must select the prior mixed-producer
        // generation, preserving its exact bytes and searchable data.
        let previous_bytes = fs::read(mixed.path().join("MANIFEST.prev"))?;
        assert_eq!(Manifest::from_bytes(&previous_bytes)?, mixed_manifest);
        let mut damaged = fs::read(mixed.path().join("MANIFEST"))?;
        damaged[0] ^= 1;
        fs::write(mixed.path().join("MANIFEST"), &damaged)?;
        assert_eq!(search(mixed.path(), "migration", &filters)?, baseline);
        assert_eq!(fs::read(mixed.path().join("MANIFEST"))?, damaged);
        let mut recovered = TantivyIndex::open_or_create(mixed.path())?;
        // Writer recovery retires the corrupt current slot; it need not
        // republish a current MANIFEST until the next real commit.
        assert_eq!(load_manifest_pair(mixed.path())?.manifest, mixed_manifest);
        assert_eq!(
            fs::read(mixed.path().join("MANIFEST.prev"))?,
            previous_bytes
        );
        assert_eq!(search(mixed.path(), "migration", &filters)?, baseline);
        assert_eq!(
            search(mixed.path(), "recovered", &filters)?.0.total_count,
            Some(0)
        );

        recovered.add_conversation(&appended)?;
        recovered.commit()?;
        drop(recovered);
        let replayed = search(mixed.path(), "migration", &filters)?;
        assert_eq!(replayed.0.doc_count, 3);
        assert_eq!(replayed.0.total_count, Some(3));
        assert_eq!(replayed.0.hits.len(), 3);
        // Recovery can reserve different global docids, and equal scores do
        // not promise tie ordering. Stable identities and hydration must agree.
        let identities = |result: &(QuillLexicalPage, Vec<Option<String>>)| {
            let mut rows: Vec<_> = result
                .0
                .hits
                .iter()
                .zip(&result.1)
                .map(|(hit, title)| (hit.document_id.clone(), title.clone()))
                .collect();
            rows.sort();
            rows
        };
        assert_eq!(identities(&replayed), identities(&published));

        // Compatible producer stamps must not relax payload integrity. Both
        // manifest slots still reference this original segment, so fallback
        // cannot conceal the damage by choosing an unrelated generation.
        let current_bytes = fs::read(mixed.path().join("MANIFEST"))?;
        let previous_bytes = fs::read(mixed.path().join("MANIFEST.prev"))?;
        let segment_id = mixed_manifest.segments[0].segment_id;
        for bytes in [&current_bytes, &previous_bytes] {
            assert!(
                Manifest::from_bytes(bytes)?
                    .segments
                    .iter()
                    .any(|s| s.segment_id == segment_id)
            );
        }
        let segment_path = mixed.path().join(segment_name(segment_id));
        let mut bytes = fs::read(&segment_path)?;
        let reader = SegmentReader::from_bytes(&bytes, CASS_SEMANTIC_SCHEMA)?;
        let postings = reader
            .section_entries()
            .iter()
            .find(|entry| entry.kind == SectionKind::POSTINGS && entry.len > 0)
            .context("fixture needs nonempty postings")?;
        let offset = usize::try_from(postings.offset)?;
        bytes[offset] ^= 1;
        fs::write(segment_path, bytes)?;
        let error = open_cass_reader(mixed.path())
            .err()
            .context("reader accepted corrupt postings")?;
        assert!(error.to_string().contains("checksum"), "{error:#}");
        assert!(TantivyIndex::open_or_create(mixed.path()).is_err());
        assert_eq!(fs::read(mixed.path().join("MANIFEST"))?, current_bytes);
        assert_eq!(
            fs::read(mixed.path().join("MANIFEST.prev"))?,
            previous_bytes
        );
        assert_eq!(
            fs::read(mixed.path().join("schema_hash.json"))?,
            schema_bytes
        );
        Ok(())
    }
}
