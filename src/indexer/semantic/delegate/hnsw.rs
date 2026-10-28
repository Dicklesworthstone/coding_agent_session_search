//! Hold the CASS artifact lease through native graph verification/publication.
//! Native HNSW save locks serialize graph files, not their relationship to
//! backfill/rebuild/recovery. The caller still owns canonical source selection.

use super::*;
use frankensearch::index::HnswIndex;
use frankensearch::index::hnsw::HnswAppendDisposition;

pub(super) fn run(
    indexer: &SemanticIndexer,
    vector_index: &VectorIndex,
    data_dir: &Path,
    m: Option<usize>,
    ef_construction: Option<usize>,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<PathBuf> {
    check_cancelled()?;
    // Share the lock-only publication admission used by shards. In particular,
    // never sweep a caller's retained, unreferenced staging source before using
    // it to build a graph. Both ordinary success and failure only release the
    // lease; HNSW generation directories are not added to the recovery sweep.
    let _artifacts = super::super::artifacts::BackfillArtifacts::for_shard_publication(data_dir)?;
    check_cancelled()?;
    // Keep the native contract, configuration checks, existing-graph reuse and
    // readback in one implementation. Do not report cancellation after it has
    // acknowledged publication, or try to retire anything from the Drop path.
    indexer
        .inner
        .build_hnsw_index(vector_index, data_dir, m, ef_construction)
}

impl SemanticIndexer {
    /// Append to an existing native graph using retained, identity-complete
    /// previous/current FSVI generations (of9i7 / GH #460).
    ///
    /// This is a model-free maintenance API, not an automatic CLI migration.
    /// `ann_filename` names one `.chsw` file inside this archive's vector_index.
    /// The caller must hold its canonical maintenance lock from source selection
    /// through any subsequent manifest update, and retain both source readers.
    /// A source from another archive also needs that archive's coordination.
    /// This call takes the same artifact lease as backfill, rebuild and recovery.
    ///
    /// Only `Appended` publishes; `NoOp` verifies an already-current native graph.
    /// `FullRebuildRequired` leaves the graph untouched and NEVER starts a full
    /// build, embeds text, upgrades a legacy source, or promotes manifest readiness.
    /// The existing full-build API remains an explicit, separately budgeted choice.
    /// No older source or graph generation is reclaimed by this operation.
    ///
    /// `max_insertions` must be positive and bounds new graph insertions, NOT
    /// validation, load/save cost or process memory: the native engine still
    /// validates both complete sources and loads/saves the complete graph.
    /// Cancellation is cooperative around native calls, including immediately
    /// before save. It cannot interrupt a native call, and an acknowledged save
    /// is not reported as a cancellation because a signal arrived afterwards.
    pub fn append_hnsw_generation(
        previous: &VectorIndex,
        current: &VectorIndex,
        data_dir: &Path,
        ann_filename: &str,
        max_insertions: usize,
        mut check_cancelled: impl FnMut() -> Result<()>,
    ) -> Result<HnswAppendDisposition> {
        ensure!(
            max_insertions > 0,
            "HNSW append requires a positive insertion budget"
        );
        let mut components = Path::new(ann_filename).components();
        ensure!(
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none()
                && ann_filename.ends_with(".chsw")
                && !ann_filename.contains('\\')
                && !ann_filename.contains(':')
                && !ann_filename.chars().any(char::is_control),
            "HNSW append requires a single .chsw filename, not a path"
        );
        check_cancelled()?;
        if previous.wal_record_count() > 0 || current.wal_record_count() > 0 {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "source_wal_not_compacted",
            });
        }
        if previous.metadata().identity_v2.is_none() || current.metadata().identity_v2.is_none() {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "missing_source_identity",
            });
        }
        if previous.dimension() != current.dimension()
            || previous.embedder_id() != current.embedder_id()
        {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "embedding_identity_changed",
            });
        }

        // Do not create an archive, sweep scratch, or infer an old generation
        // from a filename. The retained readers are the caller's explicit inputs.
        let data_dir = data_dir.canonicalize()?;
        let _artifacts =
            super::super::artifacts::BackfillArtifacts::for_existing_writer(&data_dir)?;
        check_cancelled()?;
        let ann_path = data_dir
            .join(crate::search::vector_index::VECTOR_INDEX_DIR)
            .join(ann_filename);
        let before = ObservedSemanticFile::capture(&ann_path)?;
        if before.is_none() {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "prior_accelerator_missing",
            });
        }

        // Idempotent retry after a successful save, including a caller that
        // retained the previous input pair after losing the acknowledgement.
        let already_current = HnswIndex::try_load_native(&ann_path, current)?;
        check_cancelled()?;
        if already_current.is_some() {
            ensure!(
                ObservedSemanticFile::capture(&ann_path)? == before,
                "HNSW publication changed during current-generation verification"
            );
            return Ok(HnswAppendDisposition::NoOp);
        }
        // Do not keep a second native graph alive when admitting the prior one.
        drop(already_current);
        if current
            .record_count()
            .saturating_sub(previous.record_count())
            > max_insertions
        {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "append_budget_exceeded",
            });
        }
        let Some(graph) = HnswIndex::try_load_native(&ann_path, previous)? else {
            return Ok(HnswAppendDisposition::FullRebuildRequired {
                reason: "prior_accelerator_unavailable",
            });
        };
        check_cancelled()?;
        let config = graph.config();
        // This consuming API checks exact space/storage/producer/generation
        // binding, duplicate and missing IDs, and every old vector's bits.
        // A failed insertion discards its candidate, not the persisted graph.
        let (graph, disposition) = graph.append_from_vector_index(previous, current)?;
        check_cancelled()?;
        if !matches!(&disposition, HnswAppendDisposition::Appended { .. }) {
            return Ok(disposition);
        }
        ensure!(
            ObservedSemanticFile::capture(&ann_path)? == before,
            "HNSW publication changed while preparing the appended generation"
        );
        // The retained metadata handle is only needed for the comparison.
        // Release it before native atomic replacement, including on Windows.
        drop(before);
        graph
            .save(&ann_path)
            .context("publish appended native HNSW generation")?;
        drop(graph);
        let verified = HnswIndex::try_load_native(&ann_path, current)?;
        ensure!(
            verified
                .as_ref()
                .is_some_and(|graph| graph.config() == config),
            "appended HNSW generation failed native readback against its current source"
        );
        tracing::info!(
            path = %ann_path.display(),
            previous_records = previous.record_count(),
            current_records = current.record_count(),
            ?disposition,
            "published identity-bound incremental HNSW generation"
        );
        Ok(disposition)
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;
    use crate::search::embedder::Embedder;
    use crate::search::hash_embedder::HashEmbedder;
    use frankensearch::core::generation::{ArtifactGenerationIdentityV1, QuantizationFormat};
    use frankensearch::index::{FsviV2IdentityBinding, HnswConfig};
    use std::collections::BTreeMap;

    const ANN: &str = "incremental.chsw";
    const BASE: &[(u64, &str)] = &[
        (1, "compiler lexer parser syntax tree"),
        (2, "network socket packet router"),
    ];
    const ADDED: &[(u64, &str)] = &[
        (1, "compiler lexer parser syntax tree"),
        (2, "network socket packet router"),
        (3, "database transaction checkpoint storage"),
    ];

    fn document(message_id: u64) -> String {
        SemanticDocId {
            message_id,
            chunk_idx: 0,
            agent_id: 1,
            workspace_id: 2,
            source_id: 3,
            role: 1,
            created_at_ms: 100,
            content_hash: None,
        }
        .to_doc_id_string()
    }

    fn generation(
        root: &Path,
        name: &str,
        sequence: u8,
        rows: &[(u64, &str)],
    ) -> Result<VectorIndex> {
        let model = HashEmbedder::new(384);
        let mut identity = model.identity()?.clone();
        identity.storage.format = "fsvi-v2".into();
        identity.storage.quantization = QuantizationFormat::F16;
        identity.storage.endianness = "little-endian".into();
        let binding = FsviV2IdentityBinding::new(
            ArtifactGenerationIdentityV1::new(sequence.into(), [sequence; 16])?,
            identity.freeze()?,
        )?;
        let dir = root.join(crate::search::vector_index::VECTOR_INDEX_DIR);
        fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        let mut writer = VectorIndex::create_v2(&path, binding)?;
        for (id, text) in rows {
            writer.write_record(&document(*id), &model.embed_bound_sync(text)?.values)?;
        }
        writer.finish()?;
        Ok(VectorIndex::open_read_only(&path)?)
    }

    fn graph_path(root: &Path) -> PathBuf {
        root.join(crate::search::vector_index::VECTOR_INDEX_DIR)
            .join(ANN)
    }

    fn install_graph(root: &Path, source: &VectorIndex) -> Result<()> {
        HnswIndex::build_from_vector_index(source, HnswConfig::default())?
            .save(&graph_path(root))?;
        Ok(())
    }

    fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        fn visit(dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    visit(&entry.path(), files)?;
                } else {
                    files.insert(entry.path(), fs::read(entry.path())?);
                }
            }
            Ok(())
        }
        let mut files = BTreeMap::new();
        visit(
            &root.join(crate::search::vector_index::VECTOR_INDEX_DIR),
            &mut files,
        )?;
        Ok(files)
    }

    fn append(
        root: &Path,
        previous: &VectorIndex,
        current: &VectorIndex,
        budget: usize,
    ) -> Result<HnswAppendDisposition> {
        SemanticIndexer::append_hnsw_generation(previous, current, root, ANN, budget, || Ok(()))
    }

    #[test]
    fn append_publishes_current_native_graph_and_queries_old_and_new_evidence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        install_graph(root.path(), &previous)?;
        let source_dir = root
            .path()
            .join(crate::search::vector_index::VECTOR_INDEX_DIR);
        let source_bytes = [
            fs::read(source_dir.join("previous.fsvi"))?,
            fs::read(source_dir.join("current.fsvi"))?,
        ];
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::Appended { inserted: 1 }
        ));
        assert!(HnswIndex::try_load_native(&graph_path(root.path()), &previous)?.is_none());
        let graph = HnswIndex::try_load_native(&graph_path(root.path()), &current)?
            .context("current native graph")?;
        let model = HashEmbedder::new(384);
        for (id, text) in ADDED {
            let (hits, stats) =
                graph.knn_search_with_stats_against(&current, &model.embed_sync(text)?, 1, 64)?;
            assert_eq!(hits[0].doc_id, document(*id));
            assert!(
                stats.is_approximate,
                "the native path, not exact repair, must answer"
            );
        }
        assert_eq!(fs::read(source_dir.join("previous.fsvi"))?, source_bytes[0]);
        assert_eq!(fs::read(source_dir.join("current.fsvi"))?, source_bytes[1]);
        let before_retry = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::NoOp
        ));
        assert_eq!(snapshot(root.path())?, before_retry);
        Ok(())
    }

    #[test]
    fn refusals_do_not_rebuild_or_change_persisted_graphs() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        install_graph(root.path(), &previous)?;
        for (name, sequence, rows, reason) in [
            (
                "edited.fsvi",
                2,
                vec![BASE[0], (2, "completely changed mathematics"), ADDED[2]],
                "old_vector_changed",
            ),
            (
                "deleted.fsvi",
                3,
                vec![BASE[0], ADDED[2], (4, "unrelated additional message")],
                "old_document_missing",
            ),
            (
                "same-generation.fsvi",
                1,
                ADDED.to_vec(),
                "generation_not_advanced",
            ),
            ("not-growth.fsvi", 4, BASE.to_vec(), "not_an_append"),
            (
                "duplicate.fsvi",
                5,
                vec![BASE[0], BASE[1], ADDED[2], ADDED[2]],
                "duplicate_document_id",
            ),
        ] {
            let current = generation(root.path(), name, sequence, &rows)?;
            let before = snapshot(root.path())?;
            let disposition = append(root.path(), &previous, &current, 16)?;
            assert!(matches!(
                disposition,
                HnswAppendDisposition::FullRebuildRequired { reason: actual } if actual == reason
            ));
            assert_eq!(snapshot(root.path())?, before);
            assert!(HnswIndex::try_load_native(&graph_path(root.path()), &previous)?.is_some());
        }
        Ok(())
    }

    #[test]
    fn a_graph_bound_to_another_generation_cannot_authorize_append() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        let foreign = generation(root.path(), "foreign.fsvi", 3, BASE)?;
        // Even equal vectors and document IDs do not replace exact provenance.
        install_graph(root.path(), &foreign)?;
        let before = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &previous, &current, 16)?,
            HnswAppendDisposition::FullRebuildRequired {
                reason: "prior_accelerator_unavailable"
            }
        ));
        assert_eq!(snapshot(root.path())?, before);
        assert!(HnswIndex::try_load_native(&graph_path(root.path()), &foreign)?.is_some());
        Ok(())
    }

    #[test]
    fn pending_source_wal_is_never_compacted_or_certified_by_graph_maintenance() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        let path = root
            .path()
            .join(crate::search::vector_index::VECTOR_INDEX_DIR)
            .join("pending.fsvi");
        let model = HashEmbedder::new(384);
        let mut writer = VectorIndex::create_with_revision(
            &path,
            "fnv1a-384",
            "legacy",
            384,
            Quantization::F16,
        )?;
        writer.write_record(&document(1), &model.embed_sync(BASE[0].1)?)?;
        writer.finish()?;
        {
            let base = VectorIndex::open_read_only(&path)?;
            install_graph(root.path(), &base)?;
        }
        {
            let mut writer = VectorIndex::open_writer(&path)?;
            writer
                .append_batch(&[(document(99), model.embed_sync("new durable WAL evidence")?)])?;
        }
        let pending = VectorIndex::open_read_only(&path)?;
        assert!(pending.wal_record_count() > 0);
        let before = snapshot(root.path())?;
        for (old, new) in [(&pending, &current), (&previous, &pending)] {
            assert!(matches!(
                append(root.path(), old, new, 16)?,
                HnswAppendDisposition::FullRebuildRequired {
                    reason: "source_wal_not_compacted"
                }
            ));
            assert_eq!(snapshot(root.path())?, before);
        }
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn retained_vector_owners_survive_renamed_source_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        install_graph(root.path(), &previous)?;
        let dir = root
            .path()
            .join(crate::search::vector_index::VECTOR_INDEX_DIR);
        fs::rename(
            dir.join("previous.fsvi"),
            dir.join("retained-previous.fsvi"),
        )?;
        fs::rename(dir.join("current.fsvi"), dir.join("retained-current.fsvi"))?;
        let before = [
            fs::read(dir.join("retained-previous.fsvi"))?,
            fs::read(dir.join("retained-current.fsvi"))?,
        ];
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::Appended { inserted: 1 }
        ));
        assert_eq!(fs::read(dir.join("retained-previous.fsvi"))?, before[0]);
        assert_eq!(fs::read(dir.join("retained-current.fsvi"))?, before[1]);
        assert!(!dir.join("previous.fsvi").exists());
        assert!(!dir.join("current.fsvi").exists());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_graph_metadata_cannot_redirect_a_publication() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        let outside = root.path().join("unrelated-file");
        fs::write(&outside, b"preserve unrelated contents")?;
        std::os::unix::fs::symlink(&outside, graph_path(root.path()))?;
        assert!(append(root.path(), &previous, &current, 1).is_err());
        assert_eq!(fs::read(&outside)?, b"preserve unrelated contents");
        assert!(
            fs::symlink_metadata(graph_path(root.path()))?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn insertion_budget_refuses_before_mutation_but_does_not_prevent_noop_retry() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let mut rows = ADDED.to_vec();
        rows.push((4, "independent fourth retrieval evidence"));
        let current = generation(root.path(), "current.fsvi", 2, &rows)?;
        install_graph(root.path(), &previous)?;
        let before = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::FullRebuildRequired {
                reason: "append_budget_exceeded"
            }
        ));
        assert_eq!(snapshot(root.path())?, before);
        assert!(matches!(
            append(root.path(), &previous, &current, 2)?,
            HnswAppendDisposition::Appended { inserted: 2 }
        ));
        let before = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::NoOp
        ));
        assert_eq!(snapshot(root.path())?, before);
        Ok(())
    }

    #[test]
    fn legacy_sources_and_missing_graphs_remain_explicit_rebuild_decisions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let before = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::FullRebuildRequired {
                reason: "prior_accelerator_missing"
            }
        ));
        assert_eq!(snapshot(root.path())?, before);
        let path = root
            .path()
            .join(crate::search::vector_index::VECTOR_INDEX_DIR)
            .join("legacy.fsvi");
        let mut writer = VectorIndex::create_with_revision(
            &path,
            "fnv1a-384",
            "legacy",
            384,
            Quantization::F16,
        )?;
        writer.write_record(&document(1), &HashEmbedder::new(384).embed_sync(BASE[0].1)?)?;
        writer.finish()?;
        let legacy = VectorIndex::open_read_only(&path)?;
        install_graph(root.path(), &legacy)?;
        let before = snapshot(root.path())?;
        assert!(matches!(
            append(root.path(), &legacy, &current, 16)?,
            HnswAppendDisposition::FullRebuildRequired {
                reason: "missing_source_identity"
            }
        ));
        assert_eq!(snapshot(root.path())?, before);
        Ok(())
    }

    #[test]
    fn cancellation_and_competing_owner_preserve_graph_and_release_lease() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        install_graph(root.path(), &previous)?;
        for stop in 1..=5 {
            let before = snapshot(root.path())?;
            let mut calls = 0;
            let error = SemanticIndexer::append_hnsw_generation(
                &previous,
                &current,
                root.path(),
                ANN,
                1,
                || {
                    calls += 1;
                    anyhow::ensure!(calls != stop, "cancelled append fixture");
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(error.to_string().contains("cancelled append fixture"));
            assert_eq!(snapshot(root.path())?, before);
        }
        let lease =
            super::super::super::artifacts::BackfillArtifacts::for_existing_writer(root.path())?;
        let before = snapshot(root.path())?;
        assert!(append(root.path(), &previous, &current, 1).is_err());
        assert_eq!(snapshot(root.path())?, before);
        drop(lease);
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::Appended { inserted: 1 }
        ));
        Ok(())
    }

    #[test]
    fn changed_publication_is_not_overwritten_after_candidate_preparation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        install_graph(root.path(), &previous)?;
        let path = graph_path(root.path());
        let mut changed = fs::read(&path)?;
        changed.push(b'\n');
        let mut expected = snapshot(root.path())?;
        expected.insert(path.clone(), changed.clone());
        let mut calls = 0;
        let error = SemanticIndexer::append_hnsw_generation(
            &previous,
            &current,
            root.path(),
            ANN,
            1,
            || {
                calls += 1;
                if calls == 5 {
                    // Simulate a writer outside the CASS coordination contract.
                    // The changed metadata remains its work, not ours to undo.
                    fs::write(&path, &changed)?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("publication changed"));
        assert_eq!(snapshot(root.path())?, expected);
        Ok(())
    }

    #[test]
    fn callback_unwind_drops_candidate_and_releases_artifact_lease() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        install_graph(root.path(), &previous)?;
        let before = snapshot(root.path())?;
        let mut calls = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = SemanticIndexer::append_hnsw_generation(
                &previous,
                &current,
                root.path(),
                ANN,
                1,
                || {
                    calls += 1;
                    assert_ne!(calls, 5, "intentional pre-publication unwind");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());
        assert_eq!(snapshot(root.path())?, before);
        assert!(matches!(
            append(root.path(), &previous, &current, 1)?,
            HnswAppendDisposition::Appended { inserted: 1 }
        ));
        Ok(())
    }

    #[test]
    fn invalid_arguments_cannot_create_or_escape_an_archive() -> Result<()> {
        let root = tempfile::tempdir()?;
        let previous = generation(root.path(), "previous.fsvi", 1, BASE)?;
        let current = generation(root.path(), "current.fsvi", 2, ADDED)?;
        let absent = root.path().join("absent");
        for name in [
            "../escape.chsw",
            "/escape.chsw",
            "subdir/escape.chsw",
            "subdir\\escape.chsw",
            "C:escape.chsw",
            "current.fsvi",
            "",
        ] {
            assert!(
                SemanticIndexer::append_hnsw_generation(
                    &previous,
                    &current,
                    &absent,
                    name,
                    1,
                    || Ok(())
                )
                .is_err()
            );
            assert!(!absent.exists());
        }
        assert!(append(&absent, &previous, &current, 0).is_err());
        assert!(!absent.exists());
        Ok(())
    }
}
