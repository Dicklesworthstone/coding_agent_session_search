use coding_agent_search::storage::sqlite::FrankenStorage;

#[test]
fn test_query_after_migrations() {
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");

    let fs = FrankenStorage::open(&db_path).unwrap();

    // Instead of querying sqlite_master, try querying the table directly
    let res = fs.raw().query("SELECT 1 FROM meta LIMIT 1;");
    println!("query meta direct: {:?}", res.is_ok());

    let res = fs.raw().query("SELECT 1 FROM non_existent_table LIMIT 1;");
    println!("query non_existent: {:?}", res.is_ok());
}
