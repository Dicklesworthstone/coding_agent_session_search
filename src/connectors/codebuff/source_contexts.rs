//! Narrow each admitted transcript without cloning the entire root selection.
//!
//! The index borrows the immutable selection and preserves the old first-match
//! rule (path plus complete Origin). A single parsing context owns at most one
//! root; all maps and progress hooks still come from the original request.

use std::collections::HashMap;
use std::path::Path;

use super::{DiscoveredSourceFile, ScanContext, ScanRoot};
use crate::connectors::{Origin, SourceKind};

#[derive(Hash, PartialEq, Eq)]
struct RootKey<'a> {
    path: &'a Path,
    source_id: &'a str,
    kind: SourceKind,
    host: Option<&'a str>,
}

impl<'a> RootKey<'a> {
    fn new(path: &'a Path, origin: &'a Origin) -> Self {
        // Exhaustive on the pinned FAD Origin: a future provenance field must
        // not silently be omitted from the lookup's identity contract.
        let Origin {
            source_id,
            kind,
            host,
        } = origin;
        Self {
            path,
            source_id,
            kind: *kind,
            host: host.as_deref(),
        }
    }
}

pub(super) struct SourceContexts<'a> {
    roots: HashMap<RootKey<'a>, &'a ScanRoot>,
    single: ScanContext,
    active: Option<&'a ScanRoot>,
}

impl<'a> SourceContexts<'a> {
    pub(super) fn new(ctx: &'a ScanContext) -> Self {
        let mut roots = HashMap::with_capacity(ctx.scan_roots.len());
        for root in &ctx.scan_roots {
            // `collect` would keep the last duplicate and change which
            // workspace rewrites belong to overlapping selected roots.
            roots
                .entry(RootKey::new(&root.path, &root.origin))
                .or_insert(root);
        }
        Self {
            roots,
            single: ScanContext {
                data_dir: ctx.data_dir.clone(),
                scan_roots: Vec::with_capacity(1),
                // Source admission, not a second timestamp cutoff, owns resume.
                since_ts: None,
                progress_tick: ctx.progress_tick.clone(),
            },
            active: None,
        }
    }

    pub(super) fn for_source(&mut self, source: &DiscoveredSourceFile) -> &ScanContext {
        let selected = self
            .roots
            .get(&RootKey::new(&source.scan_root, &source.origin))
            .copied();
        match (self.active, selected) {
            (Some(previous), Some(next)) if std::ptr::eq(previous, next) => {
                // Many chats share one selected store. Retain its immutable
                // rewrites instead of cloning them again for every transcript.
                self.single.scan_roots[0]
                    .path
                    .clone_from(&source.source_path);
            }
            _ => {
                let root = selected.map_or_else(
                    || {
                        ScanRoot::remote(
                            source.source_path.clone(),
                            source.origin.clone(),
                            source.platform,
                        )
                    },
                    |root| root.with_path(source.source_path.clone()),
                );
                self.single.scan_roots.clear();
                self.single.scan_roots.push(root);
            }
        }
        self.active = selected;
        &self.single
    }
}

#[cfg(test)]
mod tests;
