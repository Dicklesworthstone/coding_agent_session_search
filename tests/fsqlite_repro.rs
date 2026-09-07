use coding_agent_search::storage::sqlite::FrankenStorage;

#[test]
fn test_query_after_migrations() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");

    let fs = FrankenStorage::open(&db_path).unwrap();

    let rows = fs
        .raw()
        .query("SELECT 1 FROM meta LIMIT 1;")
        .expect("the migrated meta table must be queryable");
    assert_eq!(rows.len(), 1, "migrations must populate the meta table");

    assert!(
        fs.raw()
            .query("SELECT 1 FROM non_existent_table LIMIT 1;")
            .is_err(),
        "a missing table must return an error instead of an empty result"
    );
}
