// Harness-only admission into the exact production raw-I/O observer. The
// closure performs a real FIFO open; no filesystem implementation is replaced.
pub(crate) fn harness_open_fifo(
    path: &Path,
    progress: &Arc<crate::indexer::IndexingProgress>,
) -> std::io::Result<fs::File> {
    let ticking = Arc::clone(progress);
    let stopping = Arc::clone(progress);
    let _scope = observe_raw_mirror_io(
        Arc::new(move || ticking.tick_activity()),
        Arc::new(move || stopping.stop_requested()),
    );
    raw_mirror_io("harness_open_source_fifo", path, || fs::File::open(path))
}
