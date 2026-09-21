//! Select a message context using only streamed (id, idx) pairs.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Anchor {
    pub id: i64,
    pub idx: i64,
    pub number: usize,
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
}

impl Selection {
    pub fn new(target: usize, context: usize) -> Self {
        Self {
            anchors: VecDeque::new(),
            total: 0,
            found: false,
            target,
            context,
            remaining_after: context,
            previous: None,
        }
    }

    pub fn observe(&mut self, id: i64, idx: i64) -> Result<(), &'static str> {
        let number = usize::try_from(idx)
            .ok()
            .and_then(|idx| idx.checked_add(1))
            .ok_or("Archive contains an invalid message index")?;
        if self.previous.is_some_and(|previous| number <= previous) {
            return Err("Archive contains duplicate or unordered message indices");
        }
        self.previous = Some(number);
        self.total = self.total.checked_add(1).ok_or("Message count overflow")?;
        let anchor = Anchor { id, idx, number };
        if self.found {
            if self.remaining_after > 0 {
                self.anchors.push_back(anchor);
                self.remaining_after -= 1;
            }
        } else if number == self.target {
            self.anchors.push_back(anchor);
            self.found = true;
        } else if number < self.target && self.context > 0 {
            if self.anchors.len() == self.context {
                let _ = self.anchors.pop_front();
            }
            self.anchors.push_back(anchor);
        }
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
                let mut selection = Selection::new(target, context);
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
        let mut selection = Selection::new(100_001, 2);
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
            let mut selection = Selection::new(1, 0);
            selection.observe(1, 0).unwrap();
            selection.observe(2, 1).unwrap();
            assert!(selection.observe(3, bad).is_err());
        }
    }
}
