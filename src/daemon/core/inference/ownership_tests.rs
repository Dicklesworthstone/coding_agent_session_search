//! Exercise the real foreground dispatcher and chunk planner with controlled
//! producers. Hash vectors are real, but the registered MiniLM routing name is
//! a test double; these tests do not qualify native model accuracy or loading.

use super::*;
use crate::daemon::core::DaemonConfig;
use crate::daemon::models::ModelManager;
use crate::daemon::{
    DAEMON_ATTESTATION_PROTOCOL_REVISION, initialize_daemon_attestation_authority,
};
use crate::search::embedder::{EmbedderError, EmbedderResult};
use crate::search::reranker::{RerankDocument, RerankScore, Reranker, RerankerResult};
use frankensearch::core::{EmbeddingIdentityBundleV1, IdentityBoundEmbedding};
use frankensearch::{
    DAEMON_CONNECTION_IDENTITY_SCHEMA_V1, DaemonConnectionIdentityV1, HashAlgorithm, HashEmbedder,
    ModelCategory,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Hook = Mutex<Option<Box<dyn FnOnce() + Send>>>;

#[derive(Default)]
struct Calls {
    raw: AtomicUsize,
    bound: AtomicUsize,
    single: AtomicUsize,
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    ForeignResponse,
    NonFiniteResponse,
    ShortResponse,
    IdentityDrift,
}

struct Producer {
    hash: HashEmbedder,
    identity: EmbeddingIdentityBundleV1,
    foreign: EmbeddingIdentityBundleV1,
    drifted: AtomicBool,
    calls: Arc<Calls>,
    hook: Hook,
    fault: Fault,
    fail_at: usize,
}

impl Producer {
    fn new(algorithm: HashAlgorithm) -> Self {
        let hash = HashEmbedder::new(384, algorithm);
        let identity = frankensearch::Embedder::identity(&hash).unwrap().clone();
        let foreign_hash = HashEmbedder::new(384, HashAlgorithm::JLProjection { seed: 987 });
        let foreign = frankensearch::Embedder::identity(&foreign_hash)
            .unwrap()
            .clone();
        Self {
            hash,
            identity,
            foreign,
            drifted: AtomicBool::new(false),
            calls: Arc::new(Calls::default()),
            hook: Mutex::new(None),
            fault: Fault::None,
            fail_at: 0,
        }
    }

    fn infer(&self, texts: &[&str], bound: bool) -> EmbedderResult<Vec<Vec<f32>>> {
        let counter = if bound {
            &self.calls.bound
        } else {
            &self.calls.raw
        };
        let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let hook = self.hook.lock().take();
        if let Some(hook) = hook {
            hook();
        }
        if call == self.fail_at {
            return Err(EmbedderError::EmbeddingFailed {
                model: "injected-owner-failure".into(),
                source: Box::new(std::io::Error::other("injected later-chunk failure")),
            });
        }
        Ok(texts
            .iter()
            .map(|text| self.hash.embed_sync(text))
            .collect())
    }
}

impl Embedder for Producer {
    fn embed_sync(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        Ok(self.infer(&[text], false)?.remove(0))
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        self.infer(texts, false)
    }

    fn embed_bound_sync(&self, text: &str) -> EmbedderResult<IdentityBoundEmbedding> {
        self.calls.single.fetch_add(1, Ordering::SeqCst);
        Ok(IdentityBoundEmbedding {
            values: self.infer(&[text], true)?.remove(0),
            identity: self.identity.clone(),
        })
    }

    fn embed_batch_bound_sync(
        &self,
        texts: &[&str],
    ) -> EmbedderResult<Vec<IdentityBoundEmbedding>> {
        let mut values = self.infer(texts, true)?;
        if matches!(self.fault, Fault::ShortResponse) {
            values.pop();
        }
        if matches!(self.fault, Fault::NonFiniteResponse) {
            values[0][0] = f32::NAN;
        }
        if matches!(self.fault, Fault::IdentityDrift) {
            self.drifted.store(true, Ordering::SeqCst);
        }
        let identity = if matches!(self.fault, Fault::ForeignResponse) {
            &self.foreign
        } else {
            &self.identity
        };
        Ok(values
            .into_iter()
            .map(|values| IdentityBoundEmbedding {
                values,
                identity: identity.clone(),
            })
            .collect())
    }

    fn identity(&self) -> EmbedderResult<&EmbeddingIdentityBundleV1> {
        Ok(if self.drifted.load(Ordering::SeqCst) {
            &self.foreign
        } else {
            &self.identity
        })
    }
    fn id(&self) -> &str {
        "minilm-384"
    }
    fn dimension(&self) -> usize {
        384
    }
    fn is_semantic(&self) -> bool {
        false
    }
    fn category(&self) -> ModelCategory {
        ModelCategory::HashEmbedder
    }
}

fn daemon() -> (tempfile::TempDir, Arc<ModelDaemon>) {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Arc::new(ModelDaemon::new(
        DaemonConfig::default(),
        ModelManager::new(dir.path()),
    ));
    (dir, daemon)
}

fn texts() -> Vec<String> {
    (0..33)
        .map(|n| format!("authentication middleware item {n}"))
        .collect()
}

fn dispatch(daemon: &ModelDaemon, request: Request) -> Response {
    daemon.handle_request("owner-regression".into(), request, Duration::from_secs(30))
}

fn plain_request(texts: Vec<String>) -> Request {
    Request::Embed {
        texts,
        model: "default".into(),
        dims: Some(384),
    }
}

fn attested_request(daemon: &ModelDaemon, dir: &std::path::Path, texts: Vec<String>) -> Request {
    let (identity, category) = daemon.models.embedder_attestation_identity().unwrap();
    let (authority, generation) = initialize_daemon_attestation_authority(dir).unwrap();
    let state = DaemonAttestationState {
        connection: DaemonConnectionIdentityV1 {
            schema_version: DAEMON_CONNECTION_IDENTITY_SCHEMA_V1,
            endpoint_fingerprint: "11".repeat(32),
            executable_fingerprint: "22".repeat(32),
            protocol_revision: DAEMON_ATTESTATION_PROTOCOL_REVISION.into(),
            key_id: authority.key_id().into(),
            generation,
            embedding_identity: identity,
            model_category: category,
        },
        authority,
    };
    let input = texts.iter().map(String::as_str).collect::<Vec<_>>();
    let challenge = DaemonChallengeV1::for_inputs(
        "aa".repeat(32),
        DaemonOperationV1::EmbedBatch,
        &input,
        &state.connection,
    )
    .unwrap();
    *daemon.attestation.write() = Some(state);
    Request::EmbedAttested {
        texts,
        model: "default".into(),
        dims: Some(384),
        challenge,
    }
}

#[test]
fn plain_request_does_not_mix_producers_after_first_chunk_replacement() {
    let (_dir, daemon) = daemon();
    let input = texts();
    let original = Arc::new(Producer::new(HashAlgorithm::FnvModular));
    let replacement = Arc::new(Producer::new(HashAlgorithm::JLProjection { seed: 7 }));
    let expected: Vec<_> = input
        .iter()
        .map(|text| original.hash.embed_sync(text))
        .collect();
    let next_expected: Vec<_> = input
        .iter()
        .map(|text| replacement.hash.embed_sync(text))
        .collect();
    assert_ne!(
        expected, next_expected,
        "replacement must change actual vectors"
    );
    let old_calls = Arc::clone(&original.calls);
    let new_calls = Arc::clone(&replacement.calls);
    let weak = Arc::downgrade(&original);
    let models = Arc::downgrade(&daemon.models);
    *original.hook.lock() = Some(Box::new(move || {
        let models = models.upgrade().unwrap();
        models.unload_all();
        models.install_test_embedder(replacement);
    }));
    daemon.models.install_test_embedder(original);
    let Response::Embed(response) = dispatch(&daemon, plain_request(input.clone())) else {
        panic!("whole request failed after replacing a published slot");
    };
    assert_eq!(response.embeddings, expected);
    assert_eq!(response.model, "minilm-384");
    assert_eq!(old_calls.raw.load(Ordering::SeqCst), 3);
    assert_eq!(new_calls.raw.load(Ordering::SeqCst), 0);
    assert!(weak.upgrade().is_none(), "request leaked the evicted owner");
    let Response::Embed(next) = dispatch(&daemon, plain_request(input)) else {
        panic!("next request failed");
    };
    assert_eq!(next.embeddings, next_expected);
    assert_eq!(new_calls.raw.load(Ordering::SeqCst), 3);
}

#[test]
fn attested_request_survives_eviction_and_verifies_the_owner_that_produced_its_vectors()
-> TestResult {
    for replace in [false, true] {
        let (dir, daemon) = daemon();
        let input = texts();
        let original = Arc::new(Producer::new(HashAlgorithm::FnvModular));
        let expected: Vec<_> = input
            .iter()
            .map(|text| original.hash.embed_sync(text))
            .collect();
        let calls = Arc::clone(&original.calls);
        let weak = Arc::downgrade(&original);
        let models = Arc::downgrade(&daemon.models);
        *original.hook.lock() = Some(Box::new(move || {
            let models = models.upgrade().unwrap();
            models.unload_all();
            if replace {
                models.install_test_embedder(Arc::new(Producer::new(
                    HashAlgorithm::JLProjection { seed: 7 },
                )));
            }
        }));
        daemon.models.install_test_embedder(original);
        let request = attested_request(&daemon, dir.path(), input.clone());
        let Request::EmbedAttested { challenge, .. } = &request else {
            unreachable!()
        };
        let challenge = challenge.clone();
        let Response::AttestedEmbedding(response) = dispatch(&daemon, request) else {
            panic!("request lost its attested producer after slot eviction");
        };
        assert_eq!(response.vectors, expected);
        {
            let state = daemon.attestation.read();
            let state = state.as_ref().unwrap();
            response.attestation.validate_against(
                &challenge,
                &state.connection,
                &response.vectors,
            )?;
            response
                .attestation
                .authenticate_hmac_sha256(state.authority.secret())?;
        }
        assert_eq!(calls.bound.load(Ordering::SeqCst), 3);
        assert_eq!(
            calls.raw.load(Ordering::SeqCst),
            0,
            "attested override was bypassed"
        );
        assert_eq!(daemon.models.embedder_loaded(), replace);
        assert!(weak.upgrade().is_none());
        if replace {
            // The old connection identity must NOT vouch for the next producer.
            let response = dispatch(
                &daemon,
                Request::EmbedAttested {
                    texts: input,
                    model: "default".into(),
                    dims: Some(384),
                    challenge,
                },
            );
            assert!(
                matches!(response, Response::Error(error) if error.code == ErrorCode::ModelLoadFailed)
            );
        }
    }
    Ok(())
}

#[test]
fn bound_response_identity_and_shape_are_checked_before_signing() {
    for (fault, code) in [
        (Fault::ForeignResponse, ErrorCode::ModelLoadFailed),
        (Fault::IdentityDrift, ErrorCode::ModelLoadFailed),
        (Fault::NonFiniteResponse, ErrorCode::Internal),
        (Fault::ShortResponse, ErrorCode::Internal),
    ] {
        let (dir, daemon) = daemon();
        let mut producer = Producer::new(HashAlgorithm::FnvModular);
        producer.fault = fault;
        let calls = Arc::clone(&producer.calls);
        daemon.models.install_test_embedder(Arc::new(producer));
        let request = attested_request(&daemon, dir.path(), texts());
        let response = dispatch(&daemon, request);
        assert!(matches!(response, Response::Error(error) if error.code == code));
        assert_eq!(calls.bound.load(Ordering::SeqCst), 1);
        assert_eq!(calls.raw.load(Ordering::SeqCst), 0);
        assert!(daemon.inference_gate.try_enter().is_ok());
    }
}

#[test]
fn later_chunk_failure_withholds_the_entire_response_and_releases_its_owner() {
    for signed in [false, true] {
        let (dir, daemon) = daemon();
        let mut producer = Producer::new(HashAlgorithm::FnvModular);
        producer.fail_at = 2;
        let producer = Arc::new(producer);
        let calls = Arc::clone(&producer.calls);
        let weak = Arc::downgrade(&producer);
        let models = Arc::downgrade(&daemon.models);
        *producer.hook.lock() = Some(Box::new(move || models.upgrade().unwrap().unload_all()));
        daemon.models.install_test_embedder(producer);
        let request = if signed {
            attested_request(&daemon, dir.path(), texts())
        } else {
            plain_request(texts())
        };
        let response = dispatch(&daemon, request);
        assert!(matches!(response, Response::Error(error) if error.code == ErrorCode::Internal));
        assert_eq!(
            calls.bound.load(Ordering::SeqCst) + calls.raw.load(Ordering::SeqCst),
            2
        );
        assert!(!daemon.models.embedder_loaded());
        assert!(weak.upgrade().is_none());
        assert!(daemon.inference_gate.try_enter().is_ok());
    }
}

#[test]
fn shutdown_during_inference_never_signs_or_runs_another_chunk() {
    let (dir, daemon) = daemon();
    let producer = Arc::new(Producer::new(HashAlgorithm::FnvModular));
    let calls = Arc::clone(&producer.calls);
    let weak = Arc::downgrade(&producer);
    let stopping = Arc::downgrade(&daemon);
    *producer.hook.lock() = Some(Box::new(move || {
        let daemon = stopping.upgrade().unwrap();
        daemon.models.unload_all();
        daemon.shutdown.store(true, Ordering::Release);
    }));
    daemon.models.install_test_embedder(producer);
    let request = attested_request(&daemon, dir.path(), texts());
    let response = dispatch(&daemon, request);
    assert!(matches!(response, Response::Error(error) if error.code == ErrorCode::Overloaded));
    assert_eq!(calls.bound.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_none());
}

struct Scorer {
    score: f32,
    calls: Arc<AtomicUsize>,
    hook: Hook,
}

impl Reranker for Scorer {
    fn rerank_sync(
        &self,
        _: &str,
        documents: &[RerankDocument],
    ) -> RerankerResult<Vec<RerankScore>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let hook = self.hook.lock().take();
        if let Some(hook) = hook {
            hook();
        }
        // Reverse backend order to exercise the production ID restoration too.
        Ok(documents
            .iter()
            .enumerate()
            .rev()
            .map(|(rank, document)| RerankScore {
                doc_id: document.doc_id.clone(),
                score: self.score,
                original_rank: rank,
                raw_logit: None,
            })
            .collect())
    }
    fn id(&self) -> &str {
        FastEmbedReranker::reranker_id_static()
    }
    fn model_name(&self) -> &str {
        self.id()
    }
    fn is_available(&self) -> bool {
        true
    }
}

#[test]
fn reranking_uses_one_owner_across_chunks_and_next_request_uses_the_replacement() {
    let (_dir, daemon) = daemon();
    let calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let scorer = Arc::new(Scorer {
        score: 2.0,
        calls: Arc::clone(&calls),
        hook: Mutex::new(None),
    });
    let replacement = Arc::new(Scorer {
        score: 9.0,
        calls: Arc::clone(&new_calls),
        hook: Mutex::new(None),
    });
    let weak = Arc::downgrade(&scorer);
    let models = Arc::downgrade(&daemon.models);
    *scorer.hook.lock() = Some(Box::new(move || {
        let models = models.upgrade().unwrap();
        models.unload_all();
        models.install_test_reranker(replacement);
    }));
    daemon.models.install_test_reranker(scorer);
    let request = || Request::Rerank {
        query: "authentication".into(),
        documents: texts(),
        model: "default".into(),
    };
    let Response::Rerank(response) = dispatch(&daemon, request()) else {
        panic!("rerank request failed");
    };
    assert_eq!(response.scores, vec![2.0; 33]);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(new_calls.load(Ordering::SeqCst), 0);
    assert!(weak.upgrade().is_none());
    let Response::Rerank(next) = dispatch(&daemon, request()) else {
        panic!("next rerank failed");
    };
    assert_eq!(next.scores, vec![9.0; 33]);
    assert_eq!(new_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn singleton_attested_operations_call_the_corresponding_producer_override() -> TestResult {
    for operation in [DaemonOperationV1::Embed, DaemonOperationV1::EmbedBatch] {
        let (dir, daemon) = daemon();
        let producer = Arc::new(Producer::new(HashAlgorithm::FnvModular));
        let calls = Arc::clone(&producer.calls);
        let text = "singleton request";
        let expected = producer.hash.embed_sync(text);
        daemon.models.install_test_embedder(producer);
        let mut request = attested_request(&daemon, dir.path(), vec![text.to_owned()]);
        let challenge = {
            let state = daemon.attestation.read();
            DaemonChallengeV1::for_inputs(
                "bb".repeat(32),
                operation,
                &[text],
                &state.as_ref().unwrap().connection,
            )?
        };
        if let Request::EmbedAttested {
            challenge: value, ..
        } = &mut request
        {
            *value = challenge.clone();
        }
        let Response::AttestedEmbedding(response) = dispatch(&daemon, request) else {
            panic!("singleton attested operation failed");
        };
        assert_eq!(response.vectors, vec![expected]);
        assert_eq!(calls.raw.load(Ordering::SeqCst), 0);
        assert_eq!(calls.bound.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls.single.load(Ordering::SeqCst),
            usize::from(operation == DaemonOperationV1::Embed)
        );
        let state = daemon.attestation.read();
        let state = state.as_ref().unwrap();
        response
            .attestation
            .validate_against(&challenge, &state.connection, &response.vectors)?;
        response
            .attestation
            .authenticate_hmac_sha256(state.authority.secret())?;
    }
    Ok(())
}
