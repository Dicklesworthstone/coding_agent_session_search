//! Manual diagnostic for bead `coding_agent_session_search-mot85`.
//!
//! mot85 tracks the upstream fsqlite feature required to eliminate the
//! last two `rusqlite::Connection::open` call sites in cass
//! (`src/storage/sqlite.rs::rusqlite_test_fixture_conn`): supporting
//! `INSERT INTO sqlite_master` when `PRAGMA writable_schema = ON` is set.
//!
//! Run manually:
//!
//! ```bash
//! CARGO_TARGET_DIR=/tmp/rch_target_cass_cc1 cargo test --test _probe_mot85 -- --ignored --nocapture
//! ```
//!
//! When this test *passes*, close mot85 and bump the fsqlite rev in
//! `Cargo.toml` so cass can migrate the two remaining rusqlite sites to
//! frankensqlite. When it still fails with `Err(Internal("no such table:
//! sqlite_master"))`, mot85 is still blocked upstream.
//!
//! Do NOT run this as part of the regular test suite — it intentionally
//! panics on failure to surface the upstream status, and the `#[ignore]`
//! attribute keeps it out of CI.
#[test]
#[ignore = "manual diagnostic for bead mot85; run with --ignored"]
fn probe_mot85_fsqlite_writable_schema_writes() {
    use frankensqlite::Connection as FrankenConnection;
    let tmpdir = tempfile::tempdir().unwrap();
    let db_path = tmpdir.path().join("probe.db");
    let conn = FrankenConnection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute("CREATE TABLE foo (id INTEGER)").unwrap();
    let pragma = conn.execute("PRAGMA writable_schema = ON");
    eprintln!("[mot85-probe] writable_schema=ON result: {pragma:?}");
    let insert = conn.execute(
        "INSERT INTO sqlite_master(type, name, tbl_name, rootpage, sql) \
         VALUES('table', 'fake_tbl', 'fake_tbl', 0, 'CREATE TABLE fake_tbl(x)')",
    );
    eprintln!("[mot85-probe] INSERT sqlite_master result: {insert:?}");
    assert!(
        insert.is_ok(),
        "mot85 is still blocked upstream: INSERT INTO sqlite_master returned {insert:?}"
    );
}
