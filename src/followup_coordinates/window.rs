//! Select a message context using only streamed (id, idx) pairs.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Anchor {
    pub id: i64,
    pub idx: i64,
    pub number: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SelectionError {
    InvalidIndex(&'static str),
    ResourceLimit,
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIndex(reason) => formatter.write_str(reason),
            Self::ResourceLimit => formatter.write_str("canonical window exceeds its record limit"),
        }
    }
}

/// Retain only the requested context, while validating every canonical index.
/// No capacity is reserved from an untrusted CLI context value.
pub(super) struct Selection {
    pub anchors: VecDeque<Anchor>,
    pub total: usize,
    pub found: bool,
    target: usize,
    context: usize,
    remaining_after: usize,
    previous: Option<usize>,
    max_anchors: usize,
}

impl Selection {
    pub fn new(target: usize, context: usize, max_anchors: usize) -> Self {
        Self {
            anchors: VecDeque::new(),
            total: 0,
            found: false,
            target,
            context,
            remaining_after: context,
            previous: None,
            max_anchors,
        }
    }

    pub fn observe(&mut self, id: i64, idx: i64) -> Result<(), SelectionError> {
        let number = usize::try_from(idx)
            .ok()
            .and_then(|idx| idx.checked_add(1))
            .ok_or(SelectionError::InvalidIndex(
                "Archive contains an invalid message index",
            ))?;
        if self.previous.is_some_and(|previous| number <= previous) {
            return Err(SelectionError::InvalidIndex(
                "Archive contains duplicate or unordered message indices",
            ));
        }
        self.previous = Some(number);
        self.total = self
            .total
            .checked_add(1)
            .ok_or(SelectionError::InvalidIndex("Message count overflow"))?;
        let anchor = Anchor { id, idx, number };
        if self.found {
            if self.remaining_after > 0 {
                self.push(anchor)?;
                self.remaining_after -= 1;
            }
        } else if number == self.target {
            self.push(anchor)?;
            self.found = true;
        } else if number < self.target && self.context > 0 {
            if self.anchors.len() == self.context {
                let _ = self.anchors.pop_front();
            }
            self.push(anchor)?;
        }
        Ok(())
    }

    fn push(&mut self, anchor: Anchor) -> Result<(), SelectionError> {
        if self.anchors.len() >= self.max_anchors {
            return Err(SelectionError::ResourceLimit);
        }
        self.anchors.push_back(anchor);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_windows_match_a_full_vector_oracle_for_sparse_indices() {
        let indices = [0, 2, 7, 12, 49, 1000];
        for target in [0, 1, 2, 3, 7, 8, 12, 13, 50, 1001, usize::MAX] {
            for context in [0, 1, 2, 5, 6, 999, usize::MAX] {
                let mut selection = Selection::new(target, context, 4096);
                for (id, idx) in indices.into_iter().enumerate() {
                    selection.observe(id as i64 + 1, idx).unwrap();
                }
                let position = indices.iter().position(|idx| *idx as usize + 1 == target);
                assert_eq!(selection.found, position.is_some());
                assert_eq!(selection.total, indices.len());
                if let Some(position) = position {
                    let start = position.saturating_sub(context);
                    let end = position
                        .saturating_add(context)
                        .saturating_add(1)
                        .min(indices.len());
                    let selected: Vec<_> =
                        selection.anchors.iter().map(|anchor| anchor.idx).collect();
                    assert_eq!(selected, indices[start..end]);
                    let ids: Vec<_> = selection.anchors.iter().map(|anchor| anchor.id).collect();
                    assert_eq!(
                        ids,
                        (start..end).map(|idx| idx as i64 + 1).collect::<Vec<_>>()
                    );
                    assert_eq!(
                        selection
                            .anchors
                            .iter()
                            .filter(|anchor| anchor.number == target)
                            .count(),
                        1
                    );
                }
            }
        }
    }

    #[test]
    fn tiny_context_never_retains_the_whole_transcript() {
        let mut selection = Selection::new(100_001, 2, 4096);
        for idx in 0..200_000 {
            selection.observe(idx + 1, idx).unwrap();
            assert!(selection.anchors.len() <= 5);
        }
        assert_eq!(selection.total, 200_000);
        assert_eq!(selection.anchors.front().unwrap().idx, 99_998);
        assert_eq!(selection.anchors.back().unwrap().idx, 100_002);
    }

    #[test]
    fn invalid_tail_indices_are_not_hidden_by_a_completed_window() {
        for bad in [-1, 0, 1] {
            let mut selection = Selection::new(1, 0, 4096);
            selection.observe(1, 0).unwrap();
            selection.observe(2, 1).unwrap();
            assert!(selection.observe(3, bad).is_err());
        }
    }

    #[test]
    fn actual_retained_anchors_are_capped_before_during_and_after_the_target() {
        for target in [1, 3, 999] {
            let mut selection = Selection::new(target, usize::MAX, 4);
            for idx in 0..4 {
                selection.observe(idx + 1, idx).unwrap();
            }
            assert_eq!(selection.observe(5, 4), Err(SelectionError::ResourceLimit));
            assert_eq!(selection.anchors.len(), 4);
        }
        // A complete small window can validate an arbitrarily long tail without
        // spending its retention allowance on metadata it will not render.
        let mut selection = Selection::new(2, 1, 3);
        for idx in 0..1000 {
            selection.observe(idx + 1, idx).unwrap();
        }
        assert_eq!(selection.anchors.len(), 3);
        assert_eq!(selection.total, 1000);
    }
}
