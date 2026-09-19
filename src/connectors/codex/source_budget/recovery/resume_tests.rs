//! Exercise the caller-owned reuse predicate across incomplete scans.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use super::*;
use super::tests::{corpus, message};
use crate::connectors::codex::CodexConnector;

#[test]
fn completed_sources_are_reused_while_the_failed_source_retries() -> Result<()> {
    let (_root, paths, mut ctx) = corpus()?;
    fs::write(&paths[1], format!("{}{{", message("incomplete")))?;
    // Model the host's metadata reuse decisions, not its persistence layer.
    let ledger = RefCell::new(HashMap::<PathBuf, DiscoveredSourceFile>::new());
    let connector = CodexConnector::new();
    for phase in 0..3 {
        if phase == 2 {
            fs::write(&paths[1], message("complete after retry"))?;
        }
        ctx.since_ts = (phase > 0).then_some(0);
        let mut admitted = Vec::new();
        let mut predicate = |source: &DiscoveredSourceFile| {
            if ledger.borrow().get(&source.source_path) == Some(source) {
                return false;
            }
            admitted.push(source.source_path.clone());
            true
        };
        let mut complete = |done: &SourceCompletion| {
            assert_eq!(done.conversations_emitted, 1);
            ledger
                .borrow_mut()
                .insert(done.source.source_path.clone(), done.source.clone());
            Ok(())
        };
        let mut hooks = SourceScanHooks {
            should_scan_source: Some(&mut predicate),
            on_source_complete: Some(&mut complete),
        };
        let mut delivered = Vec::new();
        let result = connector.scan_with_source_boundaries(&ctx, &mut hooks, &mut |item| {
            delivered.push(item.source_path);
            Ok(())
        });
        if phase == 2 {
            result?;
            assert_eq!(delivered, [paths[1].clone()]);
            assert_eq!(ledger.borrow().len(), 3);
        } else {
            let error = result.unwrap_err();
            let report = error.downcast_ref::<FailureReport>().unwrap();
            assert_eq!(report.failed_source_count, 1);
            assert_eq!(ledger.borrow().len(), 2);
            assert!(!ledger.borrow().contains_key(&paths[1]));
            if phase == 0 {
                assert_eq!(delivered, [paths[0].clone(), paths[2].clone()]);
            } else {
                assert!(delivered.is_empty());
            }
        }
        if phase == 0 {
            assert_eq!(admitted, paths);
        } else {
            assert_eq!(admitted, [paths[1].clone()]);
        }
    }
    Ok(())
}
