//! Lazy source-verified phases sharing one snapshot, byte account and deadline.
//! No thread or task is started here. An error drops the underlying continuation;
//! neither a later next() nor partial progress can restart native quality work.

use super::*;
use std::iter::FusedIterator;

struct HydratedStream<'snapshot, 'connection, I> {
    inner: Option<I>,
    remaining_phases: usize,
    transaction: &'snapshot Transaction<'connection>,
    projection: PassageProjection,
    work: Work,
}

impl<I, B> Iterator for HydratedStream<'_, '_, I>
where
    I: Iterator<Item = SemanticReaderResult<B>>,
    B: BatchSource,
{
    type Item = HydratedSearchResult<B>;

    fn next(&mut self) -> Option<Self::Item> {
        // Take ownership before native work. Both an error and a panic leave
        // this stream terminal; unwinding drops the local continuation rather
        // than letting a later next() start quality after failed verification.
        let mut inner = self.inner.take()?;
        if let Err(error) = self.work.check() {
            return Some(Err(error.into()));
        }
        let result = match inner.next()? {
            Ok(batch) => verified(batch, self.transaction, self.projection, &mut self.work),
            Err(error) => Err(error.into()),
        };
        self.remaining_phases -= 1;
        if result.is_ok() && self.remaining_phases > 0 {
            self.inner = Some(inner);
        }
        // Finish proactively after Refined. A late poll cannot manufacture a
        // third timeout item; cached candidates are released immediately too.
        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.inner {
            Some(inner) => (
                0,
                Some(
                    inner
                        .size_hint()
                        .1
                        .unwrap_or(self.remaining_phases)
                        .min(self.remaining_phases),
                ),
            ),
            None => (0, Some(0)),
        }
    }
}

impl<I, B> FusedIterator for HydratedStream<'_, '_, I>
where
    I: Iterator<Item = SemanticReaderResult<B>>,
    B: BatchSource,
{
}

fn stream<'snapshot, 'connection, I>(
    inner: I,
    transaction: &'snapshot Transaction<'connection>,
    projection: PassageProjection,
    work: Work,
) -> Result<HydratedStream<'snapshot, 'connection, I>, HydrationError> {
    work.check()?;
    Ok(HydratedStream {
        inner: Some(inner),
        remaining_phases: 2,
        transaction,
        projection,
        work,
    })
}

impl TextSemanticSearch<'_, '_> {
    /// Deliver verified Initial text before starting independent quality work.
    /// Both phases borrow the SAME caller-owned transaction. Keep its snapshot
    /// and filter stable until this iterator is dropped; it never finalizes it.
    ///
    /// Admission reserves 2*k passage slots. Source/output byte charges are
    /// cumulative across phases, including rereads; there is no cross-phase
    /// full-message cache. The deadline includes time between next() calls.
    /// Dropping after Initial does not execute quality inference or retrieval.
    pub fn progressive_hydrated_in_transaction<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        transaction: &'call Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<
        impl FusedIterator<Item = HydratedSearchResult<SemanticSearchBatch>> + 'call,
        HydratedSearchError,
    > {
        let work = prepare(k, 2, budget)?;
        Ok(stream(
            self.progressive(k, filter)?,
            transaction,
            projection,
            work,
        )?)
    }

    /// The same snapshot and fail-closed phase contract, using only already
    /// admitted native graphs and retaining the original per-shard receipts.
    pub fn progressive_hydrated_with_ann_in_transaction<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
        transaction: &'call Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<
        impl FusedIterator<Item = HydratedSearchResult<SemanticSearchBatch>> + 'call,
        HydratedSearchError,
    > {
        let work = prepare(k, 2, budget)?;
        Ok(stream(
            self.progressive_with_ann(k, filter, policy)?,
            transaction,
            projection,
            work,
        )?)
    }
}

impl SelectedTextSemanticSearch<'_, '_> {
    /// Both verified phases retain the exact selected publication, even when
    /// current.json advances. Initial stays Initial if refinement fails.
    /// Snapshot ownership stays with the caller; the byte/deadline account is
    /// shared by both phases, with worst-case 2*k passage admission.
    pub fn progressive_hydrated_in_transaction<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        transaction: &'call Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<
        impl FusedIterator<Item = HydratedSearchResult<SelectedSemanticBatch>> + 'call,
        HydratedSearchError,
    > {
        let work = prepare(k, 2, budget)?;
        Ok(stream(
            self.progressive(k, filter)?,
            transaction,
            projection,
            work,
        )?)
    }

    pub fn progressive_hydrated_with_ann_in_transaction<'call>(
        &'call self,
        k: usize,
        filter: Option<&'call dyn SearchFilter>,
        policy: AnnSearchPolicy,
        transaction: &'call Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<
        impl FusedIterator<Item = HydratedSearchResult<SelectedSemanticBatch>> + 'call,
        HydratedSearchError,
    > {
        let work = prepare(k, 2, budget)?;
        Ok(stream(
            self.progressive_with_ann(k, filter, policy)?,
            transaction,
            projection,
            work,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn expired_stream_never_polls_backend_and_is_permanently_fused()
    -> Result<(), HydratedSearchError> {
        let conn = Connection::open(":memory:").map_err(|_| HydrationError::Database)?;
        let mut transaction = conn.transaction().map_err(|_| HydrationError::Database)?;
        let polled = Cell::new(0);
        let inner = std::iter::from_fn(|| -> Option<SemanticReaderResult<SemanticSearchBatch>> {
            polled.set(polled.get() + 1);
            panic!("expired stream must not start native work");
        });
        let work = prepare(1, 2, HydrationBudget::default())?;
        let mut stream = stream(inner, &transaction, PassageProjection::RawPassagesV2, work)?;
        // Deterministic expiry at the real driver boundary, without sleeps or
        // a global clock hook affecting parallel tests.
        stream.work.budget.timeout = Duration::ZERO;
        assert!(matches!(
            stream.next(),
            Some(Err(HydratedSearchError::Hydration(
                HydrationError::DeadlineExceeded
            )))
        ));
        assert!(stream.next().is_none());
        assert!(stream.next().is_none());
        assert_eq!(polled.get(), 0);
        assert_eq!(stream.size_hint(), (0, Some(0)));
        drop(stream);
        transaction
            .rollback()
            .map_err(|_| HydrationError::Rollback)?;
        Ok(())
    }

    #[test]
    fn retrieval_error_drops_continuation_without_polling_it_again()
    -> Result<(), HydratedSearchError> {
        let conn = Connection::open(":memory:").map_err(|_| HydrationError::Database)?;
        let mut transaction = conn.transaction().map_err(|_| HydrationError::Database)?;
        let polled = Cell::new(0);
        let inner = std::iter::from_fn(|| -> Option<SemanticReaderResult<SemanticSearchBatch>> {
            polled.set(polled.get() + 1);
            Some(Err(SemanticReaderError::InvalidCandidate))
        });
        let mut stream = stream(
            inner,
            &transaction,
            PassageProjection::RawPassagesV2,
            prepare(1, 2, HydrationBudget::default())?,
        )?;
        assert!(matches!(
            stream.next(),
            Some(Err(HydratedSearchError::Retrieval(
                SemanticReaderError::InvalidCandidate
            )))
        ));
        assert!(stream.next().is_none());
        assert!(stream.inner.is_none());
        assert_eq!(polled.get(), 1);
        drop(stream);
        transaction
            .rollback()
            .map_err(|_| HydrationError::Rollback)?;
        Ok(())
    }

    #[test]
    fn caught_panic_cannot_resume_the_native_continuation() -> Result<(), HydratedSearchError> {
        let conn = Connection::open(":memory:").map_err(|_| HydrationError::Database)?;
        let mut transaction = conn.transaction().map_err(|_| HydrationError::Database)?;
        let polled = Cell::new(0);
        let inner = std::iter::from_fn(|| -> Option<SemanticReaderResult<SemanticSearchBatch>> {
            polled.set(polled.get() + 1);
            panic!("injected native continuation panic");
        });
        let mut stream = stream(
            inner,
            &transaction,
            PassageProjection::RawPassagesV2,
            prepare(1, 2, HydrationBudget::default())?,
        )?;
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| stream.next()));
        assert!(failed.is_err());
        assert!(stream.next().is_none());
        assert_eq!(stream.size_hint(), (0, Some(0)));
        assert_eq!(polled.get(), 1);
        drop(stream);
        transaction
            .rollback()
            .map_err(|_| HydrationError::Rollback)?;
        Ok(())
    }
}
